//! Hooks, shims and the CLI together, wired the way SessionStart wires them:
//! the env file's AGENT_LOCK_AGENT_ID and PATH, the shim links it creates,
//! claims the edit hooks make and the shims and the CLI read. Each scenario
//! plays real multi-agent sessions in a checkout with a bare remote.
mod common;
use common::*;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SUB1: &str = "a1111111122222222"; // Claude Code agent_id → aaaaaaaa:11111111 in SA
const SUB2: &str = "a3333333344444444"; // → aaaaaaaa:33333333
const A1: &str = "aaaaaaaa:11111111";
const A2: &str = "aaaaaaaa:33333333";

/// Another live process to own a session ("another Claude Code").
struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn other_claude() -> Proc {
    Proc(Command::new("sleep").arg("60").spawn().unwrap())
}

/// SessionStart with an env file; returns the variables it exported.
fn start(r: &Repo, session: &str, pid: u32) -> Vec<(&'static str, String)> {
    let envf = r.base.join(format!("env-{session}"));
    std::fs::write(&envf, "").unwrap();
    let p = payload(r, session, r#""hook_event_name":"SessionStart","source":"startup""#);
    let o = run_bin(r, None, &["hook", "session-start"], &r.dir, &[("CLAUDE_PID", pid.to_string()), ("CLAUDE_ENV_FILE", envf.display().to_string())], &p);
    assert_eq!(o.status, 0, "{}", o.stderr);
    let text = std::fs::read_to_string(&envf).unwrap();
    let id = text.lines().find_map(|l| l.strip_prefix("export AGENT_LOCK_AGENT_ID=")).expect("agent id exported").to_string();
    // shims/core, then one dir per package manager (the env line adds those the shell finds)
    let d = text.lines().find_map(|l| l.split("export PATH=\"").nth(1)?.split("/core:").next().map(str::to_string)).expect("PATH exported");
    let shims = ["core", "npm", "pnpm", "yarn", "bun"].map(|t| format!("{d}/{t}")).join(":");
    assert!(text.lines().any(|l| l == "export GIT_OPTIONAL_LOCKS=0"), "{text}");
    vec![
        ("AGENT_LOCK_AGENT_ID", id),
        ("PATH", format!("{shims}:{}", std::env::var("PATH").unwrap())),
        ("CLAUDE_PID", pid.to_string()),
        ("GIT_OPTIONAL_LOCKS", "0".to_string()),
    ]
}

fn edit_file(r: &Repo, session: &str, f: &Path, tu: &str, pid: u32) {
    let pre = hook(r, "pre-edit", &payload(r, session, &edit(f, tu)), pid);
    assert_ne!(pre.decision().as_deref(), Some("deny"), "{}", pre.reason());
    std::fs::write(f, format!("{}{tu}\n", std::fs::read_to_string(f).unwrap_or_default())).unwrap();
    hook(r, "post-edit", &payload(r, session, &edit(f, tu).replace("PreToolUse", "PostToolUse")), pid);
}

fn tool(r: &Repo, name: &str, args: &[&str], env: &[(&str, String)]) -> Run {
    let path = &env.iter().find(|(k, _)| *k == "PATH").unwrap().1;
    let exe = std::env::split_paths(path).map(|d| d.join(name)).find(|p| p.exists()).unwrap();
    run_bin(r, Some(&exe), args, &r.dir, env, "")
}

fn refused(o: &Run) -> bool {
    o.status == 1 && o.stderr.starts_with("agent-lock: ") && o.stderr.trim_end().lines().count() == 1
}

// ── one agent, as Claude Code drives it ───────────────────────────────────

static TU: AtomicUsize = AtomicUsize::new(0);
fn tu() -> String {
    format!("toolu_{}", TU.fetch_add(1, Ordering::SeqCst))
}

/// A main agent (or, with `sub`, one of its subagents) of a session: hook
/// payloads carry its ids; its Bash runs with the session's env file applied.
#[derive(Clone)]
struct Agent<'r> {
    r: &'r Repo,
    sid: &'static str,
    pid: u32,
    env: Vec<(&'static str, String)>,
    sub: Option<&'static str>,
}

/// One Bash tool call: the pre-bash verdict, and the command's run unless denied.
struct Bash {
    pre: Run,
    run: Option<Run>,
}

impl Bash {
    fn ran(&self) -> &Run {
        self.run.as_ref().unwrap_or_else(|| panic!("pre-bash denied it: {}", self.pre.reason()))
    }
}

impl<'r> Agent<'r> {
    fn start(r: &'r Repo, sid: &'static str, pid: u32) -> Agent<'r> {
        let mut env = start(r, sid, pid);
        env.push(("CLAUDE_CODE_SESSION_ID", sid.to_string()));
        Agent { r, sid, pid, env, sub: None }
    }

    /// SubagentStart for `agent_id`; the subagent shares the session's environment.
    fn spawn(&self, agent_id: &'static str) -> Agent<'r> {
        let a = Agent { sub: Some(agent_id), ..self.clone() };
        a.hook("subagent-start", r#""hook_event_name":"SubagentStart""#);
        a
    }

    fn finish(&self) {
        self.hook("subagent-stop", r#""hook_event_name":"SubagentStop","stop_hook_active":false"#);
    }

    fn hook(&self, event: &str, extra: &str) -> Run {
        let extra = match self.sub {
            Some(a) => format!(r#""agent_id":"{a}","agent_type":"general-purpose",{extra}"#),
            None => extra.to_string(),
        };
        let o = run_bin(self.r, None, &["hook", event], &self.r.dir, &[("CLAUDE_PID", self.pid.to_string())], &payload(self.r, self.sid, &extra));
        assert_eq!((o.status, o.stderr.as_str()), (0, ""), "{event}");
        assert!(o.stdout.is_empty() || serde_free::strict(&o.stdout).is_some(), "{event}: {}", o.stdout);
        o
    }

    fn pre(&self, f: &Path) -> Run {
        self.hook("pre-edit", &edit(f, &tu()))
    }

    /// A whole Edit tool call that must be allowed: claim, write, record.
    fn edit(&self, f: &Path, text: &str) {
        let id = tu();
        let pre = self.hook("pre-edit", &edit(f, &id));
        assert_ne!(pre.decision().as_deref(), Some("deny"), "{}", pre.reason());
        std::fs::write(f, text).unwrap();
        self.hook("post-edit", &edit(f, &id).replace("PreToolUse", "PostToolUse"));
    }

    fn stop(&self, active: bool) -> Run {
        self.hook("stop", &format!(r#""hook_event_name":"Stop","stop_hook_active":{active}"#))
    }

    /// A command in the agent's shell environment, with no hooks around it.
    fn sh_in(&self, dir: &Path, cmd: &str) -> Run {
        run_bin(self.r, Some(Path::new("/bin/sh")), &["-c", cmd], dir, &self.env, "")
    }

    /// A Bash tool call: pre-bash, the command (unless denied), post-bash.
    fn bash(&self, cmd: &str) -> Bash {
        let id = tu();
        let pre = self.hook("pre-bash", &bash(cmd, &id));
        if pre.decision().is_some() {
            return Bash { pre, run: None };
        }
        let run = self.sh_in(&self.r.dir, cmd);
        let post = bash(cmd, &id).replace("PreToolUse", "PostToolUse");
        self.hook("post-bash", &format!(r#"{post},"tool_response":{{"stdout":"","stderr":"","interrupted":false}}"#));
        Bash { pre, run: Some(run) }
    }
}

fn rev(r: &Repo, dir: &Path, what: &str) -> String {
    git(r, dir, &["rev-parse", what])
}

fn files_of(r: &Repo, commit: &str) -> String {
    git(r, &r.dir, &["show", "--name-only", "--format=", commit])
}

fn porcelain(r: &Repo) -> String {
    let o = Command::new("git").args(["status", "--porcelain"]).current_dir(&r.dir).env_clear().envs(env_for(r)).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

/// Commit `rel` = `text` in another clone of origin and push it.
fn upstream_change(r: &Repo, rel: &str, text: &str) -> String {
    let up = r.base.join("up");
    if !up.exists() {
        clone(r, "up");
    }
    git(r, &up, &["pull", "-q", "--ff-only", "origin", "main"]);
    write(&up, rel, text);
    git(r, &up, &["add", "-A"]);
    git(r, &up, &["commit", "-q", "-m", &format!("up: {rel}")]);
    git(r, &up, &["push", "-q", "origin", "main"]);
    rev(r, &up, "HEAD")
}

fn origin_main(r: &Repo) -> String {
    rev(r, r.origin.as_ref().unwrap(), "main")
}

// ── the shims see the hooks' claims ───────────────────────────────────────

#[test]
fn claims_from_edit_hooks_guard_the_git_shim() {
    let r = make_repo(false, &[("own.txt", "own\n"), ("other.txt", "other\n")]);
    let b = other_claude();
    let ea = start(&r, SA, std::process::id());
    let eb = start(&r, SB, b.0.id());
    assert_eq!(ea[0].1, "aaaaaaaa");
    edit_file(&r, SA, &r.dir.join("own.txt"), "e1", std::process::id());
    edit_file(&r, SB, &r.dir.join("other.txt"), "e2", b.0.id());
    assert_eq!(holder(&r, "other.txt").as_deref(), Some("bbbbbbbb"));

    for args in [&["add", "-A"][..], &["commit", "-am", "x"], &["stash"], &["checkout", "--", "."]] {
        let o = tool(&r, "git", args, &ea);
        assert!(refused(&o), "{args:?}: {}", o.out());
        assert!(o.stderr.contains("other.txt by bbbbbbbb"), "{args:?}: {}", o.stderr);
    }
    assert_eq!(git(&r, &r.dir, &["diff", "--cached", "--name-only"]), "", "nothing ran");
    assert_eq!(git(&r, &r.dir, &["stash", "list"]), "");
    let o = tool(&r, "git", &["add", "own.txt"], &ea);
    assert_eq!(o.status, 0, "an agent's own file: {}", o.out());
    assert!(tool(&r, "git", &["status", "--short"], &ea).stdout.contains("M  own.txt"));
    // and the other way round
    let o = tool(&r, "git", &["add", "."], &eb);
    assert!(refused(&o) && o.stderr.contains("own.txt by aaaaaaaa"), "{}", o.out());

    // once B's session is gone, A works alone again
    drop(b);
    let o = tool(&r, "git", &["add", "-A"], &ea);
    assert_eq!(o.status, 0, "{}", o.out());
}

/// A fake npm after the shims: sleeps for `long*`, appends to package-lock.json.
fn pkg_repo() -> (Repo, PathBuf) {
    let r = make_repo(false, &[("package.json", "{}\n"), ("package-lock.json", "{}\n")]);
    let fake = r.base.join("fakebin");
    write(&fake, "npm", "#!/bin/sh\ncase \"$2\" in long*) sleep 3 ;; esac\necho \"$2\" >> package-lock.json\n");
    Command::new("chmod").args(["+x", fake.join("npm").to_str().unwrap()]).status().unwrap();
    (r, fake)
}

fn with_fake(env: &[(&'static str, String)], fake: &Path) -> Vec<(&'static str, String)> {
    let sys = std::env::var("PATH").unwrap();
    env.iter().map(|(k, v)| if *k == "PATH" { (*k, v.replacen(&sys, &format!("{}:{sys}", fake.display()), 1)) } else { (*k, v.clone()) }).collect()
}

fn wait_for(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn install_claims_are_shared_with_other_sessions_and_count_as_edits() {
    let (r, fake) = pkg_repo();
    let b = other_claude();
    let ea = with_fake(&start(&r, SA, std::process::id()), &fake);
    start(&r, SB, b.0.id());
    let o = tool(&r, "npm", &["install", "x"], &ea);
    assert_eq!(o.status, 0, "{}", o.out());
    let c = state(&r).get("claims").get("package-lock.json").clone();
    assert_eq!((c.get("by").as_str(), c.get("edits").as_u64(), c.get("pending").is_null()), (Some("aaaaaaaa"), Some(1), true));
    assert_eq!(c.get("session").as_str(), Some(SA));
    assert!(holder(&r, "package.json").is_none(), "an unchanged manifest is let go");
    // a lockfile is a shared file: another session joins it, and is told who else edits it
    let d = hook(&r, "pre-edit", &payload(&r, SB, &edit(&r.dir.join("package-lock.json"), "e1")), b.0.id());
    assert_eq!(d.decision(), None, "{}", d.out());
    assert!(d.context().contains("shared file, also being edited by aaaaaaaa"), "{}", d.context());
    let c = state(&r).get("claims").get("package-lock.json").clone();
    assert_eq!((c.get("by").as_str(), c.get("with").arr().first().and_then(|x| x.as_str())), (Some("aaaaaaaa"), Some("bbbbbbbb")));
    // while B's edit is in flight, A waits for it (one write at a time)
    let d = hook(&r, "pre-edit", &payload(&r, SA, &edit(&r.dir.join("package-lock.json"), "e2")), std::process::id());
    assert_eq!(d.decision().as_deref(), Some("deny"), "{}", d.out());
    // once it is written, A edits again
    hook(&r, "post-edit", &payload(&r, SB, &edit(&r.dir.join("package-lock.json"), "e1")), b.0.id());
    let d = hook(&r, "pre-edit", &payload(&r, SA, &edit(&r.dir.join("package-lock.json"), "e2")), std::process::id());
    assert_eq!(d.decision(), None, "{}", d.out());
    // A sticks to non-shared files: those still block
    write(&r.dir, "src.js", "x\n");
    hook(&r, "pre-edit", &payload(&r, SA, &edit(&r.dir.join("src.js"), "e3")), std::process::id());
    let d = hook(&r, "pre-edit", &payload(&r, SB, &edit(&r.dir.join("src.js"), "e4")), b.0.id());
    assert_eq!(d.decision().as_deref(), Some("deny"));
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[test]
fn a_killed_install_leaves_no_stuck_claims() {
    let (r, fake) = pkg_repo();
    let ea = with_fake(&start(&r, SA, std::process::id()), &fake);
    let path = &ea.iter().find(|(k, _)| *k == "PATH").unwrap().1;
    let shim = std::env::split_paths(path).map(|d| d.join("npm")).find(|p| p.exists()).unwrap();
    let mut install = Command::new(shim)
        .args(["install", "long"])
        .current_dir(&r.dir)
        .env_clear()
        .envs(env_for(&r))
        .envs(ea.iter().cloned())
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pending = |rel: &str| state(&r).get("claims").get(rel).get("pending").as_str().map(str::to_string);
    wait_for("the install's claims", || pending("package.json").is_some_and(|p| p.starts_with("pkg:")));
    let prompt = payload(&r, SA, r#""hook_event_name":"UserPromptSubmit","prompt":"hi""#);
    hook(&r, "prompt", &prompt, std::process::id());
    assert!(pending("package.json").is_some_and(|p| p.starts_with("pkg:")), "a running install keeps its marker");

    // kill the shim and the tool (the whole process group), as a crash would
    // kill(2) with a negative pid: the external `kill -9 -<pgid>` is read differently by procps (Linux)
    assert_eq!(unsafe { kill(-(install.id() as i32), 9) }, 0, "killing the install's process group");
    install.wait().unwrap();
    hook(&r, "prompt", &prompt, std::process::id());
    assert_eq!(pending("package.json"), None, "the dead install's marker is cleared");
    assert!(holder(&r, "package.json").is_some());
    let stop = payload(&r, SA, r#""hook_event_name":"Stop","stop_hook_active":false"#);
    hook(&r, "stop", &stop, std::process::id());
    assert!(holder(&r, "package.json").is_none() && holder(&r, "package-lock.json").is_none(), "{:?}", state(&r).get("claims").keys());
    assert!(state(&r).get("locks").get("pkg").is_null());
}

// ── (a) two sessions, one checkout ────────────────────────────────────────

#[test]
fn two_sessions_commit_and_push_only_their_own_files() {
    let r = make_repo(true, &[("x.ts", "x\n"), ("y.ts", "y\n")]);
    let bp = other_claude();
    let a = Agent::start(&r, SA, std::process::id());
    let b = Agent::start(&r, SB, bp.0.id());
    let (x, y) = (r.dir.join("x.ts"), r.dir.join("y.ts"));

    a.edit(&x, "x by A\n");
    let d = b.pre(&x);
    assert_eq!(d.decision().as_deref(), Some("deny"));
    assert!(d.reason().contains("BLOCKED: x.ts is claimed by aaaaaaaa ("), "{}", d.reason());
    assert!(d.reason().contains(&format!("agent-lock wait --claim {} --timeout 540", x.display())), "absolute path: {}", d.reason());
    b.edit(&y, "y by B\n");

    // B's whole-tree commit, heredoc message and all, is refused by the git shim
    let head = rev(&r, &r.dir, "HEAD");
    let cmd = "git add -A && git commit -m \"$(cat <<'EOF'\nB: everything\n\nCo-Authored-By: B\nEOF\n)\"";
    let o = b.bash(cmd);
    let ran = o.ran();
    assert_eq!(ran.status, 1, "{}", ran.out());
    assert!(ran.stderr.starts_with("agent-lock: ") && ran.stderr.contains("x.ts by aaaaaaaa"), "{}", ran.stderr);
    assert_eq!(rev(&r, &r.dir, "HEAD"), head, "nothing committed");
    assert_eq!(git(&r, &r.dir, &["diff", "--cached", "--name-only"]), "", "nothing staged");

    // A commits and pushes exactly its file
    let o = a.bash("agent-lock commit -m \"$(cat <<'EOF'\nA: x\n\nbody line\nEOF\n)\" --push");
    let ran = o.ran();
    assert_eq!(ran.status, 0, "{}", ran.out());
    assert!(ran.stdout.contains("committed ") && ran.stdout.contains("  x.ts") && ran.stdout.contains("push: "), "{}", ran.stdout);
    assert_eq!(files_of(&r, "HEAD"), "x.ts");
    assert_eq!(git(&r, &r.dir, &["log", "-1", "--format=%B"]), "A: x\n\nbody line");
    assert_eq!(origin_main(&r), rev(&r, &r.dir, "HEAD"), "pushed");
    assert_eq!(porcelain(&r), " M y.ts", "B's work is untouched and uncommitted");
    assert_eq!(holder(&r, "y.ts").as_deref(), Some("bbbbbbbb"));
    assert_eq!(holder(&r, "x.ts"), None, "a commit ends the claim");

    // and the file is B's to take now
    b.edit(&x, "x by B\n");
    assert_eq!(holder(&r, "x.ts").as_deref(), Some("bbbbbbbb"));
    let o = b.bash("agent-lock commit -m 'B: x and y'");
    assert_eq!(o.ran().status, 0, "{}", o.ran().out());
    assert_eq!(files_of(&r, "HEAD"), "x.ts\ny.ts");
    assert_eq!(porcelain(&r), "");
}

// ── (b) a main agent and two parallel subagents ───────────────────────────

#[test]
fn parallel_subagents_block_each_other_and_hand_back_to_main() {
    let r = make_repo(true, &[("p.ts", "p\n"), ("q.ts", "q\n")]);
    let a = Agent::start(&r, SA, std::process::id());
    let s1 = a.spawn(SUB1);
    let s2 = a.spawn(SUB2);
    let (p, q) = (r.dir.join("p.ts"), r.dir.join("q.ts"));

    s1.edit(&p, "p by sub 1\n");
    let d = s2.pre(&p);
    assert_eq!(d.decision().as_deref(), Some("deny"));
    assert!(d.reason().contains(&format!("is claimed by {A1}")), "{}", d.reason());
    assert!(d.reason().contains(&format!("agent-lock wait --as {A2} --claim")), "{}", d.reason());
    s2.edit(&q, "q by sub 2\n");
    let m = a.pre(&p);
    assert!(m.reason().contains(&format!("your subagent {A1}, which is still running")), "{}", m.reason());
    assert_eq!((holder(&r, "p.ts").as_deref(), holder(&r, "q.ts").as_deref()), (Some(A1), Some(A2)));

    s1.finish();
    assert_eq!(holder(&r, "p.ts").as_deref(), Some("aaaaaaaa"), "a finished subagent's files pass to main");
    assert_eq!(holder(&r, "q.ts").as_deref(), Some(A2), "the running one keeps its own");
    s2.finish();
    assert_eq!(holder(&r, "q.ts").as_deref(), Some("aaaaaaaa"));

    let o = a.bash("agent-lock commit -m 'work of both subagents' --push");
    assert_eq!(o.ran().status, 0, "{}", o.ran().out());
    assert_eq!(files_of(&r, "HEAD"), "p.ts\nq.ts");
    assert_eq!(origin_main(&r), rev(&r, &r.dir, "HEAD"));
    assert!(state(&r).get("claims").keys().is_empty(), "{:?}", state(&r).get("claims").keys());
}

// ── (c) push integrates upstream around another agent's work ─────────────

#[test]
fn push_integrates_upstream_without_touching_another_agents_work() {
    let r = make_repo(true, &[("x.ts", "x\n"), ("y.ts", "y\n")]);
    let bp = other_claude();
    let a = Agent::start(&r, SA, std::process::id());
    let b = Agent::start(&r, SB, bp.0.id());
    let (x, y) = (r.dir.join("x.ts"), r.dir.join("y.ts"));
    b.edit(&y, "y by B\n");

    // another machine pushes z.ts; A commits x.ts and pushes on top of it
    let up = upstream_change(&r, "z.ts", "z upstream\n");
    a.edit(&x, "x by A\n");
    assert_eq!(a.bash("agent-lock commit -m 'A: x'").ran().status, 0);
    let o = a.bash("agent-lock push");
    assert_eq!(o.ran().status, 0, "{}", o.ran().out());
    assert_eq!(origin_main(&r), rev(&r, &r.dir, "HEAD"));
    assert_eq!(rev(&r, &r.dir, "HEAD~1"), up, "A's commit replayed on top of upstream");
    assert_eq!(files_of(&r, "HEAD"), "x.ts");
    assert_eq!(read(&r.dir.join("z.ts")), "z upstream\n");
    assert_eq!(read(&y), "y by B\n");
    assert_eq!(porcelain(&r), " M y.ts");
    assert_eq!(holder(&r, "y.ts").as_deref(), Some("bbbbbbbb"));

    // upstream now changes B's uncommitted file: A's push is blocked, nothing moves
    let up2 = upstream_change(&r, "y.ts", "y upstream\n");
    a.edit(&x, "x by A, again\n");
    assert_eq!(a.bash("agent-lock commit -m 'A: x again'").ran().status, 0);
    let mine = rev(&r, &r.dir, "HEAD");
    let o = a.bash("agent-lock push");
    let ran = o.ran();
    assert_eq!(ran.status, 1, "{}", ran.out());
    assert!(ran.stderr.contains("push: blocked: upstream changed 1 file that is claimed"), "{}", ran.stderr);
    assert!(ran.stderr.contains("y.ts: being edited by bbbbbbbb"), "{}", ran.stderr);
    assert_eq!(rev(&r, &r.dir, "HEAD"), mine, "HEAD unchanged");
    assert_eq!(origin_main(&r), up2, "origin unchanged");
    assert_eq!(read(&y), "y by B\n");
    assert_eq!(porcelain(&r), " M y.ts");
    assert!(state(&r).get("locks").get("sync").is_null() && state(&r).get("locks").get("git").is_null());
    assert!(!r.git_dir.join("index.lock").exists());
}

// ── (d) the end-of-turn reminder ──────────────────────────────────────────

#[test]
fn end_of_turn_reminder_only_while_another_agent_works() {
    let r = make_repo(false, &[("x.ts", "x\n"), ("y.ts", "y\n")]);
    let a = Agent::start(&r, SA, std::process::id());
    a.edit(&r.dir.join("x.ts"), "x by A\n");
    assert!(a.stop(false).stdout.is_empty(), "alone: no reminder");
    assert_eq!(holder(&r, "x.ts").as_deref(), Some("aaaaaaaa"), "a dirty claim outlives the turn");

    let bp = other_claude();
    let b = Agent::start(&r, SB, bp.0.id());
    b.edit(&r.dir.join("y.ts"), "y by B\n");
    let s = a.stop(false);
    assert_eq!(s.decision().as_deref(), Some("block"), "{}", s.stdout);
    assert!(s.reason().contains("uncommitted changes in files you claimed: x.ts."), "{}", s.reason());
    assert!(s.reason().contains("`agent-lock commit -m \"<msg>\"`") && !s.reason().contains("y.ts"), "{}", s.reason());
    assert!(a.stop(true).stdout.is_empty(), "never while continuing from a block");
    assert!(a.stop(false).stdout.is_empty(), "once per set of files");
    let s = b.stop(false);
    assert!(s.reason().contains("files you claimed: y.ts."), "B is reminded too: {}", s.stdout);

    // B's session ends: A works alone again, so even a new file brings no reminder
    b.hook("session-end", r#""hook_event_name":"SessionEnd","reason":"prompt_input_exit""#);
    drop(bp);
    a.edit(&r.dir.join("y.ts"), "y by A\n");
    assert_eq!(holder(&r, "y.ts").as_deref(), Some("aaaaaaaa"));
    assert!(a.stop(false).stdout.is_empty(), "alone again: no reminder");
}

// ── (e) a dead session's file ─────────────────────────────────────────────

#[test]
fn a_dead_sessions_file_is_taken_over_after_one_warning() {
    let r = make_repo(false, &[("x.ts", "x\n"), ("y.ts", "y\n")]);
    let mut bp = other_claude();
    let cp = other_claude();
    let a = Agent::start(&r, SA, std::process::id());
    let b = Agent::start(&r, SB, bp.0.id());
    let c = Agent::start(&r, SC, cp.0.id());
    let x = r.dir.join("x.ts");
    b.edit(&x, "x by B\n");
    c.edit(&r.dir.join("y.ts"), "y by C\n");
    let d = a.pre(&x);
    assert_eq!(d.decision().as_deref(), Some("deny"), "B is alive: blocked");
    assert!(d.reason().contains("BLOCKED"), "{}", d.reason());

    let _ = bp.0.kill(); // B's Claude Code dies with its edit uncommitted
    bp.0.wait().unwrap();
    let d = a.pre(&x);
    assert_eq!(d.decision().as_deref(), Some("deny"), "refused once: {}", d.stdout);
    let t = d.reason();
    assert!(t.contains("TAKEOVER: x.ts was claimed by bbbbbbbb (its process has exited), which left 1 uncommitted edit"), "{t}");
    assert!(t.contains(&format!("git -C {} diff -- x.ts", r.dir.display())), "{t}");
    assert_eq!(holder(&r, "x.ts").as_deref(), Some("aaaaaaaa"), "the claim is A's now");

    let retry = a.pre(&x);
    assert_ne!(retry.decision().as_deref(), Some("deny"), "{}", retry.stdout);
    std::fs::write(&x, "x by B\nx by A\n").unwrap();
    a.hook("post-edit", &edit(&x, "toolu_post").replace("PreToolUse", "PostToolUse"));
    let o = a.bash("agent-lock commit -m 'A: x, with what B left'");
    assert_eq!(o.ran().status, 0, "{}", o.ran().out());
    assert_eq!(files_of(&r, "HEAD"), "x.ts");
    assert_eq!(git(&r, &r.dir, &["show", "HEAD:x.ts"]), "x by B\nx by A");
    assert_eq!(holder(&r, "y.ts").as_deref(), Some("cccccccc"), "C's work is not touched");
}

// ── (f) stress: many hook processes, two files, three sessions ───────────

/// The state file as a locked reader sees it.
fn locked_state(r: &Repo) -> Option<String> {
    let f = std::fs::File::open(r.git_dir.join("agent-lock.json")).ok()?;
    f.lock_shared().ok()?;
    let mut s = String::new();
    std::io::Read::read_to_string(&mut &f, &mut s).ok()?;
    let _ = f.unlock();
    Some(s)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn stress_concurrent_hooks_never_let_two_agents_hold_one_file() {
    const ROUNDS: usize = 350;
    let r = Arc::new(make_repo(false, &[("f0.txt", "0\n"), ("f1.txt", "1\n")]));
    let procs: Vec<Proc> = (0..3).map(|_| other_claude()).collect();
    let sessions = [SA, SB, SC];
    let marks = r.base.join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    for (i, s) in sessions.iter().enumerate() {
        let p = payload(&r, s, r#""hook_event_name":"SessionStart","source":"startup""#);
        assert_eq!(hook(&r, "session-start", &p, procs[i].0.id()).status, 0);
    }
    // nine agents: each session's main agent and two of its subagents
    struct W {
        sid: &'static str,
        pid: u32,
        sub: Option<String>,
        me: String,
    }
    let mut workers = Vec::new();
    for w in 0..9usize {
        let (sid, pid) = (sessions[w % 3], procs[w % 3].0.id());
        let sub = (w >= 3).then(|| format!("a{:08x}{:08x}", 0x5000_0000 + w, w));
        let me = match &sub {
            Some(a) => format!("{}:{}", &sid[..8], &a[1..9]),
            None => sid[..8].to_string(),
        };
        if let Some(a) = &sub {
            let p = payload(&r, sid, &format!(r#""hook_event_name":"SubagentStart","agent_id":"{a}","agent_type":"general-purpose""#));
            assert_eq!(hook(&r, "subagent-start", &p, pid).status, 0);
        }
        workers.push(W { sid, pid, sub, me });
    }
    let problems = Arc::new(Mutex::new(Vec::<String>::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let windows = Arc::new(Mutex::new(std::collections::BTreeMap::<&'static str, usize>::new()));
    let done = Arc::new(AtomicBool::new(false));

    // a reader that keeps checking the state file always parses
    let monitor = {
        let (r, problems, done) = (r.clone(), problems.clone(), done.clone());
        std::thread::spawn(move || {
            let mut reads = 0;
            while !done.load(Ordering::SeqCst) {
                if let Some(text) = locked_state(&r) {
                    reads += 1;
                    if !text.is_empty() && serde_free::strict(&text).is_none() {
                        problems.lock().unwrap().push(format!("state file does not parse: {text:?}"));
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            reads
        })
    };

    let t0 = Instant::now();
    let threads: Vec<_> = workers
        .into_iter()
        .enumerate()
        .map(|(w, wk)| {
            let (r, marks, problems, calls, windows) = (r.clone(), marks.clone(), problems.clone(), calls.clone(), windows.clone());
            std::thread::spawn(move || {
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ ((w as u64 + 1) * 0x1000_0001));
                let call = |event: &str, extra: String| -> Run {
                    let extra = match &wk.sub {
                        Some(a) => format!(r#""agent_id":"{a}","agent_type":"general-purpose",{extra}"#),
                        None => extra,
                    };
                    let o = hook(&r, event, &payload(&r, wk.sid, &extra), wk.pid);
                    calls.fetch_add(1, Ordering::SeqCst);
                    if o.status != 0 || !o.stderr.is_empty() || !(o.stdout.is_empty() || serde_free::strict(&o.stdout).is_some()) {
                        problems.lock().unwrap().push(format!("{} {event}: exit {} stderr {:?} stdout {:?}", wk.me, o.status, o.stderr, o.stdout));
                    }
                    o
                };
                for n in 0..ROUNDS {
                    let rel = if rng.next().is_multiple_of(2) { "f0.txt" } else { "f1.txt" };
                    let f = r.dir.join(rel);
                    let id = format!("w{w}r{n}");
                    let pre = call("pre-edit", edit(&f, &id));
                    if pre.decision().as_deref() == Some("deny") {
                        let why = pre.reason();
                        if !(why.contains("BLOCKED") || why.contains("DEADLOCK")) || why.contains(&format!("claimed by {} (", wk.me)) {
                            problems.lock().unwrap().push(format!("{} on {rel}: unexpected deny: {why}", wk.me));
                        }
                        continue;
                    }
                    // the edit window: nobody else may be inside it for this file
                    let mark = marks.join(rel);
                    match std::fs::OpenOptions::new().write(true).create_new(true).open(&mark) {
                        Ok(mut m) => {
                            let _ = std::io::Write::write_all(&mut m, wk.me.as_bytes());
                        }
                        Err(_) => {
                            let other = std::fs::read_to_string(&mark).unwrap_or_default();
                            problems.lock().unwrap().push(format!("{} was let into {rel} while {other} held it", wk.me));
                            continue;
                        }
                    }
                    *windows.lock().unwrap().entry(wk.sid).or_default() += 1;
                    let st = serde_free::parse(&locked_state(&r).unwrap_or_default());
                    let by = st.get("claims").get(rel).get("by").as_str().map(str::to_string);
                    if by.as_deref() != Some(wk.me.as_str()) {
                        problems.lock().unwrap().push(format!("{} was let into {rel}, but the state says {by:?}", wk.me));
                    }
                    if rng.next().is_multiple_of(3) {
                        std::thread::sleep(Duration::from_micros(rng.next() % 2000));
                    }
                    std::fs::remove_file(&mark).unwrap();
                    if wk.sub.is_none() {
                        // the main agent's edit is rejected at the permission prompt
                        call("edit-failed", edit(&f, &id).replace("PreToolUse", "PermissionDenied"));
                    } else {
                        call("post-edit", edit(&f, &id).replace("PreToolUse", "PostToolUse"));
                        let o = run_bin(&r, None, &["release", "--as", &wk.me, f.to_str().unwrap()], &r.dir, &[("CLAUDE_PID", wk.pid.to_string())], "");
                        calls.fetch_add(1, Ordering::SeqCst);
                        if o.status != 0 || !o.stderr.is_empty() {
                            problems.lock().unwrap().push(format!("{} release {rel}: exit {} {}", wk.me, o.status, o.out()));
                        }
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    done.store(true, Ordering::SeqCst);
    let reads = monitor.join().unwrap();
    let problems = problems.lock().unwrap();
    assert!(problems.is_empty(), "{} problems, first: {:#?}", problems.len(), &problems[..problems.len().min(10)]);
    let calls = calls.load(Ordering::SeqCst);
    let windows = windows.lock().unwrap().clone();
    eprintln!("stress: {calls} calls in {:?}, edit windows per session {windows:?}, {reads} locked reads", t0.elapsed());
    assert!(calls >= 2000, "{calls} calls");
    assert_eq!(windows.len(), 3, "every session got the files at some point: {windows:?}");
    assert!(reads > 50, "{reads} reads");
    assert!(!r.git_dir.join("agent-lock.json.bad").exists(), "the state file was never corrupt");
    let st = serde_free::strict(&locked_state(&r).unwrap()).expect("final state parses");
    assert!(st.get("claims").keys().is_empty(), "every window was closed: {:?}", st.get("claims").keys());
}

// ── (g) the npm shim serializes installs ──────────────────────────────────

#[test]
fn npm_shim_runs_one_install_at_a_time() {
    let r = make_repo(
        false,
        &[("a/package.json", "{}\n"), ("a/package-lock.json", "{}\n"), ("b/package.json", "{}\n"), ("b/package-lock.json", "{}\n")],
    );
    let fake = r.base.join("fakebin");
    let (running, log) = (r.base.join("npm-running"), r.base.join("npm.log"));
    write(
        &fake,
        "npm",
        "#!/bin/sh\nmkdir \"$NPM_RUNNING\" 2>/dev/null || echo overlap >> \"$NPM_LOG\"\necho \"start ${PWD##*/}\" >> \"$NPM_LOG\"\nsleep 1\necho \"$2\" >> package-lock.json\necho \"end ${PWD##*/}\" >> \"$NPM_LOG\"\nrmdir \"$NPM_RUNNING\"\n",
    );
    Command::new("chmod").args(["+x", fake.join("npm").to_str().unwrap()]).status().unwrap();
    let bp = other_claude();
    let mut a = Agent::start(&r, SA, std::process::id());
    let mut b = Agent::start(&r, SB, bp.0.id());
    for ag in [&mut a, &mut b] {
        ag.env = with_fake(&ag.env, &fake);
        ag.env.push(("NPM_RUNNING", running.display().to_string()));
        ag.env.push(("NPM_LOG", log.display().to_string()));
    }
    let (da, db) = (r.dir.join("a"), r.dir.join("b"));
    let (ra, rb) = std::thread::scope(|s| {
        let ta = s.spawn(|| a.sh_in(&da, "npm install left-pad"));
        std::thread::sleep(Duration::from_millis(150));
        let tb = s.spawn(|| b.sh_in(&db, "npm install right-pad"));
        (ta.join().unwrap(), tb.join().unwrap())
    });
    assert_eq!((ra.status, rb.status), (0, 0), "{}\n{}", ra.out(), rb.out());
    let lines: Vec<String> = read(&log).lines().map(str::to_string).collect();
    assert_eq!(lines, ["start a", "end a", "start b", "end b"], "one install at a time");
    assert!(rb.stderr.contains("agent-lock: waiting for `npm install left-pad` (aaaaaaaa"), "{}", rb.stderr);
    assert_eq!(holder(&r, "a/package-lock.json").as_deref(), Some("aaaaaaaa"));
    assert_eq!(holder(&r, "b/package-lock.json").as_deref(), Some("bbbbbbbb"));
    assert!(holder(&r, "a/package.json").is_none() && holder(&r, "b/package.json").is_none());
    assert!(state(&r).get("locks").get("pkg").is_null());

    // B may also install into the lockfile A changed (a shared file): B joins it
    let o = b.sh_in(&da, "npm install other");
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(read(&log).lines().count(), 6);
    let c = state(&r).get("claims").get("a/package-lock.json").clone();
    assert_eq!((c.get("by").as_str(), c.get("with").arr().first().and_then(|x| x.as_str()), c.get("pending").is_null()), (Some("aaaaaaaa"), Some("bbbbbbbb"), true));
    assert!(state(&r).get("locks").get("pkg").is_null(), "and holds no lock");
}

// ── (h) an interrupted sync ───────────────────────────────────────────────

fn real_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap()).map(|d| d.join("git")).find(|p| p.is_file()).expect("git on PATH")
}

/// A git that SIGKILLs its caller (the sync) at the stage named in `kill_at`, once.
fn killer_git(r: &Repo) -> (PathBuf, PathBuf) {
    let kill_at = r.base.join("kill-at");
    let script = format!(
        r#"#!/bin/sh
real='{real}'
if [ -e '{k}' ]; then
  stage=$(cat '{k}')
  case "$stage:$*" in
    before-read-tree:*"read-tree -m -u "*) rm -f '{k}'; kill -9 $PPID; exit 1 ;;
    after-read-tree:*"read-tree -m -u "*) rm -f '{k}'; "$real" "$@"; s=$?; kill -9 $PPID; exit $s ;;
    after-update-ref:*"update-ref -m agent-lock sync"*) rm -f '{k}'; "$real" "$@"; s=$?; kill -9 $PPID; exit $s ;;
  esac
fi
exec "$real" "$@"
"#,
        real = real_git().display(),
        k = kill_at.display()
    );
    let w = write(&r.base, "killer-git", &script);
    Command::new("chmod").args(["+x", w.to_str().unwrap()]).status().unwrap();
    (w, kill_at)
}

#[test]
fn an_interrupted_sync_is_finished_or_undone_by_the_next_one() {
    for stage in ["before-read-tree", "after-read-tree", "after-update-ref"] {
        let r = make_repo(true, &[("x.ts", "x\n"), ("y.ts", "y\n")]);
        let bp = other_claude();
        let a = Agent::start(&r, SA, std::process::id());
        let b = Agent::start(&r, SB, bp.0.id());
        b.edit(&r.dir.join("y.ts"), "y by B\n");
        upstream_change(&r, "x.ts", "x upstream\n");
        let tip = upstream_change(&r, "w.ts", "w upstream\n");
        let before = rev(&r, &r.dir, "HEAD");

        let (killer, kill_at) = killer_git(&r);
        std::fs::write(&kill_at, stage).unwrap();
        let mut env = a.env.clone();
        env.push(("AGENT_LOCK_GIT", killer.display().to_string()));
        let killed = run_bin(&r, Some(Path::new("/bin/sh")), &["-c", "agent-lock sync"], &r.dir, &env, "");
        assert_ne!(killed.status, 0, "{stage}: the sync was killed: {}", killed.out());
        assert!(!kill_at.exists(), "{stage}: the kill happened");
        assert!(r.git_dir.join("index.lock").exists(), "{stage}: a killed sync leaves its index.lock");
        if stage == "after-update-ref" {
            assert_eq!(rev(&r, &r.dir, "HEAD"), tip, "HEAD had moved");
        } else {
            assert_eq!(rev(&r, &r.dir, "HEAD"), before);
        }

        let o = a.bash("agent-lock sync");
        let ran = o.ran();
        assert_eq!(ran.status, 0, "{stage}: {}", ran.out());
        assert_eq!(rev(&r, &r.dir, "HEAD"), tip, "{stage}");
        assert_eq!(read(&r.dir.join("x.ts")), "x upstream\n", "{stage}");
        assert_eq!(read(&r.dir.join("w.ts")), "w upstream\n", "{stage}");
        assert_eq!(read(&r.dir.join("y.ts")), "y by B\n", "{stage}: B's work survives");
        assert_eq!(porcelain(&r), " M y.ts", "{stage}: index and worktree agree with HEAD");
        assert!(!r.git_dir.join("index.lock").exists(), "{stage}");
        assert!(state(&r).get("locks").get("sync").is_null(), "{stage}");
        assert_eq!(holder(&r, "y.ts").as_deref(), Some("bbbbbbbb"), "{stage}");
    }
}

// ── (i) a subagent's identity ─────────────────────────────────────────────

#[test]
fn a_subagent_commit_under_the_wrong_id_is_denied_with_the_fix() {
    let r = make_repo(false, &[("s.ts", "s\n"), ("m.ts", "m\n")]);
    let a = Agent::start(&r, SA, std::process::id());
    a.edit(&r.dir.join("m.ts"), "m by main\n");
    let s1 = a.spawn(SUB1);
    s1.edit(&r.dir.join("s.ts"), "s by sub\n");
    let head = rev(&r, &r.dir, "HEAD");

    let o = s1.bash("agent-lock commit -m \"sub: s\"");
    assert!(o.run.is_none(), "denied before it runs");
    assert_eq!(o.pre.decision().as_deref(), Some("deny"));
    let why = o.pre.reason();
    assert!(why.contains(&format!("this would run agent-lock as aaaaaaaa, but you are {A1}")), "{why}");
    let fixed = why.lines().last().unwrap().trim().to_string();
    assert_eq!(fixed, format!("agent-lock commit --as {A1} -m \"sub: s\""));
    let o = s1.bash("agent-lock commit --as bbbbbbbb -m \"sub: s\"");
    assert!(o.run.is_none() && o.pre.reason().contains("as bbbbbbbb, but you are"), "{}", o.pre.stdout);
    assert_eq!(rev(&r, &r.dir, "HEAD"), head, "nothing committed");

    let o = s1.bash(&fixed);
    assert_eq!(o.ran().status, 0, "{}", o.ran().out());
    assert_eq!(files_of(&r, "HEAD"), "s.ts", "only the subagent's own file");
    assert_eq!(holder(&r, "m.ts").as_deref(), Some("aaaaaaaa"));
}

// ── (j) a running subagent keeps its files ────────────────────────────────

/// `secs` pass with no hook at all (Read, Grep, thinking): every recent
/// timestamp in the state file moves back.
fn age(r: &Repo, secs: u64) {
    let p = r.git_dir.join("agent-lock.json");
    let text = std::fs::read_to_string(&p).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let (mut out, mut num) = (String::new(), String::new());
    for ch in text.chars().chain(['\n']) {
        if ch.is_ascii_digit() {
            num.push(ch);
            continue;
        }
        match num.parse::<u64>() {
            Ok(n) if n + 3600 > now && n <= now + 5 => out.push_str(&(n - secs).to_string()),
            _ => out.push_str(&num),
        }
        num.clear();
        out.push(ch);
    }
    out.pop();
    std::fs::write(&p, out).unwrap();
}

#[test]
fn a_subagent_quiet_for_15_minutes_still_holds_its_file() {
    // state/F2, hooks/H1: reading and thinking fire no hook; idle is not finished
    let r = make_repo(false, &[("c.ts", "c\n")]);
    let a = Agent::start(&r, SA, std::process::id());
    let (w1, w2) = (a.spawn(SUB1), a.spawn(SUB2));
    let c = r.dir.join("c.ts");
    w1.edit(&c, "c by W1, part 1 of 3\n");
    // main's turn ends while both run in the background
    let bg = format!(
        r#""hook_event_name":"Stop","stop_hook_active":false,"background_tasks":[{{"id":"{SUB1}","type":"subagent","status":"running"}},{{"id":"{SUB2}","type":"subagent","status":"running"}}]"#
    );
    a.hook("stop", &bg);
    age(&r, 15 * 60);
    let d = w2.pre(&c);
    assert_eq!(d.decision().as_deref(), Some("deny"), "a sibling: {}", d.stdout);
    assert!(d.reason().contains(&format!("is claimed by {A1}")), "{}", d.reason());
    let d = a.pre(&c);
    assert_eq!(d.decision().as_deref(), Some("deny"), "its parent: {}", d.stdout);
    a.hook("prompt", r#""hook_event_name":"UserPromptSubmit","prompt":"status?""#);
    assert_eq!(holder(&r, "c.ts").as_deref(), Some(A1), "a prompt hands nothing back");
    a.hook("stop", &bg);
    a.stop(false); // a Claude Code that reports no background tasks
    assert_eq!(holder(&r, "c.ts").as_deref(), Some(A1));
    w1.edit(&c, "c by W1, part 2 of 3\n");
    assert_eq!(holder(&r, "c.ts").as_deref(), Some(A1), "its own next edit is no takeover");
    assert!(state(&r).get("claims").get("c.ts").get("inherited").is_null());
}

// ── (k) a subagent's whole-tree git ───────────────────────────────────────

#[test]
fn a_subagents_whole_tree_git_spares_its_parents_work() {
    // shim/SHIM-1: the subagent's Bash runs under its parent's AGENT_LOCK_AGENT_ID
    let r = make_repo(false, &[("a.txt", "orig\n"), ("b.txt", "b\n")]);
    let a = Agent::start(&r, SA, std::process::id());
    a.edit(&r.dir.join("a.txt"), "main's work\n");
    let s = a.spawn(SUB1); // holds nothing itself
    for cmd in ["git checkout -- .", "git stash", "git reset --hard"] {
        let o = s.bash(cmd);
        assert!(refused(o.ran()), "{cmd}: {}", o.ran().out());
    }
    assert_eq!(read(&r.dir.join("a.txt")), "main's work\n");
    assert_eq!(git(&r, &r.dir, &["stash", "list"]), "");
    assert_eq!(s.bash("git checkout -- b.txt").ran().status, 0, "a file nobody holds");
    s.finish();
    assert_eq!(s.bash("git status --short").ran().stdout, " M a.txt\n");
}

// ── (l) one file, one claim, whatever the spelling ───────────────────────

#[test]
fn an_nfd_directory_and_a_miscased_leaf_are_one_claim_that_stop_and_commit_keep() {
    // state/F1, gitcli/GIT-1, gitcli/GIT-2: Finder writes NFD names; git prints NFC
    if !cfg!(target_os = "macos") {
        return;
    }
    let (nfc, nfd) = ("Caf\u{e9}-docs", "Cafe\u{301}-docs");
    let r = make_repo(false, &[(&format!("{nfd}/Notes.md"), "notes\n"), ("x.txt", "x\n")]);
    if !r.dir.join("X.TXT").exists() {
        return; // a case-sensitive volume
    }
    let bp = other_claude();
    let a = Agent::start(&r, SA, std::process::id());
    let b = Agent::start(&r, SB, bp.0.id());
    let git_name = format!("{nfc}/Notes.md");
    a.edit(&r.dir.join(format!("{nfc}/notes.md")), "notes by A\n");
    assert_eq!(state(&r).get("claims").keys(), std::slice::from_ref(&git_name), "keyed by git's spelling");
    let d = b.pre(&r.dir.join(format!("{nfd}/NOTES.MD")));
    assert_eq!(d.decision().as_deref(), Some("deny"), "another spelling, the same file: {}", d.stdout);
    assert_eq!(state(&r).get("claims").keys(), std::slice::from_ref(&git_name), "one claim");
    a.stop(false);
    assert_eq!(holder(&r, &git_name).as_deref(), Some("aaaaaaaa"), "dirty: Stop keeps it");
    let o = a.bash("agent-lock commit -m 'A: notes'");
    assert_eq!(o.ran().status, 0, "{}", o.ran().out());
    assert!(!o.ran().stdout.contains("unchanged"), "{}", o.ran().stdout);
    assert_eq!(git(&r, &r.dir, &["-c", "core.quotepath=false", "show", "--name-only", "--format=", "HEAD"]), git_name);
    assert!(state(&r).get("claims").keys().is_empty(), "committed, then released");
}
