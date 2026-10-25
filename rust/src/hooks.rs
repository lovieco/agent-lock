//! Claude Code hooks: `agent-lock hook <event>` reads one JSON payload on
//! stdin, prints at most one JSON object and always exits 0 (fail open).
//!
//! Edit hooks act in the edited file's repo. Session-level hooks act in the
//! cwd repo plus every repo the session index lists for this session
//! (`<data_dir>/sessions.json`), so work in a second repo is still handed
//! back and cleaned up. Hooks read the index without a lock (a hint) and
//! change it in a `transact_json` only when something is new. No repo
//! transaction is held while git runs or while the index is locked.
//!
//! The index entry also says what outlives a turn: `bg` (subagents launched
//! in the background) and `workflows` (background workflows and other
//! agent-like tasks whose workers cannot be told apart). PostToolUse on
//! Agent|Task|Workflow records launches; Stop reconciles both against
//! `background_tasks`.

use crate::json::{self, Json, object};
use crate::rules::{self, Ask, Ctx, Got, Marker, Touch};
use crate::state::{self, BashSnap, Claim, Orphan, Phase, Session, State, Sub, Takeover, strs};
use crate::sys::{self, Live, Owner, Repo};
use crate::{git, ids, msg, shim};
use std::cell::OnceCell;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAX_INPUT: u64 = 64 << 20;
/// A denied takeover's retry within this window proceeds.
const TAKEOVER_RETRY: u64 = 300;
/// A pending write this old with no edits never ran (rejected at a prompt).
const NEVER_RAN: u64 = 60;
const DAY: u64 = 86_400;
const INDEX_DAYS: u64 = 30;

pub fn run(event: &str) -> i32 {
    if std::env::var_os("CLAUDE_FILE_LOCK").is_some_and(|v| v == "0") {
        return 0;
    }
    std::panic::set_hook(Box::new(|i| eprintln!("[agent-lock] internal error (ignored): {i}")));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut raw = Vec::new();
        std::io::stdin().take(MAX_INPUT + 1).read_to_end(&mut raw).ok()?;
        if raw.len() as u64 > MAX_INPUT {
            return None;
        }
        let p = json::parse(&String::from_utf8_lossy(&raw)).ok()?;
        dispatch(event, &Hook::new(&p)?)
    }));
    if let Ok(Some(v)) = out {
        let mut o = std::io::stdout().lock();
        let _ = o.write_all(v.to_string().as_bytes());
        let _ = o.flush();
    }
    0
}

fn dispatch(event: &str, h: &Hook) -> Option<Json> {
    match event {
        "session-start" => session_start(h),
        "subagent-start" => subagent_start(h),
        "prompt" => prompt(h),
        "pre-edit" => pre_edit(h),
        "post-edit" => post_edit(h),
        "edit-failed" => edit_failed(h),
        "pre-bash" => pre_bash(h),
        "post-bash" => post_bash(h),
        "post-agent" => post_agent(h),
        "subagent-stop" => subagent_stop(h),
        "stop" => stop(h),
        "session-end" => session_end(h),
        _ => None,
    }
}

// ── one hook call ─────────────────────────────────────────────────────────

struct Hook<'a> {
    p: &'a Json,
    sid: &'a str,
    main: String,
    agent: String,
    owner: Option<Owner>,
    cwd: PathBuf,
    now: u64,
    here: OnceCell<Option<Repo>>,
}

impl<'a> Hook<'a> {
    fn new(p: &'a Json) -> Option<Hook<'a>> {
        let sid = p.get("session_id").text()?;
        let cwd = p
            .get("cwd")
            .text()
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("CLAUDE_PROJECT_DIR").filter(|d| !d.is_empty()).map(PathBuf::from))
            .map(|d| std::fs::canonicalize(&d).unwrap_or(d))
            .unwrap_or_else(sys::cwd);
        Some(Hook {
            p,
            sid,
            main: ids::sid8(sid),
            agent: ids::agent_id(sid, p.get("agent_id").text()),
            owner: sys::claude_owner(),
            cwd,
            now: sys::now(),
            here: OnceCell::new(),
        })
    }

    fn sub(&self) -> bool {
        self.agent != self.main
    }

    fn tool_use(&self) -> Option<&'a str> {
        self.p.get("tool_use_id").text()
    }

    /// The repo of the agent's shell cwd.
    fn here(&self) -> Option<&Repo> {
        self.here.get_or_init(|| sys::find_repo(&self.cwd)).as_ref()
    }

    /// `Some(top)` when the shell is not in `repo` (commands then need `-C`).
    fn away<'r>(&self, repo: &'r Repo) -> Option<&'r Path> {
        if self.here().is_some_and(|r| r.git_dir == repo.git_dir) { None } else { Some(&repo.top) }
    }

    fn ask(&self, pending: Option<&str>) -> Ask {
        Ask {
            agent: self.agent.clone(),
            session: Some(self.sid.to_string()),
            owner: self.owner,
            pending: pending.map(str::to_string),
            made_by: pending.map(str::to_string),
            reason: None,
        }
    }
}

// ── output ────────────────────────────────────────────────────────────────

fn decision(kind: &str, reason: String) -> Json {
    object([(
        "hookSpecificOutput",
        object([("hookEventName", "PreToolUse".into()), ("permissionDecision", kind.into()), ("permissionDecisionReason", reason.into())]),
    )])
}

fn context(event: &str, parts: Vec<String>) -> Option<Json> {
    let text: Vec<String> = parts.into_iter().filter(|s| !s.is_empty()).collect();
    if text.is_empty() {
        return None;
    }
    Some(object([("hookSpecificOutput", object([("hookEventName", event.into()), ("additionalContext", text.join("\n\n").into())]))]))
}

fn block(reason: String) -> Json {
    object([("decision", "block".into()), ("reason", reason.into())])
}

// ── state helpers (inside a transaction) ──────────────────────────────────

/// Heartbeat: create or refresh this session's record.
fn beat<'s>(st: &'s mut State, h: &Hook) -> &'s mut Session {
    rules::see_session(st, h.sid, &h.main, h.owner, h.now)
}

/// A subagent's tool call starts (+1) or ends (-1). A starting call proves
/// it runs, so a premature `done` is undone.
fn sub_call(s: &mut Session, h: &Hook, d: i32) {
    if h.sub() {
        rules::see_sub(s, &h.agent, h.now, d);
        if d > 0
            && let Some(x) = s.subs.get_mut(&h.agent)
        {
            x.done = false;
        }
    }
}

/// A pending marker that may be cleared where no tool call is in flight: a
/// tool call's, or a process marker whose commit or install is gone. A live
/// `commit:`/`pkg:` marker belongs to that process, which clears its own.
fn stale_marker(c: &Claim) -> bool {
    c.pending.as_deref().is_some_and(|p| rules::marker(p) == Marker::Tool || rules::marker_dead(p))
}

fn clear_pending(st: &mut State, pred: impl Fn(&Claim) -> bool) {
    for c in st.claims.values_mut() {
        if stale_marker(c) && pred(c) {
            c.pending = None;
        }
    }
}

/// Hand `from`'s claims to main (clearing their tool-call markers first:
/// a finished agent has nothing in flight).
fn hand_back(st: &mut State, h: &Hook, from: &BTreeSet<String>) {
    if from.is_empty() {
        return;
    }
    clear_pending(st, |c| from.contains(&c.by));
    rules::transfer_all(st, |c| from.contains(&c.by), &h.main, Some(h.sid.to_string()), h.owner, h.now);
}

fn notices(st: &mut State, repo: &Repo, h: &Hook, agent: &str) -> Vec<String> {
    rules::take_notices(st, agent).into_iter().map(|(w, p)| msg::waiter_notice(&w, &p, &repo.abs(&p), agent, h.away(repo))).collect()
}

// ── session index ─────────────────────────────────────────────────────────

fn index_path() -> PathBuf {
    sys::data_dir().join("sessions.json")
}

/// This session's entry, read without a lock: a hint (changes re-check under
/// `transact_json`). Only the entry is parsed, not the whole file.
fn index_peek(sid: &str) -> Json {
    let Ok(text) = std::fs::read_to_string(index_path()) else { return Json::Null };
    let mut key = String::new();
    json::write_str(&mut key, sid);
    key.push(':');
    let Some(at) = text.find(&key) else { return Json::Null };
    json::parse_prefix(&text[at + key.len()..]).map(|(v, _)| v).unwrap_or(Json::Null)
}

fn slot<'j>(o: &'j mut Json, k: &str) -> &'j mut Json {
    let Json::Obj(m) = o else { unreachable!("transact_json hands over an object") };
    let i = match m.iter().position(|(x, _)| x == k) {
        Some(i) => i,
        None => {
            m.push((k.to_string(), Json::obj()));
            m.len() - 1
        }
    };
    if !matches!(m[i].1, Json::Obj(_)) {
        m[i].1 = Json::obj();
    }
    &mut m[i].1
}

fn add_strs(e: &mut Json, k: &str, add: &[String]) {
    let mut cur = strs(e.get(k));
    for a in add {
        if !cur.contains(a) {
            cur.push(a.clone());
        }
    }
    e.set(k, Json::Arr(cur.into_iter().map(Json::from).collect()));
}

fn set_strs(e: &mut Json, k: &str, v: Vec<String>) {
    if let Json::Obj(m) = e {
        m.retain(|(x, _)| x != k);
    }
    if !v.is_empty() {
        e.set(k, Json::Arr(v.into_iter().map(Json::from).collect()));
    }
}

fn gd(r: &Repo) -> String {
    r.git_dir.to_string_lossy().into_owned()
}

/// Record repos (and that `shown` got its context) in this session's entry.
/// Writes only when something is new (or the entry is a day old).
fn note(h: &Hook, peek: &Json, repos: &[&Repo], shown: Option<&str>) {
    let have = strs(peek.get("repos"));
    let new: Vec<String> = repos.iter().map(|r| gd(r)).filter(|g| !have.contains(g)).collect();
    let show = shown.filter(|a| !strs(peek.get("shown")).iter().any(|x| x == a));
    let old = !peek.is_null() && peek.get("seen").u64().is_none_or(|t| h.now.saturating_sub(t) > DAY);
    if new.is_empty() && show.is_none() && !old {
        return;
    }
    let _ = state::transact_json(&index_path(), |all| {
        let e = slot(all, h.sid);
        add_strs(e, "repos", &new);
        if let Some(a) = show {
            add_strs(e, "shown", &[a.to_string()]);
        }
        e.set("seen", h.now.into());
        if h.owner.is_some() {
            e.set("owner", state::owner_json(h.owner));
        }
    });
}

