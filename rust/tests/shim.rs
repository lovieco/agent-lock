//! The git and package-manager shims, driven the way agents reach them: a
//! folder of links named git/npm → the binary, first on PATH.
mod common;
use common::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ME: &str = "aaaaaaaa";
const OTHER: &str = "bbbbbbbb";

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// The shim folder SessionStart creates: every shim name linked to the binary.
fn shims(r: &Repo) -> PathBuf {
    let d = r.base.join("shims");
    if !d.exists() {
        std::fs::create_dir_all(&d).unwrap();
        for n in ["git", "npm", "pnpm", "yarn", "bun", "cargo", "uv", "poetry", "go", "bundle", "bundler"] {
            std::os::unix::fs::symlink(BIN, d.join(n)).unwrap();
        }
    }
    d
}

fn path(dirs: &[&Path]) -> String {
    let mut v: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    v.push(std::env::var("PATH").unwrap());
    v.join(":")
}

fn real_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap()).map(|d| d.join("git")).find(|p| p.is_file()).unwrap()
}

/// Write the v3 state file: claims (path, agent, owner pid). No pid = an
/// ownerless claim, live for the TTL after `touched`.
fn claims(r: &Repo, list: &[(&str, &str, Option<u32>)], extra: &str) {
    let t = now();
    let body: Vec<String> = list
        .iter()
        .map(|(p, by, pid)| {
            let owner = pid.map(|p| format!(r#","owner":{{"pid":{p},"start":1}}"#)).unwrap_or_default();
            format!(r#""{p}":{{"by":"{by}","at":{t},"touched":{t},"edits":1{owner}}}"#)
        })
        .collect();
    let text = format!(r#"{{"v":3{extra},"claims":{{{}}},"sessions":{{}},"locks":{{}}}}"#, body.join(","));
    std::fs::write(r.git_dir.join("agent-lock.json"), text).unwrap();
}

/// A live owner: this test process (start 0 = check the pid only).
fn live() -> String {
    format!(r#"{{"pid":{},"start":0}}"#, std::process::id())
}

/// Write the state file from claims (path, agent, owner JSON or ""), raw
/// session members and raw top-level members (each with a leading comma).
fn put_state(r: &Repo, list: &[(&str, &str, &str)], sessions: &str, extra: &str) {
    let t = now();
    let body: Vec<String> = list
        .iter()
        .map(|(p, by, owner)| {
            let owner = if owner.is_empty() { String::new() } else { format!(r#","owner":{owner}"#) };
            format!(r#""{p}":{{"by":"{by}","at":{t},"touched":{t},"edits":1{owner}}}"#)
        })
        .collect();
    let text = format!(r#"{{"v":3{extra},"claims":{{{}}},"sessions":{{{sessions}}},"locks":{{}}}}"#, body.join(","));
    std::fs::write(r.git_dir.join("agent-lock.json"), text).unwrap();
}

/// A live session record for `agent`, with subagents (id, done).
fn session(agent: &str, subs: &[(&str, bool)]) -> String {
    let t = now();
    let subs: Vec<String> = subs.iter().map(|(id, done)| format!(r#""{id}":{{"seen":{t},"done":{done},"busy":0}}"#)).collect();
    format!(r#""{agent}-sess":{{"agent":"{agent}","owner":{},"started":{t},"seen":{t},"phase":"active","subs":{{{}}}}}"#, live(), subs.join(","))
}

const SUB: &str = "aaaaaaaa:11111111";

/// Run a shim as agent ME; `env` overrides (PATH included).
fn shim(r: &Repo, name: &str, cwd: &Path, args: &[&str], env: &[(&str, String)]) -> Run {
    let s = shims(r);
    let mut e = vec![("PATH", path(&[&s])), ("AGENT_LOCK_AGENT_ID", ME.to_string())];
    e.extend(env.iter().cloned());
    run_bin(r, Some(&s.join(name)), args, cwd, &e, "")
}

fn sgit(r: &Repo, args: &[&str]) -> Run {
    shim(r, "git", &r.dir, args, &[])
}

fn direct(r: &Repo, cwd: &Path, args: &[&str]) -> Run {
    run_bin(r, Some(&real_git()), args, cwd, &[], "")
}

fn porcelain(r: &Repo) -> String {
    git(r, &r.dir, &["status", "--porcelain"])
}

/// own.txt (ME) and other.txt (OTHER) both modified and claimed.
fn shared(remote: bool) -> Repo {
    let r = make_repo(remote, &[("own.txt", "own\n"), ("other.txt", "other\n")]);
    write(&r.dir, "own.txt", "own changed\n");
    write(&r.dir, "other.txt", "other changed\n");
    claims(&r, &[("own.txt", ME, None), ("other.txt", OTHER, None)], "");
    git(&r, &r.dir, &["config", "alias.co", "checkout"]);
    r
}

fn refused(o: &Run) -> bool {
    o.status != 0 && o.stderr.starts_with("agent-lock: ") && o.stderr.trim_end().lines().count() == 1
}

const SWEEPING: &[&[&str]] = &[
    &["add", "-A"],
    &["add", "."],
    &["commit", "-am", "x"],
    &["commit", "-m", "x"],
    &["stash"],
    &["checkout", "--", "."],
    &["reset", "--hard"],
    &["pull"],
    &["co", "."],
    &["-c", "alias.zz=checkout", "zz", "."],
    &["-C", "<repo>", "add", "."],
    &["--git-dir=work/.git", "--work-tree=work", "add", "-A"],
    &["-C", "work", "--work-tree=.", "add", "-u"],
];

/// Commands with -C/--git-dir run from outside the worktree.
fn cwd_for(r: &Repo, a: &[&str]) -> PathBuf {
    if a[0].starts_with('-') && a[0] != "-c" { r.base.clone() } else { r.dir.clone() }
}

fn args_for<'a>(r: &'a Repo, a: &'a [&'a str]) -> Vec<&'a str> {
    a.iter().map(|x| if *x == "<repo>" { r.dir.to_str().unwrap() } else { *x }).collect()
}

const XARGS: &str = "git diff --name-only | xargs git checkout --";

fn sh(r: &Repo, script: &str, env: &[(&str, String)]) -> Run {
    let s = shims(r);
    let mut e = vec![("PATH", path(&[&s])), ("AGENT_LOCK_AGENT_ID", ME.to_string())];
    e.extend(env.iter().cloned());
    run_bin(r, Some(Path::new("/bin/sh")), &["-c", script], &r.dir, &e, "")
}

#[test]
fn whole_tree_git_is_refused_while_another_agent_works() {
    let r = shared(true);
    let before = porcelain(&r);
    for a in SWEEPING {
        let a = args_for(&r, a);
        let o = shim(&r, "git", &cwd_for(&r, &a), &a, &[]);
        assert!(refused(&o), "{a:?}: {} {}", o.status, o.out());
        assert!(o.stderr.contains("(other.txt by bbbbbbbb)"), "{a:?}: {}", o.stderr);
        assert_eq!(o.status, 1);
        assert_eq!(porcelain(&r), before, "{a:?} must not run");
    }
    let o = sh(&r, XARGS, &[]);
    assert!(o.status != 0 && o.stderr.contains("agent-lock: `git checkout -- "), "{}", o.out());
    assert_eq!(porcelain(&r), before);
    assert_eq!(git(&r, &r.dir, &["stash", "list"]), "");
    // the safe alternatives are named
    let o = sgit(&r, &["commit", "-am", "fix it"]);
    assert!(o.stderr.contains("agent-lock commit -m 'fix it'"), "{}", o.stderr);
    assert!(sgit(&r, &["pull"]).stderr.contains("agent-lock sync"));
    assert!(sgit(&r, &["co", "."]).stderr.contains("(alias for git checkout)"));
}

/// A copy of `t` (worktree, origin, home): cheaper than a fresh repo.
fn copy_of(t: &Repo, name: &str) -> Repo {
    let base = t.base.with_file_name(format!("{}-{name}", t.base.file_name().unwrap().to_str().unwrap()));
    let _ = std::fs::remove_dir_all(&base);
    assert!(Command::new("cp").arg("-R").arg(&t.base).arg(&base).status().unwrap().success());
    let dir = base.join("work");
    Repo { git_dir: dir.join(".git"), dir, origin: t.origin.clone(), home: base.join("home"), data: base.join("data"), base }
}

#[test]
fn whole_tree_git_runs_unchanged_when_alone() {
    // Each command runs in two identical copies: through the shim and with the
    // real git. Only ME holds claims, so the results must be the same.
    let t = make_repo(true, &[("own.txt", "own\n"), ("other.txt", "other\n")]);
    write(&t.dir, "own.txt", "own changed\n");
    write(&t.dir, "other.txt", "other changed\n");
    claims(&t, &[("own.txt", ME, None), ("other.txt", ME, None)], "");
    git(&t, &t.dir, &["config", "alias.co", "checkout"]);
    for (i, a) in SWEEPING.iter().enumerate() {
        let (s, d) = (copy_of(&t, &format!("s{i}")), copy_of(&t, &format!("d{i}")));
        let (sa, da) = (args_for(&s, a), args_for(&d, a));
        let o = shim(&s, "git", &cwd_for(&s, &sa), &sa, &[]);
        let want = direct(&d, &cwd_for(&d, &da), &da);
        assert_eq!(o.status, want.status, "{a:?}: {}", o.out());
        assert!(!o.stderr.contains("agent-lock"), "{a:?}: {}", o.stderr);
        assert_eq!(porcelain(&s), porcelain(&d), "{a:?}");
    }
    let r = copy_of(&t, "xargs");
    let o = sh(&r, XARGS, &[]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(porcelain(&r), "", "both files restored");
}

#[test]
fn safe_commands_pass_straight_through() {
    let r = shared(false);
    for a in [
        &["status", "--porcelain"][..],
        &["log", "--oneline"],
        &["rev-parse", "--show-toplevel"],
        &["diff", "--stat"],
        &["show", "no-such-rev"],
        &["--version"],
        &["merge", "--abort"],
        &["rebase", "--quit"],
        &["cherry-pick", "--abort"],
        &["revert", "--quit"],
        &["am", "--abort"],
        &["stash", "list"],
        &["commit", "--dry-run", "-a"],
        &["reset", "-q"],
        &["not-a-command"],
    ] {
        let o = sgit(&r, a);
        let want = direct(&r, &r.dir, a);
        assert_eq!((o.status, &o.stdout, &o.stderr), (want.status, &want.stdout, &want.stderr), "{a:?}");
    }
    assert_ne!(direct(&r, &r.dir, &["show", "no-such-rev"]).status, 0, "exit codes pass through");
}

#[test]
fn scoped_git_checks_the_paths() {
    let r = shared(false);
    let before = porcelain(&r);
    for a in [
        &["add", "other.txt"][..],
        &["add", "*.txt"],
        &["add", ":/"],
        &["add", "--", "./other.txt"],
        &["add", r.dir.join("other.txt").to_str().unwrap()],
        &["rm", "--cached", "other.txt"],
        &["restore", "other.txt"],
        &["checkout", "HEAD", "--", "other.txt"],
        &["stash", "push", "--", "other.txt"],
        &["mv", "other.txt", "x.txt"],
        &["commit", "-m", "x", "--", "other.txt"],
        &["commit", "--amend", "-m", "x", "--", "own.txt"],
        &["switch", "-c", "x"],
        &["checkout", "-b", "x"],
        &["reset", "HEAD~1"],
        &["clean", "-fdx"],
    ] {
        let o = sgit(&r, a);
        if a[0] == "clean" {
            assert_eq!(o.status, 0, "nothing untracked is claimed: {}", o.out());
            continue;
        }
        assert!(refused(&o), "{a:?}: {}", o.out());
        assert_eq!(porcelain(&r), before, "{a:?}");
    }
    // own files, exclusions, index-only forms
    assert_eq!(sgit(&r, &["add", "own.txt"]).status, 0);
    assert_eq!(sgit(&r, &["restore", "--staged", "."]).status, 0);
    assert_eq!(sgit(&r, &["add", ":!other.txt", "."]).status, 0);
    assert_eq!(porcelain(&r), " M other.txt\nM  own.txt".trim());
    let o = sgit(&r, &["commit", "-q", "-m", "mine", "--", "own.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(git(&r, &r.dir, &["log", "-1", "--format=%s"]), "mine");
    assert_eq!(porcelain(&r), "M other.txt", "their work is untouched");
    write(&r.dir, "own.txt", "again\n");
    assert_eq!(sgit(&r, &["checkout", "--", "own.txt"]).status, 0);
    assert_eq!(std::fs::read_to_string(r.dir.join("own.txt")).unwrap(), "own changed\n");
}

#[test]
fn clean_only_minds_untracked_claimed_files() {
    let r = shared(false);
    write(&r.dir, "junk.txt", "x");
    write(&r.dir, "theirs-new.txt", "y");
    claims(&r, &[("other.txt", OTHER, None), ("theirs-new.txt", OTHER, None)], "");
    assert!(refused(&sgit(&r, &["clean", "-f"])));
    assert!(r.dir.join("junk.txt").exists());
    assert_eq!(sgit(&r, &["clean", "-f", "junk.txt"]).status, 0);
    assert!(!r.dir.join("junk.txt").exists() && r.dir.join("theirs-new.txt").exists());
    assert_eq!(sgit(&r, &["clean", "-n"]).status, 0);
    claims(&r, &[("other.txt", OTHER, None)], "");
    assert_eq!(sgit(&r, &["clean", "-f"]).status, 0);
    assert!(!r.dir.join("theirs-new.txt").exists());
    assert!(r.dir.join("other.txt").exists(), "tracked files are not cleaned");
}

#[test]
fn bang_alias_git_is_judged_from_its_text() {
    // git puts its exec-path first on a `!` alias's PATH, so the alias's own
    // git calls never reach the shim: the shim judges them before running it.
    let r = shared(false);
    git(&r, &r.dir, &["config", "alias.sweep", "!echo saving && git add -A"]);
    git(&r, &r.dir, &["config", "alias.wipe", "!f() { git checkout \"$@\"; }; f"]);
    git(&r, &r.dir, &["config", "alias.mine", "!git add own.txt && git status --short"]);
    git(&r, &r.dir, &["config", "alias.st", "status --short"]);
    let o = sgit(&r, &["sweep"]);
    assert!(refused(&o), "{}", o.out());
    assert!(o.stderr.contains("agent-lock: `git add -A` (run by the alias `git sweep` = !echo saving && git add -A)"), "{}", o.stderr);
    assert!(refused(&sgit(&r, &["wipe", "--", "."])), "$@ carries the alias's arguments");
    assert_eq!(porcelain(&r), " M other.txt\n M own.txt".trim_start());
    let o = sgit(&r, &["mine"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(porcelain(&r), " M other.txt\nM  own.txt".trim());
    // a plain alias resolves through the real git (no shim recursion) and runs
    let o = sgit(&r, &["st"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("other.txt"));
}

#[test]
fn disabled_unknown_caller_and_dead_owners_pass_through() {
    let r = shared(false);
    let o = shim(&r, "git", &r.dir, &["add", "other.txt"], &[("CLAUDE_FILE_LOCK", "0".into())]);
    assert_eq!(o.status, 0, "{}", o.out());
    git(&r, &r.dir, &["reset", "-q"]);
    let o = shim(&r, "git", &r.dir, &["add", "other.txt"], &[("AGENT_LOCK_AGENT_ID", String::new())]);
    assert_eq!(o.status, 0, "no agent identity: {}", o.out());
    git(&r, &r.dir, &["reset", "-q"]);
    // the session id is an identity too
    let o = shim(&r, "git", &r.dir, &["add", "other.txt"], &[("AGENT_LOCK_AGENT_ID", String::new()), ("CLAUDE_CODE_SESSION_ID", SA.into())]);
    assert!(refused(&o), "{}", o.out());
    claims(&r, &[("other.txt", OTHER, None)], r#","off":true"#);
    assert_eq!(sgit(&r, &["add", "other.txt"]).status, 0, "agent-lock off");
    git(&r, &r.dir, &["reset", "-q"]);
    claims(&r, &[("other.txt", OTHER, Some(dead_pid()))], "");
    assert_eq!(sgit(&r, &["add", "-A"]).status, 0, "a dead owner's claim is no live work");
    std::fs::remove_file(r.git_dir.join("agent-lock.json")).unwrap();
    git(&r, &r.dir, &["reset", "-q"]);
    assert_eq!(sgit(&r, &["add", "-A"]).status, 0, "no state file");
}

#[test]
fn subagents_see_their_parents_claims_and_get_as() {
    let r = shared(false);
    claims(&r, &[("own.txt", ME, None)], "");
    let o = shim(&r, "git", &r.dir, &["commit", "-am", "x"], &[("AGENT_LOCK_AGENT_ID", format!("{ME}:11111111"))]);
    assert!(refused(&o), "{}", o.out());
    assert!(o.stderr.contains("agent-lock commit --as aaaaaaaa:11111111 -m x"), "{}", o.stderr);
}

#[test]
fn the_real_tool_is_the_next_on_path() {
    let r = make_repo(false, &[]);
    let fake = r.base.join("fake");
    write(&fake, "git", "#!/bin/sh\necho \"fake git $*\"\nexit 7\n");
    Command::new("chmod").args(["+x", fake.join("git").to_str().unwrap()]).status().unwrap();
    let s = shims(&r);
    let o = shim(&r, "git", &r.dir, &["--version"], &[("PATH", path(&[&s, &fake]))]);
    assert_eq!((o.status, o.stdout.as_str()), (7, "fake git --version\n"));
    let o = shim(&r, "git", &r.dir, &["--version"], &[("PATH", s.display().to_string())]);
    assert_eq!(o.status, 127);
    assert!(o.stderr.contains("command not found"), "{}", o.stderr);

    // shims of another agent-lock copy (a plugin's and a cargo-installed one) are skipped too
    let other = r.base.join("other");
    std::fs::create_dir_all(other.join("shims")).unwrap();
    std::fs::copy(BIN, other.join("agent-lock-linux-x86_64")).unwrap();
    std::os::unix::fs::symlink(other.join("agent-lock-linux-x86_64"), other.join("shims/git")).unwrap();
    let o = shim(&r, "git", &r.dir, &["--version"], &[("PATH", path(&[&s, &other.join("shims"), &fake]))]);
    assert_eq!((o.status, o.stdout.as_str()), (7, "fake git --version\n"), "{}", o.stderr);
}

// ── package managers ──────────────────────────────────────────────────────

/// A fake npm after the shims: logs start/end, sleeps for `slow*`/`long*`,
/// installs again for `nested`, dies of SIGTERM for `sig`, appends its
/// package name to package-lock.json, exits $FAKE_EXIT.
fn pkg_repo() -> (Repo, Vec<(&'static str, String)>) {
    let r = make_repo(false, &[("package.json", "{}\n"), ("package-lock.json", "{}\n")]);
    let fake = r.base.join("fakebin");
    write(
        &fake,
        "npm",
        "#!/bin/sh\necho \"start $*\" >> \"$FAKE_LOG\"\ncase \"$2\" in slow*) sleep 1 ;; long*) sleep 3 ;; nested) npm install inner ;; sig) kill -TERM $$ ;; esac\n\
         [ \"$1\" = install ] && echo \"$2\" >> package-lock.json\necho \"end $*\" >> \"$FAKE_LOG\"\nexit ${FAKE_EXIT:-0}\n",
    );
    Command::new("chmod").args(["+x", fake.join("npm").to_str().unwrap()]).status().unwrap();
    let s = shims(&r);
    let env = vec![
        ("PATH", path(&[&s, &fake])),
        ("FAKE_LOG", r.base.join("log").display().to_string()),
        ("CLAUDE_PID", std::process::id().to_string()),
    ];
    (r, env)
}

fn npm(r: &Repo, args: &[&str], env: &[(&'static str, String)]) -> Run {
    shim(r, "npm", &r.dir, args, env)
}

fn log(r: &Repo) -> Vec<String> {
    std::fs::read_to_string(r.base.join("log")).unwrap_or_default().lines().map(str::to_string).collect()
}

fn wait_for(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn pkg_locked(r: &Repo) -> bool {
    !state(r).get("locks").get("pkg").is_null()
}

#[test]
fn concurrent_installs_serialize_and_claims_are_counted() {
    let (r, env) = pkg_repo();
    std::thread::scope(|s| {
        let a = s.spawn(|| npm(&r, &["install", "slowA"], &env));
        wait_for("the first install's lock", || pkg_locked(&r));
        assert_eq!(holder(&r, "package-lock.json").as_deref(), Some(ME), "claimed while it runs");
        assert!(state(&r).get("claims").get("package-lock.json").get("pending").as_str().is_some_and(|p| p.starts_with("pkg:")));
        let b = npm(&r, &["install", "b"], &env);
        let a = a.join().unwrap();
        assert_eq!((a.status, b.status), (0, 0), "{} / {}", a.out(), b.out());
        let lines: Vec<&str> = b.stderr.lines().collect();
        assert_eq!(lines.len(), 1, "one waiting line: {}", b.stderr);
        assert!(lines[0].starts_with("agent-lock: waiting for `npm install slowA`"), "{}", lines[0]);
    });
    assert_eq!(log(&r), ["start install slowA", "end install slowA", "start install b", "end install b"]);
    assert_eq!(std::fs::read_to_string(r.dir.join("package-lock.json")).unwrap(), "{}\nslowA\nb\n");
    let st = state(&r);
    let c = st.get("claims").get("package-lock.json");
    assert_eq!((c.get("by").as_str(), c.get("edits").as_u64(), c.get("pending").is_null()), (Some(ME), Some(2), true));
    assert!(st.get("claims").get("package.json").is_null(), "unchanged manifest claim dropped");
    assert!(st.get("locks").get("pkg").is_null());
}

#[test]
fn install_refused_when_another_agent_holds_the_manifest() {
    let (r, env) = pkg_repo();
    no_shared(&r);
    claims(&r, &[("package.json", OTHER, None)], "");
    let o = npm(&r, &["install", "x"], &env);
    assert!(refused(&o), "{}", o.out());
    assert!(o.stderr.contains("package.json, which bbbbbbbb is editing"), "{}", o.stderr);
    assert!(o.stderr.contains(&format!("--claim {}", r.dir.join("package.json").display())));
    assert!(log(&r).is_empty(), "npm never ran");
    let st = state(&r);
    assert!(st.get("locks").get("pkg").is_null());
    assert!(st.get("claims").get("package-lock.json").is_null(), "all or nothing");
}

#[test]
fn exit_codes_pass_through_and_non_mutating_commands_skip_the_lock() {
    let (r, mut env) = pkg_repo();
    env.push(("FAKE_EXIT", "3".into()));
    let o = npm(&r, &["install", "x"], &env);
    assert_eq!(o.status, 3, "{}", o.out());
    assert!(state(&r).get("locks").get("pkg").is_null(), "released after a failure");
    assert_eq!(state(&r).get("claims").get("package-lock.json").get("edits").as_u64(), Some(1));
    let o = npm(&r, &["install", "sig"], &env);
    assert_eq!(o.status, 128 + 15, "a signal is 128+n: {}", o.out());
    assert!(state(&r).get("locks").get("pkg").is_null());
    assert!(state(&r).get("claims").get("package-lock.json").get("pending").is_null());
    std::fs::remove_file(r.git_dir.join("agent-lock.json")).unwrap();
    let o = npm(&r, &["run", "build"], &env);
    assert_eq!(o.status, 3);
    assert!(!r.git_dir.join("agent-lock.json").exists(), "npm run never touches the state");
    assert_eq!(log(&r).last().map(String::as_str), Some("end run build"));
}

fn spawn_npm(r: &Repo, args: &[&str], env: &[(&'static str, String)]) -> std::process::Child {
    Command::new(shims(r).join("npm"))
        .args(args)
        .current_dir(&r.dir)
        .env_clear()
        .envs(env_for(r))
        .envs(env.iter().cloned())
        .env("AGENT_LOCK_AGENT_ID", ME)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// The pkg lock, once it belongs to the tool rather than the shim `shim`.
fn tool_holds_lock(r: &Repo, shim: u32) -> bool {
    state(r).get("locks").get("pkg").get("owner").get("pid").as_u64().is_some_and(|p| p != shim as u64)
}

#[test]
fn the_lock_follows_the_install_not_the_shim() {
    let (r, env) = pkg_repo();
    // a shim killed on its own (SIGKILL cannot be passed on): its install still runs and keeps the lock
    let mut a = spawn_npm(&r, &["install", "longA"], &env);
    wait_for("the tool to own the lock", || tool_holds_lock(&r, a.id()));
    let marker = state(&r).get("claims").get("package-lock.json").get("pending").as_str().unwrap_or_default().to_string();
    assert!(marker.starts_with("pkg:") && !marker.starts_with(&format!("pkg:{}:", a.id())), "the claim marks the tool too: {marker}");
    a.kill().unwrap();
    a.wait().unwrap();
    let o = npm(&r, &["install", "b"], &env);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stderr.contains("waiting for `npm install longA`"), "{}", o.stderr);
    assert_eq!(log(&r), ["start install longA", "end install longA", "start install b", "end install b"], "never two at once");

    // a TERM aimed at the shim alone stops the tool, and the shim still settles
    let r2 = copy_of(&r, "term");
    std::fs::remove_file(r2.base.join("log")).ok();
    let env2: Vec<(&'static str, String)> = env.iter().map(|(k, v)| (*k, if *k == "FAKE_LOG" { r2.base.join("log").display().to_string() } else { v.replace(r.base.to_str().unwrap(), r2.base.to_str().unwrap()) })).collect();
    let mut a = spawn_npm(&r2, &["install", "longA"], &env2);
    wait_for("the tool to own the lock", || tool_holds_lock(&r2, a.id()) && !log(&r2).is_empty());
    assert!(Command::new("kill").args(["-TERM", &a.id().to_string()]).status().unwrap().success());
    let t = Instant::now();
    let code = a.wait().unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "the shim waited for the tool, not for its sleep");
    assert_eq!(code.code(), Some(128 + 15), "{code:?}");
    assert!(state(&r2).get("locks").get("pkg").is_null(), "released");
    assert!(state(&r2).get("claims").get("package-lock.json").get("pending").is_null());
    assert_eq!(log(&r2), ["start install longA"], "the tool was stopped");
}

#[test]
fn a_subagents_install_keeps_its_own_claims() {
    // The shim only knows the session's main id (subagents share its environment).
    let (r, env) = pkg_repo();
    put_state(&r, &[("package-lock.json", SUB, &live()), ("package.json", SUB, &live())], &session(ME, &[(SUB, false)]), "");
    let o = npm(&r, &["install", "x"], &env);
    assert_eq!(o.status, 0, "never blocked by its own family: {}", o.out());
    let st = state(&r);
    for f in ["package.json", "package-lock.json"] {
        let c = st.get("claims").get(f);
        assert_eq!((c.get("by").as_str(), c.get("pending").is_null()), (Some(SUB), true), "{f} stays with its holder");
    }
    assert_eq!(st.get("claims").get("package-lock.json").get("edits").as_u64(), Some(2), "the change counts for the holder");
    assert_eq!(st.get("claims").get("package.json").get("edits").as_u64(), Some(1));
    // another family still blocks (a file that is not shared), and a subagent is told how to wait as itself
    no_shared(&r);
    put_state(&r, &[("package.json", OTHER, &live())], &session(ME, &[(SUB, false)]), "");
    let o = npm(&r, &["install", "x"], &env);
    assert!(refused(&o) && o.stderr.contains("(a subagent adds --as <its id>)"), "{}", o.out());
}

#[test]
fn installs_over_a_dead_agents_work_say_so() {
    let (r, env) = pkg_repo();
    put_state(&r, &[("package-lock.json", OTHER, &format!(r#"{{"pid":{},"start":1}}"#, dead_pid()))], "", "");
    let o = npm(&r, &["install", "x"], &env);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stderr.contains("[agent-lock] Note: package-lock.json was claimed by bbbbbbbb (its process has exited), which left 1 uncommitted edit"), "{}", o.stderr);
    let st = state(&r);
    assert_eq!(holder(&r, "package-lock.json").as_deref(), Some(ME));
    assert_eq!(st.get("orphans").get("package-lock.json").get("from").as_str(), Some(OTHER), "remembered until committed");
    // a claim created over another agent's leftovers
    let t = now();
    put_state(&r, &[], "", &format!(r#","orphans":{{"package-lock.json":{{"from":"{OTHER}","at":{t}}},"package.json":{{"from":"{OTHER}","at":{t}}}}}"#));
    let o = npm(&r, &["install", "y"], &env);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stderr.contains("[agent-lock] Note: package-lock.json may still hold uncommitted changes by bbbbbbbb"), "{}", o.stderr);
    assert!(o.stderr.contains("[agent-lock] Note: package.json may still hold"), "{}", o.stderr);
    let st = state(&r);
    assert_eq!(holder(&r, "package-lock.json").as_deref(), Some(ME));
    assert!(st.get("orphans").get("package-lock.json").is_null(), "the changed file's claim took them over");
    assert_eq!(st.get("orphans").get("package.json").get("from").as_str(), Some(OTHER), "unchanged and unclaimed: still a warning for the next one");
    assert!(st.get("claims").get("package.json").is_null());
}

/// A monorepo: root workspaces packages/*, web (@acme/web) and api.
fn mono() -> (Repo, Vec<(&'static str, String)>) {
    let (r, env) = pkg_repo();
    write(&r.dir, "package.json", r#"{"name":"root","workspaces":["packages/*"]}"#);
    write(&r.dir, "packages/web/package.json", r#"{"name":"@acme/web"}"#);
    write(&r.dir, "packages/api/package.json", r#"{"name":"api"}"#);
    let fake = r.base.join("fakebin");
    for n in ["pnpm", "yarn", "bun"] {
        std::os::unix::fs::symlink(fake.join("npm"), fake.join(n)).unwrap();
    }
    git(&r, &r.dir, &["add", "-A"]);
    git(&r, &r.dir, &["commit", "-qm", "mono"]);
    (r, env)
}

#[test]
fn workspace_installs_claim_the_workspace_manifest() {
    let (r, env) = mono();
    no_shared(&r);
    put_state(&r, &[("packages/web/package.json", OTHER, &live())], "", "");
    for (tool, args) in [
        ("npm", &["install", "x", "-w", "packages/web"][..]),
        ("npm", &["install", "x", "--workspace=@acme/web"]),
        ("npm", &["update", "--workspaces"]),
        ("pnpm", &["--filter", "web", "add", "x"]),
        ("pnpm", &["add", "x", "--filter", "./packages/web"]),
        ("pnpm", &["-r", "update"]),
        ("pnpm", &["--filter", "@acme/*", "add", "x"]),
        ("yarn", &["workspace", "@acme/web", "add", "x"]),
        ("bun", &["add", "x", "--filter", "@acme/web"]),
    ] {
        let o = shim(&r, tool, &r.dir, args, &env);
        assert!(refused(&o) && o.stderr.contains("packages/web/package.json, which bbbbbbbb is editing"), "{tool} {args:?}: {}", o.out());
    }
    assert!(log(&r).is_empty(), "none of them ran");
    // another workspace is free, and the root manifest is not the workspace's
    put_state(&r, &[("package.json", OTHER, &live())], "", "");
    let o = npm(&r, &["install", "x", "-w", "packages/api"], &env);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(holder(&r, "package.json").as_deref(), Some(OTHER));
    assert_eq!(holder(&r, "package-lock.json").as_deref(), Some(ME), "the root lockfile changed");
    assert!(state(&r).get("claims").get("packages/api/package.json").is_null(), "unchanged: dropped again");
}

#[test]
fn other_package_managers_claim_their_manifests() {
    let r = make_repo(false, &[("Cargo.toml", ""), ("go.mod", ""), ("pyproject.toml", ""), ("Gemfile", "")]);
    no_shared(&r);
    let fake = r.base.join("fakebin");
    write(&fake, "tool", "#!/bin/sh\necho \"$(basename \"$0\") $*\" >> \"$FAKE_LOG\"\n[ -n \"$FAKE_TOUCH\" ] && echo x >> \"$FAKE_TOUCH\"\nexit 0\n");
    Command::new("chmod").args(["+x", fake.join("tool").to_str().unwrap()]).status().unwrap();
    for n in ["cargo", "uv", "poetry", "go", "bundle"] {
        std::os::unix::fs::symlink(fake.join("tool"), fake.join(n)).unwrap();
    }
    let s = shims(&r);
    let env = |touch: &str| vec![("PATH", path(&[&s, &fake])), ("FAKE_LOG", r.base.join("log").display().to_string()), ("FAKE_TOUCH", r.dir.join(touch).display().to_string()), ("CLAUDE_PID", std::process::id().to_string())];
    for (tool, args, manifest, lock) in [
        ("cargo", &["add", "serde"][..], "Cargo.toml", "Cargo.lock"),
        ("go", &["get", "example.com/x"], "go.mod", "go.sum"),
        ("uv", &["add", "requests"], "pyproject.toml", "uv.lock"),
        ("poetry", &["add", "x"], "pyproject.toml", "poetry.lock"),
        ("bundle", &["add", "rails"], "Gemfile", "Gemfile.lock"),
    ] {
        put_state(&r, &[(manifest, OTHER, &live())], "", "");
        let o = shim(&r, tool, &r.dir, args, &env(lock));
        assert!(refused(&o) && o.stderr.contains(&format!("{manifest}, which bbbbbbbb is editing")), "{tool}: {}", o.out());
        put_state(&r, &[], "", "");
        let o = shim(&r, tool, &r.dir, args, &env(lock));
        assert_eq!(o.status, 0, "{tool}: {}", o.out());
        let c = state(&r).get("claims").get(lock).clone();
        assert_eq!((c.get("by").as_str(), c.get("edits").as_u64()), (Some(ME), Some(1)), "{tool}: the lockfile it wrote is claimed");
        assert!(state(&r).get("claims").get(manifest).is_null(), "{tool}: unchanged manifest released");
    }
    // everything else runs straight through, without the state
    std::fs::remove_file(r.git_dir.join("agent-lock.json")).unwrap();
    for (tool, args) in [("cargo", &["build", "--release"][..]), ("go", &["test", "./..."]), ("uv", &["run", "x"]), ("bundle", &["exec", "rake"])] {
        assert_eq!(shim(&r, tool, &r.dir, args, &env("x")).status, 0);
    }
    assert!(!r.git_dir.join("agent-lock.json").exists());
    assert_eq!(shim(&r, "uv", &r.dir, &["sync"], &env("x")).status, 0);
    let st = state(&r);
    assert!(st.get("locks").get("pkg").is_null() && st.get("claims").keys().is_empty(), "an install only takes the lock");
}

#[test]
fn waiting_gives_up_after_the_limit() {
    let (r, env) = pkg_repo();
    std::thread::scope(|s| {
        let a = s.spawn(|| npm(&r, &["install", "slowA"], &env));
        wait_for("the lock", || pkg_locked(&r));
        let mut e = env.clone();
        e.push(("AGENT_LOCK_PKG_WAIT", "0.3".into()));
        let b = npm(&r, &["install", "b"], &e);
        assert_eq!(b.status, 1);
        assert!(b.stderr.contains("gave up waiting"), "{}", b.stderr);
        assert_eq!(a.join().unwrap().status, 0);
    });
    assert!(!log(&r).iter().any(|l| l == "start install b"));
}

#[test]
fn nested_installs_and_other_repos_do_not_wait() {
    let (r, env) = pkg_repo();
    let t = Instant::now();
    let o = npm(&r, &["install", "nested"], &env);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(t.elapsed() < Duration::from_secs(4), "a lifecycle script's install runs inside the outer lock");
    assert!(log(&r).contains(&"start install inner".to_string()));
    let outside = tmp("noreop");
    let o = shim(&r, "npm", &outside, &["install", "x"], &env);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(outside.join("package-lock.json").exists());
}

#[test]
fn while_a_subagent_runs_the_main_agents_files_are_spared_too() {
    // Subagents run under the main id (CLAUDE_ENV_FILE), so "me" may be one.
    let r = shared(false);
    write(&r.dir, "new-own.txt", "mine, untracked\n");
    let claims = [("own.txt", ME, &live()[..]), ("new-own.txt", ME, &live()[..])];
    put_state(&r, &claims, &session(ME, &[(SUB, false)]), "");
    let before = porcelain(&r);
    for a in [&["stash"][..], &["checkout", "--", "."], &["reset", "--hard"], &["commit", "-am", "x"], &["clean", "-f"], &["add", "-A"], &["restore", "."]] {
        let o = sgit(&r, a);
        assert!(refused(&o), "{a:?}: {}", o.out());
        assert!(o.stderr.contains("own.txt by aaaaaaaa") && o.stderr.contains("AGENT_LOCK_AGENT_ID=<its id> git"), "{a:?}: {}", o.stderr);
    }
    assert_eq!(porcelain(&r), before, "nothing ran");
    assert_eq!(git(&r, &r.dir, &["stash", "list"]), "");
    // naming its own file is still fine
    assert_eq!(sgit(&r, &["add", "own.txt"]).status, 0);
    let o = sgit(&r, &["commit", "-qm", "mine", "--", "own.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    // a subagent's own claim is named with the way out
    put_state(&r, &[("other.txt", SUB, &live())], &session(ME, &[(SUB, false)]), "");
    let o = sgit(&r, &["add", "other.txt"]);
    assert!(refused(&o) && o.stderr.contains("other.txt by aaaaaaaa:11111111") && o.stderr.contains("AGENT_LOCK_AGENT_ID"), "{}", o.out());
    assert_eq!(shim(&r, "git", &r.dir, &["add", "other.txt"], &[("AGENT_LOCK_AGENT_ID", SUB.into())]).status, 0, "as itself");
    git(&r, &r.dir, &["reset", "-q"]);
    // once the subagent is done, the main agent's own sweeps run again
    put_state(&r, &[("other.txt", ME, &live())], &session(ME, &[(SUB, true)]), "");
    assert_eq!(sgit(&r, &["add", "-A"]).status, 0);
}

#[test]
fn bang_aliases_the_shim_cannot_follow_are_refused() {
    let r = shared(false);
    let before = porcelain(&r);
    for alias in [
        "!sh -c 'git checkout -- .'",
        "!bash -c \"git reset --hard\"",
        "!git ls-files -m | xargs git checkout --",
        "!git ls-files -m -z | xargs -0 git checkout --",
        "!git -C \"${GIT_PREFIX:-.}\" checkout -- .",
        "!for f in $(git ls-files -m); do git checkout -- \"$f\"; done",
        "!eval git stash",
        "!git --git-dir=\"$GIT_DIR\" stash",
        "!if true; then git stash; fi",
    ] {
        let o = sgit(&r, &["-c", &format!("alias.zz={alias}"), "zz"]);
        assert!(refused(&o), "{alias}: {}", o.out());
        assert!(o.stderr.contains("other.txt by bbbbbbbb"), "{alias}: {}", o.stderr);
        assert_eq!(porcelain(&r), before, "{alias}");
    }
    assert_eq!(git(&r, &r.dir, &["stash", "list"]), "");
    let o = sgit(&r, &["-c", "alias.zz=!sh -c 'git status --short'", "zz"]);
    assert_eq!(o.status, 0, "read-only git in a shell: {}", o.out());
}

#[test]
fn a_whole_index_commit_spares_another_sessions_staged_files() {
    // B is alive and staged a file it never claimed (made from Bash).
    let r = make_repo(false, &[("own.txt", "own\n"), ("old.txt", "old\n")]);
    write(&r.dir, "own.txt", "own changed\n");
    write(&r.dir, "gen.txt", "generated by B\n");
    git(&r, &r.dir, &["add", "gen.txt"]);
    git(&r, &r.dir, &["mv", "old.txt", "new.txt"]);
    put_state(&r, &[("own.txt", ME, &live())], &format!("{},{}", session(ME, &[]), session(OTHER, &[])), "");
    assert_eq!(sgit(&r, &["add", "own.txt"]).status, 0);
    for a in [&["commit", "-qm", "A's commit"][..], &["commit", "-qam", "x"], &["commit", "--amend", "--no-edit"]] {
        let o = sgit(&r, a);
        assert!(refused(&o), "{a:?}: {}", o.out());
        assert!(o.stderr.contains("gen.txt, new.txt, old.txt"), "{a:?}: {}", o.stderr);
        assert!(o.stderr.contains("agent-lock commit -m") && o.stderr.contains("-- <your paths>"), "{}", o.stderr);
    }
    let o = sgit(&r, &["commit", "-qm", "mine", "--", "own.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(git(&r, &r.dir, &["show", "--name-only", "--format=", "HEAD"]), "own.txt");
    // alone again: the commit runs as asked
    put_state(&r, &[], &session(ME, &[]), "");
    assert_eq!(sgit(&r, &["commit", "-qm", "all"]).status, 0);
}

#[test]
fn a_pathspec_in_another_unicode_spelling_still_meets_the_claim() {
    if !cfg!(target_os = "macos") {
        return; // git precomposes only on macOS
    }
    let (nfc, nfd) = ("caf\u{e9}.txt", "cafe\u{301}.txt");
    let r = make_repo(false, &[(nfc, "x\n")]);
    write(&r.dir, nfc, "theirs\n");
    claims(&r, &[(nfc, OTHER, None)], "");
    for spelling in [nfc, nfd] {
        let o = sgit(&r, &["add", spelling]);
        assert!(refused(&o), "{spelling:?}: {}", o.out());
    }
    assert_eq!(git(&r, &r.dir, &["diff", "--cached", "--name-only"]), "", "nothing staged");
}

#[test]
fn a_refused_stash_pop_names_the_way_back() {
    let r = shared(false);
    claims(&r, &[("own.txt", ME, None), ("other.txt", OTHER, None)], "");
    assert_eq!(sgit(&r, &["stash", "push", "-q", "-m", "A wip", "--", "own.txt"]).status, 0);
    for a in [&["stash", "pop"][..], &["stash", "apply", "stash@{0}"], &["stash", "drop"]] {
        let o = sgit(&r, a);
        assert!(refused(&o), "{a:?}: {}", o.out());
        assert!(o.stderr.contains("git stash show --name-only stash@{0}, then git checkout stash@{0} -- <your paths>"), "{a:?}: {}", o.stderr);
    }
    assert_eq!(sgit(&r, &["checkout", "stash@{0}", "--", "own.txt"]).status, 0, "the way back is allowed");
    assert_eq!(std::fs::read_to_string(r.dir.join("own.txt")).unwrap(), "own changed\n");
}

#[test]
fn a_stuck_state_lock_still_judges_whole_tree_git() {
    let r = shared(false);
    let f = std::fs::OpenOptions::new().read(true).write(true).open(r.git_dir.join("agent-lock.json")).unwrap();
    f.lock().unwrap(); // a frozen agent-lock process, past the 5 s wait
    let before = porcelain(&r);
    let o = sgit(&r, &["add", "-A"]);
    assert!(refused(&o), "{}", o.out());
    assert_eq!(porcelain(&r), before);
    f.unlock().unwrap();
}

#[test]
fn arguments_that_are_not_utf8_are_judged_too() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let r = shared(false);
    write(&r.dir, "new.txt", "B's untracked work\n");
    claims(&r, &[("own.txt", ME, None), ("other.txt", OTHER, None), ("new.txt", OTHER, None)], "");
    let s = shims(&r);
    let raw = |args: &[&[u8]]| {
        let o = Command::new(s.join("git"))
            .args(args.iter().map(|a| OsStr::from_bytes(a)))
            .current_dir(&r.dir)
            .env_clear()
            .envs(env_for(&r))
            .env("PATH", path(&[&s]))
            .env("AGENT_LOCK_AGENT_ID", ME)
            .output()
            .unwrap();
        Run { status: o.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&o.stdout).into(), stderr: String::from_utf8_lossy(&o.stderr).into() }
    };
    let before = porcelain(&r);
    for a in [&[&b"commit"[..], b"-qam", b"caf\xe9"][..], &[b"clean", b"-fd", b".", b"\xff"], &[b"checkout", b"caf\xe9"]] {
        let o = raw(a);
        assert!(refused(&o), "{a:?}: {}", o.out());
        assert!(o.stderr.contains("other.txt by bbbbbbbb") || o.stderr.contains("new.txt by bbbbbbbb"), "{a:?}: {}", o.stderr);
        assert_eq!(porcelain(&r), before, "{a:?} must not run");
    }
    assert!(r.dir.join("new.txt").exists());
    // naming only your own file runs, and git gets the message's raw bytes
    // (git itself reads the stray byte as Latin-1, never as U+FFFD)
    let o = raw(&[b"commit", b"-qm", b"caf\xe9", b"--", b"own.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(git(&r, &r.dir, &["show", "--name-only", "--format=", "HEAD"]), "own.txt");
    assert_eq!(git(&r, &r.dir, &["log", "-1", "--format=%s"]), "café");
}

#[test]
fn copies_of_the_binary_named_git_never_exec_each_other() {
    let r = make_repo(false, &[]);
    let fake = r.base.join("fake");
    write(&fake, "git", "#!/bin/sh\necho \"fake git $*\"\nexit 7\n");
    Command::new("chmod").args(["+x", fake.join("git").to_str().unwrap()]).status().unwrap();
    let (h1, h2) = (r.base.join("hl1"), r.base.join("hl2"));
    for h in [&h1, &h2] {
        std::fs::create_dir_all(h).unwrap();
    }
    std::fs::copy(BIN, h1.join("git")).unwrap();
    std::fs::hard_link(h1.join("git"), h2.join("git")).unwrap();
    for first in [&h1, &h2] {
        let mut c = Command::new(first.join("git"))
            .arg("--version")
            .current_dir(&r.dir)
            .env_clear()
            .envs(env_for(&r))
            .env("PATH", path(&[&h1, &h2, &fake]))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let t = Instant::now();
        while c.try_wait().unwrap().is_none() {
            if t.elapsed() > Duration::from_secs(5) {
                c.kill().unwrap();
                panic!("the copies exec each other forever");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let o = c.wait_with_output().unwrap();
        assert_eq!((o.status.code(), String::from_utf8_lossy(&o.stdout).as_ref()), (Some(7), "fake git --version\n"));
    }
}

// ── overhead ──────────────────────────────────────────────────────────────

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn time(n: usize, f: impl Fn() -> Run) -> Duration {
    median(
        (0..n)
            .map(|_| {
                let t = Instant::now();
                assert_eq!(f().status, 0);
                t.elapsed()
            })
            .collect(),
    )
}

#[test]
fn shim_overhead() {
    let r = shared(false);
    let n = 31;
    let direct_v = time(n, || direct(&r, &r.dir, &["--version"]));
    let shim_v = time(n, || sgit(&r, &["--version"]));
    let direct_s = time(n, || direct(&r, &r.dir, &["status", "--porcelain"]));
    let shim_s = time(n, || sgit(&r, &["status", "--porcelain"]));
    let direct_a = time(n, || direct(&r, &r.dir, &["add", "own.txt"]));
    let shim_a = time(n, || sgit(&r, &["add", "own.txt"]));
    let us = |d: Duration| d.as_micros() as i64;
    eprintln!(
        "shim overhead (median of {n}): --version {}µs (direct {}µs, shim {}µs); status {}µs; checked `add own.txt` with others' claims {}µs",
        us(shim_v) - us(direct_v),
        us(direct_v),
        us(shim_v),
        us(shim_s) - us(direct_s),
        us(shim_a) - us(direct_a)
    );
    assert!(shim_v < direct_v + Duration::from_millis(50), "the pass-through stays cheap");
}

#[test]
fn arguments_that_are_not_utf8_reach_the_real_tool() {
    use std::os::unix::ffi::OsStrExt;
    let r = shared(false);
    let s = shims(&r);
    let o = Command::new(s.join("git"))
        .args(["rev-parse", "--sq-quote"])
        .arg(std::ffi::OsStr::from_bytes(b"caf\xe9"))
        .current_dir(&r.dir)
        .env_clear()
        .envs(env_for(&r))
        .env("PATH", path(&[&s]))
        .env("AGENT_LOCK_AGENT_ID", ME)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(o.stdout.windows(6).any(|w| w == b"'caf\xe9'"), "{:?}", o.stdout);
}
