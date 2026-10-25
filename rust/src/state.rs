//! The one state file, `<gitdir>/agent-lock.json`, and its transaction.
//!
//! Every change is: open → flock(EX) → read → parse → change in memory →
//! one write at offset 0 + truncate → close (the kernel drops the lock, also
//! when the process dies). The inode never changes, so every waiter locks
//! the same file. A write cut short leaves trailing bytes; the reader parses
//! the first JSON value and ignores the rest.

use crate::json::{self, object, Json};
use crate::sys::{self, Owner, Repo};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::time::{Duration, Instant};

pub const VERSION: u64 = 3;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Claim {
    pub path: String, // repo-relative, as first written
    pub by: String,
    pub session: Option<String>,
    pub owner: Option<Owner>,
    pub at: u64,
    pub touched: u64,
    pub edits: u32,
    pub pending: Option<String>,   // in flight: a tool_use_id, or "<kind>:<pid>:<start>" (rules::marker)
    pub made_by: Option<String>,   // tool_use_id that created the claim
    pub inherited: Option<String>, // agent whose uncommitted edits came with it
    pub reason: Option<String>,    // "wait" for a `wait --claim` reservation
    pub with: Vec<String>,         // co-holders of a shared file (crate::shared), besides `by`
}

impl Claim {
    /// `agent` holds this claim: its holder, or a co-holder of a shared file.
    pub fn holds(&self, agent: &str) -> bool {
        self.by == agent || self.with.iter().any(|w| w == agent)
    }