/// A worktree from its git dir (as the index stores it).
fn repo_at(git_dir: &Path) -> Option<Repo> {
    if !git_dir.join("HEAD").is_file() {
        return None;
    }
    let top = if git_dir.file_name()? == ".git" {
        git_dir.parent()?.to_path_buf()
    } else if let Ok(t) = std::fs::read_to_string(git_dir.join("gitdir")) {
        let p = PathBuf::from(t.trim()); // linked worktree: <top>/.git
        let p = if p.is_absolute() { p } else { git_dir.join(p) };
        p.parent()?.to_path_buf()
    } else {
        git_dir.join(core_worktree(git_dir)?) // submodule: core.worktree
    };
    let top = std::fs::canonicalize(top).ok()?;
    sys::find_repo(&top).filter(|r| r.git_dir == git_dir)
}

fn core_worktree(git_dir: &Path) -> Option<String> {
    let cfg = std::fs::read_to_string(git_dir.join("config")).ok()?;
    let mut core = false;
    for line in cfg.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            core = l.to_ascii_lowercase().starts_with("[core");
        } else if core
            && let Some((k, v)) = l.split_once('=')
            && k.trim().eq_ignore_ascii_case("worktree")
        {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// The cwd repo, then every indexed repo that still exists.
fn session_repos(h: &Hook, peek: &Json) -> Vec<Repo> {
    let mut out: Vec<Repo> = h.here().cloned().into_iter().collect();
    for g in strs(peek.get("repos")) {
        if out.iter().any(|r| r.git_dir.as_os_str() == g.as_str()) {
            continue;
        }
        if let Some(r) = repo_at(Path::new(&g)) {
            out.push(r);
        }
    }
    out
}

// ── session-start ─────────────────────────────────────────────────────────

/// The package managers among the shims (git is always shimmed).
fn managers() -> impl Iterator<Item = &'static str> {
    shim::NAMES.iter().copied().filter(|n| *n != "git")
}

/// Links to this binary (tmp link + rename): `data_dir()/shims/core/{git,
/// agent-lock}` and one dir per package manager, `shims/<tool>/<tool>`, which
/// the env file puts on PATH only where the real tool exists. The dir is
/// machine-wide: only a new session (`startup`) repoints a working link, so
/// an older session's resume or compact never downgrades the others.
fn shim_links(startup: bool) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let base = sys::data_dir().join("shims");
    for name in managers().chain(["git", "agent-lock"]) {
        let dir = base.join(if matches!(name, "git" | "agent-lock") { "core" } else { name });
        if std::fs::symlink_metadata(&dir).is_ok_and(|m| !m.is_dir()) {
            let _ = std::fs::remove_file(&dir); // a v3.0 link where the dir goes
        }
        std::fs::create_dir_all(&dir).ok()?;
        let link = dir.join(name);
        if std::fs::read_link(&link).is_ok_and(|t| t == exe) || (!startup && sys::is_executable(&link)) {
            continue;
        }
        let tmp = dir.join(format!(".{name}.{}.tmp", sys::pid()));
        let _ = std::fs::remove_file(&tmp);
        if std::os::unix::fs::symlink(&exe, &tmp).is_err() || std::fs::rename(&tmp, &link).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    Some(base)
}

