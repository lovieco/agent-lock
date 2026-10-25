//! Black-box tests of the CLI: the real binary, temp repos with remotes,
//! sessions via CLAUDE_CODE_SESSION_ID + CLAUDE_PID, claims made by real
//! hook calls.
mod common;
use common::*;
use std::path::Path;
use std::process::{Child, Command};

const A: &str = "aaaaaaaa";
const B: &str = "bbbbbbbb";
const SUB1: &str = "a1111111122222222"; // Claude Code agent_id → aaaaaaaa:11111111 in SA
const A1: &str = "aaaaaaaa:11111111";

fn me() -> u32 {
    std::process::id()
}

/// Another live process to own a session ("another Claude Code").
struct Proc(Child);
impl Proc {
    fn new() -> Proc {
        Proc(Command::new("sleep").arg("60").spawn().unwrap())
    }
    fn pid(&self) -> u32 {
        self.0.id()
    }
    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

fn ev(r: &Repo, event: &str, session: &str, extra: &str, pid: u32) -> Run {
    let out = hook(r, event, &payload(r, session, extra), pid);
    assert_eq!(out.status, 0, "{event}: {}", out.stderr);
    out
}

fn start(r: &Repo, session: &str, pid: u32) {
    ev(r, "session-start", session, r#""hook_event_name":"SessionStart","source":"startup""#, pid);
}

fn with_agent(agent: Option<&str>, rest: String) -> String {
    match agent {
        Some(a) => format!(r#""agent_id":"{a}","agent_type":"general-purpose",{rest}"#),
        None => rest,
    }
}

/// A full edit through the hooks: claim, write, record. `agent`: a subagent's Claude Code id.
fn edit_as(r: &Repo, s: &str, agent: Option<&str>, f: &Path, text: &str, tu: &str, pid: u32) {
    let pre = ev(r, "pre-edit", s, &with_agent(agent, edit(f, tu)), pid);
    assert_ne!(pre.decision().as_deref(), Some("deny"), "{}", pre.reason());
    std::fs::write(f, text).unwrap();
    ev(r, "post-edit", s, &with_agent(agent, edit(f, tu).replace("PreToolUse", "PostToolUse")), pid);
}

fn edit_file(r: &Repo, s: &str, rel: &str, text: &str, tu: &str) {
    edit_as(r, s, None, &r.dir.join(rel), text, tu, me());
}

/// The CLI as the main agent of `session`.
fn al(r: &Repo, session: &str, args: &[&str]) -> Run {
    cli(r, &r.dir, args, Some(session), me())
}

fn al_env(r: &Repo, args: &[&str], env: &[(&str, String)]) -> Run {
    run_bin(r, None, args, &r.dir, env, "")
}

fn head_files(r: &Repo) -> String {
    git(r, &r.dir, &["show", "--name-only", "--format=", "HEAD"]).replace('\n', ",")
}

fn claim(r: &Repo, rel: &str) -> serde_free::Json {
    state(r).get("claims").get(rel).clone()
}

/// The claim carries an `agent-lock commit` marker ("commit:<pid>:<start>").
fn committing(r: &Repo, rel: &str) -> bool {
    claim(r, rel).get("pending").as_str().is_some_and(|p| p.starts_with("commit:"))
}

fn repo() -> Repo {
    make_repo(true, &[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")])
}

/// Two live sessions: A (this process) and B (another process).
fn two() -> (Repo, Proc) {
    let r = repo();
    let b = Proc::new();
    start(&r, SA, me());
    start(&r, SB, b.pid());
    (r, b)
}

fn edit_b(r: &Repo, b: &Proc, rel: &str, text: &str, tu: &str) {
    edit_as(r, SB, None, &r.dir.join(rel), text, tu, b.pid());
}

/// A commit pushed to origin from another clone ("another machine").
fn upstream_change(r: &Repo, file: &str, text: &str) -> String {
    let up = r.base.join("up");
    if !up.exists() {
        clone(r, "up");
    }
    git(r, &up, &["pull", "-q", "--ff-only", "origin", "main"]);
    write(&up, file, text);
    git(r, &up, &["add", "-A"]);
    git(r, &up, &["commit", "-q", "-m", &format!("up: {file}")]);
    git(r, &up, &["push", "-q", "origin", "main"]);
    git(r, &up, &["rev-parse", "HEAD"])
}

// ── identity ──────────────────────────────────────────────────────────────

#[test]
fn identity_comes_from_as_then_env_then_session() {
    let r = repo();
    start(&r, SA, me());
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    let a_txt = r.dir.join("a.txt").display().to_string();
    let pid = ("CLAUDE_PID", me().to_string());
    let sess = ("CLAUDE_CODE_SESSION_ID", SA.to_string());
    // the session id names the main agent
    assert_eq!(al_env(&r, &["mine"], &[pid.clone(), sess.clone()]).stdout.trim(), a_txt);
    // $AGENT_LOCK_AGENT_ID beats it, --as beats both (also before the command, and v2 spellings)
    let env_b = ("AGENT_LOCK_AGENT_ID", B.to_string());
    assert_eq!(al_env(&r, &["mine"], &[pid.clone(), sess.clone(), env_b.clone()]).stdout.trim(), "");
    assert_eq!(al_env(&r, &["mine", "--as", A], &[pid.clone(), env_b.clone()]).stdout.trim(), a_txt);
    assert_eq!(al_env(&r, &["--as=claude-code-sess-AAAAAAAA", "mine"], std::slice::from_ref(&env_b)).stdout.trim(), a_txt);
    assert_eq!(al_env(&r, &["mine", "--as", SA], &[]).stdout.trim(), a_txt, "a full session id");
    // nobody → usage error
    let o = al_env(&r, &["mine"], std::slice::from_ref(&pid));
    assert_eq!(o.status, 2, "{}", o.out());
    assert!(o.stderr.contains("cannot tell which agent you are"), "{}", o.stderr);
    assert_eq!(al_env(&r, &["commit", "-m", "x"], &[]).status, 2);
    assert_eq!(al_env(&r, &["mine", "--as", " "], &[]).status, 2, "an empty --as");
}

// ── commit ────────────────────────────────────────────────────────────────

#[test]
fn commit_takes_only_own_files_and_leaves_others_work_alone() {
    let (r, b) = two();
    edit_file(&r, SA, "a.txt", "A's change\n", "t1");
    write(&r.dir, "new-by-a.txt", "unclaimed\n");
    // B: a staged version and newer unstaged changes on top
    edit_b(&r, &b, "b.txt", "B staged\n", "t2");
    git(&r, &r.dir, &["add", "b.txt"]);
    edit_b(&r, &b, "b.txt", "B staged\nB unstaged\n", "t3");
    let o = al(&r, SA, &["commit", "-m", "A: a.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("committed ") && o.stdout.contains("1 file\n  a.txt"), "{}", o.stdout);
    assert!(o.stdout.contains("other agents have uncommitted changes in 1 file (not included)"), "{}", o.stdout);
    assert!(o.stdout.contains("unattributed changes (no agent claims them; not included): 1 file\n  new-by-a.txt"), "{}", o.stdout);
    assert_eq!(head_files(&r), "a.txt");
    assert_eq!(git(&r, &r.dir, &["log", "-1", "--format=%s"]), "A: a.txt");
    // B's staged and unstaged work is exactly as it was
    assert_eq!(git(&r, &r.dir, &["diff", "--cached", "--name-only"]), "b.txt");
    assert_eq!(git(&r, &r.dir, &["show", ":b.txt"]), "B staged");
    assert_eq!(std::fs::read_to_string(r.dir.join("b.txt")).unwrap(), "B staged\nB unstaged\n");
    assert!(claim(&r, "a.txt").is_null(), "a committed, clean claim is released");
    assert_eq!(claim(&r, "b.txt").get("by").as_str(), Some(B));
    // nothing left of A's: says so, exit 0
    let o = al(&r, SA, &["commit", "-m", "again"]);
    assert_eq!(o.status, 0);
    assert!(o.stdout.contains("nothing to commit: you hold no claimed files"), "{}", o.stdout);
}

#[test]
fn explicit_paths_restrict_and_claim_unclaimed_files() {
    let (r, _b) = two();
    edit_file(&r, SA, "a.txt", "A1\n", "t1");
    edit_file(&r, SA, "c.txt", "C1\n", "t2");
    let o = al(&r, SA, &["commit", "-m", "only a", "--", "a.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(head_files(&r), "a.txt");
    assert_eq!(claim(&r, "c.txt").get("by").as_str(), Some(A), "c.txt stays claimed and dirty");
    // a file nobody claimed (written by a script) is claimed and committed
    write(&r.dir, "gen/out.txt", "generated\n");
    let o = al(&r, SA, &["commit", "-m", "generated", "--", "gen/out.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("gen/out.txt (was unclaimed)"), "{}", o.stdout);
    assert_eq!(head_files(&r), "gen/out.txt");
    assert!(claim(&r, "gen/out.txt").is_null());
    // a directory means your own claims under it; an unchanged file is reported
    edit_file(&r, SA, "gen/out.txt", "v2\n", "t3");
    let o = al(&r, SA, &["commit", "-m", "dir", "--", "gen", "b.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("not committed:\n  b.txt (unchanged)"), "{}", o.stdout);
    assert_eq!(head_files(&r), "gen/out.txt");
    assert!(state(&r).get("claims").get("b.txt").is_null(), "an unchanged unclaimed path is not claimed");
    // outside the worktree → usage
    let o = al(&r, SA, &["commit", "-m", "x", "--", "/etc/hosts"]);
    assert_eq!(o.status, 2, "{}", o.out());
}

#[test]
fn another_agents_file_is_refused_and_nothing_is_committed() {
    let (r, b) = two();
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    edit_b(&r, &b, "b.txt", "B\n", "t2");
    let before = git(&r, &r.dir, &["rev-parse", "HEAD"]);
    let o = al(&r, SA, &["commit", "-m", "grab", "--", "a.txt", "b.txt"]);
    assert_eq!(o.status, 1, "{}", o.out());
    assert!(o.stderr.contains("nothing was committed:\n  b.txt is held by bbbbbbbb (process alive"), "{}", o.stderr);
    assert!(o.stderr.contains("1 edit uncommitted"), "{}", o.stderr);
    assert_eq!(git(&r, &r.dir, &["rev-parse", "HEAD"]), before);
    assert_eq!(claim(&r, "b.txt").get("by").as_str(), Some(B));
    assert!(claim(&r, "a.txt").get("pending").is_null(), "no marker left behind");
    // a dry run says what would be committed and changes nothing
    let o = al(&r, SA, &["commit", "--dry-run"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("dry run: would commit 1 file:\n  a.txt"), "{}", o.stdout);
    assert_eq!(git(&r, &r.dir, &["rev-parse", "HEAD"]), before);
}

#[test]
fn a_subagent_commits_its_own_files_not_the_inherited_ones() {
    let r = repo();
    start(&r, SA, me());
    ev(&r, "subagent-start", SA, &format!(r#""hook_event_name":"SubagentStart","agent_id":"{SUB1}","agent_type":"general-purpose""#), me());
    edit_file(&r, SA, "a.txt", "parent's work\n", "t1");
    // the subagent edits the parent's dirty file: the claim moves with the parent's edits
    edit_as(&r, SA, Some(SUB1), &r.dir.join("a.txt"), "parent's work\nsub's line\n", "t2", me());
    edit_as(&r, SA, Some(SUB1), &r.dir.join("s.txt"), "sub's own\n", "t3", me());
    assert_eq!(claim(&r, "a.txt").get("inherited").as_str(), Some(A));
    let o = al(&r, SA, &["commit", "--as", A1, "-m", "sub work"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(head_files(&r), "s.txt");
    assert!(o.stdout.contains("inherited from aaaaaaaa: left for your parent to commit"), "{}", o.stdout);
    assert_eq!(claim(&r, "a.txt").get("by").as_str(), Some(A1));
    // named explicitly, it is included
    let o = al(&r, SA, &["commit", "--as", A1, "-m", "with the parent's", "--", "a.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(head_files(&r), "a.txt");
    // the main agent sees its running subagent's files as not included
    edit_as(&r, SA, Some(SUB1), &r.dir.join("s.txt"), "more\n", "t4", me());
    let o = al(&r, SA, &["commit", "-m", "main"]);
    assert!(o.stdout.contains("held by your subagent aaaaaaaa:11111111: 1 file, not included"), "{}", o.stdout);
    // and cannot take it while it runs
    let o = al(&r, SA, &["commit", "-m", "main", "--", "s.txt"]);
    assert_eq!(o.status, 1, "{}", o.out());
    // once it has finished, naming the file takes it over
    ev(&r, "subagent-stop", SA, &format!(r#""hook_event_name":"SubagentStop","agent_id":"{SUB1}""#), me());
    edit_as(&r, SA, Some(SUB1), &r.dir.join("c.txt"), "late\n", "t5", me()); // a stray call revives it…
    ev(&r, "subagent-stop", SA, &format!(r#""hook_event_name":"SubagentStop","agent_id":"{SUB1}""#), me()); // …until it stops
    assert_eq!(claim(&r, "c.txt").get("by").as_str(), Some(A), "subagent-stop hands its files back");
}

#[test]
fn a_finished_subagents_file_is_taken_over_by_naming_it() {
    let r = repo();
    start(&r, SA, me());
    ev(&r, "subagent-start", SA, &format!(r#""hook_event_name":"SubagentStart","agent_id":"{SUB1}","agent_type":"general-purpose""#), me());
    edit_as(&r, SA, Some(SUB1), &r.dir.join("s.txt"), "sub\n", "t1", me());
    // its claim is still pending a hand-over (no SubagentStop yet), but it is marked done
    let st = std::fs::read_to_string(r.git_dir.join("agent-lock.json")).unwrap();
    std::fs::write(r.git_dir.join("agent-lock.json"), st.replace("\"done\": false", "\"done\": true")).unwrap();
    let o = al(&r, SA, &["commit", "-m", "finish the sub's work", "--", "s.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("s.txt (taken over from aaaaaaaa:11111111)"), "{}", o.stdout);
    assert!(claim(&r, "s.txt").is_null());
}

#[test]
fn a_manifest_brings_its_dirty_lockfile() {
    let r = make_repo(false, &[("package.json", "{}\n"), ("package-lock.json", "{}\n"), ("app/package.json", "{}\n")]);
    let b = Proc::new();
    start(&r, SA, me());
    start(&r, SB, b.pid());
    edit_file(&r, SA, "package.json", "{\"dependencies\":{\"x\":\"1\"}}\n", "t1");
    write(&r.dir, "package-lock.json", "{\"x\":1}\n"); // npm rewrote it
    let o = al(&r, SA, &["commit", "-m", "add x"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("package-lock.json (lockfile of package.json)"), "{}", o.stdout);
    assert_eq!(head_files(&r), "package-lock.json,package.json");
    // a workspace package pairs with the root lockfile; another agent's changes in it
    // (a shared file) go in with it
    edit_file(&r, SA, "app/package.json", "{\"y\":1}\n", "t2");
    edit_as(&r, SB, None, &r.dir.join("package-lock.json"), "{\"b\":1}\n", "t3", b.pid());
    let o = al(&r, SA, &["commit", "-m", "app"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("package-lock.json (lockfile of app/package.json; shared, with changes by bbbbbbbb)"), "{}", o.stdout);
    assert!(claim(&r, "package-lock.json").is_null(), "committed for every holder");
    // not shared (the repo opts it out): held by another agent → refused
    write(&r.dir, ".agent-lock-shared", "!package-lock.json\n");
    edit_file(&r, SA, "app/package.json", "{\"y\":2}\n", "t4");
    edit_as(&r, SB, None, &r.dir.join("package-lock.json"), "{\"b\":2}\n", "t5", b.pid());
    let o = al(&r, SA, &["commit", "-m", "app", "--", "app/package.json"]);
    assert_eq!(o.status, 1, "{}", o.out());
    assert!(
        o.stderr.contains("package-lock.json (lockfile of app/package.json, which must be committed with it) is held by bbbbbbbb"),
        "{}",
        o.stderr
    );
    assert_eq!(head_files(&r), "app/package.json,package-lock.json");
}

#[test]
fn a_failing_pre_commit_hook_keeps_the_claims_and_stages_nothing() {
    let (r, _b) = two();
    let hook = write(&r.git_dir, "hooks/pre-commit", "#!/bin/sh\necho 'lint: 3 problems' >&2\nexit 1\n");
    Command::new("chmod").args(["+x", hook.to_str().unwrap()]).status().unwrap();
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    write(&r.dir, "u.txt", "untracked\n");
    let before = git(&r, &r.dir, &["rev-parse", "HEAD"]);
    let o = al(&r, SA, &["commit", "-m", "x", "--", "a.txt", "u.txt"]);
    assert_eq!(o.status, 1, "{}", o.out());
    assert!(o.stderr.contains("commit failed (hook-failed)") && o.stderr.contains("lint: 3 problems"), "{}", o.stderr);
    assert!(o.stderr.contains("Nothing was committed or staged; your claims are kept."), "{}", o.stderr);
    assert_eq!(git(&r, &r.dir, &["rev-parse", "HEAD"]), before);
    assert_eq!(git(&r, &r.dir, &["diff", "--cached", "--name-only"]), "");
    assert_eq!(git(&r, &r.dir, &["status", "--porcelain", "--", "u.txt"]), "?? u.txt", "no intent-to-add left");
    let c = claim(&r, "a.txt");
    assert_eq!((c.get("by").as_str(), c.get("edits").as_u64()), (Some(A), Some(1)));
    assert!(c.get("pending").is_null() && c.get("reason").is_null(), "marker cleared: {c:?}");
    assert!(claim(&r, "u.txt").is_null(), "the claim this commit made is undone");
    // --no-verify skips the hook
    let o = al(&r, SA, &["commit", "--no-verify", "-m", "x", "--", "a.txt", "u.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(head_files(&r), "a.txt,u.txt");
}

#[test]
fn an_edit_that_lands_in_the_commit_is_released_not_kept() {
    // gitcli/GIT-7: an own Edit during the hooks replaced the marker; git commits that content
    let (r, _b) = two();
    edit_file(&r, SA, "a.txt", "A1\n", "t1");
    let child = slow_commit(&r, &[]);
    edit_file(&r, SA, "a.txt", "A2\n", "t2");
    let o = child.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(git(&r, &r.dir, &["show", "HEAD:a.txt"]), "A2");
    assert!(!out.contains("changed again"), "{out}");
    assert!(claim(&r, "a.txt").is_null(), "clean: released, {:?}", claim(&r, "a.txt"));
}

#[test]
fn a_case_only_rename_is_explained_as_one() {
    // gitcli/GIT-9: not "staged for removal (`git rm --cached`)"
    let (r, _b) = two();
    if !r.dir.join("A.TXT").exists() {
        return; // case-sensitive filesystem
    }
    git(&r, &r.dir, &["mv", "a.txt", "A.txt"]);
    let o = al(&r, SA, &["commit", "-m", "rename", "--", "a.txt", "A.txt"]);
    assert_eq!(o.status, 1, "{}", o.out());
    assert!(o.stderr.contains("(case-rename)") && o.stderr.contains("a.txt → A.txt") && !o.stderr.contains("rm --cached"), "{}", o.stderr);
}

/// A state file holding just these claims: `(path as stored, agent)`.
fn put_claims(r: &Repo, list: &[(&str, &str)]) {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let body: Vec<String> = list.iter().map(|(p, by)| format!(r#""{p}":{{"by":"{by}","at":{t},"touched":{t},"edits":1}}"#)).collect();
    std::fs::write(r.git_dir.join("agent-lock.json"), format!(r#"{{"v":3,"claims":{{{}}}}}"#, body.join(","))).unwrap();
}

#[test]
fn a_claim_spelled_unlike_git_is_committed_or_kept_never_dropped() {
    // state/F1 (cli side): git prints its own spelling; a release needs git's word
    let r = repo();
    if r.dir.join("A.TXT").exists() {
        // case: claimed as A.TXT, tracked as a.txt
        write(&r.dir, "a.txt", "A changed\n");
        put_claims(&r, &[("A.TXT", A)]);
        let o = al(&r, SA, &["commit", "-m", "case"]);
        assert_eq!(o.status, 0, "{}", o.out());
        assert_eq!(head_files(&r), "a.txt", "{}", o.out());
        assert!(state(&r).get("claims").keys().is_empty(), "{:?}", state(&r).get("claims"));
    }
    if cfg!(target_os = "macos") {
        // Unicode: an NFD claim key (a v3.0 state file) for git's NFC file
        let (nfc, nfd) = ("caf\u{e9}.txt", "cafe\u{301}.txt");
        write(&r.dir, nfc, "x\n");
        git(&r, &r.dir, &["add", nfc]);
        git(&r, &r.dir, &["commit", "-q", "-m", "cafe"]);
        write(&r.dir, nfc, "dirty\n");
        put_claims(&r, &[(nfd, A)]);
        let o = al(&r, SA, &["commit", "-m", "nfd"]);
        assert_eq!(o.status, 0, "{}", o.out());
        assert_eq!(state(&r).get("claims").get(nfd).get("by").as_str(), Some(A), "not confirmed clean: kept\n{}", o.out());
    }
}

/// `agent-lock commit` in the background with a slow pre-commit hook.
fn slow_commit(r: &Repo, extra: &[&str]) -> Child {
    let hook = write(&r.git_dir, "hooks/pre-commit", "#!/bin/sh\nsleep 1\n");
    Command::new("chmod").args(["+x", hook.to_str().unwrap()]).status().unwrap();
    let mut cmd = Command::new(BIN);
    cmd.args(["commit", "-m", "slow"]).args(extra).current_dir(&r.dir).env_clear().envs(env_for(r));
    cmd.env("CLAUDE_CODE_SESSION_ID", SA).env("CLAUDE_PID", me().to_string());
    let child = cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let t = std::time::Instant::now();
    while !committing(r, "a.txt") {
        assert!(t.elapsed().as_secs() < 5, "the commit never marked its claim");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child
}

#[test]
fn a_terminate_signal_waits_until_the_claims_are_settled() {
    let (r, _b) = two();
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    let child = slow_commit(&r, &[]);
    Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(143), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(head_files(&r), "a.txt", "the commit finished first");
    assert!(claim(&r, "a.txt").is_null(), "and its claim was settled");
}

#[test]
fn a_killed_commit_leaves_a_marker_the_next_command_clears() {
    let (r, _b) = two();
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    write(&r.dir, "u.txt", "unclaimed\n");
    let mut child = slow_commit(&r, &["--", "a.txt", "u.txt"]);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(committing(&r, "a.txt"), "SIGKILL cannot be deferred");
    let o = cli(&r, &r.dir, &["doctor"], None, me());
    assert!(o.stdout.contains("[stuck] a.txt — aaaaaaaa: an `agent-lock commit` was killed before it finished"), "{}", o.stdout);
    // the owner's next commit clears it and goes ahead
    let o = al(&r, SA, &["commit", "--no-verify", "-m", "again", "--", "a.txt", "u.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(head_files(&r), "a.txt,u.txt");
    assert!(claim(&r, "a.txt").is_null() && claim(&r, "u.txt").is_null());
}

#[test]
fn the_next_prompt_clears_a_killed_commits_marker_but_never_a_running_ones() {
    let (r, _b) = two();
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    let prompt = r#""hook_event_name":"UserPromptSubmit","prompt":"next""#;
    let mut child = slow_commit(&r, &[]);
    ev(&r, "prompt", SA, prompt, me());
    assert!(committing(&r, "a.txt"), "a running commit keeps its marker");
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(committing(&r, "a.txt"));
    ev(&r, "prompt", SA, prompt, me());
    let c = claim(&r, "a.txt");
    assert!(c.get("pending").is_null() && c.get("reason").is_null(), "{c:?}");
    assert_eq!(c.get("by").as_str(), Some(A), "the claim itself stays");
}

#[test]
fn an_edit_through_a_symlink_commits_the_file_it_points_to() {
    let r = repo();
    start(&r, SA, me());
    std::os::unix::fs::symlink("c.txt", r.dir.join("link.txt")).unwrap();
    git(&r, &r.dir, &["add", "link.txt"]);
    git(&r, &r.dir, &["commit", "-q", "-m", "link"]);
    edit_file(&r, SA, "link.txt", "through the link\n", "t1");
    assert_eq!(std::fs::read_to_string(r.dir.join("c.txt")).unwrap(), "through the link\n");
    assert_eq!(claim(&r, "c.txt").get("by").as_str(), Some(A), "the claim is on the file the link points to");
    assert!(claim(&r, "link.txt").is_null());
    // so another agent editing the file by its own name is blocked
    let b = Proc::new();
    start(&r, SB, b.pid());
    let d = ev(&r, "pre-edit", SB, &edit(&r.dir.join("c.txt"), "t2"), b.pid());
    assert_eq!(d.decision().as_deref(), Some("deny"), "{}", d.stdout);
    assert!(d.reason().contains("c.txt is claimed by aaaaaaaa"), "{}", d.reason());
    let o = al(&r, SA, &["commit", "-m", "via link"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(head_files(&r), "c.txt");
    assert!(claim(&r, "link.txt").is_null() && claim(&r, "c.txt").is_null());
}

#[test]
fn a_claimed_symlink_still_commits_the_file_it_points_to() {
    // a link claim made by name (e.g. by an older version) stands for its target
    let r = repo();
    start(&r, SA, me());
    std::os::unix::fs::symlink("c.txt", r.dir.join("link.txt")).unwrap();
    git(&r, &r.dir, &["add", "link.txt"]);
    git(&r, &r.dir, &["commit", "-q", "-m", "link"]);
    std::fs::write(r.dir.join("c.txt"), "through the link\n").unwrap();
    let o = al(&r, SA, &["commit", "-m", "via link", "--", "link.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("c.txt (via symlink link.txt)"), "{}", o.stdout);
    assert_eq!(head_files(&r), "c.txt");
}

#[test]
fn messages_from_file_stdin_and_repeated_m() {
    let r = repo();
    start(&r, SA, me());
    edit_file(&r, SA, "a.txt", "1\n", "t1");
    let o = al(&r, SA, &["commit", "-m", "subject", "-m", "body line"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(git(&r, &r.dir, &["log", "-1", "--format=%B"]), "subject\n\nbody line");
    edit_file(&r, SA, "a.txt", "2\n", "t2");
    write(&r.dir, "../msg.txt", "from a file\n");
    let o = al(&r, SA, &["commit", "-F", "../msg.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(git(&r, &r.dir, &["log", "-1", "--format=%s"]), "from a file");
    edit_file(&r, SA, "a.txt", "3\n", "t3");
    let o =
        run_bin(&r, None, &["commit", "-F", "-"], &r.dir, &[("CLAUDE_CODE_SESSION_ID", SA.into()), ("CLAUDE_PID", me().to_string())], "from stdin\n");
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(git(&r, &r.dir, &["log", "-1", "--format=%s"]), "from stdin");
    assert_eq!(al(&r, SA, &["commit"]).status, 2, "a message is required");
    assert_eq!(al(&r, SA, &["commit", "-m", "x", "-F", "f"]).status, 2);
}

// ── push / sync ───────────────────────────────────────────────────────────

#[test]
fn commit_push_reaches_the_remote_even_when_it_moved() {
    let r = repo();
    start(&r, SA, me());
    let origin = r.origin.clone().unwrap();
    edit_file(&r, SA, "a.txt", "A1\n", "t1");
    let o = al(&r, SA, &["commit", "-m", "one", "--push"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("push: pushed"), "{}", o.stdout);
    assert_eq!(git(&r, &origin, &["rev-parse", "main"]), git(&r, &r.dir, &["rev-parse", "HEAD"]));
    // another machine pushes meanwhile: push syncs (replays) and retries
    let theirs = upstream_change(&r, "theirs.txt", "T\n");
    edit_file(&r, SA, "b.txt", "B1\n", "t2");
    let o = al(&r, SA, &["commit", "-m", "two", "--push"]);
    assert_eq!(o.status, 0, "{}", o.out());
    let head = git(&r, &r.dir, &["rev-parse", "HEAD"]);
    assert_eq!(git(&r, &origin, &["rev-parse", "main"]), head);
    assert_eq!(git(&r, &r.dir, &["rev-parse", "HEAD~1"]), theirs, "ours on top of theirs");
    assert_eq!(git(&r, &r.dir, &["log", "-1", "--format=%s"]), "two");
    assert!(state(&r).get("locks").get("git").is_null());
}

#[test]
fn push_blocked_by_your_own_dirty_claim_says_commit_it_first() {
    let r = repo();
    start(&r, SA, me());
    upstream_change(&r, "a.txt", "theirs\n");
    edit_file(&r, SA, "a.txt", "mine, uncommitted\n", "t1");
    write(&r.dir, "c.txt", "c2\n");
    git(&r, &r.dir, &["commit", "-q", "-m", "local", "--", "c.txt"]);
    let before = git(&r, &r.dir, &["rev-parse", "HEAD"]);
    let o = al(&r, SA, &["push"]);
    assert_eq!(o.status, 1, "{}", o.out());
    let e = o.stderr.to_lowercase();
    assert!(e.contains("push: blocked: upstream changed 1 file that is claimed"), "{}", o.stderr);
    assert!(e.contains("a.txt: you have uncommitted changes in it, and upstream changed it too"), "{}", o.stderr);
    assert!(e.contains("commit it first"), "{}", o.stderr);
    assert!(o.stderr.contains(&format!("-- {}", r.dir.join("a.txt").display())), "absolute path to copy: {}", o.stderr);
    assert_eq!(git(&r, &r.dir, &["rev-parse", "HEAD"]), before);
    assert_eq!(std::fs::read_to_string(r.dir.join("a.txt")).unwrap(), "mine, uncommitted\n");
}

#[test]
fn sync_push_and_a_conflict_without_a_caller() {
    let r = make_repo(true, &[("a.txt", "a\n")]);
    let pid = me();
    let u = upstream_change(&r, "u.txt", "u\n");
    write(&r.dir, "mine.txt", "dirty, uncommitted\n");
    let s = cli(&r, &r.dir, &["sync"], Some(SA), pid);
    assert_eq!(s.status, 0, "{}", s.out());
    assert!(s.stdout.contains("sync: fast-forwarded"), "{}", s.stdout);
    assert_eq!(git(&r, &r.dir, &["rev-parse", "HEAD"]), u);
    assert_eq!(std::fs::read_to_string(r.dir.join("mine.txt")).unwrap(), "dirty, uncommitted\n");
    // the caller is optional for sync and push
    write(&r.dir, "a.txt", "a2\n");
    git(&r, &r.dir, &["commit", "-q", "-am", "local"]);
    upstream_change(&r, "v.txt", "v\n");
    let p = cli(&r, &r.dir, &["push"], None, pid);
    assert_eq!(p.status, 0, "{}", p.out());
    let origin = r.origin.clone().unwrap();
    assert_eq!(git(&r, &origin, &["rev-parse", "main"]), git(&r, &r.dir, &["rev-parse", "HEAD"]));
    // a replay conflict changes nothing, exit 1
    upstream_change(&r, "a.txt", "theirs\n");
    write(&r.dir, "a.txt", "mine\n");
    git(&r, &r.dir, &["commit", "-q", "-am", "mine"]);
    let before = git(&r, &r.dir, &["rev-parse", "HEAD"]);
    let s = cli(&r, &r.dir, &["sync"], Some(SA), pid);
    assert_eq!(s.status, 1, "{}", s.out());
    assert!(s.stderr.contains("conflicts") && s.stderr.contains("a.txt"), "{}", s.stderr);
    assert_eq!(git(&r, &r.dir, &["rev-parse", "HEAD"]), before);
}

// ── wait ──────────────────────────────────────────────────────────────────

#[test]
fn wait_free_timeout_and_claim() {
    let (r, b) = two();
    edit_b(&r, &b, "b.txt", "B\n", "t1");
    let o = al(&r, SA, &["wait", "a.txt"]);
    assert_eq!((o.status, o.stdout.trim()), (0, "free: a.txt"), "{}", o.out());
    let t = std::time::Instant::now();
    let o = al(&r, SA, &["wait", "b.txt", "--timeout", "0.6"]);
    assert_eq!(o.status, 1, "{}", o.out());
    assert!(t.elapsed().as_secs_f64() >= 0.5);
    assert!(o.stderr.contains("timed out") && o.stderr.contains("b.txt is held by bbbbbbbb"), "{}", o.stderr);
    // the holder was told (a waiter notice comes with its next hook)
    let wants = state(&r).get("sessions").get(SB).get("wants").clone();
    assert_eq!(wants.arr()[0].get("waiter").as_str(), Some(A), "{wants:?}");
    let o = al(&r, SA, &["wait", "--claim", "c.txt"]);
    assert_eq!((o.status, o.stdout.trim()), (0, "claimed: c.txt"), "{}", o.out());
    let c = claim(&r, "c.txt");
    assert_eq!((c.get("by").as_str(), c.get("reason").as_str(), c.get("edits").as_u64()), (Some(A), Some("wait"), Some(0)));
    // the end-of-turn cleanup keeps a reservation
    ev(&r, "stop", SA, r#""hook_event_name":"Stop","stop_hook_active":false"#, me());
    assert_eq!(claim(&r, "c.txt").get("by").as_str(), Some(A));
    // bad input
    assert_eq!(al(&r, SA, &["wait"]).status, 2);
    assert_eq!(al(&r, SA, &["wait", "a.txt", "--timeout", "soon"]).status, 2);
    let outside = r.base.join("outside.txt");
    std::fs::write(&outside, "x").unwrap();
    let o = al(&r, SA, &["wait", outside.to_str().unwrap()]);
    assert_eq!(o.status, 2, "{}", o.out());
    assert!(o.stderr.contains("not a file in a git worktree"), "{}", o.stderr);
}

#[test]
fn wait_returns_when_the_holder_commits() {
    let (r, b) = two();
    edit_b(&r, &b, "b.txt", "B\n", "t1");
    let mut cmd = Command::new(BIN);
    cmd.args(["wait", "b.txt", "--timeout", "8"]).current_dir(&r.dir).env_clear().envs(env_for(&r));
    cmd.env("CLAUDE_CODE_SESSION_ID", SA).env("CLAUDE_PID", me().to_string()).stdout(std::process::Stdio::piped());
    let child = cmd.spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let o = cli(&r, &r.dir, &["commit", "-m", "B done"], Some(SB), b.pid());
    assert_eq!(o.status, 0, "{}", o.out());
    let t = std::time::Instant::now();
    let w = child.wait_with_output().unwrap();
    assert_eq!(w.status.code(), Some(0));
    assert!(t.elapsed().as_secs() < 3, "noticed within a poll");
    assert_eq!(String::from_utf8_lossy(&w.stdout).trim(), "free: b.txt");
}

#[test]
fn wait_detects_a_deadlock() {
    let (r, b) = two();
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    edit_b(&r, &b, "b.txt", "B\n", "t2");
    // B tries A's file and is blocked (recorded); A now waits for B's file
    let d = ev(&r, "pre-edit", SB, &edit(&r.dir.join("a.txt"), "t3"), b.pid());
    assert_eq!(d.decision().as_deref(), Some("deny"));
    let o = al(&r, SA, &["wait", "b.txt", "--timeout", "5"]);
    assert_eq!(o.status, 3, "{}", o.out());
    assert!(o.stderr.contains("DEADLOCK: b.txt is claimed by bbbbbbbb, and bbbbbbbb is waiting for a.txt (yours)"), "{}", o.stderr);
}

#[test]
fn wait_reports_a_deadlock_over_several_hops_with_its_edges() {
    // simplicity/S9: A waits for B, B for C; C waiting for A closes the ring
    let (r, b) = two();
    let c = Proc::new();
    start(&r, SC, c.pid());
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    edit_b(&r, &b, "b.txt", "B\n", "t2");
    edit_as(&r, SC, None, &r.dir.join("c.txt"), "C\n", "t3", c.pid());
    assert_eq!(ev(&r, "pre-edit", SA, &edit(&r.dir.join("b.txt"), "t4"), me()).decision().as_deref(), Some("deny"));
    assert_eq!(ev(&r, "pre-edit", SB, &edit(&r.dir.join("c.txt"), "t5"), b.pid()).decision().as_deref(), Some("deny"));
    let o = cli(&r, &r.dir, &["wait", "a.txt", "--timeout", "5"], Some(SC), c.pid());
    assert_eq!(o.status, 3, "{}", o.out());
    assert!(
        o.stderr.contains("a.txt is claimed by aaaaaaaa, and aaaaaaaa is waiting for b.txt (held by bbbbbbbb), bbbbbbbb is waiting for c.txt (yours)"),
        "{}",
        o.stderr
    );
}

#[test]
fn a_timed_out_wait_leaves_no_false_deadlock() {
    // gitcli/GIT-8: A gave up waiting for b.txt; B waiting for A's file is no deadlock
    let (r, b) = two();
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    edit_b(&r, &b, "b.txt", "B\n", "t2");
    assert_eq!(al(&r, SA, &["wait", "b.txt", "--timeout", "0.3"]).status, 1);
    assert!(state(&r).get("sessions").get(SA).get("blocked").keys().is_empty(), "{:?}", state(&r).get("sessions").get(SA));
    let o = cli(&r, &r.dir, &["wait", "a.txt", "--timeout", "0.3"], Some(SB), b.pid());
    assert_eq!(o.status, 1, "{}", o.out());
    assert!(!o.stderr.contains("DEADLOCK") && o.stderr.contains("timed out"), "{}", o.stderr);
}

#[test]
fn wait_claim_takes_over_a_dead_agents_file_and_says_so() {
    let r = repo();
    start(&r, SA, me());
    let mut b = Proc::new();
    start(&r, SB, b.pid());
    edit_b(&r, &b, "c.txt", "B's unfinished work\n", "t1");
    b.kill();
    let o = al(&r, SA, &["wait", "--claim", "c.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.starts_with("claimed: c.txt\n"), "{}", o.stdout);
    assert!(o.stdout.contains("c.txt was claimed by bbbbbbbb (its process has exited), which left 1 uncommitted edit in it"), "{}", o.stdout);
    assert!(o.stdout.contains(&format!("git -C {} diff -- c.txt", r.dir.display())), "{}", o.stdout);
    assert_eq!(claim(&r, "c.txt").get("by").as_str(), Some(A));
    assert_eq!(state(&r).get("orphans").get("c.txt").get("from").as_str(), Some(B), "remembered until committed");
    // a file somebody changed without a claim gets a note too
    write(&r.dir, "b.txt", "script output\n");
    let o = al(&r, SA, &["wait", "--claim", "b.txt"]);
    assert!(o.stdout.contains("b.txt already has uncommitted changes that were not made through your edits"), "{}", o.stdout);
    // a claim dropped without a commit: whoever takes the file next is told whose work it holds
    let c = Proc::new();
    start(&r, SC, c.pid());
    edit_file(&r, SA, "a.txt", "A's dropped work\n", "t2");
    al(&r, SA, &["release", "a.txt"]);
    let o = cli(&r, &r.dir, &["wait", "--claim", "a.txt"], Some(SC), c.pid());
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("a.txt may still hold uncommitted changes by aaaaaaaa"), "{}", o.stdout);
}

#[test]
fn two_agents_committing_at_once_each_get_exactly_their_files() {
    let (r, b) = two();
    for i in 0..4 {
        edit_file(&r, SA, &format!("a{i}.txt"), "A\n", &format!("ta{i}"));
        edit_b(&r, &b, &format!("b{i}.txt"), "B\n", &format!("tb{i}"));
    }
    let spawn = |s: &str, pid: u32, msg: &str| {
        let mut cmd = Command::new(BIN);
        cmd.args(["commit", "-m", msg]).current_dir(&r.dir).env_clear().envs(env_for(&r));
        cmd.env("CLAUDE_CODE_SESSION_ID", s)
            .env("CLAUDE_PID", pid.to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd.spawn().unwrap()
    };
    let (ca, cb) = (spawn(SA, me(), "A"), spawn(SB, b.pid(), "B"));
    for c in [ca, cb] {
        let o = c.wait_with_output().unwrap();
        assert_eq!(o.status.code(), Some(0), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    }
    for (who, files) in [("A", "a0.txt,a1.txt,a2.txt,a3.txt"), ("B", "b0.txt,b1.txt,b2.txt,b3.txt")] {
        let sha = git(&r, &r.dir, &["log", "-1", "--format=%H", &format!("--grep=^{who}$")]);
        assert_eq!(git(&r, &r.dir, &["show", "--name-only", "--format=", &sha]).replace('\n', ","), files);
    }
    assert_eq!(git(&r, &r.dir, &["status", "--porcelain"]), "");
    assert!(state(&r).get("claims").keys().is_empty(), "{:?}", state(&r).get("claims"));
}

// ── release / mine ────────────────────────────────────────────────────────

#[test]
fn release_own_stale_and_others() {
    let r = repo();
    start(&r, SA, me());
    let b = Proc::new();
    start(&r, SB, b.pid());
    let mut c = Proc::new();
    start(&r, SC, c.pid());
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    edit_b(&r, &b, "b.txt", "B\n", "t2");
    edit_as(&r, SC, None, &r.dir.join("c.txt"), "C\n", "t3", c.pid());
    c.kill();
    // own
    let o = al(&r, SA, &["release", "a.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("a.txt: released (its uncommitted changes stay on disk, unclaimed)"), "{}", o.stdout);
    assert!(claim(&r, "a.txt").is_null());
    assert_eq!(state(&r).get("orphans").get("a.txt").get("from").as_str(), Some(A));
    // a dead agent's
    let o = al(&r, SA, &["release", &r.dir.join("c.txt").display().to_string()]);
    assert!(o.stdout.contains("released stale claim of cccccccc (its process has exited)"), "{}", o.stdout);
    // a live agent's: refused, unless forced
    let o = al(&r, SA, &["release", "b.txt"]);
    assert_eq!(o.status, 1, "{}", o.out());
    assert!(o.stderr.contains("b.txt is held by bbbbbbbb") && o.stderr.contains("has another agent's uncommitted work"), "{}", o.stderr);
    assert!(o.stderr.contains(&format!("agent-lock release --force {}", r.dir.join("b.txt").display())), "{}", o.stderr);
    assert_eq!(claim(&r, "b.txt").get("by").as_str(), Some(B));
    let o = al(&r, SA, &["release", "--force", "b.txt"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("force-released (was held by bbbbbbbb; its uncommitted changes now have no owner)"), "{}", o.stdout);
    assert!(claim(&r, "b.txt").is_null());
    assert_eq!(al(&r, SA, &["release", "nothing.txt"]).stdout.trim(), "nothing.txt: not claimed");
    assert_eq!(al(&r, SA, &["release"]).status, 2);
}

#[test]
fn release_mine_takes_the_family_but_not_others() {
    let (r, b) = two();
    ev(&r, "subagent-start", SA, &format!(r#""hook_event_name":"SubagentStart","agent_id":"{SUB1}","agent_type":"general-purpose""#), me());
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    edit_as(&r, SA, Some(SUB1), &r.dir.join("s.txt"), "S\n", "t2", me());
    edit_b(&r, &b, "b.txt", "B\n", "t3");
    // a subagent releases only its own
    let o = al(&r, SA, &["release", "--mine", "--as", A1]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("released 1 claim held by aaaaaaaa:11111111"), "{}", o.stdout);
    assert_eq!(claim(&r, "a.txt").get("by").as_str(), Some(A));
    edit_as(&r, SA, Some(SUB1), &r.dir.join("s.txt"), "S2\n", "t4", me());
    let o = al(&r, SA, &["release", "--mine"]);
    assert!(
        o.stdout.contains("released 2 claims held by aaaaaaaa (and its subagents); 2 files with uncommitted changes now have no owner"),
        "{}",
        o.stdout
    );
    assert!(claim(&r, "a.txt").is_null() && claim(&r, "s.txt").is_null());
    assert_eq!(claim(&r, "b.txt").get("by").as_str(), Some(B));
    assert_eq!(al(&r, SA, &["release", "--mine", "a.txt"]).status, 2);
}

#[test]
fn mine_lists_absolute_paths_nul_separated() {
    let r = repo();
    start(&r, SA, me());
    edit_file(&r, SA, "c.txt", "C\n", "t1");
    edit_file(&r, SA, "a b.txt", "AB\n", "t2");
    edit_file(&r, SA, "gone.txt", "G\n", "t3");
    std::fs::remove_file(r.dir.join("gone.txt")).unwrap();
    let o = al(&r, SA, &["mine", "-z"]);
    assert_eq!(o.status, 0);
    assert_eq!(o.stdout, format!("{}\0{}\0", r.dir.join("a b.txt").display(), r.dir.join("c.txt").display()));
    let o = al(&r, SA, &["mine"]);
    assert_eq!(o.stdout.lines().count(), 2);
    assert!(o.stderr.contains("1 claimed file no longer exists"), "{}", o.stderr);
}

// ── off / on, status ──────────────────────────────────────────────────────

#[test]
fn off_and_on() {
    let (r, b) = two();
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    let o = al(&r, SA, &["off"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert_eq!(state(&r).get("off").as_bool(), Some(true));
    let d = ev(&r, "pre-edit", SB, &edit(&r.dir.join("a.txt"), "t2"), b.pid());
    assert_eq!(d.decision(), None, "hooks do nothing while off");
    assert!(al(&r, SA, &["status"]).stdout.contains("coordination: OFF"));
    let o = al(&r, SA, &["on"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(state(&r).get("off").is_null());
    let d = ev(&r, "pre-edit", SB, &edit(&r.dir.join("a.txt"), "t3"), b.pid());
    assert_eq!(d.decision().as_deref(), Some("deny"));
}

#[test]
fn status_shows_claims_sessions_and_the_raw_state() {
    let (r, b) = two();
    edit_file(&r, SA, "a.txt", "A\n", "t1");
    edit_b(&r, &b, "b.txt", "B\n", "t2");
    let o = al(&r, SA, &["status"]);
    assert_eq!(o.status, 0, "{}", o.out());
    let s = &o.stdout;
    assert!(s.contains("claims (2):") && s.contains("PATH   AGENT") && s.contains("STALE"), "{s}");
    let row = s.lines().find(|l| l.trim_start().starts_with("a.txt")).unwrap();
    assert!(row.contains("aaaaaaaa (you)") && row.contains(" 1 ") && row.contains("alive"), "{row}");
    assert!(s.contains("sessions (2):") && s.contains("locks: none"), "{s}");
    let j = serde_free::parse(&al(&r, SA, &["status", "--json"]).stdout);
    assert_eq!(j.get("v").as_u64(), Some(3));
    assert_eq!(j.get("claims").get("b.txt").get("by").as_str(), Some(B));
    let o = al(&r, SA, &["status", "/"]);
    assert_eq!(o.status, 1, "outside a worktree: {}", o.out());
}

// ── doctor ────────────────────────────────────────────────────────────────

#[test]
fn doctor_finds_and_fixes_leftovers() {
    let r = repo();
    start(&r, SA, me());
    let mut b = Proc::new();
    start(&r, SB, b.pid());
    edit_b(&r, &b, "b.txt", "B's work\n", "t1");
    b.kill();
    std::fs::write(r.git_dir.join("agent-lock.json.bad"), "{garbage").unwrap();
    // a killed sync's index.lock: its JSON marker names a dead process
    let head = git(&r, &r.dir, &["rev-parse", "HEAD"]);
    let marker = format!(
        r#"{{"agent-lock":"sync","by":"{A}","pid":{},"start":1,"head":"{head}","tip":"{head}","index_tmp":"{}"}}"#,
        dead_pid(),
        r.git_dir.join("agent-lock-sync-x.index").display()
    );
    std::fs::write(r.git_dir.join("index.lock"), marker).unwrap();
    assert!(
        Command::new("git").args(["add", "a.txt"]).current_dir(&r.dir).env_clear().envs(env_for(&r)).output().unwrap().status.code() != Some(0),
        "the lock blocks git"
    );
    al(&r, SA, &["install"]);
    let doctor = |args: &[&str]| cli(&r, &r.dir, args, None, me()); // outside a Claude Code Bash call
    let o = doctor(&["doctor"]);
    assert_eq!(o.status, 1, "{}", o.out());
    let s = &o.stdout;
    assert!(s.contains("[corrupt]") && s.contains("agent-lock.json.bad"), "{s}");
    assert!(s.contains("[dead-owner] b.txt — bbbbbbbb, 1 edit (its process has exited): its owner left uncommitted changes"), "{s}");
    assert!(s.contains("[interrupted-sync]"), "{s}");
    assert!(s.contains("3 problems found."), "{s}");
    let o = doctor(&["doctor", "--fix"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("fixed:") && o.stdout.contains("recovered from an interrupted agent-lock sync"), "{}", o.stdout);
    assert!(o.stdout.contains("3 problems found, 3 fixed."), "{}", o.stdout);
    assert!(!r.git_dir.join("index.lock").exists() && !r.git_dir.join("agent-lock.json.bad").exists());
    assert!(claim(&r, "b.txt").is_null());
    assert_eq!(state(&r).get("orphans").get("b.txt").get("from").as_str(), Some(B));
    git(&r, &r.dir, &["add", "a.txt"]); // git works again
    let o = doctor(&["doctor"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("clean."), "{}", o.stdout);
}

#[test]
fn doctor_reports_setup_problems() {
    let r = repo();
    write(&r.dir, ".claude/settings.local.json", r#"{"env":{"CLAUDE_FILE_LOCK":"0"}}"#);
    let o = cli(&r, &r.dir, &["doctor"], None, me());
    assert_eq!(o.status, 1);
    assert!(o.stdout.contains("[plugin] agent-lock@lovieco is not enabled"), "{}", o.stdout);
    assert!(o.stdout.contains("[kill-switch]") && o.stdout.contains("settings.local.json sets env.CLAUDE_FILE_LOCK=0"), "{}", o.stdout);
    // inside Claude Code without the session env file applied, the shims are off
    let o = run_bin(&r, None, &["doctor"], &r.dir, &[("CLAUDE_CODE_SESSION_ID", SA.into())], "");
    assert!(o.stdout.contains("[shims] AGENT_LOCK_AGENT_ID is not set"), "{}", o.stdout);
}

// ── install / uninstall ───────────────────────────────────────────────────

#[test]
fn install_and_uninstall_are_idempotent_and_keep_settings() {
    let r = repo();
    let settings = r.dir.join(".claude/settings.json");
    write(&r.dir, ".claude/settings.json", "{\n    \"model\": \"opus\",\n    \"permissions\": {\n        \"allow\": [\"Bash(ls:*)\"]\n    }\n}\n");
    let o = al(&r, SA, &["install"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(
        o.stdout.contains("added marketplace lovieco (github: lovieco/agent-lock)") && o.stdout.contains("enabled agent-lock@lovieco"),
        "{}",
        o.stdout
    );
    let text = std::fs::read_to_string(&settings).unwrap();
    assert!(
        text.starts_with("{\n    \"model\": \"opus\",\n    \"permissions\": {\n        \"allow\": [\n            \"Bash(ls:*)\""),
        "4-space indent kept:\n{text}"
    );
    let j = serde_free::parse(&text);
    assert_eq!(j.get("enabledPlugins").get("agent-lock@lovieco").as_bool(), Some(true));
    assert_eq!(j.get("extraKnownMarketplaces").get("lovieco").get("source").get("repo").as_str(), Some("lovieco/agent-lock"));
    let o = al(&r, SA, &["install"]);
    assert!(o.stdout.contains("already installed") && o.stdout.contains("nothing to change"), "{}", o.stdout);
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), text, "byte-identical");
    let o = al(&r, SA, &["uninstall"]);
    assert!(o.stdout.contains("removed enabledPlugins") && o.stdout.contains("removed marketplace lovieco"), "{}", o.stdout);
    let j = serde_free::parse(&std::fs::read_to_string(&settings).unwrap());
    assert_eq!(j.get("model").as_str(), Some("opus"));
    assert!(j.get("enabledPlugins").is_null() && j.get("extraKnownMarketplaces").is_null());
    assert!(al(&r, SA, &["uninstall"]).stdout.contains("not installed"));
    // a broken file is never touched
    write(&r.dir, ".claude/settings.json", "{ nope");
    let o = al(&r, SA, &["install"]);
    assert_eq!(o.status, 1);
    assert!(o.stderr.contains("cannot parse") && o.stderr.contains("nothing was changed"), "{}", o.stderr);
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{ nope");
}

#[test]
fn install_keeps_a_foreign_marketplace_unless_forced_and_cleans_up_v1() {
    let r = repo();
    let dir = r.base.join("project");
    write(
        &dir,
        ".claude/settings.json",
        r#"{"extraKnownMarketplaces":{"lovieco":{"source":{"source":"directory","path":"/src/fork"}}},"hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"node \"$CLAUDE_PROJECT_DIR\"/.agent-lock/hooks/pre.mjs"}]}]}}"#,
    );
    write(
        &dir,
        ".claude/settings.local.json",
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"node .agent-lock/hooks/stop.mjs"},{"type":"command","command":"say done"}]}]}}"#,
    );
    write(&dir, ".agent-lock/src/lock.cjs", "v1");
    write(&dir, ".agent-lock/hooks/pre.mjs", "v1");
    write(&dir, ".claude/skills/agent-lock/SKILL.md", "---\nname: agent-lock\ndescription: coordinate via sidecar .lock files\n---\n");
    let d = dir.display().to_string();
    let o = al(&r, SA, &["install", &d]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stderr.contains("already points at directory: /src/fork; left as it is"), "{}", o.stderr);
    for want in [
        "removed 1 legacy vendored hook entry",
        "from .claude/settings.local.json",
        "removed .agent-lock/src/",
        "removed the now-empty .agent-lock/",
        "removed .claude/skills/agent-lock/",
    ] {
        assert!(o.stdout.contains(want), "{want}: {}", o.stdout);
    }
    let j = serde_free::parse(&std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap());
    assert_eq!(j.get("extraKnownMarketplaces").get("lovieco").get("source").get("path").as_str(), Some("/src/fork"));
    assert!(j.get("hooks").is_null());
    let local = std::fs::read_to_string(dir.join(".claude/settings.local.json")).unwrap();
    assert!(local.contains("say done") && !local.contains(".agent-lock/hooks"), "{local}");
    assert!(!dir.join(".agent-lock").exists() && !dir.join(".claude/skills").exists());
    let o = al(&r, SA, &["install", &d, "--force"]);
    assert!(o.stdout.contains("re-pointed marketplace lovieco from directory: /src/fork to github: lovieco/agent-lock"), "{}", o.stdout);
    let o = al(&r, SA, &["uninstall", &d]);
    assert!(o.stdout.contains("removed marketplace lovieco"), "{}", o.stdout);
}

// ── selftest / help / version ─────────────────────────────────────────────

#[test]
fn selftest_passes() {
    let r = repo();
    let o = al_env(&r, &["selftest"], &[]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.lines().filter(|l| l.starts_with("PASS  ")).count() >= 10, "{}", o.stdout);
    assert!(o.stdout.trim_end().ends_with("PASS: all 12 steps"), "{}", o.stdout);
    assert!(!o.stdout.contains("FAIL"), "{}", o.stdout);
}

#[test]
fn help_lists_every_command() {
    let r = repo();
    let o = al_env(&r, &["help"], &[]);
    assert_eq!(o.status, 0);
    for c in
        ["commit", "push", "sync", "status", "wait", "release", "mine", "off", "on", "doctor", "install", "uninstall", "selftest", "version", "help"]
    {
        assert!(o.stdout.lines().any(|l| l.trim_start().starts_with(&format!("{c} ")) || l.trim() == c), "{c}:\n{}", o.stdout);
    }
    assert_eq!(al_env(&r, &[], &[]).stdout, o.stdout);
    assert_eq!(al_env(&r, &["--help"], &[]).stdout, o.stdout);
    let o = al_env(&r, &["hold"], &[]);
    assert_eq!(o.status, 2, "v2's removed commands are unknown");
    assert!(o.stderr.contains("unknown command: hold") && o.stderr.contains("agent-lock help"), "{}", o.stderr);
    assert!(al_env(&r, &["purge-all"], &[]).stderr.contains("v3 folds it into `agent-lock doctor --fix`"));
    assert!(al_env(&r, &["version"], &[]).stdout.starts_with("agent-lock 0."));
}

#[test]
fn two_agents_edit_one_config_file_and_either_commits_it() {
    let r = make_repo(false, &[("package.json", "{}\n"), ("src.js", "x\n")]);
    let b = Proc::new();
    start(&r, SA, me());
    start(&r, SB, b.pid());
    edit_file(&r, SA, "package.json", "{\"a\":1}\n", "t1");
    edit_as(&r, SB, None, &r.dir.join("package.json"), "{\"a\":1,\"b\":2}\n", "t2", b.pid());
    assert_eq!(claim(&r, "package.json").get("with").arr().len(), 1, "B joined A's claim");
    edit_as(&r, SB, None, &r.dir.join("src.js"), "y\n", "t3", b.pid());
    let o = cli(&r, &r.dir, &["commit", "-m", "deps"], Some(SB), b.pid());
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("package.json (shared; with changes by aaaaaaaa)"), "{}", o.stdout);
    assert_eq!(head_files(&r), "package.json,src.js");
    assert!(claim(&r, "package.json").is_null(), "released for both holders");
    let o = al(&r, SA, &["commit", "-m", "nothing left"]);
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.contains("nothing to commit"), "{}", o.stdout);
}
