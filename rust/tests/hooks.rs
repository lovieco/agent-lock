//! Black-box tests of `agent-lock hook <event>`: realistic payloads in,
//! JSON out, and the resulting state file.
mod common;
use common::*;
use std::path::Path;
use std::process::{Child, Command};

const A: &str = "aaaaaaaa";
const B: &str = "bbbbbbbb";
const C: &str = "cccccccc";
const SUB1: &str = "a1111111122222222"; // Claude Code agent_id → aaaaaaaa:11111111 in SA
const SUB2: &str = "a3333333344444444"; // → aaaaaaaa:33333333
const A1: &str = "aaaaaaaa:11111111";
const A2: &str = "aaaaaaaa:33333333";

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

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn repo() -> Repo {
    make_repo(false, &[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n"), ("d.txt", "d\n")])
}

fn ev(r: &Repo, event: &str, session: &str, extra: &str, pid: u32) -> Run {
    let out = hook(r, event, &payload(r, session, extra), pid);
    assert_eq!(out.status, 0, "{event}: {}", out.stderr);
    out
}

fn start(r: &Repo, session: &str, source: &str, pid: u32) -> Run {
    ev(r, "session-start", session, &format!(r#""hook_event_name":"SessionStart","source":"{source}""#), pid)
}

fn sub(agent: Option<&str>, rest: String) -> String {
    match agent {
        Some(a) => format!(r#""agent_id":"{a}","agent_type":"general-purpose",{rest}"#),
        None => rest,
    }
}

fn pre_as(r: &Repo, s: &str, agent: Option<&str>, f: &Path, tu: &str, pid: u32) -> Run {
    ev(r, "pre-edit", s, &sub(agent, edit(f, tu)), pid)
}

fn post_as(r: &Repo, s: &str, agent: Option<&str>, f: &Path, tu: &str, pid: u32) -> Run {
    ev(r, "post-edit", s, &sub(agent, edit(f, tu).replace("PreToolUse", "PostToolUse")), pid)
}

fn pre(r: &Repo, s: &str, f: &Path, tu: &str) -> Run {
    pre_as(r, s, None, f, tu, me())
}

fn post(r: &Repo, s: &str, f: &Path, tu: &str) -> Run {
    post_as(r, s, None, f, tu, me())
}

/// A full edit: claim, change the file on disk, record the write.
fn edit_file(r: &Repo, s: &str, agent: Option<&str>, f: &Path, tu: &str, pid: u32) {
    let p = pre_as(r, s, agent, f, tu, pid);
    assert_ne!(p.decision().as_deref(), Some("deny"), "{}", p.reason());
    let old = std::fs::read_to_string(f).unwrap_or_default();
    std::fs::write(f, format!("{old}{tu}\n")).unwrap();
    post_as(r, s, agent, f, tu, pid);
}

fn sub_start(r: &Repo, s: &str, agent: &str) -> Run {
    ev(r, "subagent-start", s, &format!(r#""hook_event_name":"SubagentStart","agent_id":"{agent}","agent_type":"general-purpose""#), me())
}

fn stop_with(r: &Repo, s: &str, extra: &str) -> Run {
    let e = if extra.is_empty() { String::new() } else { format!(",{extra}") };
    ev(r, "stop", s, &format!(r#""hook_event_name":"Stop","stop_hook_active":false{e}"#), me())
}

fn bash_ev(r: &Repo, event: &str, s: &str, agent: Option<&str>, cmd: &str, tu: &str, extra: &str) -> Run {
    let b = bash(cmd, tu).replace("PreToolUse", if event == "pre-bash" { "PreToolUse" } else { "PostToolUse" });
    let b = if extra.is_empty() { b } else { format!("{b},{extra}") };
    ev(r, event, s, &sub(agent, b), me())
}

fn by(r: &Repo, rel: &str) -> Option<String> {
    holder(r, rel)
}

fn claim(r: &Repo, rel: &str) -> serde_free::Json {
    state(r).get("claims").get(rel).clone()
}

fn session(r: &Repo, s: &str) -> serde_free::Json {
    state(r).get("sessions").get(s).clone()
}

fn write_state(r: &Repo, json: &str) {
    std::fs::write(r.git_dir.join("agent-lock.json"), json).unwrap();
}

fn index(r: &Repo) -> serde_free::Json {
    serde_free::parse(&std::fs::read_to_string(r.data.join("sessions.json")).unwrap_or_default())
}

/// Move every timestamp in the state file `secs` into the past.
fn age(r: &Repo, secs: u64) {
    let p = r.git_dir.join("agent-lock.json");
    let text = std::fs::read_to_string(&p).unwrap();
    let out: Vec<String> = text
        .lines()
        .map(|l| {
            for k in ["\"seen\": ", "\"touched\": ", "\"at\": ", "\"started\": "] {
                if let Some(i) = l.find(k) {
                    let (head, rest) = l.split_at(i + k.len());
                    let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if let Ok(v) = n.parse::<u64>() {
                        return format!("{head}{}{}", v - secs, &rest[n.len()..]);
                    }
                }
            }
            l.to_string()
        })
        .collect();
    std::fs::write(&p, out.join("\n")).unwrap();
}

fn prompt(r: &Repo, s: &str) -> Run {
    ev(r, "prompt", s, r#""hook_event_name":"UserPromptSubmit","prompt":"next""#, me())
}

/// PostToolUse of an Agent/Workflow call with this tool_response.
fn launched(r: &Repo, s: &str, tool: &str, response: &str) -> Run {
    ev(
        r,
        "post-agent",
        s,
        &format!(
            r#""hook_event_name":"PostToolUse","tool_name":"{tool}","tool_input":{{"prompt":"x"}},"tool_use_id":"ag1","tool_response":{response}"#
        ),
        me(),
    )
}

// ── pre-edit / post-edit ──────────────────────────────────────────────────

#[test]
fn two_sessions_block_with_absolute_path() {
    let r = repo();
    start(&r, SA, "startup", me());
    start(&r, SB, "startup", me());
    let a = r.dir.join("a.txt");
    edit_file(&r, SA, None, &a, "t1", me());
    let d = pre(&r, SB, &a, "t2");
    assert_eq!(d.decision().as_deref(), Some("deny"));
    let why = d.reason();
    assert!(why.contains("BLOCKED: a.txt is claimed by aaaaaaaa"), "{why}");
    assert!(why.contains("1 uncommitted edit") && why.contains("holder process alive"), "{why}");
    assert!(why.contains(&format!("agent-lock wait --claim {} --timeout 540", a.display())), "{why}");
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A));
    // the waiter is recorded on both sides
    assert_eq!(session(&r, SB).get("blocked").get(B).get("path").as_str(), Some("a.txt"));
    assert_eq!(session(&r, SA).get("wants").arr()[0].get("waiter").as_str(), Some(B));
}

#[test]
fn same_agent_reedits_and_post_edit_counts() {
    let r = repo();
    start(&r, SA, "startup", me());
    let a = r.dir.join("a.txt");
    let p = pre(&r, SA, &a, "t1");
    assert!(p.stdout.is_empty(), "nothing to say: {}", p.stdout);
    assert_eq!(claim(&r, "a.txt").get("pending").as_str(), Some("t1"));
    post(&r, SA, &a, "t1");
    assert_eq!(claim(&r, "a.txt").get("edits").as_u64(), Some(1));
    assert!(claim(&r, "a.txt").get("pending").is_null());
    assert!(pre(&r, SA, &a, "t2").stdout.is_empty());
    assert_eq!(claim(&r, "a.txt").get("pending").as_str(), Some("t2"));
    post(&r, SA, &a, "t2");
    assert_eq!(claim(&r, "a.txt").get("edits").as_u64(), Some(2));
    // a write with no pre-edit is claimed after the fact
    post(&r, SA, &r.dir.join("b.txt"), "t3");
    assert_eq!(claim(&r, "b.txt").get("edits").as_u64(), Some(1));
}

#[test]
fn lazy_session_context_once_per_agent() {
    let r = repo();
    let first = pre(&r, SA, &r.dir.join("a.txt"), "t1");
    assert!(first.context().contains("Your agent id is aaaaaaaa."), "{}", first.stdout);
    assert!(first.decision().is_none(), "context never carries a permission decision");
    assert!(pre(&r, SA, &r.dir.join("b.txt"), "t2").stdout.is_empty());
    let idx = index(&r);
    assert_eq!(idx.get(SA).get("shown").arr()[0].as_str(), Some(A));
    assert_eq!(idx.get(SA).get("repos").arr()[0].as_str(), Some(r.git_dir.to_str().unwrap()));
    // a subagent gets its own once
    let s = pre_as(&r, SA, Some(SUB1), &r.dir.join("c.txt"), "t3", me());
    assert!(s.context().contains("pass `--as aaaaaaaa:11111111`"), "{}", s.stdout);
}

#[test]
fn failed_first_edit_releases_but_a_reservation_survives() {
    let r = repo();
    start(&r, SA, "startup", me());
    let a = r.dir.join("a.txt");
    pre(&r, SA, &a, "t1");
    ev(&r, "edit-failed", SA, &edit(&a, "t1").replace("PreToolUse", "PostToolUseFailure"), me());
    assert!(by(&r, "a.txt").is_none(), "the claim this very call made is dropped");
    // a `wait --claim` reservation is kept
    let t = now();
    write_state(&r, &format!(r#"{{"v":3,"claims":{{"b.txt":{{"by":"{A}","session":"{SA}","at":{t},"touched":{t},"edits":0,"reason":"wait"}}}}}}"#));
    let b = r.dir.join("b.txt");
    pre(&r, SA, &b, "t2");
    ev(&r, "edit-failed", SA, &edit(&b, "t2").replace("PreToolUse", "PermissionDenied"), me());
    assert_eq!(by(&r, "b.txt").as_deref(), Some(A));
    assert!(claim(&r, "b.txt").get("pending").is_null());
}

#[test]
fn post_edit_on_someone_elses_claim_warns() {
    let r = repo();
    let a = r.dir.join("a.txt");
    edit_file(&r, SB, None, &a, "t1", me());
    let w = post(&r, SA, &a, "t2");
    assert!(w.context().contains("Warning: your write to a.txt landed while it is claimed by bbbbbbbb"), "{}", w.stdout);
}

#[test]
fn first_claim_of_a_file_someone_changed_says_so() {
    let r = repo();
    start(&r, SA, "startup", me());
    start(&r, SB, "startup", me());
    let a = r.dir.join("a.txt");
    std::fs::write(&a, "changed by B's sed -i\n").unwrap(); // no claim made
    let p = pre(&r, SA, &a, "t1");
    assert!(p.context().contains("a.txt already has uncommitted changes that were not made through your edits"), "{}", p.stdout);
    assert!(p.context().contains(&format!("git -C {} diff -- a.txt", r.dir.display())), "{}", p.stdout);
    assert!(pre(&r, SA, &r.dir.join("b.txt"), "t2").stdout.is_empty(), "a clean file: nothing to say");
    // alone: no git call and no note
    let solo = repo();
    start(&solo, SA, "startup", me());
    std::fs::write(solo.dir.join("a.txt"), "changed\n").unwrap();
    assert!(!pre(&solo, SA, &solo.dir.join("a.txt"), "t1").context().contains("already has"));
}

#[test]
fn a_new_files_claim_takes_gits_spelling_once_it_exists() {
    if !cfg!(target_os = "macos") {
        return; // git precomposes Unicode on macOS only
    }
    let r = repo();
    start(&r, SA, "startup", me());
    let (nfd, nfc) = ("re\u{301}sume\u{301}.md", "r\u{e9}sum\u{e9}.md");
    let f = r.dir.join(nfd);
    pre(&r, SA, &f, "t1");
    assert_eq!(by(&r, nfd).as_deref(), Some(A), "a missing file keeps its typed name");
    std::fs::write(&f, "new\n").unwrap();
    post(&r, SA, &f, "t1");
    let claims = state(&r).get("claims").keys();
    assert_eq!(claims, vec![nfc.to_string()], "one claim, in git's spelling");
    assert_eq!((claim(&r, nfc).get("edits").as_u64(), claim(&r, nfc).get("pending").is_null()), (Some(1), true));
    // the other spelling meets the same claim
    let d = pre(&r, SB, &r.dir.join(nfc), "t2");
    assert_eq!(d.decision().as_deref(), Some("deny"), "{}", d.stdout);
}

#[test]
fn stop_releases_only_what_git_confirms_clean() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let (nfd, nfc) = ("re\u{301}sume\u{301}.md", "r\u{e9}sum\u{e9}.md");
    let r = make_repo(false, &[(nfc, "a\n")]);
    std::fs::write(r.dir.join(nfc), "dirty\n").unwrap();
    // a claim keyed by another spelling of the dirty file (an older agent-lock made it)
    let t = now();
    write_state(
        &r,
        &format!(
            r#"{{"v":3,"claims":{{"{nfd}":{{"by":"{A}","session":"{SA}","owner":{{"pid":{},"start":0}},"at":{t},"touched":{t},"edits":1}}}}}}"#,
            me()
        ),
    );
    stop_with(&r, SA, "");
    assert_eq!(by(&r, nfd).as_deref(), Some(A), "git never said it is clean");
}

// ── subagents ─────────────────────────────────────────────────────────────

#[test]
fn main_hands_over_to_its_subagent_with_inherited_note() {
    let r = repo();
    start(&r, SA, "startup", me());
    let a = r.dir.join("a.txt");
    edit_file(&r, SA, None, &a, "t1", me());
    let s = sub_start(&r, SA, SUB1);
    assert!(s.context().contains("Your agent id is aaaaaaaa:11111111; pass `--as aaaaaaaa:11111111`"), "{}", s.stdout);
    let p = pre_as(&r, SA, Some(SUB1), &a, "t2", me());
    assert!(p.context().contains("carries uncommitted changes by aaaaaaaa, your parent agent;"), "{}", p.stdout);
    let c = claim(&r, "a.txt");
    assert_eq!(c.get("by").as_str(), Some(A1));
    assert_eq!(c.get("inherited").as_str(), Some(A));
    assert_eq!(session(&r, SA).get("subs").get(A1).get("busy").as_u64(), Some(1));
}

#[test]
fn sibling_subagents_block_and_main_is_told_to_wait() {
    let r = repo();
    start(&r, SA, "startup", me());
    sub_start(&r, SA, SUB1);
    sub_start(&r, SA, SUB2);
    let a = r.dir.join("a.txt");
    edit_file(&r, SA, Some(SUB1), &a, "t1", me());
    let d = pre_as(&r, SA, Some(SUB2), &a, "t2", me());
    assert_eq!(d.decision().as_deref(), Some("deny"));
    assert!(d.reason().contains("is claimed by aaaaaaaa:11111111"), "{}", d.reason());
    assert!(d.reason().contains("agent-lock wait --as aaaaaaaa:33333333 --claim"), "{}", d.reason());
    let m = pre(&r, SA, &a, "t3");
    assert!(m.reason().contains("your subagent aaaaaaaa:11111111, which is still running"), "{}", m.reason());
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A1));
}

#[test]
fn subagent_stop_hands_back() {
    let r = repo();
    start(&r, SA, "startup", me());
    sub_start(&r, SA, SUB1);
    let a = r.dir.join("a.txt");
    edit_file(&r, SA, Some(SUB1), &a, "t1", me());
    // a call of it that never completed does not pin the file after it is done
    pre_as(&r, SA, Some(SUB1), &r.dir.join("b.txt"), "t2", me());
    ev(&r, "subagent-stop", SA, &format!(r#""hook_event_name":"SubagentStop","agent_id":"{SUB1}","stop_hook_active":false"#), me());
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A));
    assert_eq!(by(&r, "b.txt").as_deref(), Some(A));
    assert!(claim(&r, "a.txt").get("inherited").is_null());
    assert_eq!(session(&r, SA).get("subs").get(A1).get("done").as_bool(), Some(true));
}

#[test]
fn an_idle_running_subagent_keeps_its_file() {
    let r = repo();
    start(&r, SA, "startup", me());
    sub_start(&r, SA, SUB1);
    sub_start(&r, SA, SUB2);
    let c = r.dir.join("c.txt");
    edit_file(&r, SA, Some(SUB1), &c, "t1", me());
    age(&r, 31 * 60); // reading and thinking fire no hook
    let d = pre_as(&r, SA, Some(SUB2), &c, "t2", me());
    assert_eq!(d.decision().as_deref(), Some("deny"), "a sibling never takes it: {}", d.stdout);
    let m = pre(&r, SA, &c, "t3");
    assert!(m.reason().contains("your subagent aaaaaaaa:11111111, which is still running"), "{}", m.stdout);
    assert_ne!(pre_as(&r, SA, Some(SUB1), &c, "t4", me()).decision().as_deref(), Some("deny"), "its own edit goes ahead");
    assert_eq!(by(&r, "c.txt").as_deref(), Some(A1));
}

#[test]
fn prompt_finishes_foreground_subagents_but_not_background_ones() {
    let r = repo();
    start(&r, SA, "startup", me());
    sub_start(&r, SA, SUB1);
    sub_start(&r, SA, SUB2);
    launched(&r, SA, "Agent", &format!(r#"{{"status":"async_launched","agentId":"{SUB2}","description":"x"}}"#));
    assert_eq!(index(&r).get(SA).get("bg").arr()[0].as_str(), Some(A2));
    let (a, b, c) = (r.dir.join("a.txt"), r.dir.join("b.txt"), r.dir.join("c.txt"));
    edit_file(&r, SA, Some(SUB1), &a, "t1", me());
    pre_as(&r, SA, Some(SUB1), &b, "t2", me()); // Esc at its next edit: no PostToolUse, no SubagentStop
    edit_file(&r, SA, Some(SUB2), &c, "t3", me());
    age(&r, 40 * 60);
    prompt(&r, SA);
    let subs = session(&r, SA).get("subs").clone();
    assert_eq!((subs.get(A1).get("done").as_bool(), subs.get(A1).get("busy").as_u64()), (Some(true), Some(0)));
    assert_eq!(subs.get(A2).get("done").as_bool(), Some(false), "a background subagent runs on");
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A));
    assert_eq!(by(&r, "b.txt").as_deref(), Some(A), "the interrupted call's marker does not pin it");
    assert!(claim(&r, "b.txt").get("pending").is_null());
    assert_eq!(by(&r, "c.txt").as_deref(), Some(A2));
    assert_ne!(pre(&r, SA, &b, "t4").decision().as_deref(), Some("deny"));
    // a background launch given as a JSON string counts the same
    let r2 = repo();
    launched(&r2, SA, "Task", &format!(r#""{{\"status\":\"async_launched\",\"agentId\":\"{SUB1}\"}}""#));
    assert_eq!(index(&r2).get(SA).get("bg").arr()[0].as_str(), Some(A1));
    // a call that ran to the end records nothing
    launched(&r2, SA, "Agent", r#"{"status":"completed","agentId":"a7777777788888888"}"#);
    assert_eq!(index(&r2).get(SA).get("bg").arr().len(), 1);
}

#[test]
fn nothing_moves_while_a_workflow_runs() {
    let r = repo();
    start(&r, SA, "startup", me());
    sub_start(&r, SA, SUB1); // one of its workers
    let a = r.dir.join("a.txt");
    edit_file(&r, SA, Some(SUB1), &a, "t1", me());
    launched(&r, SA, "Workflow", r#"{"status":"async_launched","taskId":"wf1","taskType":"local_workflow"}"#);
    assert_eq!(index(&r).get(SA).get("workflows").arr()[0].as_str(), Some("wf1"));
    prompt(&r, SA);
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A1));
    stop_with(&r, SA, r#""background_tasks":[{"id":"wf1","type":"local_workflow","status":"running"}]"#);
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A1), "Stop sees it running");
    assert_eq!(index(&r).get(SA).get("workflows").arr()[0].as_str(), Some("wf1"));
    prompt(&r, SA);
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A1));
    assert_eq!(session(&r, SA).get("subs").get(A1).get("done").as_bool(), Some(false));
    // once it is no longer listed, its workers are finished
    stop_with(&r, SA, r#""background_tasks":[]"#);
    assert!(index(&r).get(SA).get("workflows").is_null(), "{:?}", index(&r).get(SA));
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A));
}

#[test]
fn stop_without_background_tasks_waits_half_an_hour() {
    let r = repo();
    start(&r, SA, "startup", me());
    sub_start(&r, SA, SUB1);
    let a = r.dir.join("a.txt");
    edit_file(&r, SA, Some(SUB1), &a, "t1", me());
    age(&r, 11 * 60);
    stop_with(&r, SA, "");
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A1), "older Claude Code: still running as far as anyone knows");
    age(&r, 20 * 60);
    stop_with(&r, SA, "");
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A), "silent for 31 minutes: finished");
}

#[test]
fn subagent_stop_takes_it_off_the_background_list() {
    let r = repo();
    start(&r, SA, "startup", me());
    sub_start(&r, SA, SUB1);
    launched(&r, SA, "Agent", &format!(r#"{{"status":"async_launched","agentId":"{SUB1}"}}"#));
    stop_with(&r, SA, &format!(r#""background_tasks":[{{"id":"{SUB1}","type":"local_agent","status":"running"}}]"#));
    assert_eq!(session(&r, SA).get("bg").arr()[0].as_str(), Some(A1));
    ev(&r, "subagent-stop", SA, &format!(r#""hook_event_name":"SubagentStop","agent_id":"{SUB1}""#), me());
    assert!(session(&r, SA).get("bg").arr().is_empty(), "{:?}", session(&r, SA).get("bg"));
    assert!(index(&r).get(SA).get("bg").is_null(), "{:?}", index(&r).get(SA));
}

#[test]
fn pending_never_changes_hands() {
    let r = repo();
    start(&r, SA, "startup", me());
    sub_start(&r, SA, SUB1);
    let a = r.dir.join("a.txt");
    pre(&r, SA, &a, "t1"); // main's write in flight
    let d = pre_as(&r, SA, Some(SUB1), &a, "t2", me());
    assert_eq!(d.decision().as_deref(), Some("deny"), "{}", d.stdout);
    post(&r, SA, &a, "t1");
    let ok = pre_as(&r, SA, Some(SUB1), &a, "t3", me());
    assert_ne!(ok.decision().as_deref(), Some("deny"));
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A1));
}

// ── takeover, orphans, waits ──────────────────────────────────────────────

#[test]
fn dead_owner_takeover_denied_once_when_others_work() {
    let r = repo();
    let mut other = Proc::new();
    let a = r.dir.join("a.txt");
    edit_file(&r, SB, None, &a, "t1", other.pid());
    other.kill();
    start(&r, SC, "startup", me()); // a third, live agent: multi-agent mode
    start(&r, SA, "startup", me());
    let d = pre(&r, SA, &a, "t2");
    assert_eq!(d.decision().as_deref(), Some("deny"), "{}", d.stdout);
    let why = d.reason();
    assert!(why.contains("TAKEOVER: a.txt was claimed by bbbbbbbb (its process has exited), which left 1 uncommitted edit"), "{why}");
    assert!(why.contains(&format!("git -C {} diff -- a.txt", r.dir.display())), "{why}");
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A), "the claim stays with the new agent");
    assert!(claim(&r, "a.txt").get("pending").is_null(), "the denied call never runs");
    let again = pre(&r, SA, &a, "t3");
    assert_ne!(again.decision().as_deref(), Some("deny"), "the retry proceeds");
    assert_eq!(claim(&r, "a.txt").get("pending").as_str(), Some("t3"));
}

#[test]
fn dead_owner_takeover_is_only_a_note_when_solo() {
    let r = repo();
    let mut other = Proc::new();
    let a = r.dir.join("a.txt");
    edit_file(&r, SB, None, &a, "t1", other.pid());
    other.kill();
    start(&r, SA, "startup", me());
    let p = pre(&r, SA, &a, "t2");
    assert!(p.decision().is_none(), "{}", p.stdout);
    assert!(p.context().contains("Note: a.txt was claimed by bbbbbbbb (its process has exited)"), "{}", p.stdout);
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A));
}

#[test]
fn orphan_note_after_release_without_commit() {
    let r = repo();
    let t = now();
    write_state(&r, &format!(r#"{{"v":3,"orphans":{{"a.txt":{{"from":"{B}","at":{t}}}}}}}"#));
    start(&r, SA, "startup", me());
    let p = pre(&r, SA, &r.dir.join("a.txt"), "t1");
    assert!(p.context().contains("a.txt may still hold uncommitted changes by bbbbbbbb, whose claim on it ended without a commit"), "{}", p.stdout);
    assert!(state(&r).get("orphans").is_null(), "told once");
    assert!(pre(&r, SA, &r.dir.join("a.txt"), "t2").stdout.is_empty());
}

#[test]
fn deadlock_is_named() {
    let r = repo();
    start(&r, SA, "startup", me());
    start(&r, SB, "startup", me());
    let (a, b) = (r.dir.join("a.txt"), r.dir.join("b.txt"));
    edit_file(&r, SA, None, &a, "t1", me());
    edit_file(&r, SB, None, &b, "t2", me());
    assert!(pre(&r, SA, &b, "t3").reason().contains("BLOCKED"));
    let d = pre(&r, SB, &a, "t4");
    assert_eq!(d.decision().as_deref(), Some("deny"));
    assert!(d.reason().contains("DEADLOCK: a.txt is claimed by aaaaaaaa, and aaaaaaaa is waiting for b.txt (yours)"), "{}", d.reason());
}

#[test]
fn a_deadlock_over_three_agents_names_every_wait() {
    let r = repo();
    for x in [SA, SB, SC] {
        start(&r, x, "startup", me());
    }
    let (a, b, c) = (r.dir.join("a.txt"), r.dir.join("b.txt"), r.dir.join("c.txt"));
    edit_file(&r, SA, None, &a, "t1", me());
    edit_file(&r, SB, None, &b, "t2", me());
    edit_file(&r, SC, None, &c, "t3", me());
    assert!(pre(&r, SA, &b, "t4").reason().contains("BLOCKED"));
    assert!(pre(&r, SB, &c, "t5").reason().contains("BLOCKED"));
    let d = pre(&r, SC, &a, "t6");
    assert_eq!(d.decision().as_deref(), Some("deny"));
    assert!(
        d.reason().contains(
            "DEADLOCK: a.txt is claimed by aaaaaaaa, and aaaaaaaa is waiting for b.txt (held by bbbbbbbb), bbbbbbbb is waiting for c.txt (yours)."
        ),
        "{}",
        d.reason()
    );
}

#[test]
fn a_live_commit_or_install_marker_survives_prompt_and_stop() {
    let r = repo();
    let t = now();
    let c = |rel: &str, pending: &str| {
        format!(
            r#""{rel}":{{"by":"{A}","session":"{SA}","owner":{{"pid":{},"start":0}},"at":{t},"touched":{t},"edits":1,"pending":"{pending}"}}"#,
            me()
        )
    };
    let live = format!("commit:{}:0", me());
    let dead = format!("pkg:{}:0", dead_pid());
    write_state(&r, &format!(r#"{{"v":3,"claims":{{{},{},{}}}}}"#, c("a.txt", &live), c("b.txt", &dead), c("c.txt", "toolu_1")));
    prompt(&r, SA);
    assert_eq!(claim(&r, "a.txt").get("pending").as_str(), Some(live.as_str()), "a running commit keeps its marker");
    assert!(claim(&r, "b.txt").get("pending").is_null(), "a killed install's marker goes");
    assert!(claim(&r, "c.txt").get("pending").is_null(), "no tool call is in flight between turns");
    stop_with(&r, SA, "");
    assert_eq!(claim(&r, "a.txt").get("pending").as_str(), Some(live.as_str()));
}

#[test]
fn copy_paste_commands_name_the_repo_from_outside_it() {
    let r = repo();
    let outside = payload(&r, SA, &edit(&r.dir.join("a.txt"), "t1"))
        .replace(&format!(r#""cwd":"{}""#, r.dir.display()), &format!(r#""cwd":"{}""#, r.base.display()));
    let p = hook(&r, "pre-edit", &outside, me());
    assert!(p.context().contains(&format!("`agent-lock commit -C {} -m", r.dir.display())), "{}", p.stdout);
    assert!(pre(&r, SB, &r.dir.join("b.txt"), "t2").context().contains("`agent-lock commit -m"), "from inside: no -C");
}

#[test]
fn waiter_notice_reaches_the_holder_once() {
    let r = repo();
    start(&r, SA, "startup", me());
    start(&r, SB, "startup", me());
    let a = r.dir.join("a.txt");
    edit_file(&r, SA, None, &a, "t1", me());
    pre(&r, SB, &a, "t2");
    pre(&r, SA, &a, "t3");
    let n = post(&r, SA, &a, "t3");
    assert!(n.context().contains("bbbbbbbb is blocked on a.txt (yours)"), "{}", n.stdout);
    assert!(n.context().contains(&format!("agent-lock commit -m \"<msg>\" -- {}", a.display())), "{}", n.stdout);
    pre(&r, SA, &a, "t4");
    assert!(post(&r, SA, &a, "t4").stdout.is_empty());
    assert!(ev(&r, "prompt", SA, r#""prompt":"x""#, me()).stdout.is_empty());
}

#[test]
fn sync_lock_blocks_edits_on_its_paths_while_its_owner_lives() {
    let r = repo();
    start(&r, SA, "startup", me());
    edit_file(&r, SA, None, &r.dir.join("a.txt"), "t1", me());
    let o = claim(&r, "a.txt").get("owner").clone();
    let (pid, st) = (o.get("pid").as_u64().unwrap(), o.get("start").as_u64().unwrap());
    let t = now();
    let lock = |pid: u64, st: u64| {
        format!(
            r#"{{"v":3,"locks":{{"sync":{{"by":"{B}","owner":{{"pid":{pid},"start":{st}}},"since":{t},"head":"h","tip":"t","paths":["b.txt"]}}}}}}"#
        )
    };
    write_state(&r, &lock(pid, st));
    let d = pre(&r, SA, &r.dir.join("b.txt"), "t2");
    assert_eq!(d.decision().as_deref(), Some("deny"));
    assert!(d.reason().contains("`agent-lock sync` (run by bbbbbbbb) is updating b.txt"), "{}", d.reason());
    assert_ne!(pre(&r, SA, &r.dir.join("c.txt"), "t3").decision().as_deref(), Some("deny"), "other paths are free");
    write_state(&r, &lock(dead_pid() as u64, 1));
    assert_ne!(pre(&r, SA, &r.dir.join("b.txt"), "t4").decision().as_deref(), Some("deny"));
}

#[test]
fn a_stuck_state_lock_never_lets_an_edit_through_on_a_claimed_file() {
    let r = repo();
    start(&r, SA, "startup", me());
    start(&r, SB, "startup", me());
    let a = r.dir.join("a.txt");
    edit_file(&r, SA, None, &a, "t1", me());
    // a frozen agent-lock process holds the state lock for longer than the 5 s wait
    let f = std::fs::OpenOptions::new().read(true).write(true).open(r.git_dir.join("agent-lock.json")).unwrap();
    f.lock().unwrap();
    let d = pre(&r, SB, &a, "t2");
    assert_eq!(d.decision().as_deref(), Some("deny"), "{}", d.stdout);
    assert!(d.reason().contains("a.txt is claimed by aaaaaaaa") && d.reason().contains("Retry it in a moment"), "{}", d.reason());
    // a file nobody holds still goes ahead (fail open), with a line on stderr
    let d = pre(&r, SB, &r.dir.join("b.txt"), "t3");
    assert_eq!(d.decision(), None, "{}", d.stdout);
    assert!(d.stderr.contains("was not coordinated"), "{}", d.stderr);
    f.unlock().unwrap();
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A));
}

// ── stop ──────────────────────────────────────────────────────────────────

#[test]
fn stop_gc_releases_unwritten_and_clean_keeps_dirty() {
    let r = repo();
    start(&r, SA, "startup", me());
    pre(&r, SA, &r.dir.join("a.txt"), "t1"); // never written (rejected at a prompt)
    pre(&r, SA, &r.dir.join("b.txt"), "t2");
    post(&r, SA, &r.dir.join("b.txt"), "t2"); // "written", but the file matches HEAD
    edit_file(&r, SA, None, &r.dir.join("c.txt"), "t3", me());
    let s = stop_with(&r, SA, "");
    assert!(s.stdout.is_empty(), "solo: no reminder: {}", s.stdout);
    assert!(by(&r, "a.txt").is_none());
    assert!(by(&r, "b.txt").is_none());
    assert_eq!(by(&r, "c.txt").as_deref(), Some(A));
    assert!(state(&r).get("orphans").is_null(), "clean is committed, not orphaned");
    assert_eq!(session(&r, SA).get("phase").as_str(), Some("idle"));
}

#[test]
fn stop_keeps_clean_claims_when_a_stash_is_newer() {
    let r = repo();
    start(&r, SA, "startup", me());
    edit_file(&r, SA, None, &r.dir.join("a.txt"), "t1", me());
    git(&r, &r.dir, &["stash", "-q"]);
    stop_with(&r, SA, "");
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A), "the work is in the stash, not committed");
}

#[test]
fn stop_reminder_only_when_others_work_once_per_set() {
    let solo = repo();
    start(&solo, SA, "startup", me());
    edit_file(&solo, SA, None, &solo.dir.join("a.txt"), "t1", me());
    assert!(stop_with(&solo, SA, "").stdout.is_empty());

    let r = repo();
    start(&r, SA, "startup", me());
    start(&r, SB, "startup", me());
    edit_file(&r, SA, None, &r.dir.join("a.txt"), "t1", me());
    let s = stop_with(&r, SA, "");
    assert_eq!(s.decision().as_deref(), Some("block"), "{}", s.stdout);
    assert!(s.reason().contains("uncommitted changes in files you claimed: a.txt."), "{}", s.reason());
    assert!(s.reason().contains("`agent-lock commit -m \"<msg>\"`"));
    assert!(stop_with(&r, SA, "").stdout.is_empty(), "same set: once");
    edit_file(&r, SA, None, &r.dir.join("b.txt"), "t2", me());
    let again = stop_with(&r, SA, "");
    assert!(again.reason().contains("a.txt, b.txt"), "{}", again.stdout);
    edit_file(&r, SA, None, &r.dir.join("c.txt"), "t3", me());
    let active = ev(&r, "stop", SA, r#""hook_event_name":"Stop","stop_hook_active":true"#, me());
    assert!(active.stdout.is_empty(), "never while continuing from a block");
    let bg = stop_with(&r, SA, &format!(r#""background_tasks":[{{"id":"{SUB1}","type":"subagent","status":"running"}}]"#));
    assert!(bg.stdout.is_empty(), "not while a background subagent runs: {}", bg.stdout);
    // a waiter brings the reminder back
    pre(&r, SB, &r.dir.join("c.txt"), "t9");
    let w = stop_with(&r, SA, r#""background_tasks":[]"#);
    assert!(w.reason().contains("bbbbbbbb is waiting for c.txt."), "{}", w.stdout);
}

#[test]
fn stop_reconciles_background_tasks() {
    let r = repo();
    start(&r, SA, "startup", me());
    sub_start(&r, SA, SUB1);
    sub_start(&r, SA, SUB2);
    edit_file(&r, SA, Some(SUB1), &r.dir.join("a.txt"), "t1", me());
    edit_file(&r, SA, Some(SUB2), &r.dir.join("b.txt"), "t2", me());
    stop_with(
        &r,
        SA,
        &format!(
            r#""background_tasks":[{{"id":"{SUB1}","type":"subagent","status":"running","description":"x"}},{{"id":"b9","type":"local_bash","status":"running"}}]"#
        ),
    );
    let s = session(&r, SA);
    assert_eq!(s.get("subs").get(A1).get("done").as_bool(), Some(false));
    assert_eq!(s.get("subs").get(A2).get("done").as_bool(), Some(true));
    assert_eq!(s.get("bg").arr()[0].as_str(), Some(A1));
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A1), "still running in the background");
    assert_eq!(by(&r, "b.txt").as_deref(), Some(A), "finished: back to main (dirty, so kept)");
    // the next Stop without it running hands it back too
    stop_with(&r, SA, r#""background_tasks":[]"#);
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A));
    assert!(session(&r, SA).get("bg").is_null());
}

// ── session lifecycle ─────────────────────────────────────────────────────

#[test]
fn session_end_keeps_dirty_claims_and_marks_ended() {
    let r = repo();
    start(&r, SA, "startup", me());
    edit_file(&r, SA, None, &r.dir.join("a.txt"), "t1", me());
    pre(&r, SA, &r.dir.join("b.txt"), "t2");
    ev(&r, "session-end", SA, r#""hook_event_name":"SessionEnd","reason":"logout""#, me());
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A));
    assert!(by(&r, "b.txt").is_none());
    let s = session(&r, SA);
    assert_eq!(s.get("phase").as_str(), Some("ended"));
    assert_eq!(s.get("end_reason").as_str(), Some("logout"));
    assert_eq!(index(&r).get(SA).get("ended").as_bool(), Some(true));
}

#[test]
fn clear_adopts_the_same_process_claims_in_every_repo() {
    let r = repo();
    let r2 = make_repo(false, &[("x.txt", "x\n")]);
    start(&r, SA, "startup", me());
    edit_file(&r, SA, None, &r.dir.join("a.txt"), "t1", me());
    edit_file(&r, SA, Some(SUB1), &r.dir.join("b.txt"), "t2", me());
    edit_file(&r, SA, None, &r2.dir.join("x.txt"), "t3", me()); // only the index knows this repo
    ev(&r, "session-end", SA, r#""reason":"clear""#, me());
    let s = start(&r, SC, "clear", me());
    assert!(s.context().contains("Your agent id is cccccccc."), "{}", s.stdout);
    assert_eq!(by(&r, "a.txt").as_deref(), Some(C));
    assert_eq!(by(&r, "b.txt").as_deref(), Some(C));
    assert_eq!(by(&r2, "x.txt").as_deref(), Some(C), "the cleared conversation's other repo too");
    assert_eq!(claim(&r2, "x.txt").get("session").as_str(), Some(SC));
    assert_eq!(session(&r, SA).get("end_reason").as_str(), Some("clear"));
    let idx = index(&r);
    assert!(idx.get(SC).get("repos").arr().iter().any(|g| g.as_str() == Some(r2.git_dir.to_str().unwrap())));
}

#[test]
fn clear_leaves_other_processes_and_other_endings_alone() {
    let r = repo();
    let other = Proc::new();
    edit_file(&r, SA, None, &r.dir.join("a.txt"), "t1", other.pid());
    edit_file(&r, SB, None, &r.dir.join("b.txt"), "t2", me());
    ev(&r, "session-end", SB, r#""reason":"resume""#, me()); // ended for another reason
    start(&r, SC, "clear", me());
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A), "another process");
    assert_eq!(by(&r, "b.txt").as_deref(), Some(B), "resumed elsewhere, not cleared");
}

#[test]
fn resume_reowns_and_restart_hands_back_stray_subagent_claims() {
    let r = repo();
    let mut old = Proc::new();
    edit_file(&r, SA, None, &r.dir.join("a.txt"), "t1", old.pid());
    edit_file(&r, SA, Some(SUB1), &r.dir.join("b.txt"), "t2", old.pid());
    pre_as(&r, SA, Some(SUB1), &r.dir.join("c.txt"), "t3", old.pid()); // its call never finished
    old.kill();
    start(&r, SA, "resume", me());
    let c = claim(&r, "a.txt");
    assert_eq!(c.get("owner").get("pid").as_u64(), Some(me() as u64));
    assert_eq!(by(&r, "b.txt").as_deref(), Some(A));
    assert_eq!(by(&r, "c.txt").as_deref(), Some(A));
    assert_eq!(session(&r, SA).get("subs").get(A1).get("done").as_bool(), Some(true));
}

#[test]
fn compact_keeps_running_subagents() {
    let r = repo();
    start(&r, SA, "startup", me());
    sub_start(&r, SA, SUB1);
    edit_file(&r, SA, Some(SUB1), &r.dir.join("a.txt"), "t1", me());
    start(&r, SA, "compact", me());
    assert_eq!(by(&r, "a.txt").as_deref(), Some(A1));
    assert_eq!(session(&r, SA).get("subs").get(A1).get("done").as_bool(), Some(false));
}

#[test]
fn env_file_lines_once_and_shim_links() {
    let r = repo();
    let envf = r.base.join("env.sh");
    std::fs::write(&envf, "export FOO=1").unwrap();
    let shims = r.data.join("shims");
    // a v3.0 flat link where a tool's dir goes now
    std::fs::create_dir_all(&shims).unwrap();
    std::os::unix::fs::symlink(BIN, shims.join("npm")).unwrap();
    for _ in 0..2 {
        let p = payload(&r, SA, r#""source":"startup""#);
        let o = run_bin(
            &r,
            None,
            &["hook", "session-start"],
            &r.dir,
            &[("CLAUDE_PID", me().to_string()), ("CLAUDE_ENV_FILE", envf.display().to_string())],
            &p,
        );
        assert!(o.context().contains("Your agent id is aaaaaaaa."));
    }
    let text = std::fs::read_to_string(&envf).unwrap();
    let d = shims.display();
    for line in ["export AGENT_LOCK_AGENT_ID=aaaaaaaa", "export GIT_OPTIONAL_LOCKS=0"] {
        assert_eq!(text.lines().filter(|l| *l == line).count(), 1, "{line} in:\n{text}");
    }
    // one line, shell builtins only: a manager's dir only where the shell finds the real tool
    let path: Vec<&str> = text.lines().filter(|l| l.contains("export PATH=")).collect();
    assert_eq!(path.len(), 1, "{text}");
    assert!(path[0].starts_with("_alp=; for _alt in npm pnpm yarn bun"), "{text}");
    assert!(path[0].contains(&format!("do command -v \"$_alt\" >/dev/null 2>&1 && _alp=\"{d}/$_alt:$_alp\"; done;")), "{text}");
    assert!(path[0].ends_with(&format!("; export PATH=\"{d}/core:$_alp$PATH\"; unset _alp _alt")), "{text}");
    assert!(text.starts_with("export FOO=1\n"));
    let bin = std::fs::canonicalize(BIN).unwrap();
    for n in ["core/git", "core/agent-lock", "npm/npm", "pnpm/pnpm", "yarn/yarn", "bun/bun"] {
        let l = shims.join(n);
        assert!(std::fs::symlink_metadata(&l).unwrap().file_type().is_symlink(), "{n}");
        assert_eq!(std::fs::canonicalize(&l).unwrap(), bin, "{n}");
    }
}

#[test]
fn only_a_new_session_repoints_the_shared_links() {
    let r = repo();
    let other = r.base.join("old-agent-lock");
    std::fs::copy(BIN, &other).unwrap();
    let link = r.data.join("shims/core/agent-lock");
    let target = || std::fs::read_link(&link).unwrap();
    let start_with = |exe: &Path, source: &str| {
        let p = payload(&r, SA, &format!(r#""hook_event_name":"SessionStart","source":"{source}""#));
        assert_eq!(run_bin(&r, Some(exe), &["hook", "session-start"], &r.dir, &[("CLAUDE_PID", me().to_string())], &p).status, 0);
    };
    start_with(Path::new(BIN), "startup");
    assert_eq!(target(), Path::new(BIN));
    for source in ["compact", "resume", "clear"] {
        start_with(&other, source);
        assert_eq!(target(), Path::new(BIN), "{source} in an older session never repoints");
    }
    start_with(&other, "startup");
    assert_eq!(target(), other, "a new session does");
    // a link whose binary is gone is fixed by anyone
    std::fs::remove_file(&other).unwrap();
    start_with(Path::new(BIN), "compact");
    assert_eq!(target(), Path::new(BIN));
}

// ── Bash ──────────────────────────────────────────────────────────────────

#[test]
fn pre_bash_denies_a_subagent_acting_as_its_parent() {
    let r = repo();
    start(&r, SA, "startup", me());
    let d = bash_ev(&r, "pre-bash", SA, Some(SUB1), "agent-lock commit -m \"fix x\"", "b1", "");
    assert_eq!(d.decision().as_deref(), Some("deny"), "{}", d.stdout);
    assert!(d.reason().contains("this would run agent-lock as aaaaaaaa, but you are aaaaaaaa:11111111"), "{}", d.reason());
    assert!(d.reason().contains("agent-lock commit --as aaaaaaaa:11111111 -m \"fix x\""), "{}", d.reason());
    assert!(bash_ev(&r, "pre-bash", SA, Some(SUB1), "agent-lock commit --as aaaaaaaa:11111111 -m x", "b2", "").stdout.is_empty());
    assert!(bash_ev(&r, "pre-bash", SA, None, "agent-lock commit -m x", "b3", "").stdout.is_empty());
    let other = bash_ev(&r, "pre-bash", SA, None, "agent-lock push --as bbbbbbbb", "b4", "");
    assert_eq!(other.decision().as_deref(), Some("deny"));
}

#[test]
fn pre_bash_asks_before_switching_coordination_off() {
    let r = repo();
    start(&r, SA, "startup", me());
    for (cmd, says) in [
        ("CLAUDE_FILE_LOCK=0 git commit -am wip", "`CLAUDE_FILE_LOCK=0` turns off file coordination"),
        ("rm -f .git/agent-lock.json", "modifies agent-lock's lock state (.git/agent-lock.json)"),
        ("rm .git/index.lock", "(.git/index.lock)"),
        ("agent-lock off", "`agent-lock off` disables coordination"),
        ("agent-lock release --force a.txt", "`agent-lock release --force` takes a file"),
    ] {
        let o = bash_ev(&r, "pre-bash", SA, None, cmd, "b1", "");
        assert_eq!(o.decision().as_deref(), Some("ask"), "{cmd}: {}", o.stdout);
        assert!(o.reason().contains(says), "{cmd}: {}", o.reason());
    }
    assert!(bash_ev(&r, "pre-bash", SA, None, "ls -la", "b2", "").stdout.is_empty());
}

#[test]
fn post_bash_warns_when_a_command_changed_another_agents_file() {
    let r = repo();
    start(&r, SA, "startup", me());
    start(&r, SB, "startup", me());
    let a = r.dir.join("a.txt");
    edit_file(&r, SB, None, &a, "t1", me());
    std::thread::sleep(std::time::Duration::from_millis(1100)); // the holder's last touch is in an earlier second
    bash_ev(&r, "pre-bash", SA, None, "sed -i '' s/a/z/ a.txt", "b1", "");
    assert!(!session(&r, SA).get("bash").get("b1").is_null(), "snapshot taken");
    std::fs::write(&a, "z\n").unwrap();
    let w = bash_ev(&r, "post-bash", SA, None, "sed -i '' s/a/z/ a.txt", "b1", r#""tool_response":{"stdout":"","stderr":"","interrupted":false}"#);
    assert!(w.context().contains("changed files other agents have claimed: a.txt (claimed by bbbbbbbb)"), "{}", w.stdout);
    assert!(session(&r, SA).get("bash").is_null(), "snapshot removed");
    // an untouched file: nothing to say
    bash_ev(&r, "pre-bash", SA, None, "ls", "b2", "");
    assert!(bash_ev(&r, "post-bash", SA, None, "ls", "b2", "").stdout.is_empty());
    // a background command is still running: no verdict yet
    bash_ev(&r, "pre-bash", SA, None, "./slow.sh", "b3", "");
    std::fs::write(&a, "zz\n").unwrap();
    let bg = bash_ev(&r, "post-bash", SA, None, "./slow.sh", "b3", r#""tool_response":{"backgroundTaskId":"bash_1"}"#);
    assert!(bg.stdout.is_empty(), "{}", bg.stdout);
    // the holder changed it itself during the command
    bash_ev(&r, "pre-bash", SA, None, "make", "b4", "");
    edit_file(&r, SB, None, &a, "t5", me());
    assert!(bash_ev(&r, "post-bash", SA, None, "make", "b4", "").stdout.is_empty());
}

#[test]
fn bash_snapshot_covers_own_and_others_claims() {
    let r = repo();
    start(&r, SA, "startup", me());
    bash_ev(&r, "pre-bash", SA, None, "ls", "b0", "");
    assert!(session(&r, SA).get("bash").is_null(), "no claims: nothing to watch");
    edit_file(&r, SA, None, &r.dir.join("a.txt"), "t1", me());
    bash_ev(&r, "pre-bash", SA, None, "ls", "b1", "");
    let snap = session(&r, SA).get("bash").get("b1").clone();
    assert_eq!(snap.get("files").arr()[0].arr()[0].as_str(), Some("a.txt"), "its own claims too: a Bash call in flight");
    sub_start(&r, SA, SUB1);
    bash_ev(&r, "pre-bash", SA, Some(SUB1), "ls", "b2", "");
    assert_eq!(session(&r, SA).get("subs").get(A1).get("busy").as_u64(), Some(1));
    assert!(!session(&r, SA).get("bash").get("b2").is_null(), "the parent's claims are another agent's");
    bash_ev(&r, "post-bash", SA, Some(SUB1), "ls", "b2", "");
    assert_eq!(session(&r, SA).get("subs").get(A1).get("busy").as_u64(), Some(0));
}

#[test]
fn a_holders_own_bash_write_is_not_blamed_on_another_agent() {
    let r = repo();
    start(&r, SA, "startup", me());
    start(&r, SB, "startup", me());
    let b = r.dir.join("b.txt");
    edit_file(&r, SB, None, &b, "t1", me());
    std::thread::sleep(std::time::Duration::from_millis(1100)); // the holder's last touch is in an earlier second
    bash_ev(&r, "pre-bash", SA, None, "npm test", "a1", "");
    bash_ev(&r, "pre-bash", SB, None, "agent-lock mine -z | xargs -0 -r prettier --write", "b1", "");
    std::fs::write(&b, "formatted\n").unwrap();
    // A's command ends while B's formatter still runs
    assert!(bash_ev(&r, "post-bash", SA, None, "npm test", "a1", "").stdout.is_empty());
    bash_ev(&r, "post-bash", SB, None, "agent-lock mine -z | xargs -0 -r prettier --write", "b1", "");
    let c = claim(&r, "b.txt");
    assert!(c.get("touched").as_u64().unwrap() >= now() - 1, "the holder's own Bash write touches its claim");
    assert_eq!(c.get("edits").as_u64(), Some(1));
    // after B's command ended: still not A's doing
    bash_ev(&r, "pre-bash", SA, None, "npm test", "a2", "");
    bash_ev(&r, "pre-bash", SB, None, "sed -i '' s/f/F/ b.txt", "b2", "");
    std::fs::write(&b, "Formatted again\n").unwrap();
    bash_ev(&r, "post-bash", SB, None, "sed -i '' s/f/F/ b.txt", "b2", "");
    assert!(bash_ev(&r, "post-bash", SA, None, "npm test", "a2", "").stdout.is_empty());
    // a reservation written through Bash counts as an edit
    let d = r.dir.join("d.txt");
    let w = cli(&r, &r.dir, &["wait", "--claim", d.to_str().unwrap(), "--timeout", "1"], Some(SA), me());
    assert_eq!(w.status, 0, "{}", w.out());
    assert_eq!(claim(&r, "d.txt").get("edits").as_u64(), Some(0));
    bash_ev(&r, "pre-bash", SA, None, "./gen.sh", "a3", "");
    std::fs::write(&d, "generated\n").unwrap();
    bash_ev(&r, "post-bash", SA, None, "./gen.sh", "a3", "");
    assert_eq!(claim(&r, "d.txt").get("edits").as_u64(), Some(1));
    stop_with(&r, SA, "");
    assert_eq!(by(&r, "d.txt").as_deref(), Some(A), "written, dirty: kept at Stop");
}

// ── several repos ─────────────────────────────────────────────────────────

#[test]
fn second_repo_is_cleaned_by_stop_and_handed_back_by_subagent_stop() {
    let r = repo();
    let r2 = make_repo(false, &[("u.txt", "u\n"), ("x.txt", "x\n")]);
    start(&r, SA, "startup", me());
    sub_start(&r, SA, SUB1);
    let (u, x) = (r2.dir.join("u.txt"), r2.dir.join("x.txt"));
    pre(&r, SA, &u, "t1");
    post(&r, SA, &u, "t1"); // clean vs HEAD
    edit_file(&r, SA, Some(SUB1), &x, "t2", me());
    assert_eq!(by(&r2, "u.txt").as_deref(), Some(A));
    ev(&r, "subagent-stop", SA, &format!(r#""agent_id":"{SUB1}""#), me());
    assert_eq!(by(&r2, "x.txt").as_deref(), Some(A), "handed back in the other repo");
    stop_with(&r, SA, "");
    assert!(by(&r2, "u.txt").is_none(), "clean: released in the other repo");
    assert_eq!(by(&r2, "x.txt").as_deref(), Some(A), "dirty: kept");
}

// ── fail open ─────────────────────────────────────────────────────────────

#[test]
fn kill_switch_malformed_input_and_outside_files_do_nothing() {
    let r = repo();
    let a = r.dir.join("a.txt");
    let p = payload(&r, SA, &edit(&a, "t1"));
    let o = run_bin(&r, None, &["hook", "pre-edit"], &r.dir, &[("CLAUDE_FILE_LOCK", "0".into())], &p);
    assert_eq!((o.status, o.stdout.as_str()), (0, ""));
    assert!(!r.git_dir.join("agent-lock.json").exists());
    for bad in ["{not json", "", "[]", r#"{"session_id":""}"#, "\u{0}\u{1}"] {
        let o = run_bin(&r, None, &["hook", "pre-edit"], &r.dir, &[], bad);
        assert_eq!((o.status, o.stdout.as_str()), (0, ""), "{bad:?}");
    }
    let o = run_bin(&r, None, &["hook", "no-such-event"], &r.dir, &[], &p);
    assert_eq!((o.status, o.stdout.as_str()), (0, ""));
    // switched off in the state file
    write_state(&r, r#"{"v":3,"off":true}"#);
    assert!(pre(&r, SA, &a, "t1").stdout.is_empty());
    assert!(by(&r, "a.txt").is_none());
    assert!(bash_ev(&r, "pre-bash", SA, None, "agent-lock off", "b1", "").stdout.is_empty());
    // outside any repo, or inside .git
    let outside = r.base.join("loose.txt");
    assert!(pre(&r, SA, &outside, "t2").stdout.is_empty());
    assert!(pre(&r, SA, &r.git_dir.join("config"), "t3").stdout.is_empty());
}

// ── concurrency ───────────────────────────────────────────────────────────

#[test]
fn parallel_sessions_one_winner_and_no_lost_claims() {
    let r = std::sync::Arc::new(repo());
    let sid = |i: usize| format!("{i}{i}{i}{i}{i}{i}{i}{i}-1111-4111-8111-111111111111");
    let run = |same: bool| {
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let (r, s) = (r.clone(), sid(i));
                std::thread::spawn(move || {
                    let f = if same { r.dir.join("a.txt") } else { r.dir.join(format!("f{i}.txt")) };
                    pre(&r, &s, &f, &format!("t{i}")).decision()
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
    };
    let same = run(true);
    assert_eq!(same.iter().filter(|d| d.as_deref() == Some("deny")).count(), 7, "{same:?}");
    let winner = by(&r, "a.txt").unwrap();
    assert_eq!(winner.len(), 8);
    let apart = run(false);
    assert!(apart.iter().all(|d| d.is_none()), "{apart:?}");
    let st = state(&r);
    for i in 0..8 {
        assert_eq!(st.get("claims").get(&format!("f{i}.txt")).get("by").as_str(), Some(format!("{i}{i}{i}{i}{i}{i}{i}{i}").as_str()));
    }
    assert_eq!(st.get("sessions").keys().len(), 8);
}