/// Append each line once to `$CLAUDE_ENV_FILE` (sourced before every Bash call).
/// A package manager's shim goes on PATH only when the Bash shell itself
/// finds the real tool (shell builtins only), so `command -v pnpm` stays false
/// where pnpm is not installed.
fn env_file(main: &str, shims: Option<&Path>) {
    let Some(f) = std::env::var_os("CLAUDE_ENV_FILE").filter(|v| !v.is_empty()) else { return };
    let text = std::fs::read_to_string(&f).unwrap_or_default();
    let mut lines = vec![format!("export AGENT_LOCK_AGENT_ID={main}")];
    if let Some(d) = shims {
        let d = d.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$").replace('`', "\\`");
        let tools: Vec<&str> = managers().collect();
        lines.push(format!(
            "_alp=; for _alt in {}; do command -v \"$_alt\" >/dev/null 2>&1 && _alp=\"{d}/$_alt:$_alp\"; done; export PATH=\"{d}/core:$_alp$PATH\"; unset _alp _alt",
            tools.join(" ")
        ));
    }
    lines.push("export GIT_OPTIONAL_LOCKS=0".into());
    let have: BTreeSet<&str> = text.lines().map(str::trim).collect();
    let add: Vec<&String> = lines.iter().filter(|l| !have.contains(l.as_str())).collect();
    if add.is_empty() {
        return;
    }
    let mut out = if text.is_empty() || text.ends_with('\n') { String::new() } else { "\n".into() };
    for l in add {
        out.push_str(l);
        out.push('\n');
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().append(true).create(true).open(&f) {
        let _ = file.write_all(out.as_bytes());
    }
}

fn same_owner(v: &Json, o: Owner) -> bool {
    v.get("pid").u64() == Some(o.pid as u64) && v.get("start").u64() == Some(o.start)
}

/// A session that ended for another reason than /clear is not the one being cleared.
fn cleared_away(phase_ended: bool, reason: Option<&str>) -> bool {
    phase_ended && reason.is_some_and(|r| r != "clear")
}

fn session_start(h: &Hook) -> Option<Json> {
    let source = h.p.get("source").text().unwrap_or("startup");
    let compact = source == "compact";
    let clear = source == "clear";
    env_file(&h.main, shim_links(source == "startup").as_deref());

    // The index: prune, adopt a cleared conversation's repos, list ours.
    let here = h.here().map(gd);
    let mine = state::transact_json(&index_path(), |all| {
        if let Json::Obj(m) = all {
            let cut = h.now.saturating_sub(INDEX_DAYS * DAY);
            m.retain(|(id, e)| id == h.sid || e.get("seen").u64().unwrap_or(0) >= cut);
        }
        let mut adopt = Vec::new();
        if let (true, Some(o), Json::Obj(m)) = (clear, h.owner, &mut *all) {
            for (id, e) in m.iter_mut() {
                if id == h.sid || !same_owner(e.get("owner"), o) {
                    continue;
                }
                let ended = e.get("ended").bool() == Some(true);
                if cleared_away(ended, e.get("end_reason").str()) {
                    continue;
                }
                adopt.extend(strs(e.get("repos")));
                if !ended {
                    e.set("ended", true.into());
                    e.set("end_reason", "clear".into());
                }
            }
        }
        let e = slot(all, h.sid);
        adopt.extend(here.clone());
        add_strs(e, "repos", &adopt);
        e.set("seen", h.now.into());
        e.set("ended", false.into());
        e.set("end_reason", Json::Null);
        if h.owner.is_some() {
            e.set("owner", state::owner_json(h.owner));
        }
        if !compact {
            set_strs(e, "bg", Vec::new()); // nothing in the background survives
            set_strs(e, "workflows", Vec::new());
        }
        e.clone()
    })
    .unwrap_or_else(|_| index_peek(h.sid));

    let repos = session_repos(h, &mine);
    let mut active = false;
    for repo in &repos {
        let ok = state::transact(repo, |st| {
            if st.off {
                return false;
            }
            let s = beat(st, h);
            s.phase = Phase::Active;
            s.end_reason = None;
            if compact {
                s.blocked.remove(&h.main);
            } else {
                // Subagents never survive a restart, /clear or /resume.
                for x in s.subs.values_mut() {
                    x.done = true;
                    x.busy = 0;
                }
                s.bg = None;
                s.blocked.clear();
                s.bash.clear();
            }
            // /clear: the previous conversation of this same process.
            let mut fams = BTreeSet::new();
            if let (true, Some(o)) = (clear, h.owner) {
                for (id, x) in st.sessions.iter_mut() {
                    if id == h.sid || x.agent == h.main || x.owner != Some(o) {
                        continue;
                    }
                    if cleared_away(x.phase == Phase::Ended, x.end_reason.as_deref()) {
                        continue;
                    }
                    fams.insert(x.agent.clone());
                    if x.phase != Phase::Ended {
                        x.phase = Phase::Ended;
                        x.end_reason = Some("clear".into());
                    }
                }
            }
            // Stray subagent claims of this session (on compact: finished ones).
            let from: BTreeSet<String> = st
                .claims
                .values()
                .map(|c| c.by.clone())
                .filter(|by| {
                    fams.contains(ids::family(by)) || (ids::is_sub(by) && ids::family(by) == h.main && (!compact || rules::sub_done(st, by)))
                })
                .collect();
            if compact {
                rules::transfer_all(st, |c| from.contains(&c.by), &h.main, Some(h.sid.to_string()), h.owner, h.now);
            } else {
                hand_back(st, h, &from);
            }
            // Own claims carry the current process (resume after an exit).
            if let Some(o) = h.owner {
                for c in st.claims.values_mut() {
                    if c.by == h.main && c.owner != Some(o) {
                        c.owner = Some(o);
                        c.session = Some(h.sid.to_string());
                    }
                }
            }
            true
        });
        active |= ok.unwrap_or(false);
    }
    if !active {
        return None;
    }
    note(h, &mine, &[], Some(&h.main));
    context("SessionStart", vec![msg::session_context(&h.main, away_all(h, &repos))])
}

/// `-C <top>` for copy-paste commands when the shell is in none of `repos`
/// (the first one stands for them).
fn away_all<'r>(h: &Hook, repos: &'r [Repo]) -> Option<&'r Path> {
    if h.here().is_some() { None } else { repos.first().map(|r| r.top.as_path()) }
}

// ── subagents ─────────────────────────────────────────────────────────────

fn subagent_start(h: &Hook) -> Option<Json> {
    if !h.sub() {
        return None;
    }
    let peek = index_peek(h.sid);
    let repos = session_repos(h, &peek);
    let mut any = false;
    for repo in &repos {
        let ok = state::transact(repo, |st| {
            if st.off {
                return false;
            }
            beat(st, h).subs.insert(h.agent.clone(), Sub { seen: h.now, done: false, busy: 0 });
            true
        });
        any |= ok.unwrap_or(false);
    }
    if !any {
        return None;
    }
    let here: Vec<&Repo> = h.here().into_iter().collect();
    note(h, &peek, &here, Some(&h.agent));
    context("SubagentStart", vec![msg::subagent_context(&h.agent, away_all(h, &repos))])
}

fn subagent_stop(h: &Hook) -> Option<Json> {
    if !h.sub() {
        return None;
    }
    let peek = index_peek(h.sid);
    for repo in session_repos(h, &peek) {
        let _ = state::transact(&repo, |st| {
            if st.off {
                return;
            }
            let s = beat(st, h);
            let x = s.subs.entry(h.agent.clone()).or_default();
            x.done = true;
            x.busy = 0;
            x.seen = h.now;
            s.blocked.remove(&h.agent);
            s.bash.retain(|_, b| b.agent != h.agent);
            s.bg = s.bg.take().map(|b| b.into_iter().filter(|x| *x != h.agent).collect::<Vec<_>>()).filter(|b| !b.is_empty());
            hand_back(st, h, &BTreeSet::from([h.agent.clone()]));
        });
    }
    if strs(peek.get("bg")).contains(&h.agent) {
        let _ = state::transact_json(&index_path(), |all| {
            let e = slot(all, h.sid);
            let v = strs(e.get("bg")).into_iter().filter(|b| *b != h.agent).collect();
            set_strs(e, "bg", v);
        });
    }
    None
}

/// PostToolUse on Agent|Task|Workflow: a call that returns `async_launched`
/// keeps running after the turn. With an agentId it is a background
/// subagent (index `bg`); without, a workflow whose workers cannot be told
/// apart (index `workflows`, by task id). Only the index changes: claims
/// follow at prompt and Stop.
fn post_agent(h: &Hook) -> Option<Json> {
    let r = h.p.get("tool_response");
    let parsed = r.str().and_then(|s| json::parse(s).ok());
    let r = parsed.as_ref().unwrap_or(r);
    if r.get("status").str() != Some("async_launched") {
        return None;
    }
    let (k, v) = match r.get("agentId").text() {
        Some(a) => ("bg", ids::agent_id(h.sid, Some(a))),
        None => ("workflows", r.get("taskId").text().or(r.get("task_id").text()).or(h.tool_use()).unwrap_or("?").to_string()),
    };
    let _ = state::transact_json(&index_path(), |all| add_strs(slot(all, h.sid), k, &[v]));
    None
}

// ── prompt ────────────────────────────────────────────────────────────────

fn prompt(h: &Hook) -> Option<Json> {
    let peek = index_peek(h.sid);
    let repos = session_repos(h, &peek);
    if let Some(r) = h.here() {
        note(h, &peek, &[r], None);
    }
    // While a workflow may run, any unfinished subagent may be one of its workers.
    let workflow = !peek.get("workflows").arr().is_empty();
    let launched = strs(peek.get("bg"));
    let mut out = Vec::new();
    for repo in &repos {
        let r = state::transact(repo, |st| {
            if st.off {
                return Vec::new();
            }
            let s = beat(st, h);
            s.phase = Phase::Active;
            s.end_reason = None;
            let mut bg: BTreeSet<String> = s.bg.clone().unwrap_or_default().into_iter().collect();
            bg.extend(launched.iter().cloned());
            if !workflow {
                // A foreground subagent cannot outlive its turn; one that was
                // interrupted never got a SubagentStop.
                let gone: Vec<String> = s.subs.iter().filter(|(id, x)| !x.done && !bg.contains(*id)).map(|(id, _)| id.clone()).collect();
                for id in &gone {
                    if let Some(x) = s.subs.get_mut(id) {
                        x.done = true;
                        x.busy = 0;
                    }
                    s.blocked.remove(id);
                }
                s.bash.retain(|_, b| !gone.contains(&b.agent));
            }
            // No main tool call is in flight between turns.
            clear_pending(st, |c| c.by == h.main);
            let fin: BTreeSet<String> = st
                .claims
                .values()
                .map(|c| c.by.clone())
                .filter(|by| ids::is_sub(by) && ids::family(by) == h.main && !bg.contains(by) && rules::sub_done(st, by))
                .collect();
            hand_back(st, h, &fin);
            notices(st, repo, h, &h.main)
        });
        out.extend(r.unwrap_or_default());
    }
    context("UserPromptSubmit", out)
}

// ── edits ─────────────────────────────────────────────────────────────────

fn edit_target(h: &Hook) -> Option<(Repo, String)> {
    let ti = h.p.get("tool_input");
    let f = ti.get("file_path").text().or_else(|| ti.get("notebook_path").text())?;
    if f.contains('\0') {
        return None;
    }
    sys::locate(Path::new(f), &h.cwd)
}

enum Verdict {
    Allow(Vec<String>, bool), // notes; a fresh claim to check for changes nobody claimed
    Deny(String),
}

fn pre_edit(h: &Hook) -> Option<Json> {
    let (repo, rel) = edit_target(h)?;
    let abs = repo.abs(&rel);
    let tu = h.tool_use();
    let ctx = Ctx::real(h.now);
    let v = state::transact(&repo, |st| {
        if st.off {
            return None;
        }
        beat(st, h);
        if let Some(l) = &st.locks.sync {
            let k = st.key(&rel);
            if l.paths.iter().any(|p| st.key(p) == k) && l.owner.is_some_and(|o| sys::liveness(&o) == Live::Alive) {
                return Some(Verdict::Deny(msg::sync_busy(&rel, &l.by)));
            }
        }
        let mut notes = Vec::new();
        let mut fresh = false;
        match rules::acquire(st, &rel, &h.ask(tu), &ctx) {
            Ok(Got::Created) => {
                let k = st.key(&rel);
                match st.orphans.remove(&k) {
                    Some(o) if o.from != h.agent => notes.push(msg::orphan_note(&rel, &repo.top, &o.from)),
                    Some(_) => {}
                    None => fresh = rules::multi_agent(st, &h.agent, &ctx),
                }
            }
            Ok(Got::Refreshed) => {}
            Ok(Got::Joined { with }) => notes.push(msg::shared_note(&rel, &repo.top, &with)),
            Ok(Got::Transferred { from, inherited }) => {
                if inherited {
                    notes.push(msg::inherited(&rel, &from, &h.agent, h.away(&repo)));
                }
            }
            Ok(Got::TookOver { from, why }) => {
                if from.edits > 0 {
                    // Its uncommitted work stays in the file: remembered until committed.
                    let k = st.key(&rel);
                    st.orphans.insert(k, Orphan { path: rel.clone(), from: from.by.clone(), at: h.now });
                    let multi = rules::multi_agent(st, &h.agent, &ctx);
                    let s = beat(st, h);
                    let retry = s
                        .takeover
                        .as_ref()
                        .is_some_and(|t| t.path == rel && t.from == from.by && t.agent == h.agent && h.now.saturating_sub(t.at) < TAKEOVER_RETRY);
                    if multi && !retry {
                        s.takeover = Some(Takeover { path: rel.clone(), agent: h.agent.clone(), from: from.by.clone(), at: h.now });
                        if let Some(c) = st.claim_mut(&rel) {
                            c.pending = None; // the denied call never runs
                        }
                        return Some(Verdict::Deny(msg::takeover(&abs, &rel, &repo.top, &from.by, why.says(), from.edits, &h.agent)));
                    }
                    notes.push(msg::takeover_note(&abs, &rel, &repo.top, &from.by, why.says(), from.edits, &h.agent));
                }
            }
            Err(held) => {
                let holder = held.claim.by.clone();
                let text = if let Some(chain) = rules::record_wait(st, &rel, &h.agent, &holder, h.now) {
                    msg::deadlock(&rel, &holder, &chain, &h.agent, h.away(&repo))
                } else {
                    let own = held.own_sub && !h.sub();
                    let c = &held.claim;
                    msg::blocked(&msg::Blocked {
                        abs: &abs,
                        rel: &rel,
                        holder: &holder,
                        claimed: h.now.saturating_sub(c.at),
                        idle: held.idle,
                        alive: held.alive,
                        edits: c.edits,
                        never_ran: c.edits == 0 && c.pending.is_some() && held.idle >= NEVER_RAN,
                        own_sub: own.then(|| !rules::sub_done(st, &holder)),
                        inherited: c.inherited.as_deref() == Some(h.agent.as_str()),
                        me: &h.agent,
                    })
                };
                return Some(Verdict::Deny(text));
            }
        }
        let s = beat(st, h);
        s.blocked.remove(&h.agent);
        sub_call(s, h, 1);
        Some(Verdict::Allow(notes, fresh))
    });
    let v = match v {
        Ok(v) => v?,
        Err(e) => return state_stuck(h, &repo, &rel, &e),
    };
    match v {
        Verdict::Deny(r) => Some(decision("deny", r)),
        Verdict::Allow(mut notes, fresh) => {
            // The first claim of an existing file while others work: did someone
            // change it without claiming it (a Bash command, the user, an earlier session)?
            if fresh
                && std::fs::symlink_metadata(&abs).is_ok()
                && git::dirty(&repo, std::slice::from_ref(&rel)).is_ok_and(|d| d.iter().any(|p| same_path(&repo, p, &rel)))
            {
                notes.push(msg::dirty_note(&rel, &repo.top));
            }
            let peek = index_peek(h.sid);
            let shown = strs(peek.get("shown")).contains(&h.agent);
            if !shown {
                let top = h.away(&repo);
                notes.insert(0, if h.sub() { msg::subagent_context(&h.agent, top) } else { msg::session_context(&h.main, top) });
            }
            note(h, &peek, &[&repo], (!shown).then_some(h.agent.as_str()));
            context("PreToolUse", notes)
        }
    }
}

/// The state could not be locked (a frozen holder, a slow or network
/// filesystem): fail open, unless the file as last written shows another
/// agent's claim on this path. Then the edit is refused: a retry costs nothing.
fn state_stuck(h: &Hook, repo: &Repo, rel: &str, e: &std::io::Error) -> Option<Json> {
    let held = state::peek(repo).and_then(|st| st.claim(rel).filter(|c| !c.holds(&h.agent) && !st.is_shared(&c.path)).map(|c| c.by.clone()));
    match held {
        Some(by) => Some(decision("deny", msg::state_busy(rel, &by))),
        None => {
            eprintln!("agent-lock: {}: {e}; this edit of {rel} was not coordinated", repo.state_file().display());
            None
        }
    }
}

fn post_edit(h: &Hook) -> Option<Json> {
    let (repo, rel) = edit_target(h)?;
    let r = state::transact(&repo, |st| {
        if st.off {
            return None;
        }
        sub_call(beat(st, h), h, -1);
        // A new file got git's spelling only once it existed: move the claim
        // this very call made under the spelling it had then.
        if let Some(t) = h.tool_use()
            && st.claim(&rel).is_none()
            && let Some(k) =
                st.claims.iter().find(|(_, c)| c.by == h.agent && c.edits == 0 && c.made_by.as_deref() == Some(t)).map(|(k, _)| k.clone())
            && let Some(mut c) = st.claims.remove(&k)
        {
            c.path = rel.clone();
            st.put_claim(c);
        }
        let warn = match rules::wrote(st, &rel, &h.ask(None), h.tool_use(), h.now) {
            Touch::NotOwner(by) => msg::not_owner(&rel, &by),
            Touch::Counted | Touch::PostHoc => String::new(),
        };
        let mut out = vec![warn];
        out.extend(notices(st, &repo, h, &h.agent));
        Some(out)
    });
    let out = r.ok()??;
    let peek = index_peek(h.sid);
    note(h, &peek, &[&repo], None);
    context("PostToolUse", out)
}

fn edit_failed(h: &Hook) -> Option<Json> {
    let (repo, rel) = edit_target(h)?;
    let _ = state::transact(&repo, |st| {
        if st.off {
            return;
        }
        sub_call(beat(st, h), h, -1);
        rules::failed(st, &rel, &h.agent, h.tool_use());
    });
    None
}

// ── Bash ──────────────────────────────────────────────────────────────────

fn pre_bash(h: &Hook) -> Option<Json> {
    let cmd = h.p.at(&["tool_input", "command"]).text()?;
    let mut sc = Scan::default();
    scan(&sh::parse(cmd), &h.agent, &mut sc, &mut Env::Inherit, 0);
    let denied = identity(cmd, &sc, &h.agent);
    let tu = h.tool_use();
    let peek = index_peek(h.sid);
    let repos = session_repos(h, &peek);
    if let Some(r) = h.here() {
        note(h, &peek, &[r], None);
    }
    let ctx = Ctx::real(h.now);
    let mut switched_off = false;
    for repo in &repos {
        let r = state::transact(repo, |st| {
            if st.off {
                return true;
            }
            if denied.is_some() {
                return false; // the call will not run
            }
            // Stamps of the caller's own and other agents' live claimed files,
            // to tell afterwards whether this command changed one. A snapshot
            // also says this agent has a Bash call in flight.
            let files: Vec<(String, u128, u64)> = match tu {
                Some(_) => st
                    .claims
                    .values()
                    .filter(|c| c.holds(&h.agent))
                    .chain(rules::others(st, &h.agent, &ctx))
                    .map(|c| {
                        let (t, z) = sys::stamp(&repo.abs(&c.path)).unwrap_or((0, 0));
                        (c.path.clone(), t, z)
                    })
                    .collect(),
                None => Vec::new(),
            };
            let s = beat(st, h);
            sub_call(s, h, 1);
            if let (Some(t), false) = (tu, files.is_empty()) {
                s.bash.insert(t.to_string(), BashSnap { agent: h.agent.clone(), at: h.now, files });
            }
            false
        });
        if r.unwrap_or(false) && h.here().is_some_and(|x| x.git_dir == repo.git_dir) {
            switched_off = true;
        }
    }
    if switched_off {
        return None;
    }
    if let Some(d) = denied {
        return Some(decision("deny", d));
    }
    let mut asks: Vec<String> = Vec::new();
    for a in sc.asks {
        if !asks.contains(&a) {
            asks.push(a);
        }
    }
    if asks.is_empty() { None } else { Some(decision("ask", asks.join("\n"))) }
}

fn same_path(repo: &Repo, a: &str, b: &str) -> bool {
    if repo.ignore_case() { a.to_lowercase() == b.to_lowercase() } else { a == b }
}

/// Stored stamps went through f64 (JSON): compare at that precision.
fn same_stamp(a: (u128, u64), b: (u128, u64)) -> bool {
    (a.0 as f64) as u128 == (b.0 as f64) as u128 && a.1 == b.1
}

fn background_task(v: &Json) -> bool {
    match v {
        Json::Str(s) if s.trim_start().starts_with('{') => json::parse(s).is_ok_and(|j| j.get("backgroundTaskId").text().is_some()),
        _ => v.get("backgroundTaskId").text().is_some(),
    }
}

fn post_bash(h: &Hook) -> Option<Json> {
    let event = h.p.get("hook_event_name").text().filter(|e| matches!(*e, "PostToolUseFailure" | "PermissionDenied")).unwrap_or("PostToolUse");
    let tu = h.tool_use();
    let bg = background_task(h.p.get("tool_response"));
    let peek = index_peek(h.sid);
    let mut changed: Vec<(String, String)> = Vec::new();
    let mut out = Vec::new();
    for repo in session_repos(h, &peek) {
        let r = state::transact(&repo, |st| {
            if st.off {
                return None;
            }
            let s = beat(st, h);
            sub_call(s, h, -1);
            let snap = tu.and_then(|t| s.bash.remove(t));
            let mut ch = Vec::new();
            if let (Some(snap), false) = (snap, bg) {
                for (rel, t, z) in &snap.files {
                    if same_stamp(sys::stamp(&repo.abs(rel)).unwrap_or((0, 0)), (*t, *z)) {
                        continue;
                    }
                    let busy = |by: &str| st.session_of(by).is_some_and(|s| s.bash.values().any(|b| b.agent == by));
                    match st.claim(rel) {
                        // the caller's own file: written, as an edit would have
                        Some(c) if c.holds(&h.agent) => {
                            if let Some(c) = st.claim_mut(rel) {
                                c.touched = h.now;
                                c.edits = c.edits.max(1);
                            }
                        }
                        // neither touched since the snapshot nor in its holder's own running command
                        Some(c) if c.touched < snap.at && !busy(&c.by) => ch.push((rel.clone(), c.by.clone())),
                        _ => {}
                    }
                }
            }
            let n = if event == "PermissionDenied" { Vec::new() } else { notices(st, &repo, h, &h.agent) };
            Some((ch, n))
        });
        if let Ok(Some((ch, n))) = r {
            changed.extend(ch);
            out.extend(n);
        }
    }
    if event == "PermissionDenied" {
        return None;
    }
    if !changed.is_empty() {
        out.insert(0, msg::bash_wrote(&changed));
    }
    context(event, out)
}

// ── stop ──────────────────────────────────────────────────────────────────

/// What `background_tasks` says still runs. `listed` false: no such field.
#[derive(Default)]
struct Background {
    listed: bool,
    any: bool,
    unmapped: Vec<String>, // agent-like tasks we cannot map (workflows): every unfinished subagent may be one
    ids: BTreeSet<String>,
}

fn background(h: &Hook) -> Background {
    let Json::Arr(tasks) = h.p.get("background_tasks") else { return Background::default() };
    let mut b = Background { listed: true, ..Background::default() };
    for t in tasks {
        if !matches!(t.get("status").str(), Some("running" | "pending")) {
            continue;
        }
        match t.get("type").str() {
            Some("subagent" | "agent" | "local_agent") => {
                b.any = true;
                match t.get("id").text() {
                    Some(id) => {
                        b.ids.insert(ids::agent_id(h.sid, Some(id)));
                    }
                    None => b.unmapped.push("?".into()),
                }
            }
            Some("workflow" | "local_workflow" | "teammate" | "in_process_teammate") => {
                b.any = true;
                b.unmapped.push(t.get("id").text().unwrap_or("?").to_string());
            }
            _ => {}
        }
    }
    b
}

type Candidate = (String, u32, u64); // path, edits, touched

fn stop(h: &Hook) -> Option<Json> {
    let bg = background(h);
    let peek = index_peek(h.sid);
    let repos = session_repos(h, &peek);
    if let Some(r) = h.here() {
        note(h, &peek, &[r], None);
    }
    // What runs in the background, for the next prompt.
    let (ibg, iwf): (Vec<String>, Vec<String>) = (bg.ids.iter().cloned().collect(), bg.unmapped.clone());
    if bg.listed && (strs(peek.get("bg")) != ibg || strs(peek.get("workflows")) != iwf) {
        let _ = state::transact_json(&index_path(), |all| {
            let e = slot(all, h.sid);
            set_strs(e, "bg", ibg);
            set_strs(e, "workflows", iwf);
        });
    }
    let ctx = Ctx::real(h.now);
    let main = h.main.as_str();
    let unmapped = !bg.unmapped.is_empty();

    // 1. Idle; settle subagents; drop never-written claims; list edited ones.
    let mut work: Vec<(&Repo, Vec<Candidate>)> = Vec::new();
    let mut running_any = bg.any;
    for repo in &repos {
        let r = state::transact(repo, |st| {
            if st.off {
                return None;
            }
            let s = beat(st, h);
            s.phase = Phase::Idle;
            if bg.listed {
                for (id, x) in s.subs.iter_mut() {
                    if !bg.ids.contains(id) && !(unmapped && !x.done) {
                        x.done = true;
                        x.busy = 0;
                    }
                }
                s.bg = if bg.ids.is_empty() { None } else { Some(bg.ids.iter().cloned().collect()) };
            }
            let subs: BTreeSet<String> = s.subs.keys().cloned().collect();
            let holders: BTreeSet<String> =
                st.claims.values().map(|c| c.by.clone()).filter(|by| ids::is_sub(by) && ids::family(by) == main).collect();
            // Without background_tasks (older Claude Code) a subagent runs until
            // SubagentStop or a long silence.
            let running: BTreeSet<String> = subs
                .union(&holders)
                .filter(|id| match bg.listed {
                    true => bg.ids.contains(*id) || (unmapped && !rules::sub_done(st, id)),
                    false => !rules::sub_done(st, id) && !rules::sub_idle(st, id, h.now, rules::SUB_IDLE),
                })
                .cloned()
                .collect();
            let fin: BTreeSet<String> = holders.difference(&running).cloned().collect();
            hand_back(st, h, &fin);
            clear_pending(st, |c| c.by == main);
            if let Some(s) = st.sessions.get_mut(h.sid) {
                s.blocked.retain(|a, _| running.contains(a));
                s.bash.retain(|_, b| running.contains(&b.agent));
            }
            let unwritten: Vec<String> = st
                .claims
                .values()
                .filter(|c| c.by == main && c.edits == 0 && c.pending.is_none() && c.reason.as_deref() != Some("wait"))
                .map(|c| c.path.clone())
                .collect();
            for p in unwritten {
                let _ = rules::release(st, &p, Some(main), false, h.now);
            }
            let cands: Vec<Candidate> = st
                .claims
                .values()
                .filter(|c| c.holds(main) && c.edits > 0 && c.pending.is_none())
                .map(|c| (c.path.clone(), c.edits, c.touched))
                .collect();
            Some((cands, !running.is_empty()))
        });
        if let Ok(Some((cands, run))) = r {
            running_any |= run;
            work.push((repo, cands));
        }
    }

    // 2. Ask git (no transaction held) which are clean, then release only
    //    those and decide the reminder. Not confirmed clean counts as dirty.
    let active = h.p.get("stop_hook_active").bool() == Some(true);
    let mut groups: Vec<(Option<PathBuf>, Vec<String>)> = Vec::new();
    let mut waiters: Vec<(String, String)> = Vec::new();
    for (repo, cands) in work {
        let paths: Vec<String> = cands.iter().map(|c| c.0.clone()).collect();
        let ok = if paths.is_empty() { BTreeSet::new() } else { git::clean(repo, &paths) };
        let is_dirty = |p: &str| !ok.contains(p);
        let clean: Vec<&Candidate> = cands.iter().filter(|c| !is_dirty(&c.0)).collect();
        // A hook may have stashed the work: "clean" is then not "committed".
        let stash = if clean.is_empty() { None } else { git::stash_time(repo) };
        let r = state::transact(repo, |st| {
            for (p, e, t) in &clean {
                let same = st.claim(p).is_some_and(|c| c.holds(main) && c.edits == *e && c.touched == *t && c.pending.is_none());
                if same && !stash.is_some_and(|s| s >= *t) {
                    rules::committed(st, p, main);
                }
            }
            let mut files: Vec<String> =
                cands.iter().filter(|c| is_dirty(&c.0) && st.claim(&c.0).is_some_and(|x| x.holds(main))).map(|c| c.0.clone()).collect();
            files.sort();
            let held: BTreeSet<String> = st.claims.values().filter(|c| c.holds(main)).map(|c| st.key(&c.path)).collect();
            let multi = rules::multi_agent(st, main, &ctx);
            let fkeys: BTreeSet<String> = files.iter().map(|f| st.key(f)).collect();
            let fold = st.fold;
            let k = |p: &str| if fold { p.to_lowercase() } else { p.to_string() };
            let s = beat(st, h);
            s.nagged.retain(|p| held.contains(&k(p)));
            let w: Vec<(String, String)> =
                s.wants.iter().filter(|w| w.waiter != main && fkeys.contains(&k(&w.path))).map(|w| (w.waiter.clone(), w.path.clone())).collect();
            let fresh = files.iter().any(|f| !s.nagged.contains(f));
            let due = !active && multi && !files.is_empty() && !running_any && (fresh || !w.is_empty());
            if due {
                for f in &files {
                    if !s.nagged.contains(f) {
                        s.nagged.push(f.clone());
                    }
                }
            }
            (due, files, w)
        });
        if let Ok((true, files, w)) = r {
            groups.push((h.away(repo).map(Path::to_path_buf), files));
            waiters.extend(w);
        }
    }
    if groups.is_empty() {
        return None;
    }
    Some(block(msg::commit_reminder(&groups, &waiters)))
}

// ── session-end ───────────────────────────────────────────────────────────

fn session_end(h: &Hook) -> Option<Json> {
    let reason = h.p.get("reason").text().unwrap_or("other");
    let peek = index_peek(h.sid);
    for repo in session_repos(h, &peek) {
        let _ = state::transact(&repo, |st| {
            if st.off {
                return;
            }
            // Never-written claims go; dirty ones stay (SessionEnd is not death:
            // /clear and /resume go on in the same process).
            let unwritten: Vec<(String, String)> = st
                .claims
                .values()
                .filter(|c| ids::family(&c.by) == h.main && c.edits == 0 && (c.pending.is_none() || stale_marker(c)))
                .map(|c| (c.path.clone(), c.by.clone()))
                .collect();
            for (p, by) in unwritten {
                let _ = rules::release(st, &p, Some(&by), false, h.now);
            }
            let s = beat(st, h);
            s.phase = Phase::Ended;
            s.end_reason = Some(reason.to_string());
            for x in s.subs.values_mut() {
                x.done = true;
                x.busy = 0;
            }
            s.blocked.clear();
            s.bash.clear();
        });
    }
    if !peek.is_null() {
        let _ = state::transact_json(&index_path(), |all| {
            let e = slot(all, h.sid);
            e.set("ended", true.into());
            e.set("end_reason", reason.into());
            e.set("seen", h.now.into());
        });
    }
    None
}

// ── Bash command scan ─────────────────────────────────────────────────────

const IDENTITY: &[&str] = &["commit", "push", "sync", "release", "wait", "mine"];
const KEYWORDS: &[&str] = &[
    "!", "{", "}", "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "time", "function", "for", "case", "esac", "select", "coproc",
];
const LOCK_NAMES: &[&str] = &["agent-lock.json", "index.lock"];

/// Replace `start..end` of the command with this text.
type Fix = (usize, usize, String);

#[derive(Default)]
struct Scan {
    asks: Vec<String>,
    /// agent-lock run as someone else: (effective id, fix = replace start..end with text).
    bad: Vec<(String, Option<Fix>)>,
}

/// `$AGENT_LOCK_AGENT_ID` as the command line leaves it.
#[derive(Clone)]
enum Env {
    Inherit, // the session's: the main agent
    Set(String, bool),
}

fn base(s: &str) -> &str {
    let s = s.trim_end_matches('/');
    s.rsplit('/').next().unwrap_or(s)
}

fn scan(cmds: &[sh::Cmd], me: &str, out: &mut Scan, env: &mut Env, depth: usize) {
    for c in cmds {
        scan_cmd(c, me, out, env, depth);
    }
}

fn assign(w: &sh::Word, out: &mut Scan, local: &mut Option<Env>) {
    let Some((name, value)) = w.text.split_once('=') else { return };
    match name.trim_end_matches('+') {
        "CLAUDE_FILE_LOCK" if w.dynamic || value == "0" => out.asks.push(msg::ask_killswitch(&w.text)),
        "AGENT_LOCK_AGENT_ID" => *local = Some(Env::Set(value.to_string(), w.dynamic)),
        _ => {}
    }
}

/// Skip a wrapper's options (`vals` take a value); stops at the first operand.
fn skip_opts<'w>(w: &'w [sh::Word], vals: &[&str]) -> &'w [sh::Word] {
    let mut k = 0;
    while k < w.len() {
        let t = w[k].text.as_str();
        if w[k].dynamic || w[k].assign {
            break;
        }
        if t == "--" {
            return &w[k + 1..];
        }
        if vals.contains(&t) {
            k += 2;
            continue;
        }
        if t.starts_with('-') && t.len() > 1 {
            k += 1;
            continue;
        }
        break;
    }
    &w[k.min(w.len())..]
}

/// Non-option operands (`vals` options take a value, returned separately).
fn operands<'w>(w: &'w [sh::Word], vals: &[&str]) -> (Vec<&'w sh::Word>, Vec<&'w sh::Word>) {
    let (mut ops, mut opt_vals) = (Vec::new(), Vec::new());
    let mut k = 0;
    let mut dashdash = false;
    while k < w.len() {
        let t = w[k].text.as_str();
        if !dashdash && t == "--" {
            dashdash = true;
        } else if !dashdash && vals.contains(&t) {
            if let Some(v) = w.get(k + 1) {
                opt_vals.push(v);
            }
            k += 1;
        } else if dashdash || !(t.starts_with('-') && t.len() > 1) {
            ops.push(&w[k]);
        }
        k += 1;
    }
    (ops, opt_vals)
}

fn scan_cmd(c: &sh::Cmd, me: &str, out: &mut Scan, env: &mut Env, depth: usize) {
    for t in &c.outs {
        internal(t, false, out);
    }
    let mut w: &[sh::Word] = &c.words;
    let mut local: Option<Env> = None;
    for _ in 0..16 {
        while let Some(x) = w.first() {
            if x.assign {
                assign(x, out, &mut local);
            } else if !(!x.dynamic && KEYWORDS.contains(&x.text.as_str())) {
                break;
            }
            w = &w[1..];
        }
        let Some(x) = w.first() else {
            // Only assignments: they change the shell for the rest of the line.
            if let Some(l) = local {
                *env = l;
            }
            return;
        };
        let name = base(&x.text);
        if x.dynamic && name != "agent-lock" {
            return;
        }
        let rest = &w[1..];
        match name {
            "env" => {
                w = skip_opts(rest, &["-u", "--unset", "-C", "--chdir", "-S", "--split-string", "-P"]);
                continue;
            }
            "sudo" => {
                w = skip_opts(rest, &["-u", "-g", "-h", "-p", "-C", "-D", "-r", "-t", "-U", "-T", "-R", "--user", "--group", "--chdir"]);
                continue;
            }
            "command" if rest.iter().any(|x| x.text == "-v" || x.text == "-V") => return,
            "doas" | "nohup" | "builtin" | "exec" | "command" | "nice" | "stdbuf" | "timeout" | "xargs" | "caffeinate" | "chronic" | "ionice" => {
                let vals: &[&str] = match name {
                    "timeout" => &["-s", "-k", "--signal", "--kill-after"],
                    "nice" => &["-n", "--adjustment"],
                    "stdbuf" => &["-i", "-o", "-e"],
                    "doas" => &["-u", "-C"],
                    "exec" => &["-a"],
                    "ionice" => &["-c", "-n", "-p"],
                    "xargs" => {
                        &["-n", "-P", "-L", "-d", "-E", "-s", "-a", "-I", "-J", "-R", "-S", "--max-args", "--max-procs", "--delimiter", "--arg-file"]
                    }
                    _ => &[],
                };
                let mut r = skip_opts(rest, vals);
                if name == "timeout" && !r.is_empty() {
                    r = &r[1..]; // the duration
                }
                w = r;
                continue;
            }
            "export" | "declare" | "typeset" | "local" | "readonly" => {
                for x in rest.iter().filter(|x| x.assign) {
                    assign(x, out, &mut local);
                }
                if let Some(l) = local {
                    *env = l;
                }
            }
            "unset" => {
                if rest.iter().any(|x| x.text == "AGENT_LOCK_AGENT_ID") {
                    *env = Env::Inherit; // falls back to the session id: the main agent
                }
            }
            "bash" | "sh" | "zsh" | "dash" | "ksh" if depth < 4 => {
                // `-c <string>` (also `-lc`, after `--login`): the string is a script.
                let mut saw_c = false;
                for x in rest {
                    if saw_c {
                        scan(&sh::parse_nested(&x.text), me, out, &mut env.clone(), depth + 1);
                        break;
                    }
                    if !x.text.starts_with('-') {
                        break;
                    }
                    saw_c |= !x.text.starts_with("--") && x.text.contains('c');
                }
            }
            "eval" if depth < 4 => {
                let s: Vec<&str> = rest.iter().map(|x| x.text.as_str()).collect();
                scan(&sh::parse_nested(&s.join(" ")), me, out, &mut env.clone(), depth + 1);
            }
            "npx" | "bunx" | "pnpx" => {
                if rest.first().is_some_and(|x| x.text == "agent-lock") {
                    agent_lock(&rest[1..], c, local.as_ref().unwrap_or(env), me, out);
                }
            }
            "node" | "nodejs" => {
                let r = skip_opts(rest, &["-r", "--require", "--import", "--loader"]);
                if r.first().is_some_and(|x| base(&x.text) == "agent-lock") {
                    agent_lock(&r[1..], c, local.as_ref().unwrap_or(env), me, out);
                }
            }
            "agent-lock" => agent_lock(rest, c, local.as_ref().unwrap_or(env), me, out),
            "rm" | "unlink" | "rmdir" | "shred" | "srm" => {
                for x in operands(rest, &[]).0 {
                    internal(x, true, out);
                }
            }
            "mv" => {
                let (ops, vals) = operands(rest, &["-t", "--target-directory", "-S", "--suffix"]);
                for x in ops.into_iter().chain(vals) {
                    internal(x, true, out);
                }
            }
            "cp" | "install" | "ln" | "rsync" => {
                let (ops, vals) = operands(rest, &["-t", "--target-directory", "-S", "--suffix", "-m", "--mode", "-o", "-g", "-e"]);
                let dest = vals.first().copied().or(ops.last().copied());
                if let Some(d) = dest {
                    internal(d, false, out);
                    if base(&d.text) == ".git" {
                        for s in ops.iter().filter(|s| !std::ptr::eq(**s, d)) {
                            internal_path(&format!("{}/{}", d.text, base(&s.text)), s.glob, false, out);
                        }
                    }
                }
            }
            "tee" | "truncate" | "touch" => {
                for x in operands(rest, &["-s", "--size", "-r", "--reference", "-d", "--date", "-t"]).0 {
                    internal(x, false, out);
                }
            }
            "sed" | "gsed" | "perl" => {
                // In place: every operand after the script (given by -e/-f, else the first operand).
                let perl = name == "perl";
                let (mut inplace, mut given) = (false, false);
                let mut ops: Vec<&sh::Word> = Vec::new();
                let mut k = 0;
                while k < rest.len() {
                    let t = rest[k].text.as_str();
                    if t == "--" {
                        ops.extend(&rest[k + 1..]);
                        break;
                    } else if t.starts_with("--in-place") {
                        inplace = true;
                    } else if t == "--expression" || t == "--file" {
                        given = true;
                        k += 1;
                    } else if t.starts_with('-') && !t.starts_with("--") && t.len() > 1 {
                        for (i, ch) in t[1..].char_indices() {
                            if ch == 'i' {
                                inplace = true;
                                break; // the rest is a backup suffix
                            }
                            if ch == 'e' || (ch == 'f' && !perl) || (ch == 'E' && perl) {
                                given = true;
                                if i + 2 == t.len() {
                                    k += 1; // its value is the next word
                                }
                                break;
                            }
                        }
                    } else if !t.starts_with("--") {
                        ops.push(&rest[k]);
                    }
                    k += 1;
                }
                let files = if given { &ops[..] } else { ops.get(1..).unwrap_or(&[]) };
                if inplace {
                    for x in files {
                        internal(x, false, out);
                    }
                }
            }
            "dd" => {
                for x in rest {
                    if let Some(p) = x.text.strip_prefix("of=") {
                        internal_path(p, x.glob, false, out);
                    }
                }
            }
            "find" | "gfind" => {
                if !rest.iter().any(|x| matches!(x.text.as_str(), "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir")) {
                    return;
                }
                let roots: Vec<&sh::Word> = rest.iter().take_while(|x| !x.text.starts_with('-') && x.text != "(" && x.text != "!").collect();
                let names: Vec<&sh::Word> = rest
                    .windows(2)
                    .filter(|p| matches!(p[0].text.as_str(), "-name" | "-iname" | "-path" | "-ipath" | "-wholename"))
                    .map(|p| &p[1])
                    .collect();
                if names.is_empty() {
                    if let Some(r) = roots.iter().find(|r| base(&r.text) == ".git") {
                        out.asks.push(msg::ask_internals(&r.text));
                    }
                } else if let Some(n) = names.iter().find(|n| LOCK_NAMES.iter().any(|l| glob(base(&n.text), l))) {
                    out.asks.push(msg::ask_internals(&n.text));
                }
            }
            _ => {}
        }
        return;
    }
}

fn agent_lock(args: &[sh::Word], c: &sh::Cmd, env: &Env, me: &str, out: &mut Scan) {
    let (mut sub, mut as_w, mut force) = (None::<&sh::Word>, None::<(&sh::Word, bool)>, false);
    let mut k = 0;
    while k < args.len() {
        let x = &args[k];
        let t = x.text.as_str();
        if t == "--" && !x.dynamic {
            break;
        }
        if t == "--as" {
            as_w = args.get(k + 1).map(|v| (v, false));
            k += 2;
            continue;
        }
        if t.starts_with("--as=") {
            as_w = Some((x, true));
        } else if t == "-C" {
            k += 2;
            continue;
        } else if t == "--force" || t == "-f" {
            force = true;
        } else if sub.is_none() && !x.dynamic && !t.starts_with('-') {
            sub = Some(x);
        }
        k += 1;
    }
    let Some(sw) = sub else { return };
    match sw.text.as_str() {
        "off" => out.asks.push(msg::ask_off()),
        "release" if force => out.asks.push(msg::ask_force_release()),
        _ => {}
    }
    if !IDENTITY.contains(&sw.text.as_str()) {
        return;
    }
    let (eff, dynamic) = match as_w {
        Some((v, true)) => (v.text["--as=".len()..].to_string(), v.dynamic),
        Some((v, false)) => (v.text.clone(), v.dynamic),
        None => match env {
            Env::Set(v, d) => (v.clone(), *d),
            Env::Inherit => (ids::family(me).to_string(), false),
        },
    };
    if !dynamic && ids::normalize(&eff) == me {
        return;
    }
    let fix = c.fixable.then(|| match as_w {
        Some((v, eq)) => (v.start, v.end, if eq { format!("--as={me}") } else { me.to_string() }),
        None => (sw.end, sw.end, format!(" --as {me}")),
    });
    out.bad.push((if dynamic { format!("{eff} (unknown until it runs)") } else { eff }, fix));
}

/// The identity deny with the corrected command, if any agent-lock call runs as someone else.
fn identity(cmd: &str, sc: &Scan, me: &str) -> Option<String> {
    let (eff, _) = sc.bad.first()?;
    let fixed = if cmd.len() <= 4000 && sc.bad.iter().all(|b| b.1.is_some()) {
        let mut fixes: Vec<&Fix> = sc.bad.iter().filter_map(|b| b.1.as_ref()).collect();
        fixes.sort_by_key(|f| std::cmp::Reverse(f.0));
        let mut s = cmd.to_string();
        for (a, b, t) in fixes {
            if *a <= *b && *b <= s.len() && s.is_char_boundary(*a) && s.is_char_boundary(*b) {
                s.replace_range(*a..*b, t);
            }
        }
        s
    } else {
        format!("(add --as {me} to every agent-lock command)")
    };
    Some(msg::identity(eff, me, &fixed))
}

fn internal(w: &sh::Word, remove: bool, out: &mut Scan) {
    if !w.dynamic {
        internal_path(&w.text, w.glob, remove, out);
    }
}

/// Does a write/remove target reach agent-lock's state or the index lock?
fn internal_path(p: &str, globbed: bool, remove: bool, out: &mut Scan) {
    let t = p.trim_end_matches('/');
    let (dir, name) = match t.rfind('/') {
        Some(i) => (&t[..i], &t[i + 1..]),
        None => ("", t),
    };
    let in_git = dir == ".git" || dir.ends_with("/.git") || dir.starts_with(".git/") || dir.contains("/.git/");
    // agent-lock.json by its name anywhere; index.lock only inside a .git dir
    let hit = if globbed {
        in_git && LOCK_NAMES.iter().any(|l| glob(name, l))
    } else {
        name == "agent-lock.json" || (in_git && name == "index.lock") || (remove && name == ".git")
    };
    if hit {
        out.asks.push(msg::ask_internals(p));
    }
}

/// Shell glob: `*`, `?`, `[…]` (with `!`/`^` and ranges).
fn glob(pat: &str, s: &str) -> bool {
    let (p, s) = (pat.as_bytes(), s.as_bytes());
    let (mut pi, mut si) = (0, 0);
    let mut back: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    back = Some((pi, si));
                    pi += 1;
                    continue;
                }
                b'?' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                b'[' => {
                    if let Some((ok, len)) = class(&p[pi..], s[si]) {
                        if ok {
                            pi += len;
                            si += 1;
                            continue;
                        }
                    } else if s[si] == b'[' {
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
                c if c == s[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }
        match back {
            Some((bp, bs)) => {
                pi = bp + 1;
                si = bs + 1;
                back = Some((bp, bs + 1));
            }
            None => return false,
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// `[…]` at the start of `p`: (matches `c`, length) or None when unterminated.
fn class(p: &[u8], c: u8) -> Option<(bool, usize)> {
    let mut i = 1;
    let neg = matches!(p.get(1), Some(b'!' | b'^'));
    if neg {
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == b']' && !first {
            return Some((hit != neg, i + 1));
        }
        first = false;
        if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
            hit |= p[i] <= c && c <= p[i + 2];
            i += 3;
        } else {
            hit |= p[i] == c;
            i += 1;
        }
    }
    None
}

/// A small POSIX-shell reader: every simple command (also inside `$(…)`,
/// backticks and process substitution), its words with source spans, and
/// its output redirection targets. Heredoc bodies are data. Never fails:
/// odd input just yields fewer or odder words.
mod sh {
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct Word {
        pub text: String, // quotes removed; expansions kept as written
        pub start: usize,
        pub end: usize,
        pub dynamic: bool, // holds an expansion: the value is known only when it runs
        pub glob: bool,    // an unquoted * ? [
        pub assign: bool,  // NAME=value
    }

    #[derive(Clone, Debug, Default)]
    pub struct Cmd {
        pub words: Vec<Word>,
        pub outs: Vec<Word>,
        pub fixable: bool, // spans index into the analyzed command text
    }

    pub fn parse(src: &str) -> Vec<Cmd> {
        run(src, true, 0)
    }

    /// A string run by `bash -c` / `eval`: spans do not map back.
    pub fn parse_nested(src: &str) -> Vec<Cmd> {
        run(src, false, 1)
    }

    fn run(src: &str, fixable: bool, depth: usize) -> Vec<Cmd> {
        let mut p = P { b: src.as_bytes(), i: 0, cmds: Vec::new(), here: Vec::new(), depth, fixable };
        p.list(false);
        p.cmds
    }

    const MAX_DEPTH: usize = 24;

    struct P<'a> {
        b: &'a [u8],
        i: usize,
        cmds: Vec<Cmd>,
        here: Vec<(Vec<u8>, bool)>, // pending heredoc delimiters (strip tabs)
        depth: usize,
        fixable: bool,
    }

    fn is_name(t: &[u8]) -> bool {
        let t = t.strip_suffix(b"+").unwrap_or(t);
        !t.is_empty() && !t[0].is_ascii_digit() && t.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
    }

    impl<'a> P<'a> {
        fn peek(&self) -> Option<u8> {
            self.b.get(self.i).copied()
        }

        fn at(&self, k: usize) -> Option<u8> {
            self.b.get(self.i + k).copied()
        }

        fn end(&mut self, cur: &mut Cmd) {
            if !cur.words.is_empty() || !cur.outs.is_empty() {
                let mut c = std::mem::take(cur);
                c.fixable = self.fixable;
                self.cmds.push(c);
            }
        }

        /// Commands until the end, or (`close`) until an unmatched `)`.
        fn list(&mut self, close: bool) {
            self.depth += 1;
            let mut cur = Cmd::default();
            let mut parens = 0usize;
            while let Some(c) = self.peek() {
                match c {
                    b' ' | b'\t' | b'\r' => self.i += 1,
                    b'\\' if self.at(1) == Some(b'\n') => self.i += 2,
                    b'\n' => {
                        self.i += 1;
                        self.end(&mut cur);
                        self.bodies();
                    }
                    b'#' => {
                        while self.peek().is_some_and(|c| c != b'\n') {
                            self.i += 1;
                        }
                    }
                    b'&' if self.at(1) == Some(b'>') => self.redir(&mut cur),
                    b';' | b'&' | b'|' => {
                        self.i += 1;
                        self.end(&mut cur);
                    }
                    b'(' => {
                        self.i += 1;
                        parens += 1;
                        self.end(&mut cur);
                    }
                    b')' => {
                        self.i += 1;
                        self.end(&mut cur);
                        if parens == 0 {
                            if close {
                                break;
                            }
                        } else {
                            parens -= 1;
                        }
                    }
                    b'<' | b'>' => self.redir(&mut cur),
                    b'0'..=b'9' if self.fd_redir() => self.redir(&mut cur),
                    _ => {
                        let w = self.word();
                        cur.words.push(w);
                    }
                }
            }
            self.end(&mut cur);
            self.depth -= 1;
        }

        fn fd_redir(&self) -> bool {
            let mut k = self.i;
            while self.b.get(k).is_some_and(|c| c.is_ascii_digit()) {
                k += 1;
            }
            matches!(self.b.get(k), Some(b'<' | b'>'))
        }

        fn redir(&mut self, cur: &mut Cmd) {
            let start = self.i;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
            let amp = self.peek() == Some(b'&');
            if amp {
                self.i += 1;
            }
            let dir = self.peek();
            self.i += 1;
            if !amp && self.peek() == Some(b'(') {
                // <(…) >(…): a command whose output is a file name
                self.i += 1;
                self.sub_list();
                cur.words.push(Word { text: "<(…)".into(), start, end: self.i, dynamic: true, ..Word::default() });
                return;
            }
            let (mut heredoc, mut strip, mut dup) = (false, false, false);
            match (dir, self.peek()) {
                (Some(b'<'), Some(b'<')) => {
                    self.i += 1;
                    if self.peek() == Some(b'<') {
                        self.i += 1; // here-string: data
                    } else {
                        heredoc = true;
                        if self.peek() == Some(b'-') {
                            self.i += 1;
                            strip = true;
                        }
                    }
                }
                (Some(b'>'), Some(b'>' | b'|')) | (Some(b'<'), Some(b'>')) => self.i += 1,
                (_, Some(b'&')) => {
                    self.i += 1;
                    dup = true;
                }
                _ => {}
            }
            while matches!(self.peek(), Some(b' ' | b'\t')) {
                self.i += 1;
            }
            if self.peek().is_none_or(|c| b"\n;&|()<>".contains(&c)) {
                return;
            }
            let w = self.word();
            if heredoc {
                self.here.push((w.text.into_bytes(), strip));
                return;
            }
            let output = dir == Some(b'>') || amp;
            let fd = w.text == "-" || (!w.text.is_empty() && w.text.bytes().all(|c| c.is_ascii_digit()));
            if output && !(dup && fd) && dir != Some(b'<') {
                cur.outs.push(w);
            }
        }

        /// Skip heredoc bodies after a newline.
        fn bodies(&mut self) {
            for (delim, strip) in std::mem::take(&mut self.here) {
                while self.i < self.b.len() {
                    let e = self.b[self.i..].iter().position(|&c| c == b'\n').map(|k| self.i + k).unwrap_or(self.b.len());
                    let mut line = &self.b[self.i..e];
                    self.i = (e + 1).min(self.b.len());
                    while strip && line.first() == Some(&b'\t') {
                        line = &line[1..];
                    }
                    if line == delim.as_slice() {
                        break;
                    }
                }
            }
        }

        /// After `$(` or `<(`: the commands up to the matching `)`.
        fn sub_list(&mut self) {
            if self.depth < MAX_DEPTH {
                self.list(true);
            } else {
                self.skip_close(b'(', b')');
            }
        }

        fn skip_close(&mut self, open: u8, close: u8) {
            let mut d = 1;
            while let Some(c) = self.peek() {
                self.i += 1;
                match c {
                    b'\\' => self.i += 1,
                    b'\'' => {
                        while let Some(c) = self.peek() {
                            self.i += 1;
                            if c == b'\'' {
                                break;
                            }
                        }
                    }
                    c if c == open => d += 1,
                    c if c == close => {
                        d -= 1;
                        if d == 0 {
                            return;
                        }
                    }
                    _ => {}
                }
            }
            self.i = self.i.min(self.b.len());
        }

        fn word(&mut self) -> Word {
            let mut w = Word { start: self.i, ..Word::default() };
            let mut t: Vec<u8> = Vec::new();
            let mut plain = true;
            while let Some(c) = self.peek() {
                match c {
                    b'(' if w.assign && t.last() == Some(&b'=') => {
                        self.i += 1; // name=(…) array
                        self.skip_close(b'(', b')');
                        w.dynamic = true;
                    }
                    b' ' | b'\t' | b'\r' | b'\n' | b';' | b'&' | b'|' | b'(' | b')' | b'<' | b'>' => break,
                    b'\'' => {
                        self.i += 1;
                        plain = false;
                        while let Some(c) = self.peek() {
                            self.i += 1;
                            if c == b'\'' {
                                break;
                            }
                            t.push(c);
                        }
                    }
                    b'"' => {
                        self.i += 1;
                        plain = false;
                        self.dq(&mut t, &mut w);
                    }
                    b'\\' => {
                        self.i += 1;
                        plain = false;
                        if let Some(c) = self.peek() {
                            self.i += 1;
                            if c != b'\n' {
                                t.push(c);
                            }
                        }
                    }
                    b'$' => {
                        plain = false;
                        self.dollar(&mut t, &mut w);
                    }
                    b'`' => {
                        plain = false;
                        self.backtick(&mut t, &mut w);
                    }
                    b'=' if plain && !w.assign && is_name(&t) => {
                        w.assign = true;
                        t.push(c);
                        self.i += 1;
                    }
                    b'*' | b'?' | b'[' => {
                        w.glob = true;
                        t.push(c);
                        self.i += 1;
                    }
                    _ => {
                        t.push(c);
                        self.i += 1;
                    }
                }
            }
            w.end = self.i.min(self.b.len());
            w.text = String::from_utf8_lossy(&t).into_owned();
            w
        }

        fn dq(&mut self, t: &mut Vec<u8>, w: &mut Word) {
            while let Some(c) = self.peek() {
                match c {
                    b'"' => {
                        self.i += 1;
                        return;
                    }
                    b'\\' => {
                        self.i += 1;
                        match self.peek() {
                            Some(n @ (b'$' | b'`' | b'"' | b'\\')) => {
                                t.push(n);
                                self.i += 1;
                            }
                            Some(b'\n') => self.i += 1,
                            _ => t.push(b'\\'),
                        }
                    }
                    b'$' => self.dollar(t, w),
                    b'`' => self.backtick(t, w),
                    _ => {
                        t.push(c);
                        self.i += 1;
                    }
                }
            }
        }

        fn dollar(&mut self, t: &mut Vec<u8>, w: &mut Word) {
            let s = self.i;
            self.i += 1;
            match self.peek() {
                Some(b'(') if self.at(1) == Some(b'(') => {
                    self.i += 2;
                    self.skip_close(b'(', b')');
                    self.skip_close(b'(', b')');
                    w.dynamic = true;
                }
                Some(b'(') => {
                    self.i += 1;
                    self.sub_list();
                    w.dynamic = true;
                }
                Some(b'{') => {
                    self.i += 1;
                    self.skip_close(b'{', b'}');
                    w.dynamic = true;
                }
                Some(b'\'') => {
                    // $'…': ANSI-C quoting, a literal
                    self.i += 1;
                    while let Some(c) = self.peek() {
                        self.i += 1;
                        match c {
                            b'\'' => break,
                            b'\\' => {
                                if let Some(n) = self.peek() {
                                    self.i += 1;
                                    t.push(match n {
                                        b'n' => b'\n',
                                        b't' => b'\t',
                                        b'r' => b'\r',
                                        x => x,
                                    });
                                }
                            }
                            _ => t.push(c),
                        }
                    }
                    return;
                }
                Some(b'"') => return, // $"…": the caller reads the string
                Some(c) if c == b'_' || c.is_ascii_alphanumeric() => {
                    if c.is_ascii_digit() {
                        self.i += 1;
                    } else {
                        while self.peek().is_some_and(|c| c == b'_' || c.is_ascii_alphanumeric()) {
                            self.i += 1;
                        }
                    }
                    w.dynamic = true;
                }
                Some(b'@' | b'*' | b'#' | b'?' | b'$' | b'!' | b'-') => {
                    self.i += 1;
                    w.dynamic = true;
                }
                _ => {
                    t.push(b'$');
                    return;
                }
            }
            let e = self.i.min(self.b.len());
            t.extend_from_slice(&self.b[s..e]);
        }

        fn backtick(&mut self, t: &mut Vec<u8>, w: &mut Word) {
            let s = self.i;
            self.i += 1;
            let mut inner = Vec::new();
            while let Some(c) = self.peek() {
                self.i += 1;
                match c {
                    b'`' => break,
                    b'\\' => {
                        if let Some(n) = self.peek() {
                            self.i += 1;
                            if !matches!(n, b'`' | b'\\' | b'$') {
                                inner.push(b'\\');
                            }
                            inner.push(n);
                        }
                    }
                    _ => inner.push(c),
                }
            }
            if self.depth < MAX_DEPTH {
                let src = String::from_utf8_lossy(&inner).into_owned();
                self.cmds.extend(run(&src, false, self.depth + 1));
            }
            w.dynamic = true;
            let e = self.i.min(self.b.len());
            t.extend_from_slice(&self.b[s..e]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(src: &str) -> Vec<Vec<String>> {
        sh::parse(src).iter().map(|c| c.words.iter().map(|w| w.text.clone()).collect()).collect()
    }

    fn scan_of(src: &str, me: &str) -> Scan {
        let mut sc = Scan::default();
        scan(&sh::parse(src), me, &mut sc, &mut Env::Inherit, 0);
        sc
    }

    const MAIN: &str = "aaaaaaaa";
    const SUB: &str = "aaaaaaaa:11111111";

    #[test]
    fn shell_words_quotes_and_lists() {
        assert_eq!(words("a 'b c' \"d $e\" f\\ g"), vec![vec!["a", "b c", "d $e", "f g"]]);
        assert_eq!(words("a && b || c; d | e & f"), vec![vec!["a"], vec!["b"], vec!["c"], vec!["d"], vec!["e"], vec!["f"]]);
        assert_eq!(words("x # comment ; rm y\nz"), vec![vec!["x"], vec!["z"]]);
        assert_eq!(words("(cd a && b)"), vec![vec!["cd", "a"], vec!["b"]]);
        let c = &sh::parse("X=1 Y+=2 env")[0];
        assert!(c.words[0].assign && c.words[1].assign && !c.words[2].assign);
        assert!(!sh::parse("'X=1' y")[0].words[0].assign);
    }

    #[test]
    fn shell_substitutions_and_heredocs() {
        // a heredoc with parens and quotes inside $(…) inside "…"
        let src = "agent-lock commit -m \"$(cat <<'EOF'\nFix (a) \"b\" ) `c`\nEOF\n)\" && echo ok";
        let cmds = words(src);
        assert!(cmds.contains(&vec!["cat".to_string()]), "{cmds:?}");
        assert!(cmds.iter().any(|c| c.first().map(String::as_str) == Some("agent-lock") && c.len() == 4));
        assert!(cmds.contains(&vec!["echo".to_string(), "ok".to_string()]));
        assert!(!cmds.iter().flatten().any(|w| w == "c"), "heredoc body is data");
        assert_eq!(words("echo `rm x` $(ls y)").iter().filter(|c| c[0] == "rm" || c[0] == "ls").count(), 2);
        assert_eq!(words("diff <(sort a) b"), vec![vec!["sort", "a"], vec!["diff", "<(…)", "b"]]);
        let c = &sh::parse("echo hi > out.txt 2>&1 >> 'b c' <in")[0];
        assert_eq!(c.outs.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(), vec!["out.txt", "b c"]);
        assert_eq!(words("cat <<-X\n\tbody; rm z\n\tX\nnext"), vec![vec!["cat"], vec!["next"]]);
        assert_eq!(sh::parse("echo $((1+(2))) x").len(), 1);
    }

    #[test]
    fn spans_point_into_the_source() {
        let src = "cd x && agent-lock commit -m 'a b'";
        let c = &sh::parse(src)[1];
        assert_eq!(&src[c.words[1].start..c.words[1].end], "commit");
        assert_eq!(&src[c.words[3].start..c.words[3].end], "'a b'");
    }

    #[test]
    fn identity_rules() {
        // a subagent without --as acts as its parent
        let sc = scan_of("agent-lock commit -m x", SUB);
        assert_eq!(sc.bad.len(), 1);
        assert_eq!(
            identity("agent-lock commit -m x", &sc, SUB).unwrap().lines().last().unwrap().trim(),
            "agent-lock commit --as aaaaaaaa:11111111 -m x"
        );
        assert!(scan_of("agent-lock commit --as aaaaaaaa:11111111 -m x", SUB).bad.is_empty());
        assert!(scan_of("agent-lock --as=AAAAAAAA:11111111 push", SUB).bad.is_empty());
        assert!(scan_of("agent-lock commit -m x", MAIN).bad.is_empty());
        assert!(scan_of("agent-lock status", SUB).bad.is_empty(), "read-only commands need no id");
        // someone else's id, inline env, exported env, a dynamic value
        let src = "agent-lock push --as bbbbbbbb";
        assert!(identity(src, &scan_of(src, MAIN), MAIN).unwrap().ends_with("agent-lock push --as aaaaaaaa"));
        assert_eq!(scan_of("AGENT_LOCK_AGENT_ID=bbbbbbbb agent-lock sync", MAIN).bad.len(), 1);
        assert_eq!(scan_of("export AGENT_LOCK_AGENT_ID=aaaaaaaa:11111111; agent-lock mine", SUB).bad.len(), 0);
        assert_eq!(scan_of("agent-lock wait --as \"$ID\" x", SUB).bad.len(), 1);
        assert_eq!(scan_of("npx agent-lock release a", SUB).bad.len(), 1);
        assert_eq!(scan_of("node /p/bin/agent-lock commit -m x", SUB).bad.len(), 1);
        assert_eq!(scan_of("\"$ROOT/bin/agent-lock\" commit -m x", SUB).bad.len(), 1);
        // nested in bash -c: no automatic fix
        let src = "bash -c 'agent-lock commit -m x'";
        let sc = scan_of(src, SUB);
        assert_eq!(sc.bad.len(), 1);
        assert!(identity(src, &sc, SUB).unwrap().contains("(add --as aaaaaaaa:11111111 to every agent-lock command)"));
        // two calls fixed at once, from the right end
        let src = "agent-lock commit -m a && agent-lock push";
        assert!(
            identity(src, &scan_of(src, SUB), SUB)
                .unwrap()
                .ends_with("agent-lock commit --as aaaaaaaa:11111111 -m a && agent-lock push --as aaaaaaaa:11111111")
        );
    }

    #[test]
    fn asks() {
        let n = |s: &str| scan_of(s, MAIN).asks.len();
        for s in [
            "CLAUDE_FILE_LOCK=0 git commit -am x",
            "export CLAUDE_FILE_LOCK=0",
            "env CLAUDE_FILE_LOCK=0 make",
            "CLAUDE_FILE_LOCK=$X npm i",
            "agent-lock off",
            "agent-lock release --force src/a.ts",
            "rm .git/agent-lock.json",
            "rm -f .git/index.lock",
            "rm -f /r/.git/worktrees/wt/index.lock",
            "rm -rf .git",
            "rm -rf ./.git/",
            "rm .git/*.lock",
            "echo '{}' > .git/agent-lock.json",
            "jq . x >> /r/.git/agent-lock.json",
            "sed -i 's/a/b/' .git/agent-lock.json",
            "sed -i.bak -e 's/a/b/' .git/agent-lock.json",
            "perl -pi -e 's/a/b/' .git/agent-lock.json",
            "truncate -s 0 .git/agent-lock.json",
            "touch .git/index.lock",
            "tee .git/agent-lock.json < x",
            "mv .git/agent-lock.json /tmp/",
            "cp /tmp/x .git/agent-lock.json",
            "cp agent-lock.json .git",
            "dd if=/dev/null of=.git/index.lock",
            "find .git -name index.lock -delete",
            "sudo -u me rm .git/index.lock",
            "bash -c 'rm .git/index.lock'",
            "bash --login -ec 'rm .git/index.lock'",
            "cd .git && rm agent-lock.json",
        ] {
            assert_eq!(n(s), 1, "{s}");
        }
        for s in [
            "CLAUDE_FILE_LOCK=1 x",
            "agent-lock on",
            "agent-lock release src/a.ts",
            "cat .git/agent-lock.json",
            "cp .git/agent-lock.json /tmp/backup.json",
            "echo agent-lock.json",
            "rm *.lock",
            "sed 's/a/b/' .git/agent-lock.json",
            "find . -name '*.orig' -delete",
            "git status > /tmp/out",
            "ls 2>&1",
            "cat <<EOF\nrm .git/index.lock\nEOF",
            // another tool's index.lock: only the one in a .git dir is git's
            "rm -f dist/index.lock",
            "rm -rf node_modules/.cache/index.lock",
            "touch build/index.lock",
        ] {
            assert_eq!(n(s), 0, "{s}");
        }
    }

    #[test]
    fn globs() {
        assert!(glob("*.lock", "index.lock"));
        assert!(glob("*", "agent-lock.json"));
        assert!(glob("ind?x.l[a-z]ck", "index.lock"));
        assert!(glob("agent-lock.[!x]son", "agent-lock.json"));
        assert!(!glob("*.json", "index.lock"));
        assert!(!glob("a*b", "ac"));
        assert!(glob("a*b*c", "aXbYbc"));
    }

    #[test]
    fn index_entry_found_by_key() {
        let dir = std::env::temp_dir().join(format!("al-hooks-idx-{}-{}", sys::pid(), sys::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("sessions.json");
        state::transact_json(&f, |all| {
            slot(all, "s-1").set("repos", Json::Arr(vec!["/a/.git".into()]));
            slot(all, "s\"2").set("seen", 5u64.into());
        })
        .unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        for (sid, want) in [("s-1", true), ("s\"2", true), ("s-3", false)] {
            let mut key = String::new();
            json::write_str(&mut key, sid);
            key.push(':');
            assert_eq!(text.contains(&key), want, "{sid}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn repo_from_git_dir() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap().join(format!("al-hooks-repo-{}-{}", sys::pid(), sys::now_ms()));
        let main = base.join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        std::fs::write(main.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let r = repo_at(&main.join(".git")).unwrap();
        assert_eq!(r.top, main);
        // a linked worktree
        let wt = base.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let wgd = main.join(".git/worktrees/wt");
        std::fs::write(wgd.join("HEAD"), "ref: refs/heads/x\n").unwrap();
        std::fs::write(wgd.join("gitdir"), format!("{}\n", wt.join(".git").display())).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wgd.display())).unwrap();
        let r = repo_at(&wgd).unwrap();
        assert_eq!((r.top.clone(), r.git_dir.clone()), (wt.clone(), wgd.clone()));
        assert!(repo_at(&base.join("nope/.git")).is_none());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn stamps_compare_through_f64() {
        let t: u128 = 1_729_800_000_123_456_789;
        let stored = (t as f64) as u128;
        assert!(same_stamp((t, 10), (stored, 10)));
        assert!(!same_stamp((t + 1_000_000, 10), (stored, 10)));
        assert!(!same_stamp((t, 11), (stored, 10)));
    }
}