    /// Everyone who holds it, `by` first.
    pub fn holders(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.by.as_str()).chain(self.with.iter().map(String::as_str))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Active,
    Idle,
    Ended,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Active => "active",
            Phase::Idle => "idle",
            Phase::Ended => "ended",
        }
    }
    fn parse(s: Option<&str>) -> Phase {
        match s {
            Some("idle") => Phase::Idle,
            Some("ended") => Phase::Ended,
            _ => Phase::Active,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sub {
    pub seen: u64,
    pub done: bool,
    pub busy: u32, // tool calls in flight
}

#[derive(Clone, Debug, PartialEq)]
pub struct Want {
    pub waiter: String,
    pub path: String,
    pub at: u64,
    pub told: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Blocked {
    pub path: String,
    pub holder: String,
    pub at: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Takeover {
    pub path: String,
    pub agent: String,
    pub from: String,
    pub at: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BashSnap {
    pub agent: String,
    pub at: u64,
    pub files: Vec<(String, u128, u64)>, // repo-relative path, mtime ns, size
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Session {
    pub id: String,
    pub agent: String, // main agent id (sid8)
    pub owner: Option<Owner>,
    pub started: u64,
    pub seen: u64,
    pub phase: Phase,
    pub end_reason: Option<String>,
    pub subs: BTreeMap<String, Sub>, // full subagent ids
    pub wants: Vec<Want>,            // agents waiting for this session's files
    pub blocked: BTreeMap<String, Blocked>, // own agents waiting for others' files
    pub nagged: Vec<String>,
    pub takeover: Option<Takeover>,
    pub bash: BTreeMap<String, BashSnap>, // tool_use_id → stamps of others' claimed files
    pub bg: Option<Vec<String>>,          // subagents Stop last saw running in the background
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpLock {
    pub by: String,
    pub owner: Option<Owner>,
    pub since: u64,
    pub what: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SyncLock {
    pub by: String,
    pub owner: Option<Owner>,
    pub since: u64,
    pub head: String,
    pub tip: String,
    pub paths: Vec<String>,
    pub index_tmp: Option<String>,
    pub lock_ino: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Locks {
    pub git: Option<OpLock>,
    pub pkg: Option<OpLock>,
    pub sync: Option<SyncLock>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Orphan {
    pub path: String,
    pub from: String,
    pub at: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Upstream {
    pub remote: String,
    pub branch: String,
    pub oid: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub off: bool,
    pub claims: BTreeMap<String, Claim>, // key(path) → claim
    pub sessions: BTreeMap<String, Session>,
    pub locks: Locks,
    pub orphans: BTreeMap<String, Orphan>,
    pub upstream: Option<Upstream>,
    /// Paths compare case-insensitively (core.ignorecase). Not stored.
    pub fold: bool,
    /// Which files are shared (built-ins + `.agent-lock-shared`). Not stored.
    pub shared: crate::shared::Shared,
}

impl State {
    pub fn is_shared(&self, path: &str) -> bool {
        self.shared.is(path)
    }

    pub fn key(&self, path: &str) -> String {
        if self.fold { path.to_lowercase() } else { path.to_string() }
    }

    pub fn claim(&self, path: &str) -> Option<&Claim> {
        self.claims.get(&self.key(path))
    }

    pub fn claim_mut(&mut self, path: &str) -> Option<&mut Claim> {
        let k = self.key(path);
        self.claims.get_mut(&k)
    }

    pub fn put_claim(&mut self, c: Claim) {
        let k = self.key(&c.path);
        self.claims.insert(k, c);
    }

    pub fn take_claim(&mut self, path: &str) -> Option<Claim> {
        let k = self.key(path);
        self.claims.remove(&k)
    }

    /// The session whose main agent is `family(agent)` (newest if several).
    pub fn session_of(&self, agent: &str) -> Option<&Session> {
        let fam = crate::ids::family(agent);
        self.sessions.values().filter(|s| s.agent == fam).max_by_key(|s| s.seen)
    }

    pub fn session_of_mut(&mut self, agent: &str) -> Option<&mut Session> {
        let fam = crate::ids::family(agent).to_string();
        let id = self.sessions.values().filter(|s| s.agent == fam).max_by_key(|s| s.seen)?.id.clone();
        self.sessions.get_mut(&id)
    }
}

// ── JSON mapping ──────────────────────────────────────────────────────────

pub(crate) fn owner_json(o: Option<Owner>) -> Json {
    match o {
        Some(o) => object([("pid", o.pid.into()), ("start", o.start.into())]),
        None => Json::Null,
    }
}

pub(crate) fn owner_from(v: &Json) -> Option<Owner> {
    Some(Owner { pid: v.get("pid").u64()? as u32, start: v.get("start").u64()? })
}

fn s(v: &Json, k: &str) -> Option<String> {
    v.get(k).str().map(str::to_string)
}

fn n(v: &Json, k: &str) -> u64 {
    v.get(k).u64().unwrap_or(0)
}

pub(crate) fn strs(v: &Json) -> Vec<String> {
    v.arr().iter().filter_map(|x| x.str().map(str::to_string)).collect()
}

fn str_arr(a: &[String]) -> Json {
    Json::Arr(a.iter().map(|x| Json::from(x.as_str())).collect())
}

impl Claim {
    fn to_json(&self) -> Json {
        object([
            ("by", self.by.as_str().into()),
            ("session", self.session.clone().into()),
            ("owner", owner_json(self.owner)),
            ("at", self.at.into()),
            ("touched", self.touched.into()),
            ("edits", self.edits.into()),
            ("pending", self.pending.clone().into()),
            ("made_by", self.made_by.clone().into()),
            ("inherited", self.inherited.clone().into()),
            ("reason", self.reason.clone().into()),
            ("with", if self.with.is_empty() { Json::Null } else { str_arr(&self.with) }),
        ])
    }

    fn from_json(path: &str, v: &Json) -> Option<Claim> {
        Some(Claim {
            path: path.to_string(),
            by: s(v, "by")?,
            session: s(v, "session"),
            owner: owner_from(v.get("owner")),
            at: n(v, "at"),
            touched: n(v, "touched"),
            edits: n(v, "edits") as u32,
            pending: s(v, "pending"),
            made_by: s(v, "made_by"),
            inherited: s(v, "inherited"),
            reason: s(v, "reason"),
            with: strs(v.get("with")),
        })
    }
}

impl Session {
    fn to_json(&self) -> Json {
        let subs = Json::Obj(
            self.subs
                .iter()
                .map(|(k, x)| {
                    (k.clone(), object([("seen", x.seen.into()), ("done", x.done.into()), ("busy", x.busy.into())]))
                })
                .collect(),
        );
        let wants = Json::Arr(
            self.wants
                .iter()
                .map(|w| object([("waiter", w.waiter.as_str().into()), ("path", w.path.as_str().into()), ("at", w.at.into()), ("told", w.told.into())]))
                .collect(),
        );
        let blocked = Json::Obj(
            self.blocked
                .iter()
                .map(|(k, b)| (k.clone(), object([("path", b.path.as_str().into()), ("holder", b.holder.as_str().into()), ("at", b.at.into())])))
                .collect(),
        );
        let bash = Json::Obj(
            self.bash
                .iter()
                .map(|(k, b)| {
                    let files = Json::Arr(
                        b.files
                            .iter()
                            .map(|(p, t, z)| Json::Arr(vec![p.as_str().into(), Json::Num(*t as f64), (*z).into()]))
                            .collect(),
                    );
                    (k.clone(), object([("agent", b.agent.as_str().into()), ("at", b.at.into()), ("files", files)]))
                })
                .collect(),
        );
        object([
            ("agent", self.agent.as_str().into()),
            ("owner", owner_json(self.owner)),
            ("started", self.started.into()),
            ("seen", self.seen.into()),
            ("phase", self.phase.name().into()),
            ("end_reason", self.end_reason.clone().into()),
            ("subs", if self.subs.is_empty() { Json::Null } else { subs }),
            ("wants", if self.wants.is_empty() { Json::Null } else { wants }),
            ("blocked", if self.blocked.is_empty() { Json::Null } else { blocked }),
            ("nagged", if self.nagged.is_empty() { Json::Null } else { str_arr(&self.nagged) }),
            (
                "takeover",
                self.takeover
                    .as_ref()
                    .map(|t| object([("path", t.path.as_str().into()), ("agent", t.agent.as_str().into()), ("from", t.from.as_str().into()), ("at", t.at.into())]))
                    .unwrap_or(Json::Null),
            ),
            ("bash", if self.bash.is_empty() { Json::Null } else { bash }),
            ("bg", self.bg.as_ref().map(|b| str_arr(b)).unwrap_or(Json::Null)),
        ])
    }

    fn from_json(id: &str, v: &Json) -> Option<Session> {
        Some(Session {
            id: id.to_string(),
            agent: s(v, "agent")?,
            owner: owner_from(v.get("owner")),
            started: n(v, "started"),
            seen: n(v, "seen"),
            phase: Phase::parse(v.get("phase").str()),
            end_reason: s(v, "end_reason"),
            subs: v
                .get("subs")
                .members()
                .iter()
                .map(|(k, x)| (k.clone(), Sub { seen: n(x, "seen"), done: x.get("done").bool().unwrap_or(false), busy: n(x, "busy") as u32 }))
                .collect(),
            wants: v
                .get("wants")
                .arr()
                .iter()
                .filter_map(|w| Some(Want { waiter: s(w, "waiter")?, path: s(w, "path")?, at: n(w, "at"), told: w.get("told").bool().unwrap_or(false) }))
                .collect(),
            blocked: v
                .get("blocked")
                .members()
                .iter()
                .filter_map(|(k, b)| Some((k.clone(), Blocked { path: s(b, "path")?, holder: s(b, "holder")?, at: n(b, "at") })))
                .collect(),
            nagged: strs(v.get("nagged")),
            takeover: {
                let t = v.get("takeover");
                (|| Some(Takeover { path: s(t, "path")?, agent: s(t, "agent")?, from: s(t, "from")?, at: n(t, "at") }))()
            },
            bash: v
                .get("bash")
                .members()
                .iter()
                .map(|(k, b)| {
                    let files = b
                        .get("files")
                        .arr()
                        .iter()
                        .filter_map(|f| {
                            let a = f.arr();
                            Some((a.first()?.str()?.to_string(), match a.get(1)? { Json::Num(x) => *x as u128, _ => 0 }, a.get(2)?.u64()?))
                        })
                        .collect();
                    (k.clone(), BashSnap { agent: s(b, "agent").unwrap_or_default(), at: n(b, "at"), files })
                })
                .collect(),
            bg: if v.get("bg").is_null() { None } else { Some(strs(v.get("bg"))) },
        })
    }
}

fn oplock_json(l: &Option<OpLock>) -> Json {
    match l {
        Some(l) => object([
            ("by", l.by.as_str().into()),
            ("owner", owner_json(l.owner)),
            ("since", l.since.into()),
            ("what", l.what.as_str().into()),
        ]),
        None => Json::Null,
    }
}

fn oplock_from(v: &Json) -> Option<OpLock> {
    Some(OpLock { by: s(v, "by")?, owner: owner_from(v.get("owner")), since: n(v, "since"), what: s(v, "what").unwrap_or_default() })
}

impl State {
    pub fn to_json(&self) -> Json {
        let claims = Json::Obj(self.claims.values().map(|c| (c.path.clone(), c.to_json())).collect());
        let sessions = Json::Obj(self.sessions.iter().map(|(k, x)| (k.clone(), x.to_json())).collect());
        let sync = match &self.locks.sync {
            Some(l) => object([
                ("by", l.by.as_str().into()),
                ("owner", owner_json(l.owner)),
                ("since", l.since.into()),
                ("head", l.head.as_str().into()),
                ("tip", l.tip.as_str().into()),
                ("paths", str_arr(&l.paths)),
                ("index_tmp", l.index_tmp.clone().into()),
                ("lock_ino", l.lock_ino.into()),
            ]),
            None => Json::Null,
        };
        let locks = object([("git", oplock_json(&self.locks.git)), ("pkg", oplock_json(&self.locks.pkg)), ("sync", sync)]);
        let orphans = Json::Obj(
            self.orphans.values().map(|o| (o.path.clone(), object([("from", o.from.as_str().into()), ("at", o.at.into())]))).collect(),
        );
        object([
            ("v", VERSION.into()),
            ("off", if self.off { Json::Bool(true) } else { Json::Null }),
            ("claims", claims),
            ("sessions", sessions),
            ("locks", locks),
            ("orphans", if self.orphans.is_empty() { Json::Null } else { orphans }),
            (
                "upstream",
                self.upstream
                    .as_ref()
                    .map(|u| object([("remote", u.remote.as_str().into()), ("branch", u.branch.as_str().into()), ("oid", u.oid.as_str().into())]))
                    .unwrap_or(Json::Null),
            ),
        ])
    }

    pub fn from_json(v: &Json, fold: bool) -> State {
        let mut st = State { fold, off: v.get("off").bool().unwrap_or(false), ..State::default() };
        for (path, c) in v.get("claims").members() {
            if let Some(c) = Claim::from_json(path, c) {
                st.put_claim(c);
            }
        }
        for (id, x) in v.get("sessions").members() {
            if let Some(x) = Session::from_json(id, x) {
                st.sessions.insert(id.clone(), x);
            }
        }
        let l = v.get("locks");
        st.locks.git = oplock_from(l.get("git"));
        st.locks.pkg = oplock_from(l.get("pkg"));
        let y = l.get("sync");
        st.locks.sync = (|| {
            Some(SyncLock {
                by: s(y, "by")?,
                owner: owner_from(y.get("owner")),
                since: n(y, "since"),
                head: s(y, "head")?,
                tip: s(y, "tip")?,
                paths: strs(y.get("paths")),
                index_tmp: s(y, "index_tmp"),
                lock_ino: y.get("lock_ino").u64(),
            })
        })();
        for (path, o) in v.get("orphans").members() {
            let k = st.key(path);
            st.orphans.insert(k, Orphan { path: path.clone(), from: s(o, "from").unwrap_or_default(), at: n(o, "at") });
        }
        let u = v.get("upstream");
        st.upstream = (|| Some(Upstream { remote: s(u, "remote")?, branch: s(u, "branch")?, oid: s(u, "oid")? }))();
        st
    }
}

// ── the store ─────────────────────────────────────────────────────────────

/// How long to wait for the file lock before giving up (fail open).
const LOCK_WAIT: Duration = Duration::from_secs(5);

fn lock_file(f: &File, exclusive: bool) -> io::Result<()> {
    let deadline = Instant::now() + LOCK_WAIT;
    let mut pause = Duration::from_micros(200);
    loop {
        let r = if exclusive { f.try_lock() } else { f.try_lock_shared() };
        match r {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "agent-lock.json is locked"));
                }
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// The state, the text it came from, and the file's format version.
fn load(f: &mut File, repo: &Repo, fold: bool) -> io::Result<(State, String, u64)> {
    let (mut st, text, ver) = load_raw(f, repo, fold)?;
    st.shared = crate::shared::Shared::load(&repo.top);
    Ok((st, text, ver))
}

fn load_raw(f: &mut File, repo: &Repo, fold: bool) -> io::Result<(State, String, u64)> {
    let mut raw = Vec::new();
    f.seek(SeekFrom::Start(0))?;
    f.read_to_end(&mut raw)?;
    let text = match String::from_utf8(raw) {
        Ok(t) => t,
        Err(e) => {
            // Not UTF-8: corrupt. Keep the evidence for `doctor`, start over.
            let _ = std::fs::write(repo.git_dir.join("agent-lock.json.bad"), e.as_bytes());
            return Ok((State { fold, ..State::default() }, String::new(), VERSION));
        }
    };
    if text.trim().is_empty() {
        return Ok((State { fold, ..State::default() }, String::new(), VERSION));
    }
    match json::parse_prefix(&text) {
        Ok((v, _)) if v.get("v").u64().is_some() => Ok((State::from_json(&v, fold), text, v.get("v").u64().unwrap_or(0))),
        _ => {
            // Keep the evidence for `doctor`, start over.
            let _ = std::fs::write(repo.git_dir.join("agent-lock.json.bad"), &text);
            Ok((State { fold, ..State::default() }, String::new(), VERSION))
        }
    }
}

/// Read-modify-write under an exclusive lock. `f` returns a value; the file
/// is rewritten only if the state changed. A file written by a newer
/// agent-lock is never rewritten (its fields would be lost): Err, `f` not run.
pub fn transact<R>(repo: &Repo, f: impl FnOnce(&mut State) -> R) -> io::Result<R> {
    let fold = repo.ignore_case();
    let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(repo.state_file())?;
    lock_file(&file, true)?;
    let (mut st, before, ver) = load(&mut file, repo, fold)?;
    if ver > VERSION {
        return Err(io::Error::other(format!("agent-lock.json is format v{ver}, newer than this agent-lock (v{VERSION})")));
    }
    let original = st.clone();
    let out = f(&mut st);
    if st != original || before.is_empty() {
        prune(&mut st, sys::now());
        let text = st.to_json().to_pretty();
        if text != before {
            file.seek(SeekFrom::Start(0))?;
            file.write_all(text.as_bytes())?; // one write
            file.set_len(text.len() as u64)?;
        }
    }
    Ok(out)
}

/// Consistent read under a shared lock. A missing file is an empty state.
pub fn read(repo: &Repo) -> io::Result<State> {
    let fold = repo.ignore_case();
    let mut file = match File::open(repo.state_file()) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(State { fold, shared: crate::shared::Shared::load(&repo.top), ..State::default() }),
        Err(e) => return Err(e),
    };
    lock_file(&file, false)?;
    Ok(load(&mut file, repo, fold)?.0)
}

/// The state as last written, read without the lock: for when the lock is
/// stuck (a frozen holder, a slow filesystem). A torn or unreadable file
/// gives None, and nothing is repaired here.
pub fn peek(repo: &Repo) -> Option<State> {
    let text = std::fs::read_to_string(repo.state_file()).ok()?;
    let (v, _) = json::parse_prefix(&text).ok()?;
    v.get("v").u64()?;
    Some(State { shared: crate::shared::Shared::load(&repo.top), ..State::from_json(&v, repo.ignore_case()) })
}

/// The same locked transaction for any small JSON file (the hooks' session
/// index in the plugin data dir). A missing or unreadable file starts as {}.
pub fn transact_json<R>(path: &std::path::Path, f: impl FnOnce(&mut Json) -> R) -> io::Result<R> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    lock_file(&file, true)?;
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    let mut v = match json::parse_prefix(&text) {
        Ok((v @ Json::Obj(_), _)) => v,
        _ => Json::obj(),
    };
    let before = v.clone();
    let out = f(&mut v);
    if v != before {
        let text = v.to_pretty();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(text.as_bytes())?;
        file.set_len(text.len() as u64)?;
    }
    Ok(out)
}

const DAY: u64 = 24 * 3600;

/// Drop what nobody needs any more, so the file stays a few KB.
pub fn prune(st: &mut State, now: u64) {
    let referenced: std::collections::BTreeSet<String> = st.claims.values().filter_map(|c| c.session.clone()).collect();
    st.sessions.retain(|id, s| {
        if referenced.contains(id) {
            return true;
        }
        let dead = s.owner.map(|o| sys::liveness(&o) == sys::Live::Dead).unwrap_or(true);
        match s.phase {
            Phase::Ended => now.saturating_sub(s.seen) < DAY && !dead,
            _ => !(dead && now.saturating_sub(s.seen) > 3600) && now.saturating_sub(s.seen) < 30 * DAY,
        }
    });
    for s in st.sessions.values_mut() {
        s.wants.retain(|w| now.saturating_sub(w.at) < 1800);
        s.bash.retain(|_, b| now.saturating_sub(b.at) < 3600);
        s.blocked.retain(|_, b| now.saturating_sub(b.at) < 3600);
        if s.takeover.as_ref().is_some_and(|t| now.saturating_sub(t.at) > 600) {
            s.takeover = None;
        }
    }
    st.orphans.retain(|_, o| now.saturating_sub(o.at) < 7 * DAY);
    let gone = |o: &Option<Owner>| o.map(|o| sys::liveness(&o) == sys::Live::Dead).unwrap_or(false);
    if st.locks.git.as_ref().is_some_and(|l| gone(&l.owner)) {
        st.locks.git = None;
    }
    if st.locks.pkg.as_ref().is_some_and(|l| gone(&l.owner)) {
        st.locks.pkg = None;
    }
    // A sync lock with a dead owner is kept: the next git operation recovers it.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo() -> Repo {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!("al-state-{}-{}-{n}", sys::pid(), sys::now_ms()));
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::write(base.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let top = std::fs::canonicalize(&base).unwrap();
        Repo { git_dir: top.join(".git"), top }
    }

    #[test]
    fn round_trip_through_the_file() {
        let repo = temp_repo();
        transact(&repo, |st| {
            st.put_claim(Claim { path: "src/a.ts".into(), by: "aaaaaaaa".into(), at: 1, touched: 2, edits: 3, ..Claim::default() });
            st.sessions.insert(
                "aaaaaaaa-1".into(),
                Session { id: "aaaaaaaa-1".into(), agent: "aaaaaaaa".into(), seen: sys::now(), owner: sys::self_owner(), ..Session::default() },
            );
        })
        .unwrap();
        let st = read(&repo).unwrap();
        assert_eq!(st.claim("src/a.ts").unwrap().edits, 3);
        assert_eq!(st.sessions.len(), 1);
        let text = std::fs::read_to_string(repo.state_file()).unwrap();
        assert!(text.contains("\"src/a.ts\""));
    }

    #[test]
    fn trailing_garbage_and_corruption() {
        let repo = temp_repo();
        transact(&repo, |st| st.off = true).unwrap();
        let mut text = std::fs::read_to_string(repo.state_file()).unwrap();
        text.push_str("\n  \"leftover\": 1 }");
        std::fs::write(repo.state_file(), &text).unwrap();
        assert!(read(&repo).unwrap().off, "first value wins");
        std::fs::write(repo.state_file(), "{not json").unwrap();
        assert_eq!(read(&repo).unwrap(), State::default());
        assert!(repo.git_dir.join("agent-lock.json.bad").exists());
    }

    #[test]
    fn a_file_that_is_not_utf8_is_kept_as_evidence() {
        let repo = temp_repo();
        std::fs::write(repo.state_file(), b"{\"v\":3,\"claims\":{\"caf\xe9\":{}}}").unwrap();
        assert_eq!(read(&repo).unwrap(), State::default());
        let bad = std::fs::read(repo.git_dir.join("agent-lock.json.bad")).unwrap();
        assert!(bad.contains(&0xe9), "raw bytes kept for doctor");
    }

    #[test]
    fn concurrent_processes_lose_no_update() {
        // N threads stand in for processes: each transaction is a fresh open + flock.
        // Every transaction is read-increment-write of one counter, so any
        // overlap would lose an increment.
        let repo = temp_repo();
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let repo = repo.clone();
                std::thread::spawn(move || {
                    for i in 0..250 {
                        transact(&repo, |st| {
                            let n = st.claim("counter").map(|c| c.edits).unwrap_or(0);
                            st.put_claim(Claim { path: "counter".into(), by: format!("t{t}"), edits: n + 1, ..Claim::default() });
                            // churn a few other entries so the file length changes
                            if i % 3 == 0 {
                                st.put_claim(Claim { path: format!("tmp{t}"), by: "x".into(), ..Claim::default() });
                            } else {
                                st.take_claim(&format!("tmp{t}"));
                            }
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(read(&repo).unwrap().claim("counter").unwrap().edits, 2000);
    }

    #[test]
    fn a_newer_file_is_read_but_never_rewritten() {
        // hooks/H6: an older binary must not drop fields it does not know
        let repo = temp_repo();
        let text = r#"{"v": 99, "claims": {"a.ts": {"by": "aaaaaaaa", "edits": 1}}, "future": {"x": 1}}"#;
        std::fs::write(repo.state_file(), text).unwrap();
        assert_eq!(read(&repo).unwrap().claim("a.ts").unwrap().edits, 1);
        let mut ran = false;
        assert!(transact(&repo, |st| {
            ran = true;
            st.off = true;
        })
        .is_err());
        assert!(!ran);
        assert_eq!(std::fs::read_to_string(repo.state_file()).unwrap(), text);
    }

    #[test]
    fn older_files_still_load() {
        // v3.0 wrote OpLock.bg and bare "pkg" markers
        let repo = temp_repo();
        let old = r#"{"v":3,"claims":{"p.json":{"by":"a","pending":"pkg"}},"locks":{"pkg":{"by":"a","since":5,"what":"npm i","bg":"t1"}}}"#;
        std::fs::write(repo.state_file(), old).unwrap();
        let st = read(&repo).unwrap();
        assert_eq!(st.locks.pkg.as_ref().unwrap().what, "npm i");
        assert_eq!(crate::rules::marker(st.claim("p.json").unwrap().pending.as_deref().unwrap()), crate::rules::Marker::Tool);
        transact(&repo, |st| st.off = true).unwrap();
        assert!(!std::fs::read_to_string(repo.state_file()).unwrap().contains("\"bg\""));
    }

    #[test]
    fn case_folding_keys() {
        let mut st = State { fold: true, ..State::default() };
        st.put_claim(Claim { path: "src/Logger.ts".into(), by: "a".into(), ..Claim::default() });
        assert!(st.claim("src/logger.ts").is_some());
        assert_eq!(st.claim("SRC/LOGGER.TS").unwrap().path, "src/Logger.ts");
    }
}
