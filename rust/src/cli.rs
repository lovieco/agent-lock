//! The CLI: `agent-lock <command>`. Agent commands act as the caller
//! (`--as` › $AGENT_LOCK_AGENT_ID › $CLAUDE_CODE_SESSION_ID). Every state
//! change is one transaction, and none is held while git runs.
//!
//! Exit codes: 0 ok, 1 refused or failed, 2 usage error, 3 deadlock (`wait`).

use crate::json::{self, Json, object};
use crate::rules::{self, Ask, Conflict, Ctx, Got, Stale};
use crate::state::{self, Claim, Orphan, Phase, State};
use crate::sys::{self, Live, Owner, Repo, plural};
use crate::{git, ids, lockfiles, msg};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

const PLUGIN: &str = "agent-lock@lovieco";
const MARKET_REPO: &str = "lovieco/agent-lock";
const LEGACY_HOOK: &str = ".agent-lock/hooks/";
const WAIT: &str = "wait";
const WAIT_DEFAULT: f64 = 110.0;
const POLL: Duration = Duration::from_millis(250);
/// An index.lock or a held locks.git this old is suspicious.
const LOCK_OLD: u64 = 600;
/// A package install holding locks.pkg this long (the shims' wait limit) is suspicious.
const PKG_OLD: u64 = 20 * 60;
/// Temp files a killed commit or sync left in the git dir, swept after this.
const LITTER_AGE: u64 = 3600;

// ── plumbing ──────────────────────────────────────────────────────────────

struct Fail(i32, String);
type Res<T = i32> = Result<T, Fail>;

fn usage<T>(m: impl Into<String>) -> Res<T> {
    Err(Fail(2, m.into()))
}

fn fail<T>(m: impl Into<String>) -> Res<T> {
    Err(Fail(1, m.into()))
}

/// stdout, ignoring a closed pipe (`agent-lock mine | head -1`).
fn say(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.write_all(b"\n");
}

fn warn(s: &str) {
    let mut o = std::io::stderr().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.write_all(b"\n");
}

fn hook_entries(n: usize) -> String {
    format!("{n} legacy vendored hook {}", if n == 1 { "entry" } else { "entries" })
}

pub fn run(args: &[String]) -> i32 {
    std::panic::set_hook(Box::new(|i| eprintln!("agent-lock: internal error: {i}")));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (c, o) = parse(args)?;
        (c.run)(&o)
    }));
    let _ = std::io::stdout().flush();
    match r {
        Ok(Ok(code)) => code,
        Ok(Err(Fail(code, m))) => {
            if !m.is_empty() {
                warn(&format!("agent-lock: {m}"));
            }
            if code == 2 {
                warn("(see `agent-lock help`)");
            }
            code
        }
        Err(_) => 1,
    }
}

// ── commands and arguments ────────────────────────────────────────────────

struct Cmd {
    name: &'static str,
    args: &'static str,
    about: &'static str,
    values: &'static [&'static str],
    flags: &'static [&'static str],
    max_pos: Option<usize>,
    run: fn(&Opts) -> Res,
}

const CMDS: &[Cmd] = &[
    Cmd {
        name: "commit",
        args: "-m <msg> [-- <paths>]",
        about: "commit exactly your claimed files, or only <paths> (also -F <file|->, --push, --no-verify, --dry-run)",
        values: &["-m", "-F"],
        flags: &["--push", "--no-verify", "--dry-run"],
        max_pos: None,
        run: commit,
    },
    Cmd {
        name: "push",
        args: "[--accept-rewrite]",
        about: "publish HEAD to origin; syncs and retries when the remote moved",
        values: &[],
        flags: &["--accept-rewrite"],
        max_pos: Some(0),
        run: push,
    },
    Cmd {
        name: "sync",
        args: "[--accept-rewrite]",
        about: "bring in upstream commits without touching anyone's uncommitted work",
        values: &[],
        flags: &["--accept-rewrite"],
        max_pos: Some(0),
        run: sync,
    },
    Cmd {
        name: "wait",
        args: "<file>… [--claim] [--timeout <s>]",
        about: "wait until you could edit the files (--claim takes them); exit 0 free, 1 timeout, 3 deadlock",
        values: &["--timeout"],
        flags: &["--claim"],
        max_pos: None,
        run: wait,
    },
    Cmd {
        name: "release",
        args: "<file>… [--force] | --mine",
        about: "release your own or stale claims (--force: another agent's, with the user's approval)",
        values: &[],
        flags: &["--force", "--mine"],
        max_pos: None,
        run: release,
    },
    Cmd {
        name: "mine",
        args: "[-z]",
        about: "print the files you hold (-z: NUL-separated: `agent-lock mine -z | xargs -0 -r <tool>`)",
        values: &[],
        flags: &["-z"],
        max_pos: Some(0),
        run: mine,
    },
    Cmd {
        name: "status",
        args: "[dir] [--json]",
        about: "claims, sessions, locks and orphans of this checkout (--json: the state)",
        values: &[],
        flags: &["--json"],
        max_pos: Some(1),
        run: status,
    },
    Cmd { name: "off", args: "[dir]", about: "turn agent-lock off in this checkout", values: &[], flags: &[], max_pos: Some(1), run: off },
    Cmd { name: "on", args: "[dir]", about: "turn it back on", values: &[], flags: &[], max_pos: Some(1), run: on },
    Cmd {
        name: "doctor",
        args: "[dir] [--fix]",
        about: "report stale claims, interrupted syncs, a corrupt state file, setup problems (--fix repairs)",
        values: &[],
        flags: &["--fix"],
        max_pos: Some(1),
        run: doctor,
    },
    Cmd {
        name: "install",
        args: "[dir] [--force]",
        about: "enable the plugin for a project in <dir>/.claude/settings.json",
        values: &[],
        flags: &["--force"],
        max_pos: Some(1),
        run: install,
    },
    Cmd {
        name: "uninstall",
        args: "[dir] [--force]",
        about: "remove it from <dir>/.claude/settings.json",
        values: &[],
        flags: &["--force"],
        max_pos: Some(1),
        run: uninstall,
    },
    Cmd {
        name: "selftest",
        args: "[--keep]",
        about: "run the real hooks, shims and CLI in a throwaway repo; PASS/FAIL per step",
        values: &[],
        flags: &["--keep"],
        max_pos: Some(0),
        run: selftest,
    },
    Cmd { name: "version", args: "", about: "print the version", values: &[], flags: &[], max_pos: Some(0), run: version },
    Cmd { name: "help", args: "", about: "this text", values: &[], flags: &[], max_pos: None, run: help },
];

const ALIAS: &[(&str, &str)] = &[("--message", "-m"), ("--file", "-F"), ("--null", "-z")];

/// v2 commands v3 folded into others.
const RENAMED: &[(&str, &str)] = &[
    ("release-mine", "v3 spells it `agent-lock release --mine`"),
    ("purge-all", "v3 folds it into `agent-lock doctor --fix`"),
    ("list", "v3 shows claims with `agent-lock status`"),
    ("watch", "v3 shows claims with `agent-lock status`"),
    ("test", "v3 calls it `agent-lock selftest`"),
    ("hold", "v3 has no hold: the end-of-turn reminder asks only once per file"),
];

#[derive(Default, Debug)]
struct Opts {
    as_id: Option<String>,
    dirs: Vec<String>, // -C, chained like git's
    flags: BTreeSet<String>,
    vals: BTreeMap<String, Vec<String>>,
    pos: Vec<String>,
}

impl Opts {
    fn has(&self, f: &str) -> bool {
        self.flags.contains(f)
    }
    fn all(&self, k: &str) -> &[String] {
        self.vals.get(k).map(Vec::as_slice).unwrap_or(&[])
    }
    fn last(&self, k: &str) -> Option<&str> {
        self.all(k).last().map(String::as_str)
    }
}

fn find(name: &str) -> Option<&'static Cmd> {
    CMDS.iter().find(|c| c.name == name)
}

/// `--as` and `-C` are accepted anywhere before `--` (the Bash hook inserts
/// `--as` right after the subcommand).
fn global(o: &mut Opts, a: &[String], i: usize) -> Res<usize> {
    let t = a[i].as_str();
    if t == "--as" || t == "-C" {
        let Some(v) = a.get(i + 1) else { return usage(format!("{t} needs a value")) };
        if t == "--as" {
            o.as_id = Some(v.clone());
        } else {
            o.dirs.push(v.clone());
        }
        return Ok(2);
    }
    if let Some(v) = t.strip_prefix("--as=") {
        o.as_id = Some(v.to_string());
        return Ok(1);
    }
    if let Some(v) = t.strip_prefix("-C").filter(|v| !v.is_empty()) {
        o.dirs.push(v.to_string());
        return Ok(1);
    }
    Ok(0)
}

fn parse(args: &[String]) -> Res<(&'static Cmd, Opts)> {
    let help = find("help").expect("help");
    let mut o = Opts::default();
    let mut i = 0;
    let mut name = None;
    while i < args.len() {
        let n = global(&mut o, args, i)?;
        if n > 0 {
            i += n;
            continue;
        }
        let t = args[i].as_str();
        if t == "-h" || t == "--help" {
            return Ok((help, o));
        }
        if t.len() > 1 && t.starts_with('-') {
            return usage(format!("unknown option {t}"));
        }
        name = Some(t);
        i += 1;
        break;
    }
    let Some(name) = name else { return Ok((help, o)) };
    let Some(c) = find(name) else {
        return match RENAMED.iter().find(|(old, _)| *old == name) {
            Some((_, hint)) => usage(format!("unknown command: {name}; {hint}")),
            None => usage(format!("unknown command: {name}")),
        };
    };
    while i < args.len() {
        let t = args[i].as_str();
        if t == "--" {
            o.pos.extend(args[i + 1..].iter().cloned());
            break;
        }
        let n = global(&mut o, args, i)?;
        if n > 0 {
            i += n;
            continue;
        }
        if t == "-h" || t == "--help" {
            return Ok((help, o));
        }
        if t.len() > 1 && t.starts_with('-') {
            let (raw, inline) = match t.split_once('=') {
                Some((n, v)) if t.starts_with("--") => (n, Some(v)),
                _ => (t, None),
            };
            let key = ALIAS.iter().find(|(a, _)| *a == raw).map(|x| x.1).unwrap_or(raw);
            let short = t.get(..2).filter(|s| !t.starts_with("--") && t.len() > 2 && c.values.contains(s));
            if c.values.contains(&key) {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        match args.get(i) {
                            Some(v) => v.clone(),
                            None => return usage(format!("{}: {t} needs a value", c.name)),
                        }
                    }
                };
                o.vals.entry(key.to_string()).or_default().push(v);
            } else if let Some(s) = short {
                o.vals.entry(s.to_string()).or_default().push(t[2..].to_string()); // -m<msg>
            } else if c.flags.contains(&key) && inline.is_none() {
                o.flags.insert(key.to_string());
            } else {
                return usage(format!("{}: unknown option {t}", c.name));
            }
            i += 1;
            continue;
        }
        o.pos.push(t.to_string());
        i += 1;
    }
    if let Some(m) = c.max_pos
        && o.pos.len() > m
    {
        return usage(format!("{}: unexpected argument {}", c.name, o.pos[m]));
    }
    Ok((c, o))
}

// ── context: directories, repo, caller ────────────────────────────────────

fn base_dir(o: &Opts, arg: Option<&String>) -> Res<PathBuf> {
    let mut p = sys::cwd();
    for d in o.dirs.iter().chain(arg) {
        p = p.join(d);
    }
    match std::fs::canonicalize(&p) {
        Ok(c) if c.is_dir() => Ok(c),
        _ => usage(format!("not a directory: {}", p.display())),
    }
}

fn need_repo(base: &Path) -> Res<Repo> {
    match sys::find_repo(base) {
        Some(r) => Ok(r),
        None => fail(format!("not inside a git worktree: {}", base.display())),
    }
}

fn read_state(repo: &Repo) -> Res<State> {
    state::read(repo).or_else(|e| fail(format!("cannot read {}: {e}", repo.state_file().display())))
}

fn transact<R>(repo: &Repo, f: impl FnOnce(&mut State) -> R) -> Res<R> {
    state::transact(repo, f).or_else(|e| fail(format!("cannot update {}: {e}", repo.state_file().display())))
}

fn caller(o: &Opts, required: bool) -> Res<Option<String>> {
    if let Some(a) = &o.as_id {
        let id = ids::normalize(a);
        if id.is_empty() {
            return usage("--as is empty");
        }
        return Ok(Some(id));
    }
    match ids::caller(None) {
        Some(id) => Ok(Some(id)),
        None if required => usage(
            "cannot tell which agent you are. Pass --as <id> (subagents: the agent id from your instructions), run inside Claude Code (CLAUDE_CODE_SESSION_ID), or set AGENT_LOCK_AGENT_ID.",
        ),
        None => Ok(None),
    }
}

fn me_required(o: &Opts) -> Res<String> {
    Ok(caller(o, true)?.unwrap_or_default())
}

/// The caller's session: this Claude Code session when it is the caller's, else the newest recorded one.
fn session_id(st: &State, me: &str) -> Option<String> {
    if let Ok(s) = std::env::var("CLAUDE_CODE_SESSION_ID")
        && !s.is_empty()
        && ids::sid8(&s) == ids::family(me)
    {
        return Some(s);
    }
    st.session_of(me).map(|s| s.id.clone())
}

fn owner_of(st: &State, me: &str) -> Option<Owner> {
    sys::claude_owner().or_else(|| st.session_of(me).and_then(|s| s.owner))
}

fn ask(st: &State, me: &str, pending: Option<&str>, reason: Option<&str>) -> Ask {
    Ask {
        agent: me.to_string(),
        session: session_id(st, me),
        owner: owner_of(st, me),
        pending: pending.map(str::to_string),
        made_by: pending.map(str::to_string),
        reason: reason.map(str::to_string),
    }
}

/// ` --as <id>` for a subagent: its environment says it is the parent.
fn as_opt(me: Option<&str>) -> String {
    me.map(msg::as_flag).unwrap_or_default()
}

/// ` -C <top>` when the command was run with -C (the shell is elsewhere).
fn c_opt(o: &Opts, repo: &Repo) -> String {
    if o.dirs.is_empty() { String::new() } else { format!(" -C {}", msg::qp(&repo.top)) }
}

fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

/// What a claim's process marker is working on (`rules::marker`): "commit", "pkg", or None (a tool call).
fn marker_kind(c: &Claim) -> Option<&str> {
    let p = c.pending.as_deref()?;
    matches!(rules::marker(p), rules::Marker::Process(_)).then(|| p.split(':').next().unwrap_or(""))
}

/// "bbbbbbbb (process alive, working; 2 edits uncommitted; claimed 5m ago, last active 1m ago)"
fn holder_line(st: &State, c: &Claim, ctx: &Ctx) -> String {
    let mut bits = vec![
        match rules::owner_for(st, c).map(|o| (ctx.live)(&o)) {
            Some(Live::Alive) => "process alive",
            Some(Live::Dead) => "process gone",
            None => "no process recorded",
        }
        .to_string(),
    ];
    if let Some(s) = rules::session_for(st, c) {
        bits.push(
            match s.phase {
                Phase::Active => "working",
                Phase::Idle => "idle",
                Phase::Ended => "session ended",
            }
            .into(),
        );
    }
    let edits = if c.edits > 0 { format!("{} uncommitted", plural(c.edits as usize, "edit")) } else { "no edits yet".into() };
    let pend = match (c.pending.is_some(), marker_kind(c)) {
        (false, _) => "",
        (_, Some("commit")) => ", committing now",
        (_, Some("pkg")) => ", package install running",
        _ => ", edit in flight",
    };
    format!(
        "{} ({}; {edits}{pend}; claimed {} ago, last active {} ago)",
        c.by,
        bits.join(", "),
        msg::ago(ctx.now.saturating_sub(c.at)),
        msg::ago(ctx.now.saturating_sub(rules::last_active(st, c)))
    )
}

/// The subset of `paths` that differs from HEAD, as state keys.
fn dirty_keys(repo: &Repo, st: &State, paths: &[String]) -> Res<BTreeSet<String>> {
    let mut seen = BTreeSet::new();
    let uniq: Vec<String> = paths.iter().filter(|p| seen.insert(st.key(p))).cloned().collect();
    match git::dirty(repo, &uniq) {
        Ok(d) => Ok(d.iter().map(|p| st.key(p)).collect()),
        Err(e) => fail(e.to_string()),
    }
}

// ── commit markers ────────────────────────────────────────────────────────

/// The pending marker this `agent-lock commit` puts on its claims: it blocks
/// hand-overs, and tells a later command whether the commit still runs.
fn commit_marker() -> String {
    rules::proc_marker("commit", sys::self_owner().unwrap_or(Owner { pid: sys::pid(), start: 0 }))
}

/// Clear markers whose process is gone: a killed `agent-lock commit`, or a
/// package install that no longer runs. Returns their paths. (Tool-call
/// markers are the hooks' business.)
fn unstick(st: &mut State) -> Vec<String> {
    let mut out = Vec::new();
    for c in st.claims.values_mut().filter(|c| c.pending.as_deref().is_some_and(rules::marker_dead)) {
        c.pending = None;
        out.push(c.path.clone());
    }
    out
}

struct Mark {
    path: String,
    created: bool,
}

/// Claims marked for one commit. Dropped unsettled (an error, a panic), it
/// clears the markers and undoes the claims it created.
struct Marked<'a> {
    repo: &'a Repo,
    me: &'a str,
    marker: String,
    marks: Vec<Mark>,
    armed: bool,
}

impl Marked<'_> {
    fn paths(&self) -> Vec<String> {
        self.marks.iter().map(|m| m.path.clone()).collect()
    }

    /// After the commit: release what git confirms clean (`clean`, None when
    /// unknown), keep the rest (markers cleared), undo created claims unless
    /// something landed. Returns (released, kept).
    fn settle(&mut self, clean: Option<&BTreeSet<String>>, landed: bool) -> (Vec<String>, Vec<String>) {
        self.armed = false;
        let (me, marker) = (self.me, self.marker.as_str());
        let marks = &self.marks;
        state::transact(self.repo, |st| {
            let (mut released, mut kept) = (Vec::new(), Vec::new());
            for m in marks {
                let Some(c) = st.claim(&m.path) else { continue };
                if !c.holds(me) {
                    continue;
                }
                let clean = clean.is_some_and(|d| d.contains(&m.path));
                if c.pending.as_deref() != Some(marker) && !(landed && clean && c.pending.is_none()) {
                    // an own edit replaced the marker meanwhile and is in flight or uncommitted: the claim is that edit's
                    kept.push(m.path.clone());
                } else if clean && (landed || !m.created) {
                    rules::committed(st, &m.path, me);
                    released.push(m.path.clone());
                } else if m.created && !landed {
                    st.take_claim(&m.path);
                } else if let Some(c) = st.claim_mut(&m.path) {
                    c.pending = None;
                    if landed {
                        c.edits = c.edits.max(1); // changed again during the commit: uncommitted work
                        kept.push(m.path.clone());
                    }
                }
            }
            (released, kept)
        })
        .unwrap_or_default()
    }
}

impl Drop for Marked<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.settle(None, false);
        }
    }
}

// SIGHUP/SIGINT/SIGTERM wait while claims carry our marker, so an interrupt
// cannot strand them; the command then exits 128+n. Only SIGKILL can cut in
// (its marker is cleared by the next command: `unstick`).
const DEFERRED: [i32; 3] = [1, 2, 15];
static CAUGHT: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: i32) {
    CAUGHT.store(sig, Ordering::SeqCst);
}

struct Defer([usize; 3]);

impl Defer {
    fn new() -> Defer {
        let mut prev = [0usize; 3];
        for (i, s) in DEFERRED.iter().enumerate() {
            let p = sys::signal_catch(*s, on_signal);
            if p == sys::SIG_IGN {
                sys::signal_ignore(*s); // ignored before (nohup): stays ignored
            }
            prev[i] = if p == usize::MAX { 0 } else { p };
        }
        Defer(prev)
    }

    fn caught() -> Option<i32> {
        match CAUGHT.load(Ordering::SeqCst) {
            0 => None,
            s => Some(s),
        }
    }
}

impl Drop for Defer {
    fn drop(&mut self) {
        for (i, s) in DEFERRED.iter().enumerate() {
            sys::signal_restore(*s, self.0[i]);
        }
    }
}

// ── commit ────────────────────────────────────────────────────────────────

fn commit_message(o: &Opts, base: &Path) -> Res<Option<String>> {
    let (ms, fs) = (o.all("-m"), o.all("-F"));
    if !ms.is_empty() && !fs.is_empty() {
        return usage("commit: use -m or -F, not both");
    }
    if fs.len() > 1 {
        return usage("commit: -F given more than once");
    }
    let msg = match fs.first() {
        Some(f) if f == "-" => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).map_err(|e| Fail(2, format!("commit: cannot read the message from stdin: {e}")))?;
            s
        }
        Some(f) => std::fs::read_to_string(base.join(f)).map_err(|e| Fail(2, format!("commit: cannot read message file {f}: {e}")))?,
        None => ms.join("\n\n"),
    };
    Ok(if msg.trim().is_empty() { None } else { Some(msg) })
}

#[derive(Clone, Debug, PartialEq)]
enum Act {
    Own,
    Claim,
    Transfer(String),
    Join(Vec<String>), // a shared file: its other holders' changes go in too
}

#[derive(Clone, Debug)]
struct Item {
    rel: String,
    act: Act,
    note: Option<String>,
}

impl Item {
    fn annot(&self) -> String {
        match (&self.note, &self.act) {
            (Some(n), Act::Join(w)) => format!(" ({n}; shared, with changes by {})", w.join(", ")),
            (Some(n), _) => format!(" ({n})"),
            (None, Act::Transfer(f)) => format!(" (taken over from {f})"),
            (None, Act::Claim) => " (was unclaimed)".into(),
            (None, Act::Join(w)) => format!(" (shared; with changes by {})", w.join(", ")),
            (None, Act::Own) => String::new(),
        }
    }
}

/// The commit's file set, decided against one consistent read of the state.
struct Plan<'a> {
    st: &'a State,
    me: &'a str,
    ctx: &'a Ctx<'a>,
    repo: &'a Repo,
    items: Vec<Item>,
    seen: BTreeSet<String>,
    errors: Vec<String>,
}

impl Plan<'_> {
    /// Own, unclaimed (claimed at commit time) or a finished same-family
    /// subagent's (taken over); anyone else's is an error.
    fn consider(&mut self, rel: &str, note: Option<String>, paired: Option<&str>) {
        let st = self.st;
        if !self.seen.insert(st.key(rel)) {
            return;
        }
        let (rel, act) = match st.claim(rel) {
            None => (rel.to_string(), Act::Claim),
            Some(c) if c.holds(self.me) && c.with.is_empty() => (c.path.clone(), Act::Own),
            Some(c) if c.holds(self.me) || rules::conflict(st, c, self.me) == Conflict::Join => {
                (c.path.clone(), Act::Join(c.holders().filter(|h| *h != self.me).map(str::to_string).collect()))
            }
            Some(c) if ids::is_sub(&c.by) && rules::conflict(st, c, self.me) == Conflict::Transfer => {
                (c.path.clone(), Act::Transfer(c.by.clone()))
            }
            Some(c) => {
                let what = paired.map(|m| format!(" (lockfile of {m}, which must be committed with it)")).unwrap_or_default();
                let hint = match rules::stale(st, c, self.ctx, owner_of(st, self.me)) {
                    Some(why) => format!(
                        "; it looks abandoned ({}): `agent-lock release{} {}` frees it",
                        why.says(),
                        as_opt(Some(self.me)),
                        msg::qp(&self.repo.abs(&c.path))
                    ),
                    None => String::new(),
                };
                self.errors.push(format!("{rel}{what} is held by {}{hint}", holder_line(st, c, self.ctx)));
                return;
            }
        };
        self.items.push(Item { rel, act, note });
    }
}

fn commit(o: &Opts) -> Res {
    let base = base_dir(o, None)?;
    let repo = need_repo(&base)?;
    let me = me_required(o)?;
    let dry = o.has("--dry-run");
    let message = commit_message(o, &base)?;
    if !dry && message.is_none() {
        return usage("commit needs a message: -m \"<msg>\" (repeatable) or -F <file|->");
    }
    let message = message.unwrap_or_default();
    let st = read_state(&repo)?;
    let ctx = Ctx::real(sys::now());
    let mut p = Plan { st: &st, me: &me, ctx: &ctx, repo: &repo, items: Vec::new(), seen: BTreeSet::new(), errors: Vec::new() };

    // 1. the file set: own claims (a subagent's inherited ones aside), or exactly the named paths
    let mut inherited: Vec<Claim> = Vec::new();
    if o.pos.is_empty() {
        for c in st.claims.values().filter(|c| c.holds(&me)) {
            if c.inherited.is_some() {
                inherited.push(c.clone());
            } else {
                p.consider(&c.path, None, None);
            }
        }
    } else {
        for raw in &o.pos {
            let abs = sys::canonical(Path::new(raw), &base);
            if abs.starts_with(&repo.git_dir) || !abs.starts_with(&repo.top) {
                return usage(format!("commit: {raw} is not a file in this worktree ({})", repo.top.display()));
            }
            match repo.rel(&abs) {
                Some(rel) if is_link(&abs) || !abs.is_dir() => p.consider(&sys::git_spelling(&repo, &rel), None, None),
                under => {
                    let pre = under.map(|r| format!("{}/", st.key(&r)));
                    let mine: Vec<String> = st
                        .claims
                        .values()
                        .filter(|c| c.holds(&me) && pre.as_ref().is_none_or(|x| st.key(&c.path).starts_with(x.as_str())))
                        .map(|c| c.path.clone())
                        .collect();
                    if mine.is_empty() {
                        p.errors.push(format!("{raw}: you hold no files under this directory (name the files to commit unclaimed ones)"));
                    }
                    for m in mine {
                        p.consider(&m, None, None);
                    }
                }
            }
        }
    }
    // symlinks (an unchanged link stands for the file it points to), lockfiles
    // and inherited claims: all decided by the one git status below
    let mut links: Vec<(String, String)> = Vec::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for it in &p.items {
        let abs = repo.abs(&it.rel);
        if is_link(&abs)
            && let Ok(t) = std::fs::canonicalize(&abs)
            && t.is_file()
            && !t.starts_with(&repo.git_dir)
            && let Some(tr) = repo.rel(&t)
        {
            links.push((it.rel.clone(), tr));
        }
        for lf in lockfiles::lockfiles_for(&abs, &repo.top, None) {
            if let Some(r) = repo.rel(&lf) {
                pairs.push((r, it.rel.clone()));
            }
        }
    }
    let mut probe: Vec<String> = p.items.iter().map(|i| i.rel.clone()).collect();
    probe.extend(inherited.iter().map(|c| c.path.clone()));
    probe.extend(links.iter().map(|l| l.1.clone()));
    probe.extend(pairs.iter().map(|x| x.0.clone()));
    let dirty = dirty_keys(&repo, &st, &probe)?;
    let is_dirty = |rel: &str| dirty.contains(&st.key(rel));
    let mut left: Vec<Claim> = Vec::new();
    for c in inherited {
        if is_dirty(&c.path) {
            left.push(c); // the parent's uncommitted edits stay the parent's
        } else {
            p.consider(&c.path, None, None);
        }
    }
    for (l, t) in links {
        if !is_dirty(&l) && is_dirty(&t) {
            p.consider(&t, Some(format!("via symlink {l}")), None);
        }
    }
    for (lf, m) in pairs {
        if is_dirty(&m) && is_dirty(&lf) {
            p.consider(&lf, Some(format!("lockfile of {m}")), Some(&m));
        }
    }
    if !p.errors.is_empty() {
        return fail(format!(
            "nothing was committed:\n  {}\nCommit only your own files: another agent's uncommitted work stays theirs until they commit it.",
            p.errors.join("\n  ")
        ));
    }

    // 2. only paths with changes vs HEAD; clean own claims are released
    let (work, unchanged): (Vec<Item>, Vec<Item>) = p.items.into_iter().partition(|it| is_dirty(&it.rel));
    let released_clean = if dry { 0 } else { release_clean(&repo, &st, &me, &unchanged)? };
    let not_committed: Vec<String> = unchanged
        .iter()
        .map(|it| format!("{} ({})", it.rel, if std::fs::symlink_metadata(repo.abs(&it.rel)).is_ok() { "unchanged" } else { "no such file" }))
        .collect();
    let refuse = rules::others(&st, &me, &ctx).iter().any(|c| c.edits > 0 || c.pending.is_some());
    let rels: Vec<String> = work.iter().map(|i| i.rel.clone()).collect();

    if work.is_empty() {
        let mut l = vec![
            if !unchanged.is_empty() {
                "nothing to commit: your files have no changes vs HEAD"
            } else if !left.is_empty() {
                "nothing to commit of your own"
            } else {
                "nothing to commit: you hold no claimed files in this checkout"
            }
            .to_string(),
        ];
        if !not_committed.is_empty() {
            l.push(format!("  {}", not_committed.join("\n  ")));
        }
        if released_clean > 0 {
            l.push(format!("released {}", plural(released_clean, "clean claim")));
        }
        l.extend(inherited_notes(&left));
        l.extend(context_notes(&repo, &me));
        say(&l.join("\n"));
        return if o.has("--push") && !dry { publish(o, &repo, Some(&me), "push") } else { Ok(0) };
    }
    let annot: BTreeMap<String, String> = work.iter().map(|i| (st.key(&i.rel), i.annot())).collect();
    let note_of = |p: &str| annot.get(&st.key(p)).cloned().unwrap_or_default();

    if dry {
        let out = git::commit_paths(&repo, &me, &rels, &message, o.has("--no-verify"), true, refuse);
        if !out.ok {
            return fail(commit_failure(&out, true));
        }
        let mut l = vec![format!("dry run: would commit {}:", plural(out.committed.len(), "file"))];
        l.extend(out.committed.iter().map(|p| format!("  {p}{}", note_of(p))));
        let mut skipped = not_committed;
        skipped.extend(out.skipped.iter().map(|s| format!("{s} (unchanged or ignored)")));
        if !skipped.is_empty() {
            l.push(format!("not committed:\n  {}", skipped.join("\n  ")));
        }
        l.extend(inherited_notes(&left));
        l.extend(context_notes(&repo, &me));
        say(&l.join("\n"));
        return Ok(0);
    }

    // 3. claim and mark (the marker blocks hand-overs), commit, settle
    let defer = Defer::new();
    let mut marked = mark(&repo, &me, &commit_marker(), &work)?;
    let out = git::commit_paths(&repo, &me, &rels, &message, o.has("--no-verify"), false, refuse);
    let landed = out.ok && out.code.is_none();
    let all = if out.ok { git::status(&repo, None).ok() } else { None };
    // git's word on what is clean: a path it just committed under this very
    // spelling and reports no change for now needs no second question
    let clean = all.as_ref().map(|all| {
        let (sure, ask): (Vec<String>, Vec<String>) = marked.paths().into_iter().partition(|p| out.committed.contains(p) && !all.contains_key(p));
        let mut c = git::clean(&repo, &ask);
        c.extend(sure);
        c
    });
    let (_released, kept) = marked.settle(clean.as_ref(), landed);
    drop(marked);
    drop(defer);
    let sig = Defer::caught();
    if !out.ok {
        return Err(Fail(sig.map(|s| 128 + s).unwrap_or(1), commit_failure(&out, false)));
    }

    // 4. report
    let mut l = Vec::new();
    if let Some(r) = &out.recovery {
        l.push(format!("note: {r}"));
    }
    if landed {
        let sha = out.sha.as_deref().unwrap_or("?");
        l.push(format!("committed {}: {}", &sha[..sha.len().min(12)], plural(out.committed.len(), "file")));
        l.extend(out.committed.iter().map(|p| format!("  {p}{}", note_of(p))));
    } else {
        l.push("nothing to commit: no changes vs HEAD".into());
    }
    let mut skipped = not_committed;
    skipped.extend(out.skipped.iter().map(|s| format!("{s} (unchanged or ignored)")));
    if !skipped.is_empty() {
        l.push(format!("not committed:\n  {}", skipped.join("\n  ")));
    }
    // committed, yet still dirty: changed again while the commit ran
    let done: BTreeSet<String> = out.committed.iter().map(|p| st.key(p)).collect();
    let dirty: BTreeSet<String> = all.iter().flat_map(|m| m.keys()).map(|k| st.key(k)).collect();
    let kept: Vec<&String> = kept.iter().filter(|p| done.contains(&st.key(p)) && dirty.contains(&st.key(p))).collect();
    if landed && !kept.is_empty() {
        l.push(format!(
            "still claimed by you (changed again during the commit):\n  {}",
            kept.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n  ")
        ));
    }
    if !out.hook_output.is_empty() {
        l.push(format!("hook output:\n  {}", out.hook_output.lines().take(40).collect::<Vec<_>>().join("\n  ")));
    }
    l.extend(inherited_notes(&left));
    l.extend(context_notes_with(&repo, &me, all.as_ref()));
    say(&l.join("\n"));
    if let Some(s) = sig {
        return Ok(128 + s);
    }
    if o.has("--push") { publish(o, &repo, Some(&me), "push") } else { Ok(0) }
}

/// Release own claims git confirms have no changes (unless a stash may hold them).
fn release_clean(repo: &Repo, st: &State, me: &str, unchanged: &[Item]) -> Res<usize> {
    let own: Vec<(String, u32, u64)> =
        unchanged.iter().filter(|it| it.act == Act::Own).filter_map(|it| st.claim(&it.rel).map(|c| (c.path.clone(), c.edits, c.touched))).collect();
    if own.is_empty() {
        return Ok(0);
    }
    let clean = git::clean(repo, &own.iter().map(|o| o.0.clone()).collect::<Vec<_>>());
    let stash = if clean.is_empty() { None } else { git::stash_time(repo) };
    transact(repo, |st| {
        let mut n = 0;
        for (p, e, t) in &own {
            let same = st.claim(p).is_some_and(|c| c.holds(me) && c.edits == *e && c.touched == *t && c.pending.is_none());
            if same && clean.contains(p) && !stash.is_some_and(|s| s >= *t) && rules::committed(st, p, me) {
                n += 1;
            }
        }
        n
    })
}

/// One transaction: claim unclaimed paths, take over finished subagents'
/// files, mark them all. Anything changed since the plan → nothing happens.
fn mark<'a>(repo: &'a Repo, me: &'a str, marker: &str, work: &[Item]) -> Res<Marked<'a>> {
    let now = sys::now();
    let r = transact(repo, |st| {
        let before = st.clone();
        let ctx = Ctx::real(now);
        unstick(st);
        let a = ask(st, me, Some(marker), None);
        let mut marks = Vec::new();
        for it in work {
            let prev = st.claim(&it.rel).cloned();
            let created = match &prev {
                Some(c) if c.holds(me) => false,
                Some(c) if rules::conflict(st, c, me) == Conflict::Join => {
                    let _ = rules::acquire(st, &it.rel, &a, &ctx);
                    false
                }
                Some(c) if ids::is_sub(&c.by) && rules::conflict(st, c, me) == Conflict::Transfer => {
                    let _ = rules::acquire(st, &it.rel, &a, &ctx);
                    false
                }
                Some(c) => {
                    let by = c.by.clone();
                    *st = before;
                    return Err(format!("nothing was committed: {} was just claimed by {by}; re-run", it.rel));
                }
                None => {
                    let _ = rules::acquire(st, &it.rel, &a, &ctx);
                    true
                }
            };
            let Some(c) = st.claim_mut(&it.rel) else {
                *st = before;
                return Err(format!("nothing was committed: could not claim {}; re-run", it.rel));
            };
            c.pending = Some(marker.into());
            c.touched = now;
            marks.push(Mark { path: c.path.clone(), created });
        }
        Ok(marks)
    })?;
    match r {
        Ok(marks) => Ok(Marked { repo, me, marker: marker.to_string(), marks, armed: true }),
        Err(m) => fail(m),
    }
}

fn commit_failure(out: &git::CommitOut, dry: bool) -> String {
    let code = out.code.unwrap_or("commit-failed");
    let mut l = vec![format!("commit {} ({code}): {}", if dry { "would fail" } else { "failed" }, out.message)];
    if let Some(r) = &out.recovery {
        l.push(format!("note: {r}"));
    }
    let list = |ps: &[String]| {
        let mut v: Vec<String> = ps.iter().take(20).map(|p| format!("  {p}")).collect();
        if ps.len() > 20 {
            v.push(format!("  ... and {} more", ps.len() - 20));
        }
        v
    };
    match code {
        "hook-clobbered" => {
            if !out.extra.is_empty() {
                l.push("files outside your commit that the hook changed (other agents' or unattributed uncommitted work):".into());
                l.extend(list(&out.extra));
            }
            if !out.hook_output.is_empty() {
                l.push(format!("hook output:\n  {}", out.hook_output.lines().take(40).collect::<Vec<_>>().join("\n  ")));
            }
            l.push("Nothing was committed; your claims are kept.".into());
        }
        "staged-untrack" => {
            l.extend(list(&out.extra));
            l.push("Nothing was committed; the removal stays staged and your claims are kept.".into());
        }
        "case-rename" => l.push("Nothing was committed; the rename stays staged and your claims are kept.".into()),
        "busy" => l.push("Nothing was committed; retry shortly.".into()),
        _ if !dry => l.push("Nothing was committed or staged; your claims are kept.".into()),
        _ => {}
    }
    l.join("\n")
}

fn inherited_notes(left: &[Claim]) -> Vec<String> {
    let mut by: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for c in left {
        by.entry(c.inherited.as_deref().unwrap_or("?")).or_default().push(&c.path);
    }
    by.into_iter()
        .map(|(from, mut rels)| {
            rels.sort();
            format!("inherited from {from}: left for your parent to commit (name them to include them):\n  {}", rels.join("\n  "))
        })
        .collect()
}

fn context_notes(repo: &Repo, me: &str) -> Vec<String> {
    let all = git::status(repo, None).ok();
    context_notes_with(repo, me, all.as_ref())
}

/// Informational lines after a commit: own subagents' files, other agents'
/// dirty files (a count), changes nobody claims. Never "re-run to include them".
fn context_notes_with(repo: &Repo, me: &str, all: Option<&BTreeMap<String, git::Entry>>) -> Vec<String> {
    let Ok(st) = state::read(repo) else { return Vec::new() };
    let mine_sub = |c: &Claim| !ids::is_sub(me) && ids::is_sub(&c.by) && ids::family(&c.by) == me;
    let mut out = Vec::new();
    let mut subs: BTreeMap<&str, usize> = BTreeMap::new();
    for c in st.claims.values().filter(|c| mine_sub(c)) {
        *subs.entry(&c.by).or_default() += 1;
    }
    for (id, n) in subs {
        out.push(format!(
            "held by your subagent {id}: {}, not included (it commits its own work; once it has finished you can name them with -- <paths>)",
            plural(n, "file")
        ));
    }
    let Some(all) = all else { return out };
    let dirty: BTreeSet<String> = all.keys().map(|k| st.key(k)).collect();
    let others = st.claims.values().filter(|c| !c.holds(me) && !mine_sub(c) && dirty.contains(&st.key(&c.path))).count();
    if others > 0 {
        out.push(format!("other agents have uncommitted changes in {} (not included)", plural(others, "file")));
    }
    let un: Vec<&String> = all.keys().filter(|k| st.claim(k).is_none()).collect();
    if !un.is_empty() {
        out.push(format!("unattributed changes (no agent claims them; not included): {}", plural(un.len(), "file")));
        out.extend(un.iter().take(10).map(|p| format!("  {p}")));
        if un.len() > 10 {
            out.push(format!("  ... and {} more", un.len() - 10));
        }
    }
    out
}

// ── push / sync ───────────────────────────────────────────────────────────

fn push(o: &Opts) -> Res {
    let repo = need_repo(&base_dir(o, None)?)?;
    let me = caller(o, false)?;
    publish(o, &repo, me.as_deref(), "push")
}

fn sync(o: &Opts) -> Res {
    let repo = need_repo(&base_dir(o, None)?)?;
    let me = caller(o, false)?;
    publish(o, &repo, me.as_deref(), "sync")
}

/// Run sync or push and report; the caller only shapes the advice.
fn publish(o: &Opts, repo: &Repo, me: Option<&str>, verb: &str) -> Res {
    let accept = o.has("--accept-rewrite");
    let agent = me.unwrap_or("agent-lock");
    let (ok, status, message, files, by, recovery) = if verb == "push" {
        let r = git::push(repo, agent, accept);
        (r.ok(), r.status, r.message.clone(), r.files.clone(), r.blocked_by.clone(), r.recovery.clone())
    } else {
        let r = git::sync(repo, agent, accept);
        (r.ok(), r.status, r.message.clone(), r.files.clone(), r.blocked_by.clone(), r.recovery.clone())
    };
    let mut l = Vec::new();
    if let Some(r) = recovery {
        l.push(format!("note: {r}"));
    }
    if !by.is_empty() {
        l.push(format!(
            "{verb}: blocked: upstream changed {} that {} claimed in this checkout (sync never overwrites uncommitted work); nothing was changed",
            plural(by.len(), "file"),
            if by.len() == 1 { "is" } else { "are" }
        ));
        l.extend(blocked_detail(o, repo, me, verb, &by));
    } else {
        l.push(format!("{verb}: {message}"));
        if !ok && !files.is_empty() {
            l.extend(files.iter().take(20).map(|p| format!("  {p}")));
            match status {
                "conflict" => l.push("Nothing was changed. Ask the user how to resolve it (or resolve it in a quiet checkout).".into()),
                "blocked" => l.push("Nothing was changed.".into()),
                _ => {}
            }
        }
    }
    if ok {
        say(&l.join("\n"));
        Ok(0)
    } else {
        warn(&l.join("\n"));
        Ok(1)
    }
}

/// Who blocks a sync, from the caller's side: its own claims (commit them
/// first), its subagents' (wait, or take them over), its parent's, others'.
fn blocked_detail(o: &Opts, repo: &Repo, me: Option<&str>, verb: &str, by: &[(String, String)]) -> Vec<String> {
    let st = state::read(repo).unwrap_or_default();
    let ctx = Ctx::real(sys::now());
    let a = format!("{}{}", as_opt(me), c_opt(o, repo));
    let parent = me.filter(|m| ids::is_sub(m)).map(ids::family);
    let (mut own, mut run, mut done, mut par, mut live, mut gone) = (vec![], vec![], vec![], vec![], vec![], vec![]);
    for (p, h) in by {
        let c = st.claim(p);
        if Some(h.as_str()) == me {
            own.push(p.clone());
        } else if parent == Some(h.as_str()) {
            par.push((p.clone(), h.clone()));
        } else if let Some(m) = me.filter(|m| !ids::is_sub(m) && ids::is_sub(h) && ids::family(h) == *m) {
            if c.is_some_and(|c| rules::conflict(&st, c, m) == Conflict::Transfer) {
                done.push((p.clone(), h.clone()));
            } else {
                run.push((p.clone(), h.clone()));
            }
        } else if c.is_some_and(|c| rules::stale(&st, c, &ctx, None).is_some()) {
            gone.push((p.clone(), h.clone()));
        } else {
            live.push((p.clone(), h.clone()));
        }
    }
    let dirty =
        if own.is_empty() { BTreeSet::new() } else { dirty_keys(repo, &st, &own).unwrap_or_else(|_| own.iter().map(|p| st.key(p)).collect()) };
    let (own_dirty, own_clean): (Vec<String>, Vec<String>) = own.into_iter().partition(|p| dirty.contains(&st.key(p)));
    let abs = |ps: &[String]| ps.iter().map(|p| msg::qp(&repo.abs(p))).collect::<Vec<_>>().join(" ");
    let firsts = |xs: &[(String, String)]| xs.iter().map(|x| x.0.clone()).collect::<Vec<_>>();
    let it = |n: usize| if n == 1 { "it" } else { "them" };
    let again = format!("then run `agent-lock {verb}{a}` again");
    let mut l = Vec::new();
    for p in &own_dirty {
        l.push(format!("  {p}: you have uncommitted changes in it, and upstream changed it too"));
    }
    for p in &own_clean {
        l.push(format!("  {p}: you hold a claim on it (no changes yet), and upstream changed it"));
    }
    for (p, h) in &run {
        l.push(format!("  {p}: being edited by your subagent {h}, which is still running"));
    }
    for (p, h) in &done {
        l.push(format!("  {p}: held by your finished subagent {h}"));
    }
    for (p, h) in &par {
        l.push(format!("  {p}: held by your parent agent {h}"));
    }
    for (p, h) in &live {
        l.push(format!("  {p}: being edited by {h}"));
    }
    for (p, h) in &gone {
        l.push(format!("  {p}: claimed by {h}, which looks abandoned but left uncommitted edits"));
    }
    if !own_dirty.is_empty() {
        l.push(format!(
            "Commit {} first: `agent-lock commit{a} -m \"<msg>\" -- {}`, {again} (if upstream changed the same lines, sync reports a conflict instead).",
            it(own_dirty.len()),
            abs(&own_dirty)
        ));
    }
    if !own_clean.is_empty() {
        l.push(format!("Finish and commit your change, or release the claim (`agent-lock release{a} {}`), {again}.", abs(&own_clean)));
    }
    if !run.is_empty() {
        l.push(format!("Wait for your subagent to finish (its files pass to you), commit them, {again}."));
    }
    if !done.is_empty() {
        let ps = firsts(&done);
        l.push(format!("Commit {} (`agent-lock commit{a} -m \"<msg>\" -- {}` takes {} over), {again}.", it(ps.len()), abs(&ps), it(ps.len())));
    }
    if !par.is_empty() {
        l.push(format!("Files your parent agent holds are its uncommitted work: leave the {verb} to your parent, or retry after it commits them."));
    }
    if let Some((p, _)) = live.first() {
        l.push(format!(
            "Upstream changed files other agents are still editing. They keep them until they commit; retry afterwards (`agent-lock wait{} {}` waits until it is free).",
            as_opt(me),
            msg::qp(&repo.abs(p))
        ));
    }
    if !gone.is_empty() {
        let ps = firsts(&gone);
        l.push(format!(
            "Ask the user, or take {} over (`agent-lock wait{} --claim {}` reports what you take on) and commit {}, {again}.",
            it(ps.len()),
            as_opt(me),
            abs(&ps),
            it(ps.len())
        ));
    }
    l
}

// ── wait ──────────────────────────────────────────────────────────────────

/// The claim that stops `me` from editing `rel` now (none: free, own,
/// transferable or stale).
fn blocker(st: &State, rel: &str, me: &str, ctx: &Ctx, mine: Option<Owner>) -> Option<Claim> {
    let c = st.claim(rel)?;
    if c.by == me || rules::conflict(st, c, me) != Conflict::Block || rules::stale(st, c, ctx, mine).is_some() {
        return None;
    }
    Some(c.clone())
}

fn clear_blocked(st: &mut State, me: &str, rels: &[String]) {
    let keys: BTreeSet<String> = rels.iter().map(|r| st.key(r)).collect();
    let fold = st.fold;
    if let Some(s) = st.session_of_mut(me)
        && s.blocked.get(me).is_some_and(|b| keys.contains(&if fold { b.path.to_lowercase() } else { b.path.clone() }))
    {
        s.blocked.remove(me);
    }
}

fn wait(o: &Opts) -> Res {
    if o.pos.is_empty() {
        return usage("wait needs a file: agent-lock wait <file>... [--claim] [--timeout <s>]");
    }
    let me = me_required(o)?;
    let timeout = match o.last("--timeout") {
        None => WAIT_DEFAULT,
        Some(t) => match t.parse::<f64>() {
            Ok(x) if x.is_finite() && x >= 0.0 => x,
            _ => return usage(format!("wait: --timeout needs seconds, got {t}")),
        },
    };
    let base = base_dir(o, None)?;
    let here = sys::find_repo(&base);
    let mut targets: Vec<(Repo, Vec<String>)> = Vec::new();
    for raw in &o.pos {
        let Some((repo, rel)) = sys::locate(Path::new(raw), &base) else {
            return usage(format!("wait: {raw} is not a file in a git worktree; agent-lock only coordinates files in git worktrees"));
        };
        let i = match targets.iter().position(|(r, _)| r.git_dir == repo.git_dir) {
            Some(i) => i,
            None => {
                targets.push((repo, Vec::new()));
                targets.len() - 1
            }
        };
        if !targets[i].1.contains(&rel) {
            targets[i].1.push(rel);
        }
    }
    let away = |repo: &Repo| if here.as_ref().is_some_and(|h| h.git_dir == repo.git_dir) { None } else { Some(repo.top.clone()) };
    let names = targets.iter().flat_map(|(_, rels)| rels.iter().cloned()).collect::<Vec<_>>().join(", ");
    let r = wait_for(o, &me, &targets, &names, timeout, &away);
    if !matches!(r, Ok(3)) {
        // no longer waiting: a stale record would read as a deadlock to the next waiter
        for (repo, rels) in &targets {
            if state::read(repo).is_ok_and(|st| st.session_of(&me).is_some_and(|s| s.blocked.contains_key(&me))) {
                let _ = state::transact(repo, |st| clear_blocked(st, &me, rels));
            }
        }
    }
    r
}

fn wait_for(o: &Opts, me: &str, targets: &[(Repo, Vec<String>)], names: &str, timeout: f64, away: &dyn Fn(&Repo) -> Option<PathBuf>) -> Res {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.min(1e9));
    let mut announced = false;
    let mut last: Vec<String> = Vec::new();
    loop {
        let now = sys::now();
        let ctx = Ctx::real(now);
        let mut blocked: Vec<(usize, String, String)> = Vec::new(); // repo, rel, holder
        last.clear();
        for (i, (repo, rels)) in targets.iter().enumerate() {
            let st = read_state(repo)?;
            let mine = owner_of(&st, me);
            for rel in rels {
                let Some(c) = blocker(&st, rel, me, &ctx, mine) else { continue };
                if let Some(chain) = rules::wait_chain(&st, me, &c.by) {
                    warn(&msg::deadlock(rel, &c.by, &chain, me, away(repo).as_deref()));
                    return Ok(3);
                }
                last.push(format!("  {rel} is held by {}", holder_line(&st, &c, &ctx)));
                blocked.push((i, rel.clone(), c.by.clone()));
            }
        }
        if blocked.is_empty() {
            if !o.has("--claim") {
                say(&format!("free: {names}"));
                return Ok(0);
            }
            if let Some(notes) = take_all(targets, me, now, away)? {
                let mut l = vec![format!("claimed: {names}")];
                l.extend(notes);
                say(&l.join("\n"));
                return Ok(0);
            }
        } else if !announced {
            // tell the holders (waiter notices) and record what we wait for (deadlock detection)
            announced = true;
            for (i, (repo, _)) in targets.iter().enumerate() {
                let mine: Vec<&(usize, String, String)> = blocked.iter().filter(|b| b.0 == i).collect();
                if mine.is_empty() {
                    continue;
                }
                let dl = transact(repo, |st| mine.iter().find_map(|(_, rel, h)| Some((rel.clone(), h.clone(), rules::record_wait(st, rel, me, h, now)?))))?;
                if let Some((rel, h, chain)) = dl {
                    warn(&msg::deadlock(&rel, &h, &chain, me, away(repo).as_deref()));
                    return Ok(3);
                }
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(POLL));
    }
    let mut l = vec![format!("timed out after {}s:", if timeout.fract() == 0.0 { (timeout as u64).to_string() } else { timeout.to_string() })];
    if last.is_empty() {
        l.push("  another agent took the files first".into());
    }
    l.extend(last);
    l.push("The holder keeps a file until it commits. Prefer other work, or commit what you hold.".into());
    warn(&l.join("\n"));
    Ok(1)
}

/// `wait --claim`: take every file (reason "wait": end-of-turn cleanup keeps
/// it) or none. Returns the notes a first edit would have given, or None
/// when another agent got one first.
fn take_all(targets: &[(Repo, Vec<String>)], me: &str, now: u64, away: &dyn Fn(&Repo) -> Option<PathBuf>) -> Res<Option<Vec<String>>> {
    let ctx = Ctx::real(now);
    let mut notes = Vec::new();
    let mut taken: Vec<(usize, Vec<String>)> = Vec::new(); // created claims, for an undo
    for (i, (repo, rels)) in targets.iter().enumerate() {
        let got = transact(repo, |st| {
            let before = st.clone();
            let a = ask(st, me, None, Some(WAIT));
            let mut got = Vec::new();
            for rel in rels {
                match rules::acquire(st, rel, &a, &ctx) {
                    Ok(g) => {
                        let orphan = st.orphans.get(&st.key(rel)).filter(|x| x.from != me).map(|x| x.from.clone());
                        got.push((rel.clone(), g, orphan));
                    }
                    Err(_) => {
                        *st = before;
                        return None;
                    }
                }
            }
            for (rel, g, _) in &got {
                if let Got::TookOver { from, .. } = g
                    && from.edits > 0
                {
                    let k = st.key(rel);
                    st.orphans.insert(k, Orphan { path: rel.clone(), from: from.by.clone(), at: now });
                }
            }
            clear_blocked(st, me, rels);
            Some(got)
        })?;
        let Some(got) = got else {
            for (j, created) in &taken {
                let _ = transact(&targets[*j].0, |st| {
                    for p in created {
                        if st.claim(p).is_some_and(|c| c.by == me && c.edits == 0 && c.reason.as_deref() == Some(WAIT)) {
                            st.take_claim(p);
                        }
                    }
                });
            }
            return Ok(None);
        };
        let mut created = Vec::new();
        let mut plain = Vec::new(); // taken with no note yet: changes nobody claims?
        for (rel, g, orphan) in got {
            let abs = repo.abs(&rel);
            match g {
                Got::Created => {
                    created.push(rel.clone());
                    match orphan {
                        Some(from) => notes.push(msg::orphan_note(&rel, &repo.top, &from)),
                        None => plain.push(rel),
                    }
                }
                Got::TookOver { from, why } if from.edits > 0 => {
                    notes.push(msg::takeover_note(&abs, &rel, &repo.top, &from.by, why.says(), from.edits, me));
                }
                Got::TookOver { .. } => plain.push(rel),
                Got::Transferred { from, inherited: true } => notes.push(msg::inherited(&rel, &from, me, away(repo).as_deref())),
                Got::Joined { with } => notes.push(msg::shared_note(&rel, &repo.top, &with)),
                Got::Transferred { .. } | Got::Refreshed => {}
            }
        }
        if !plain.is_empty()
            && let Ok(st) = state::read(repo)
            && let Ok(d) = dirty_keys(repo, &st, &plain)
        {
            notes.extend(plain.iter().filter(|r| d.contains(&st.key(r))).map(|rel| msg::dirty_note(rel, &repo.top)));
        }
        taken.push((i, created));
    }
    Ok(Some(notes))
}

// ── release / mine ────────────────────────────────────────────────────────

enum Outcome {
    NotClaimed,
    Released(Claim),
    Stale(Claim, Stale),
    Forced(Claim),
    Refused(String, bool),
}

fn release(o: &Opts) -> Res {
    if o.has("--mine") {
        if !o.pos.is_empty() || o.has("--force") {
            return usage("release --mine takes no files and no --force");
        }
        return release_mine(o);
    }
    if o.pos.is_empty() {
        return usage("release needs a file: agent-lock release <file>... [--force] | --mine");
    }
    let base = base_dir(o, None)?;
    let me = caller(o, false)?;
    let force = o.has("--force");
    let mut refused = 0;
    for raw in &o.pos {
        let Some((repo, rel)) = sys::locate(Path::new(raw), &base) else {
            say(&format!("{raw}: not claimed (not in a git worktree)"));
            continue;
        };
        let now = sys::now();
        let r = transact(&repo, |st| {
            let ctx = Ctx::real(now);
            unstick(st);
            let Some(c) = st.claim(&rel).cloned() else { return Outcome::NotClaimed };
            let mine = me.as_deref().is_some_and(|m| {
                c.by == m
                    || (!ids::is_sub(m) && ids::is_sub(&c.by) && ids::family(&c.by) == m && rules::conflict(st, &c, m) == Conflict::Transfer)
            });
            let requester = me.as_deref().and_then(|m| owner_of(st, m));
            if mine {
                let _ = rules::release(st, &rel, Some(&c.by), false, now);
                Outcome::Released(c)
            } else if let Some(why) = rules::stale(st, &c, &ctx, requester) {
                let _ = rules::release(st, &rel, None, true, now);
                Outcome::Stale(c, why)
            } else if force {
                let _ = rules::release(st, &rel, None, true, now);
                Outcome::Forced(c)
            } else {
                Outcome::Refused(holder_line(st, &c, &ctx), c.edits > 0 || c.pending.is_some())
            }
        })?;
        let abs = msg::qp(&repo.abs(&rel));
        match r {
            Outcome::NotClaimed => say(&format!("{raw}: not claimed")),
            Outcome::Released(c) => {
                say(&format!("{raw}: released{}", if c.edits > 0 { " (its uncommitted changes stay on disk, unclaimed)" } else { "" }))
            }
            Outcome::Stale(c, why) => say(&format!("{raw}: released stale claim of {} ({})", c.by, why.says())),
            Outcome::Forced(c) => say(&format!(
                "{raw}: force-released (was held by {}{})",
                c.by,
                if c.edits > 0 || c.pending.is_some() { "; its uncommitted changes now have no owner" } else { "" }
            )),
            Outcome::Refused(line, dirty) => {
                refused += 1;
                let a = as_opt(me.as_deref());
                warn(&format!(
                    "[agent-lock] {rel} is held by {line}.\nIt {}; releasing it can destroy that work.\nPrefer other work; to wait for it: `agent-lock wait{a} --claim {abs} --timeout 540` (run it in the background).\nIf the holder looks abandoned, ask the user. With their approval: `agent-lock release{a} --force {abs}`",
                    if dirty { "has another agent's uncommitted work" } else { "belongs to another agent that may be about to edit it" }
                ));
            }
        }
    }
    Ok(if refused > 0 { 1 } else { 0 })
}

/// A main agent's own and its subagents' claims; a subagent's or script's exact id.
fn release_mine(o: &Opts) -> Res {
    let repo = need_repo(&base_dir(o, None)?)?;
    let me = me_required(o)?;
    let now = sys::now();
    let scope = |c: &Claim| c.by == me || (!ids::is_sub(&me) && ids::is_sub(&c.by) && ids::family(&c.by) == me);
    let (n, dirty) = transact(&repo, |st| {
        let ours: Vec<(String, String, u32)> = st.claims.values().filter(|c| scope(c)).map(|c| (c.path.clone(), c.by.clone(), c.edits)).collect();
        for (p, by, _) in &ours {
            let _ = rules::release(st, p, Some(by), false, now);
        }
        (ours.len(), ours.iter().filter(|x| x.2 > 0).count())
    })?;
    say(&format!(
        "released {} held by {}{}",
        plural(n, "claim"),
        if ids::is_sub(&me) { me.clone() } else { format!("{me} (and its subagents)") },
        if dirty > 0 {
            format!("; {} with uncommitted changes now {} no owner", plural(dirty, "file"), if dirty == 1 { "has" } else { "have" })
        } else {
            String::new()
        }
    ));
    Ok(0)
}

fn mine(o: &Opts) -> Res {
    let me = me_required(o)?;
    let repo = need_repo(&base_dir(o, None)?)?;
    let st = read_state(&repo)?;
    let mut paths: Vec<PathBuf> = st.claims.values().filter(|c| c.by == me).map(|c| repo.abs(&c.path)).collect();
    paths.sort();
    let (live, gone): (Vec<PathBuf>, Vec<PathBuf>) = paths.into_iter().partition(|p| std::fs::symlink_metadata(p).is_ok());
    if o.has("-z") {
        let mut out = std::io::stdout().lock();
        for p in &live {
            let _ = out.write_all(p.as_os_str().as_encoded_bytes());
            let _ = out.write_all(b"\0");
        }
        return Ok(0);
    }
    if !live.is_empty() {
        say(&live.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n"));
    }
    if !gone.is_empty() {
        warn(&format!(
            "({} no longer {}: {})",
            plural(gone.len(), "claimed file"),
            if gone.len() == 1 { "exists" } else { "exist" },
            gone.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(0)
}

// ── off / on / status ─────────────────────────────────────────────────────

fn off(o: &Opts) -> Res {
    let repo = need_repo(&base_dir(o, o.pos.first())?)?;
    transact(&repo, |st| st.off = true)?;
    say(&format!("agent-lock is OFF in {}: hooks and shims do nothing here until `agent-lock on`.", repo.top.display()));
    Ok(0)
}

fn on(o: &Opts) -> Res {
    let repo = need_repo(&base_dir(o, o.pos.first())?)?;
    transact(&repo, |st| st.off = false)?;
    say(&format!("agent-lock is ON in {}.", repo.top.display()));
    if std::env::var_os("CLAUDE_FILE_LOCK").is_some_and(|v| v == "0") {
        warn("note: CLAUDE_FILE_LOCK=0 is set in this environment, which also turns it off.");
    }
    Ok(0)
}

fn table(head: &[&str], rows: &[Vec<String>]) -> Vec<String> {
    let w: Vec<usize> = (0..head.len()).map(|i| rows.iter().map(|r| r[i].chars().count()).chain([head[i].len()]).max().unwrap_or(0)).collect();
    let line = |cells: Vec<&str>| {
        let mut s = String::from("  ");
        for (i, c) in cells.iter().enumerate() {
            s.push_str(c);
            if i + 1 < cells.len() {
                s.push_str(&" ".repeat(w[i] - c.chars().count() + 2));
            }
        }
        s.trim_end().to_string()
    };
    let mut out = vec![line(head.to_vec())];
    out.extend(rows.iter().map(|r| line(r.iter().map(String::as_str).collect())));
    out
}

fn stale_word(s: Stale) -> &'static str {
    match s {
        Stale::OwnerGone => "owner gone",
        Stale::SessionEnded => "session ended",
        Stale::Unwritten => "never written",
        Stale::Idle => "idle",
    }
}

fn alive_word(o: Option<Owner>, ctx: &Ctx) -> &'static str {
    match o.map(|o| (ctx.live)(&o)) {
        Some(Live::Alive) => "alive",
        Some(Live::Dead) => "dead",
        None => "-",
    }
}

fn status(o: &Opts) -> Res {
    let repo = need_repo(&base_dir(o, o.pos.first())?)?;
    let st = read_state(&repo)?;
    if o.has("--json") {
        say(st.to_json().to_pretty().trim_end());
        return Ok(0);
    }
    let me = caller(o, false)?;
    let ctx = Ctx::real(sys::now());
    let now = ctx.now;
    let mut l = vec![format!("agent-lock status: {}", repo.top.display())];
    let mut off = Vec::new();
    if st.off {
        off.push("OFF in this checkout (`agent-lock on` turns it back on)".to_string());
    }
    if std::env::var_os("CLAUDE_FILE_LOCK").is_some_and(|v| v == "0") {
        off.push("CLAUDE_FILE_LOCK=0 in this environment".into());
    }
    l.push(format!("coordination: {}", if off.is_empty() { "on".to_string() } else { off.join("; ") }));
    l.push(format!("you: {}", me.as_deref().unwrap_or("(unknown: no --as, AGENT_LOCK_AGENT_ID or CLAUDE_CODE_SESSION_ID)")));

    let mut claims: Vec<&Claim> = st.claims.values().collect();
    claims.sort_by(|a, b| (a.at, &a.path).cmp(&(b.at, &b.path)));
    l.push(String::new());
    l.push(format!("claims ({}):", claims.len()));
    if claims.is_empty() {
        l.push("  (none)".into());
    } else {
        let rows: Vec<Vec<String>> = claims
            .iter()
            .map(|c| {
                let pend = match (c.pending.is_some(), marker_kind(c)) {
                    (false, _) => "",
                    (_, Some("commit")) => " committing",
                    (_, Some("pkg")) => " installing",
                    _ => " writing",
                };
                let you = if me.as_deref() == Some(c.by.as_str()) { " (you)" } else { "" };
                vec![
                    c.path.clone(),
                    format!("{}{you}", c.by),
                    format!("{}{pend}", c.edits),
                    msg::ago(now.saturating_sub(c.at)),
                    msg::ago(now.saturating_sub(rules::last_active(&st, c))),
                    alive_word(rules::owner_for(&st, c), &ctx).into(),
                    rules::stale(&st, c, &ctx, None).map(stale_word).unwrap_or("-").into(),
                ]
            })
            .collect();
        l.extend(table(&["PATH", "AGENT", "EDITS", "CLAIMED", "IDLE", "OWNER", "STALE"], &rows));
    }

    l.push(String::new());
    l.push(format!("sessions ({}):", st.sessions.len()));
    if st.sessions.is_empty() {
        l.push("  (none)".into());
    } else {
        let mut ss: Vec<_> = st.sessions.values().collect();
        ss.sort_by_key(|s| std::cmp::Reverse(s.seen));
        let rows: Vec<Vec<String>> = ss
            .iter()
            .map(|s| {
                let running = s.subs.iter().filter(|(id, _)| !rules::sub_done(&st, id)).count();
                let n = st.claims.values().filter(|c| ids::family(&c.by) == s.agent).count();
                vec![
                    s.agent.clone(),
                    match s.phase {
                        Phase::Active => "active".into(),
                        Phase::Idle => "idle".into(),
                        Phase::Ended => format!("ended ({})", s.end_reason.as_deref().unwrap_or("?")),
                    },
                    alive_word(s.owner, &ctx).into(),
                    format!("{} ago", msg::ago(now.saturating_sub(s.seen))),
                    n.to_string(),
                    if s.subs.is_empty() { "-".into() } else { format!("{running} running / {}", s.subs.len()) },
                ]
            })
            .collect();
        l.extend(table(&["AGENT", "PHASE", "OWNER", "SEEN", "CLAIMS", "SUBAGENTS"], &rows));
    }

    l.push(String::new());
    let mut locks = Vec::new();
    let op = |name: &str, x: &state::OpLock| {
        format!(
            "  {name}: {} by {} (pid {}, {}) for {}",
            if x.what.is_empty() { "held" } else { &x.what },
            x.by,
            x.owner.map(|o| o.pid.to_string()).unwrap_or_else(|| "?".into()),
            alive_word(x.owner, &ctx),
            msg::ago(now.saturating_sub(x.since))
        )
    };
    if let Some(x) = &st.locks.git {
        locks.push(op("git", x));
    }
    if let Some(x) = &st.locks.pkg {
        locks.push(op("pkg", x));
    }
    if let Some(x) = &st.locks.sync {
        locks.push(format!(
            "  sync: by {} (pid {}, {}) for {}, installing {} over {} paths",
            x.by,
            x.owner.map(|o| o.pid.to_string()).unwrap_or_else(|| "?".into()),
            alive_word(x.owner, &ctx),
            msg::ago(now.saturating_sub(x.since)),
            &x.tip[..x.tip.len().min(12)],
            x.paths.len()
        ));
    }
    if locks.is_empty() {
        l.push("locks: none".into());
    } else {
        l.push("locks:".into());
        l.extend(locks);
    }
    if !st.orphans.is_empty() {
        l.push(String::new());
        l.push(format!("orphans ({}): uncommitted work whose claim ended without a commit", st.orphans.len()));
        let rows: Vec<Vec<String>> =
            st.orphans.values().map(|x| vec![x.path.clone(), x.from.clone(), format!("{} ago", msg::ago(now.saturating_sub(x.at)))]).collect();
        l.extend(table(&["PATH", "FROM", "DROPPED"], &rows));
    }
    if let Some(u) = &st.upstream {
        l.push(String::new());
        l.push(format!("upstream: {}/{} last integrated at {}", u.remote, u.branch, &u.oid[..u.oid.len().min(12)]));
    }
    say(&l.join("\n"));
    Ok(0)
}

// ── settings files (install, uninstall, doctor) ───────────────────────────

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn read_json(p: &Path) -> Option<Json> {
    let t = std::fs::read_to_string(p).ok()?;
    json::parse(t.trim_start_matches('\u{feff}')).ok()
}

fn is_obj(v: &Json) -> bool {
    matches!(v, Json::Obj(_))
}

fn member<'a>(v: &'a mut Json, k: &str) -> Option<&'a mut Json> {
    match v {
        Json::Obj(m) => m.iter_mut().rev().find(|(x, _)| x == k).map(|(_, v)| v),
        _ => None,
    }
}

fn remove_key(v: &mut Json, k: &str) -> bool {
    match v {
        Json::Obj(m) => {
            let n = m.len();
            m.retain(|(x, _)| x != k);
            n != m.len()
        }
        _ => false,
    }
}

fn is_empty_obj(v: &Json) -> bool {
    matches!(v, Json::Obj(m) if m.is_empty())
}

struct Settings {
    file: PathBuf,
    v: Json,
    indent: String,
    existed: bool,
}

fn load_settings(file: &Path) -> Res<Settings> {
    let raw = match std::fs::read_to_string(file) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings { file: file.to_path_buf(), v: Json::obj(), indent: "  ".into(), existed: false });
        }
        Err(e) => return fail(format!("cannot read {}: {e}; nothing was changed", file.display())),
    };
    let text = raw.trim_start_matches('\u{feff}');
    let v = if text.trim().is_empty() {
        Json::obj()
    } else {
        json::parse(text).map_err(|e| Fail(1, format!("cannot parse {}: {e}. Fix it first; nothing was changed.", file.display())))?
    };
    if !is_obj(&v) {
        return fail(format!("{} is not a JSON object; nothing was changed", file.display()));
    }
    Ok(Settings { file: file.to_path_buf(), v, indent: detect_indent(text), existed: true })
}

/// The indent of the first member line ("  " when unknown).
fn detect_indent(text: &str) -> String {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("{") {
        return "  ".into();
    }
    match lines.next() {
        Some(l) => {
            let ws: String = l.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            if ws.is_empty() || !l[ws.len()..].starts_with('"') { "  ".into() } else { ws }
        }
        None => "  ".into(),
    }
}

/// `to_pretty` indents by two spaces; strings never hold a raw newline, so
/// every line's leading spaces are indentation.
fn reindent(text: &str, unit: &str) -> String {
    if unit == "  " {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let n = line.len() - line.trim_start_matches(' ').len();
        out.push_str(&unit.repeat(n / 2));
        out.push_str(&line[n..]);
    }
    out
}

fn save_settings(s: &Settings) -> Res<()> {
    let text = reindent(&s.v.to_pretty(), &s.indent);
    let dir = s.file.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".{}.{}.tmp", s.file.file_name().and_then(|n| n.to_str()).unwrap_or("settings"), sys::pid()));
    std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&tmp, text)).and_then(|_| std::fs::rename(&tmp, &s.file)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Fail(1, format!("cannot write {}: {e}", s.file.display()))
    })
}

fn is_ours(entry: &Json) -> bool {
    entry.at(&["source", "source"]).str() == Some("github") && entry.at(&["source", "repo"]).str() == Some(MARKET_REPO)
}

fn source_text(src: &Json) -> String {
    let where_ = ["repo", "url", "path"].iter().find_map(|k| src.get(k).str()).unwrap_or("");
    format!("{}{}", src.get("source").str().unwrap_or("?"), if where_.is_empty() { String::new() } else { format!(": {where_}") })
}

/// Remove hook entries of the v1 vendored install. Returns how many.
fn strip_legacy_hooks(v: &mut Json) -> usize {
    let Some(Json::Obj(events)) = member(v, "hooks") else { return 0 };
    let (mut total, mut emptied) = (0, Vec::new());
    for (ev, groups) in events.iter_mut() {
        let Json::Arr(gs) = groups else { continue };
        let at = total;
        let mut keep = Vec::new();
        for mut g in std::mem::take(gs) {
            let legacy = |h: &Json| h.get("command").str().is_some_and(|c| c.contains(LEGACY_HOOK));
            let Some(Json::Arr(hs)) = member(&mut g, "hooks") else {
                keep.push(g);
                continue;
            };
            let before = hs.len();
            hs.retain(|h| !legacy(h));
            total += before - hs.len();
            if !hs.is_empty() || before == 0 {
                keep.push(g);
            }
        }
        if total > at && keep.is_empty() {
            emptied.push(ev.clone());
        }
        *gs = keep;
    }
    events.retain(|(ev, _)| !emptied.contains(ev));
    if total > 0 && events.is_empty() {
        remove_key(v, "hooks");
    }
    total
}

/// Team enable: the lovieco marketplace and the plugin, everything else kept.
fn install_settings(v: &mut Json, force: bool, file: &Path) -> Res<(Vec<String>, Vec<String>)> {
    let (mut changes, mut notes) = (Vec::new(), Vec::new());
    for k in ["extraKnownMarketplaces", "enabledPlugins"] {
        if !v.get(k).is_null() && !is_obj(v.get(k)) {
            return fail(format!("{}: {k} is not an object; nothing was changed", file.display()));
        }
    }
    let cur = v.get("extraKnownMarketplaces").get("lovieco").clone();
    if !is_ours(&cur) {
        let has_src = is_obj(&cur) && !cur.get("source").is_null();
        if has_src && !force {
            notes.push(format!(
                "warning: extraKnownMarketplaces.lovieco already points at {}; left as it is. Make sure it provides {PLUGIN}, or re-run with --force to point it at github: {MARKET_REPO}.",
                source_text(cur.get("source"))
            ));
        } else {
            if v.get("extraKnownMarketplaces").is_null() {
                v.set("extraKnownMarketplaces", Json::obj());
            }
            let mut entry = if is_obj(&cur) { cur.clone() } else { Json::obj() };
            entry.set("source", object([("source", "github".into()), ("repo", MARKET_REPO.into())]));
            if let Some(mk) = member(v, "extraKnownMarketplaces") {
                mk.set("lovieco", entry);
            }
            changes.push(if has_src {
                format!("re-pointed marketplace lovieco from {} to github: {MARKET_REPO}", source_text(cur.get("source")))
            } else {
                format!("added marketplace lovieco (github: {MARKET_REPO})")
            });
        }
    }
    if v.get("enabledPlugins").get(PLUGIN).bool() != Some(true) {
        if v.get("enabledPlugins").is_null() {
            v.set("enabledPlugins", Json::obj());
        }
        if let Some(ep) = member(v, "enabledPlugins") {
            ep.set(PLUGIN, Json::Bool(true));
        }
        changes.push(format!("enabled {PLUGIN}"));
    }
    let n = strip_legacy_hooks(v);
    if n > 0 {
        changes.push(format!("removed {} (v1)", hook_entries(n)));
    }
    Ok((changes, notes))
}

fn uninstall_settings(v: &mut Json, force: bool) -> (Vec<String>, Vec<String>) {
    let (mut changes, mut notes) = (Vec::new(), Vec::new());
    if let Some(ep) = member(v, "enabledPlugins")
        && remove_key(ep, PLUGIN)
    {
        changes.push(format!("removed enabledPlugins[\"{PLUGIN}\"]"));
        if is_empty_obj(ep) {
            remove_key(v, "enabledPlugins");
        }
    }
    let other = v.get("enabledPlugins").members().iter().any(|(k, _)| k.ends_with("@lovieco"));
    let cur = v.get("extraKnownMarketplaces").get("lovieco").clone();
    if !cur.is_null() {
        let theirs = is_obj(&cur) && !cur.get("source").is_null() && !is_ours(&cur);
        if other {
            notes.push("kept marketplace lovieco: other @lovieco plugins are enabled".into());
        } else if theirs && !force {
            notes.push(format!("kept marketplace lovieco: it points at {} (--force removes it)", source_text(cur.get("source"))));
        } else if let Some(mk) = member(v, "extraKnownMarketplaces") {
            remove_key(mk, "lovieco");
            changes.push("removed marketplace lovieco".into());
            if is_empty_obj(mk) {
                remove_key(v, "extraKnownMarketplaces");
            }
        }
    }
    let n = strip_legacy_hooks(v);
    if n > 0 {
        changes.push(format!("removed {} (v1)", hook_entries(n)));
    }
    (changes, notes)
}

/// `<dir>/.claude/skills/agent-lock/` when it is v1's vendored project skill
/// (sidecar `.lock` protocol), never a skill the user wrote.
fn v1_skill(dir: &Path) -> Option<PathBuf> {
    let d = dir.join(".claude/skills/agent-lock");
    let t = std::fs::read_to_string(d.join("SKILL.md")).ok()?;
    let (front, body) = match t.strip_prefix("---\n").or_else(|| t.strip_prefix("---\r\n")).and_then(|r| r.find("\n---").map(|i| (&r[..i], &r[i..])))
    {
        Some((f, b)) => (f.to_string(), b.to_string()),
        None => (String::new(), t.clone()),
    };
    let desc = front.lines().find_map(|l| l.strip_prefix("description:")).unwrap_or("").to_lowercase();
    let lock_word = desc.match_indices(".lock").any(|(i, _)| !desc[i + 5..].starts_with(|c: char| c.is_alphanumeric() || c == '_'));
    let v1 = (desc.contains("sidecar") && lock_word) || body.contains(".agent-lock/hooks/") || body.contains(".agent-lock/src/lock.cjs");
    v1.then_some(d)
}

fn legacy_hook_count(v: &Json) -> usize {
    v.get("hooks")
        .members()
        .iter()
        .flat_map(|(_, gs)| gs.arr())
        .flat_map(|g| g.get("hooks").arr())
        .filter(|h| h.get("command").str().is_some_and(|c| c.contains(LEGACY_HOOK)))
        .count()
}

/// v1 leftovers on disk: `.agent-lock/{src,hooks}` and the v1 project skill.
fn remove_legacy(dir: &Path, changes: &mut Vec<String>) {
    let legacy = dir.join(".agent-lock");
    let mut any = false;
    for sub in ["src", "hooks"] {
        let p = legacy.join(sub);
        if p.exists() && std::fs::remove_dir_all(&p).is_ok() {
            changes.push(format!("removed .agent-lock/{sub}/ (v1 vendored copy)"));
            any = true;
        }
    }
    if any {
        let _ = std::fs::remove_dir(legacy.join("locks")); // only when empty
        if std::fs::remove_dir(&legacy).is_ok() {
            changes.push("removed the now-empty .agent-lock/".into());
        }
    }
    if let Some(skill) = v1_skill(dir)
        && std::fs::remove_dir_all(&skill).is_ok()
    {
        changes.push("removed .claude/skills/agent-lock/ (v1 project skill)".into());
        let _ = std::fs::remove_dir(dir.join(".claude/skills"));
    }
}

/// v1 hook entries in settings.local.json; an unparseable file is reported, never touched.
fn strip_local_legacy(dir: &Path, changes: &mut Vec<String>, notes: &mut Vec<String>) {
    let file = dir.join(".claude/settings.local.json");
    if !file.exists() {
        return;
    }
    match load_settings(&file) {
        Err(Fail(_, m)) => notes.push(format!("note: {m} Legacy hooks there were not checked.")),
        Ok(mut s) => {
            let n = strip_legacy_hooks(&mut s.v);
            if n > 0 && save_settings(&s).is_ok() {
                changes.push(format!("removed {} from .claude/settings.local.json", hook_entries(n)));
            }
        }
    }
}

/// The project dir: the argument, else the repo top (else the directory).
fn settings_dir(o: &Opts) -> Res<PathBuf> {
    let base = base_dir(o, o.pos.first())?;
    if o.pos.is_empty()
        && let Some(r) = sys::find_repo(&base)
    {
        return Ok(r.top);
    }
    Ok(base)
}

fn install(o: &Opts) -> Res {
    let dir = settings_dir(o)?;
    let file = dir.join(".claude/settings.json");
    let mut s = load_settings(&file)?;
    let before = s.v.clone();
    let (mut changes, mut notes) = install_settings(&mut s.v, o.has("--force"), &file)?;
    if s.v != before {
        save_settings(&s)?;
    }
    strip_local_legacy(&dir, &mut changes, &mut notes);
    remove_legacy(&dir, &mut changes);
    let mut l = if changes.is_empty() {
        vec![format!("agent-lock is already installed in {}; nothing to change", file.display())]
    } else {
        let mut l = vec![format!("agent-lock installed in {}:", file.display())];
        l.extend(changes.iter().map(|c| format!("  {c}")));
        l.push(String::new());
        l.push("Commit .claude/settings.json: teammates get the plugin when they open the repo and trust the folder.".into());
        l.push(format!(
            "On this machine, if Claude Code has not loaded it yet: claude plugin install agent-lock --marketplace {MARKET_REPO}"
        ));
        l
    };
    if kill_switch_in(&s.v) {
        l.push(format!(
            "warning: {} sets env.CLAUDE_FILE_LOCK=0, which turns agent-lock off for everyone using it; remove it (`agent-lock off` turns it off for one checkout)",
            file.display()
        ));
    }
    say(&l.join("\n"));
    for n in notes {
        warn(&n);
    }
    Ok(0)
}

fn uninstall(o: &Opts) -> Res {
    let dir = settings_dir(o)?;
    let file = dir.join(".claude/settings.json");
    let mut s = load_settings(&file)?;
    let before = s.v.clone();
    let (mut changes, mut notes) = uninstall_settings(&mut s.v, o.has("--force"));
    if s.existed && s.v != before {
        save_settings(&s)?;
    }
    strip_local_legacy(&dir, &mut changes, &mut notes);
    remove_legacy(&dir, &mut changes);
    if changes.is_empty() {
        say(&format!("agent-lock is not installed in {}; nothing to change", file.display()));
    } else {
        let mut l = vec![format!("agent-lock uninstalled from {}:", file.display())];
        l.extend(changes.iter().map(|c| format!("  {c}")));
        say(&l.join("\n"));
    }
    for n in notes {
        warn(&n);
    }
    Ok(0)
}

fn kill_switch_in(v: &Json) -> bool {
    match v.at(&["env", "CLAUDE_FILE_LOCK"]) {
        Json::Str(s) => s.trim() == "0",
        Json::Num(n) => *n == 0.0,
        _ => false,
    }
}

/// Where the plugin is enabled or disabled: (enabled, file), first match wins.
fn plugin_state(dir: &Path) -> (bool, Option<PathBuf>) {
    let mut files = vec![dir.join(".claude/settings.local.json"), dir.join(".claude/settings.json")];
    files.extend(home().map(|h| h.join(".claude/settings.json")));
    for f in files {
        if let Some(b) = read_json(&f).and_then(|v| v.get("enabledPlugins").get(PLUGIN).bool()) {
            return (b, Some(f));
        }
    }
    (false, None)
}

// ── doctor ────────────────────────────────────────────────────────────────

enum Fix {
    Remove(PathBuf),
    Release(String),
    Unstick,
    Recover,
}

#[derive(Default)]
struct Found {
    items: Vec<(String, Option<Fix>)>,
    notes: Vec<String>,
}

impl Found {
    fn add(&mut self, kind: &str, detail: String, fix: Option<Fix>) {
        self.items.push((format!("[{kind}] {detail}"), fix));
    }
}

fn mtime_age(p: &Path) -> Option<u64> {
    let t = std::fs::symlink_metadata(p).ok()?.modified().ok()?;
    Some(t.elapsed().map(|d| d.as_secs()).unwrap_or(0))
}

fn first_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| sys::is_executable(p))
}

fn doctor(o: &Opts) -> Res {
    let base = base_dir(o, o.pos.first())?;
    let repo = sys::find_repo(&base);
    let fix = o.has("--fix");
    let place = repo.as_ref().map(|r| r.top.clone()).unwrap_or_else(|| base.clone());
    let mut f = Found::default();

    // environment and setup
    match git::version() {
        Some(v) if v >= (2, 40) => {}
        Some((a, b)) => f.add("git", format!("git {a}.{b} is too old: agent-lock needs git >= 2.40"), None),
        None => f.add("git", "cannot run git".into(), None),
    }
    if repo.is_none() {
        f.add("worktree", format!("{} is not inside a git worktree; agent-lock only coordinates files in git worktrees", base.display()), None);
    }
    match plugin_state(&place) {
        (true, _) => {}
        (false, Some(file)) => f.add("plugin", format!("{PLUGIN} is disabled in {} — `agent-lock install` enables it", file.display()), None),
        (false, None) => f.add("plugin", format!("{PLUGIN} is not enabled — run `agent-lock install` (project) or /plugin install {PLUGIN}"), None),
    }
    let mut setting_files = vec![place.join(".claude/settings.json"), place.join(".claude/settings.local.json")];
    setting_files.extend(home().map(|h| h.join(".claude/settings.json")));
    setting_files.dedup();
    for file in &setting_files {
        if read_json(file).is_some_and(|v| kill_switch_in(&v)) {
            f.add(
                "kill-switch",
                format!(
                    "{} sets env.CLAUDE_FILE_LOCK=0, which turns agent-lock off; remove it (`agent-lock off` turns it off for one checkout)",
                    file.display()
                ),
                None,
            );
        }
    }
    if place.join(".agent-lock").exists() {
        f.add("legacy", ".agent-lock/ (v1 vendored install) — `agent-lock install` removes src/ and hooks/; delete the rest by hand".into(), None);
    }
    if v1_skill(&place).is_some() {
        f.add("legacy", ".claude/skills/agent-lock/ is the v1 project skill (sidecar .lock protocol), loaded next to the plugin's skill — `agent-lock install` removes it".into(), None);
    }
    for name in ["settings.json", "settings.local.json"] {
        let n = read_json(&place.join(".claude").join(name)).map(|v| legacy_hook_count(&v)).unwrap_or(0);
        if n > 0 {
            f.add(
                "legacy",
                format!(".claude/{name} wires {} ({LEGACY_HOOK}) — `agent-lock install` removes them", plural(n, "v1 vendored hook")),
                None,
            );
        }
    }
    if std::env::var("CLAUDE_CODE_SESSION_ID").is_ok_and(|s| !s.is_empty()) {
        // inside a Claude Code Bash call: the session env file should have put the shims first
        let me = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok());
        if std::env::var("AGENT_LOCK_AGENT_ID").is_err() {
            f.add("shims", "AGENT_LOCK_AGENT_ID is not set in this Claude Code session: its env file was not applied, so the git and npm shims are not on PATH and whole-tree git is unguarded".into(), None);
        } else if let Some(g) = first_on_path("git") {
            let real = std::fs::canonicalize(&g).ok();
            let ours = real.is_some()
                && (real == me || real.as_ref().and_then(|r| r.file_name()).is_some_and(|n| n.to_string_lossy().starts_with("agent-lock")));
            if !ours {
                f.add("shims", format!("the first git on PATH is {}, not the agent-lock shim: whole-tree git is unguarded", g.display()), None);
            }
        }
    }
    if std::env::var_os("CLAUDE_FILE_LOCK").is_some_and(|v| v == "0") {
        f.notes.push("note: CLAUDE_FILE_LOCK=0 is set in this environment (agent-lock is off for it)".into());
    }

    if let Some(repo) = &repo {
        lock_problems(repo, &mut f)?;
    }

    // report, then fix
    let mut l = vec![format!("agent-lock doctor: {}", place.display()), String::new()];
    if f.items.is_empty() {
        l.push("  clean.".into());
    } else {
        l.extend(f.items.iter().map(|(t, _)| format!("  {t}")));
    }
    l.extend(f.notes.iter().map(|n| format!("  {n}")));
    let mut fixed = Vec::new();
    let mut remaining = f.items.len();
    if fix && let Some(repo) = &repo {
        let (done, n) = apply_fixes(repo, &f);
        fixed = done;
        remaining -= n;
    }
    if !fixed.is_empty() {
        l.push(String::new());
        l.push("fixed:".into());
        l.extend(fixed.iter().map(|x| format!("  {x}")));
    }
    if !f.items.is_empty() {
        l.push(String::new());
        l.push(if fix {
            format!("{} found, {} fixed.", plural(f.items.len(), "problem"), f.items.len() - remaining)
        } else {
            format!(
                "{} found.{}",
                plural(f.items.len(), "problem"),
                if f.items.iter().any(|x| x.1.is_some()) { " `agent-lock doctor --fix` repairs the ones marked (fixable)." } else { "" }
            )
        });
    }
    say(&l.join("\n"));
    Ok(if remaining > 0 { 1 } else { 0 })
}

fn lock_problems(repo: &Repo, f: &mut Found) -> Res<()> {
    let bad = repo.git_dir.join("agent-lock.json.bad");
    if bad.exists() {
        f.add(
            "corrupt",
            format!("{}: the state file could not be parsed and was reset (this file holds the old content) (fixable)", bad.display()),
            Some(Fix::Remove(bad)),
        );
    }
    let st = read_state(repo)?;
    let ctx = Ctx::real(sys::now());
    let now = ctx.now;
    if st.off {
        f.notes.push("note: agent-lock is OFF in this checkout (`agent-lock on` turns it back on)".into());
    }
    // stale claims: one git status over them
    let stale: Vec<(&Claim, Stale)> = st.claims.values().filter_map(|c| rules::stale(&st, c, &ctx, None).map(|w| (c, w))).collect();
    let paths: Vec<String> = stale.iter().map(|(c, _)| c.path.clone()).collect();
    let dirty = if paths.is_empty() { Some(BTreeSet::new()) } else { dirty_keys(repo, &st, &paths).ok() };
    for (c, why) in &stale {
        let kind = match why {
            Stale::OwnerGone => "dead-owner",
            Stale::SessionEnded => "ended-session",
            Stale::Unwritten => "unwritten",
            Stale::Idle => "stale",
        };
        let changed = dirty.as_ref().map(|d| d.contains(&st.key(&c.path))).unwrap_or(c.edits > 0);
        let abs = msg::qp(&repo.abs(&c.path));
        let advice = if c.edits > 0 && changed {
            format!(
                ": its owner left uncommitted changes in it. Review them (`{}`); then edit the file (your first edit takes the claim over, with a one-time warning) and commit it, or ask the user and run `agent-lock release --force {abs}` (the changes stay on disk, unattributed); --fix releases it and remembers whose changes they are",
                msg::git_diff(&repo.top, &c.path)
            )
        } else if changed {
            ": the file has uncommitted changes that were not made through this claim; --fix releases the claim, whoever made the changes commits them".into()
        } else {
            ": no uncommitted changes; --fix releases it".into()
        };
        f.add(
            kind,
            format!("{} — {}, {} ({}){advice} (fixable)", c.path, c.by, plural(c.edits as usize, "edit"), why.says()),
            Some(Fix::Release(c.path.clone())),
        );
    }
    for c in st.claims.values() {
        if !c.pending.as_deref().is_some_and(rules::marker_dead) {
            continue;
        }
        let what = match marker_kind(c) {
            Some("commit") => "an `agent-lock commit` was killed before it finished",
            Some("pkg") => "a package install was killed before it finished",
            _ => "the process working on it was killed before it finished",
        };
        f.add("stuck", format!("{} — {}: {what}; its marker blocks hand-overs and end-of-turn cleanup (fixable)", c.path, c.by), Some(Fix::Unstick));
    }
    for s in st.sessions.values().filter(|s| s.phase == Phase::Ended) {
        let n = st.claims.values().filter(|c| ids::family(&c.by) == s.agent).count();
        if n > 0 {
            f.notes.push(format!(
                "note: session {} ended but its agents still hold {} (kept until committed or taken over)",
                s.agent,
                plural(n, "claim")
            ));
        }
    }
    // an interrupted sync, the index.lock, long-held locks
    let index_lock = repo.git_dir.join("index.lock");
    let marker = git::marker_lock(&index_lock).and_then(|l| l.owner);
    match &st.locks.sync {
        Some(l) => {
            let dead = match l.owner {
                Some(o) => sys::liveness(&o) == Live::Dead,
                None => now.saturating_sub(l.since) > LOCK_OLD,
            };
            if dead {
                f.add(
                    "interrupted-sync",
                    format!("an `agent-lock sync` by {} was killed while installing an upstream update; the next agent-lock git command undoes or finishes it (fixable)", l.by),
                    Some(Fix::Recover),
                );
            } else if now.saturating_sub(l.since) > LOCK_OLD {
                f.add(
                    "lock",
                    format!(
                        "an `agent-lock sync` by {} has been installing for {}; if that process is hung, stop it",
                        l.by,
                        msg::ago(now.saturating_sub(l.since))
                    ),
                    None,
                );
            }
        }
        None => {
            if let Some(o) = marker {
                let pid = o.pid;
                if sys::liveness(&o) == Live::Dead {
                    f.add(
                        "interrupted-sync",
                        format!("{} was left by a killed `agent-lock sync` (pid {pid}); every git command that writes the index fails while it exists (fixable)", index_lock.display()),
                        Some(Fix::Recover),
                    );
                }
            }
        }
    }
    if marker.is_none()
        && let Some(age) = mtime_age(&index_lock)
        && age > LOCK_OLD
    {
        f.add(
            "index-lock",
            format!(
                "{} is {} old: a git command probably crashed and left it; every git command that writes the index fails while it exists. Make sure no git command is running in this checkout, then ask the user to delete it (agents never remove it)",
                index_lock.display(),
                msg::ago(age)
            ),
            None,
        );
    }
    for (name, l, limit) in [("git", &st.locks.git, LOCK_OLD), ("package-manager", &st.locks.pkg, PKG_OLD)] {
        if let Some(l) = l
            && l.owner.is_some_and(|o| sys::liveness(&o) == Live::Alive)
            && now.saturating_sub(l.since) > limit
        {
            f.add(
                "lock",
                format!(
                    "the {name} lock has been held by {} (pid {}, `{}`) for {}; if that process is hung, stop it",
                    l.by,
                    l.owner.map(|o| o.pid).unwrap_or(0),
                    l.what,
                    msg::ago(now.saturating_sub(l.since))
                ),
                None,
            );
        }
    }
    // temp files of killed commits and syncs (not while one of them runs)
    let live_tmp = st.locks.sync.as_ref().and_then(|l| l.index_tmp.clone());
    let committing = st.claims.values().any(|c| marker_kind(c) == Some("commit") && !c.pending.as_deref().is_some_and(rules::marker_dead));
    if let Ok(rd) = std::fs::read_dir(&repo.git_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !["agent-lock-commit-", "agent-lock-restore-", "agent-lock-sync-"].iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let p = e.path();
            let busy =
                (committing && name.starts_with("agent-lock-commit-")) || live_tmp.as_deref().is_some_and(|t| p.to_string_lossy().starts_with(t));
            if busy || mtime_age(&p).is_none_or(|a| a < LITTER_AGE) {
                continue;
            }
            f.add("leftover", format!("{}: left by a killed agent-lock command (fixable)", p.display()), Some(Fix::Remove(p)));
        }
    }
    Ok(())
}

/// Apply the fixes; returns what was done and how many problems it resolved.
fn apply_fixes(repo: &Repo, f: &Found) -> (Vec<String>, usize) {
    let (mut done, mut n) = (Vec::new(), 0);
    if f.items.iter().any(|x| matches!(x.1, Some(Fix::Recover))) {
        let k = f.items.iter().filter(|x| matches!(x.1, Some(Fix::Recover))).count();
        match git::recover(repo) {
            Some(m) => {
                done.push(m);
                n += k;
            }
            None => done.push("an interrupted sync could not be recovered now (another command may be at it); retry".into()),
        }
    }
    let release: Vec<&String> = f.items.iter().filter_map(|x| if let Some(Fix::Release(p)) = &x.1 { Some(p) } else { None }).collect();
    let unstick_n = f.items.iter().filter(|x| matches!(x.1, Some(Fix::Unstick))).count();
    if !release.is_empty() || unstick_n > 0 {
        let now = sys::now();
        let r = state::transact(repo, |st| {
            let ctx = Ctx::real(now);
            let mut out = Vec::new();
            for p in unstick(st) {
                out.push(format!("cleared the stuck marker on {p}"));
            }
            let mut released = 0;
            for p in &release {
                let Some(c) = st.claim(p).cloned() else { continue };
                if let Some(why) = rules::stale(st, &c, &ctx, None)
                    && rules::release(st, p, None, true, now).is_ok()
                {
                    released += 1;
                    out.push(format!(
                        "released {p} ({} {}){}",
                        c.by,
                        why.says(),
                        if c.edits > 0 { "; the next agent to edit it is told whose changes it holds" } else { "" }
                    ));
                }
            }
            (out, released)
        });
        if let Ok((out, released)) = r {
            n += released + unstick_n.min(out.len() - released);
            done.extend(out);
        }
    }
    for (_, fx) in &f.items {
        if let Some(Fix::Remove(p)) = fx {
            let _ = std::fs::remove_file(p);
            if std::fs::symlink_metadata(p).is_err() {
                done.push(format!("removed {}", p.display()));
                n += 1;
            }
        }
    }
    (done, n)
}

// ── selftest ──────────────────────────────────────────────────────────────

struct Ran {
    code: i32,
    out: String,
    err: String,
}

fn ran(cmd: &mut Command, input: &str) -> Result<Ran, String> {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    if let Some(mut i) = child.stdin.take() {
        let _ = i.write_all(input.as_bytes());
    }
    let o = child.wait_with_output().map_err(|e| e.to_string())?;
    Ok(Ran { code: o.status.code().unwrap_or(-1), out: String::from_utf8_lossy(&o.stdout).into(), err: String::from_utf8_lossy(&o.stderr).into() })
}

fn check(cond: bool, why: impl Into<String>) -> Result<(), String> {
    if cond { Ok(()) } else { Err(why.into()) }
}

fn selftest(o: &Opts) -> Res {
    let exe = std::env::current_exe().map_err(|e| Fail(1, format!("cannot find this binary: {e}")))?;
    let tmp = std::env::temp_dir().join(format!("agent-lock-selftest-{}-{}", sys::pid(), sys::now_ms()));
    std::fs::create_dir_all(&tmp).map_err(|e| Fail(1, format!("cannot create {}: {e}", tmp.display())))?;
    let tmp = std::fs::canonicalize(&tmp).unwrap_or(tmp);
    let (work, origin, data) = (tmp.join("work"), tmp.join("origin.git"), tmp.join("data"));
    // the git shim's folder: shims/core (older builds: shims)
    let shims = || [data.join("shims/core"), data.join("shims")].into_iter().find(|d| sys::is_executable(&d.join("git"))).unwrap_or(data.join("shims"));
    let real_git = sys::git_bin();
    let mut env: Vec<(String, String)> =
        std::env::vars().filter(|(k, _)| !["CLAUDE_", "AGENT_LOCK_", "GIT_"].iter().any(|p| k.starts_with(p))).collect();
    for (k, v) in [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_EDITOR", "true"),
        ("GIT_AUTHOR_NAME", "agent-lock selftest"),
        ("GIT_AUTHOR_EMAIL", "selftest@agent-lock.invalid"),
        ("GIT_COMMITTER_NAME", "agent-lock selftest"),
        ("GIT_COMMITTER_EMAIL", "selftest@agent-lock.invalid"),
    ] {
        env.push((k.into(), v.into()));
    }
    env.push(("CLAUDE_PID".into(), sys::pid().to_string()));
    env.push(("CLAUDE_PLUGIN_DATA".into(), data.display().to_string()));
    let cmd = |prog: &Path, extra: &[(&str, String)]| {
        let mut c = Command::new(prog);
        c.current_dir(&work).env_clear().envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        for (k, v) in extra {
            c.env(k, v);
        }
        c
    };
    let git = |args: &[&str], cwd: &Path| -> Result<String, String> {
        let mut c = cmd(&real_git, &[]);
        let r = ran(c.args(args).current_dir(cwd), "")?;
        check(r.code == 0, format!("git {}: {}", args.join(" "), r.err.trim()))?;
        Ok(r.out.trim().to_string())
    };
    let (sa, sb) = ("a11ce000-0000-4000-8000-00000000000a", "b0b00000-0000-4000-8000-00000000000b");
    let (ia, ib) = (ids::sid8(sa), ids::sid8(sb));
    let hook = |event: &str, payload: Json| -> Result<Json, String> {
        let r = ran(cmd(&exe, &[]).args(["hook", event]), &payload.to_string())?;
        check(r.code == 0, format!("hook {event} exited {}: {}", r.code, r.err.trim()))?;
        Ok(if r.out.trim().is_empty() { Json::Null } else { json::parse(&r.out).map_err(|e| format!("hook {event} printed bad JSON: {e}"))? })
    };
    let base =
        |s: &str, event: &str| object([("session_id", s.into()), ("cwd", work.display().to_string().into()), ("hook_event_name", event.into())]);
    let n = std::cell::Cell::new(0u32);
    let edit = |s: &str, rel: &str, text: &str| -> Result<bool, String> {
        n.set(n.get() + 1);
        let file = work.join(rel);
        let mut p = base(s, "PreToolUse");
        p.set("tool_name", "Write".into());
        p.set("tool_input", object([("file_path", file.display().to_string().into()), ("content", text.into())]));
        p.set("tool_use_id", format!("toolu_selftest_{}", n.get()).into());
        if hook("pre-edit", p.clone())?.at(&["hookSpecificOutput", "permissionDecision"]).str() == Some("deny") {
            return Ok(false);
        }
        std::fs::write(&file, text).map_err(|e| e.to_string())?;
        p.set("hook_event_name", "PostToolUse".into());
        hook("post-edit", p)?;
        Ok(true)
    };
    let holder = |rel: &str| -> Option<String> {
        let t = std::fs::read_to_string(work.join(".git/agent-lock.json")).ok()?;
        json::parse_prefix(&t).ok()?.0.get("claims").get(rel).get("by").str().map(str::to_string)
    };
    let cli = |s: &str, args: &[&str]| ran(cmd(&exe, &[("CLAUDE_CODE_SESSION_ID", s.to_string())]).args(args), "");

    say(&format!("agent-lock selftest in {} ({})", tmp.display(), exe.display()));
    let (mut steps, mut failed) = (0, 0);
    let mut step = |name: &str, r: Result<String, String>| {
        steps += 1;
        let (ok, detail) = match r {
            Ok(d) => (true, d),
            Err(e) => (false, e),
        };
        if !ok {
            failed += 1;
        }
        say(&format!("{}  {name}{}", if ok { "PASS" } else { "FAIL" }, if detail.is_empty() { String::new() } else { format!(" — {detail}") }));
        ok
    };
    step(
        "git >= 2.40",
        match git::version() {
            Some(v) if v >= (2, 40) => Ok(format!("git {}.{}", v.0, v.1)),
            Some(v) => Err(format!("git {}.{}", v.0, v.1)),
            None => Err(format!("cannot run {}", real_git.display())),
        },
    );
    let ready = step(
        "throwaway repo with a bare remote",
        (|| {
            std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
            git(&["init", "-q", "-b", "main"], &work)?;
            std::fs::write(work.join("README.md"), "selftest\n").map_err(|e| e.to_string())?;
            git(&["add", "-A"], &work)?;
            git(&["commit", "-q", "-m", "init"], &work)?;
            git(&["init", "-q", "--bare", "-b", "main", &origin.display().to_string()], &tmp)?;
            git(&["remote", "add", "origin", &origin.display().to_string()], &work)?;
            git(&["push", "-q", "-u", "origin", "main"], &work)?;
            Ok(String::new())
        })(),
    );
    if ready {
        step(
            "two sessions start",
            (|| {
                for s in [sa, sb] {
                    let mut p = base(s, "SessionStart");
                    p.set("source", "startup".into());
                    hook("session-start", p)?;
                }
                check(sys::is_executable(&shims().join("git")), format!("no git shim in {}", data.join("shims").display()))?;
                Ok(format!("{ia} and {ib}"))
            })(),
        );
        step(
            "A edits a.txt and claims it",
            (|| {
                check(edit(sa, "a.txt", "from A\n")?, "A was blocked")?;
                check(holder("a.txt").as_deref() == Some(ia.as_str()), format!("a.txt is held by {:?}", holder("a.txt")))?;
                Ok(String::new())
            })(),
        );
        step(
            "B is blocked from a.txt",
            (|| {
                check(!edit(sb, "a.txt", "from B\n")?, "B was allowed to write A's file")?;
                check(std::fs::read_to_string(work.join("a.txt")).unwrap_or_default() == "from A\n", "a.txt was overwritten")?;
                Ok(String::new())
            })(),
        );
        step("B edits b.txt", (|| check(edit(sb, "b.txt", "from B\n")?, "B was blocked").map(|_| String::new()))());
        step(
            "the git shim refuses `git add -A` while B holds b.txt",
            (|| {
                let path = format!("{}:{}", shims().display(), std::env::var("PATH").unwrap_or_default());
                let r = ran(cmd(&shims().join("git"), &[("AGENT_LOCK_AGENT_ID", ia.clone()), ("PATH", path)]).args(["add", "-A"]), "")?;
                check(r.code == 1 && r.err.starts_with("agent-lock:"), format!("exit {}: {}", r.code, r.err.trim()))?;
                check(git(&["diff", "--cached", "--name-only"], &work)?.is_empty(), "something was staged")?;
                Ok(String::new())
            })(),
        );
        step(
            "A commits only its own file",
            (|| {
                let r = cli(sa, &["commit", "-m", "A: a.txt"])?;
                check(r.code == 0, format!("exit {}: {}", r.code, r.err.trim()))?;
                let names = git(&["show", "--name-only", "--format=", "HEAD"], &work)?;
                check(names == "a.txt", format!("the commit holds {}", names.replace('\n', ", ")))?;
                check(git(&["status", "--porcelain"], &work)?.contains("b.txt"), "b.txt is no longer dirty")?;
                Ok(String::new())
            })(),
        );
        step("A's claim is released by its commit", check(holder("a.txt").is_none(), "a.txt is still claimed").map(|_| String::new()));
        step(
            "B commits and pushes",
            (|| {
                let r = cli(sb, &["commit", "-m", "B: b.txt", "--push"])?;
                check(r.code == 0, format!("exit {}: {}{}", r.code, r.out.trim(), r.err.trim()))?;
                check(git(&["rev-parse", "HEAD"], &work)? == git(&["rev-parse", "main"], &origin)?, "origin/main is not at HEAD")?;
                Ok(String::new())
            })(),
        );
        step(
            "worktree clean and no claims left",
            (|| {
                check(git(&["status", "--porcelain"], &work)?.is_empty(), "the worktree is dirty")?;
                let t = std::fs::read_to_string(work.join(".git/agent-lock.json")).unwrap_or_default();
                let left = json::parse_prefix(&t).map(|v| v.0.get("claims").members().len()).unwrap_or(0);
                check(left == 0, format!("{} left", plural(left, "claim")))?;
                Ok(String::new())
            })(),
        );
        step(
            "both sessions end",
            (|| {
                for s in [sa, sb] {
                    let mut p = base(s, "SessionEnd");
                    p.set("reason", "other".into());
                    hook("session-end", p)?;
                }
                Ok(String::new())
            })(),
        );
    }
    if o.has("--keep") {
        say(&format!("kept {}", tmp.display()));
    } else {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    say(&if failed > 0 { format!("FAIL: {failed} of {} failed", plural(steps, "step")) } else { format!("PASS: all {steps} steps") });
    Ok(if failed > 0 { 1 } else { 0 })
}

// ── version / help ────────────────────────────────────────────────────────

fn version(_: &Opts) -> Res {
    say(&format!("agent-lock {}", env!("CARGO_PKG_VERSION")));
    Ok(0)
}

fn help(_: &Opts) -> Res {
    let w = CMDS.iter().map(|c| c.name.len() + 1 + c.args.chars().count()).max().unwrap_or(0);
    let mut l = vec![
        "agent-lock: many agents in one shared git checkout, on one branch.".to_string(),
        String::new(),
        "Usage: agent-lock <command> [options]".into(),
        String::new(),
    ];
    for c in CMDS {
        let head = format!("{} {}", c.name, c.args);
        let pad = w - head.chars().count();
        l.push(format!("  {head}{}  {}", " ".repeat(pad), c.about));
    }
    l.extend([
        String::new(),
        "Who you are: --as <id> › $AGENT_LOCK_AGENT_ID › $CLAUDE_CODE_SESSION_ID (the session's main agent).".into(),
        "Subagents share the session's environment, so they pass --as <their id>.".into(),
        "Options anywhere: --as <id>, -C <dir> (run as if started in <dir>).".into(),
        "Exit codes: 0 ok, 1 refused or failed, 2 usage error, 3 deadlock (wait).".into(),
    ]);
    say(&l.join("\n"));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Blocked, Session, Sub};

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn parsed(s: &[&str]) -> (&'static str, Opts) {
        match parse(&args(s)) {
            Ok((c, o)) => (c.name, o),
            Err(Fail(code, m)) => panic!("{s:?}: {code} {m}"),
        }
    }

    fn code(s: &[&str]) -> i32 {
        match parse(&args(s)) {
            Ok(_) => 0,
            Err(Fail(c, _)) => c,
        }
    }

    #[test]
    fn arguments() {
        let (c, o) = parsed(&["--as", "a1b2c3d4:9f3e01aa", "commit", "-mfirst", "--message=second", "-C", "sub", "--push", "--", "-x.txt", "--as"]);
        assert_eq!(c, "commit");
        assert_eq!(o.as_id.as_deref(), Some("a1b2c3d4:9f3e01aa"));
        assert_eq!(o.all("-m"), ["first", "second"]);
        assert_eq!(o.dirs, ["sub"]);
        assert!(o.has("--push"));
        assert_eq!(o.pos, ["-x.txt", "--as"], "after -- everything is a path");
        let (_, o) = parsed(&["commit", "-m", "-starts-with-dash", "--as=bbbbbbbb", "a.txt"]);
        assert_eq!(
            (o.all("-m"), o.as_id.as_deref(), o.pos.as_slice()),
            (&["-starts-with-dash".to_string()][..], Some("bbbbbbbb"), &["a.txt".to_string()][..])
        );
        assert!(parsed(&["mine", "--null"]).1.has("-z"));
        assert_eq!(parsed(&[]).0, "help");
        assert_eq!(parsed(&["wait", "-h"]).0, "help");
        assert_eq!(code(&["nope"]), 2);
        assert_eq!(code(&["commit", "--bogus"]), 2);
        assert_eq!(code(&["commit", "-m"]), 2, "a value is missing");
        assert_eq!(code(&["push", "extra"]), 2);
        assert_eq!(code(&["status", "--json=1"]), 2, "a flag takes no value");
        assert_eq!(code(&["--as"]), 2);
    }

    #[test]
    fn settings_install_and_uninstall_keep_everything_else() {
        let f = Path::new("/x/settings.json");
        let mut v = json::parse(r#"{"model":"opus","enabledPlugins":{"other@x":true},"hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"node .agent-lock/hooks/pre.mjs"},{"type":"command","command":"mine.sh"}]}],"Stop":[{"hooks":[{"type":"command","command":"node .agent-lock/hooks/stop.mjs"}]}],"Notification":[]}}"#).unwrap();
        let (changes, notes) = install_settings(&mut v, false, f).ok().unwrap();
        assert_eq!(changes.len(), 3, "{changes:?}");
        assert!(notes.is_empty());
        assert_eq!(v.get("model").str(), Some("opus"));
        assert_eq!(v.get("enabledPlugins").get("other@x").bool(), Some(true));
        assert_eq!(v.get("enabledPlugins").get(PLUGIN).bool(), Some(true));
        assert!(is_ours(v.get("extraKnownMarketplaces").get("lovieco")));
        assert_eq!(v.at(&["hooks", "PreToolUse"]).arr()[0].get("hooks").arr().len(), 1, "the user's own hook stays");
        assert!(v.at(&["hooks", "Stop"]).is_null(), "an emptied event goes");
        assert!(!v.at(&["hooks", "Notification"]).is_null(), "an event that was empty already stays");
        let again = v.clone();
        assert!(install_settings(&mut v, false, f).ok().unwrap().0.is_empty(), "idempotent");
        assert_eq!(v, again);
        // someone else's lovieco marketplace: kept with a warning, unless --force
        let mut w =
            json::parse(r#"{"extraKnownMarketplaces":{"lovieco":{"source":{"source":"directory","path":"/dev/al"},"autoUpdate":true}}}"#).unwrap();
        let (c, n) = install_settings(&mut w, false, f).ok().unwrap();
        assert_eq!(c, vec![format!("enabled {PLUGIN}")]);
        assert!(n[0].contains("directory: /dev/al"));
        let (c, _) = install_settings(&mut w, true, f).ok().unwrap();
        assert!(c[0].starts_with("re-pointed marketplace lovieco from directory: /dev/al"));
        assert_eq!(w.at(&["extraKnownMarketplaces", "lovieco", "autoUpdate"]).bool(), Some(true), "other fields stay");
        assert!(install_settings(&mut json::parse(r#"{"enabledPlugins":[]}"#).unwrap(), false, f).is_err());
        // uninstall
        let (c, _) = uninstall_settings(&mut v, false);
        assert_eq!(c.len(), 2, "{c:?}");
        assert!(v.get("extraKnownMarketplaces").is_null());
        assert_eq!(v.get("enabledPlugins").get("other@x").bool(), Some(true));
        assert!(uninstall_settings(&mut v, false).0.is_empty());
        let mut x = json::parse(r#"{"enabledPlugins":{"agent-lock@lovieco":true,"tool@lovieco":true},"extraKnownMarketplaces":{"lovieco":{"source":{"source":"github","repo":"lovieco/agent-lock"}}}}"#).unwrap();
        let (c, n) = uninstall_settings(&mut x, true);
        assert_eq!(c.len(), 1);
        assert!(n[0].contains("other @lovieco plugins"));
    }

    #[test]
    fn indentation_is_kept() {
        assert_eq!(detect_indent("{\n    \"a\": 1\n}"), "    ");
        assert_eq!(detect_indent("{\n\t\"a\": 1\n}"), "\t");
        assert_eq!(detect_indent("{\"a\":1}"), "  ");
        let v = json::parse(r#"{"a":{"b":[1,"x  y"]}}"#).unwrap();
        assert_eq!(reindent(&v.to_pretty(), "\t"), "{\n\t\"a\": {\n\t\t\"b\": [\n\t\t\t1,\n\t\t\t\"x  y\"\n\t\t]\n\t}\n}\n");
    }

    #[test]
    fn v1_skill_fingerprint() {
        let d = std::env::temp_dir().join(format!("al-cli-skill-{}-{}", sys::pid(), sys::now_ms()));
        let s = d.join(".claude/skills/agent-lock");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("SKILL.md"), "---\nname: agent-lock\ndescription: Shared checkout via sidecar .lock files\n---\nbody").unwrap();
        assert!(v1_skill(&d).is_some());
        std::fs::write(s.join("SKILL.md"), "---\nname: agent-lock\ndescription: claims, held until commit\n---\nRun agent-lock commit.").unwrap();
        assert!(v1_skill(&d).is_none(), "the plugin's own skill is never touched");
        std::fs::write(s.join("SKILL.md"), "no front matter; see .agent-lock/hooks/pre.mjs").unwrap();
        assert!(v1_skill(&d).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn alive(o: &Owner) -> Live {
        if o.pid == 11 { Live::Dead } else { Live::Alive }
    }

    #[test]
    fn deadlock_chains_and_blockers() {
        let ctx = Ctx { now: 1000, live: &alive };
        let mut st = State::default();
        let own = Some(Owner { pid: 10, start: 1 });
        for (id, agent) in [("a-s", "aaaaaaaa"), ("b-s", "bbbbbbbb"), ("c-s", "cccccccc")] {
            st.sessions.insert(id.into(), Session { id: id.into(), agent: agent.into(), owner: own, seen: 1000, ..Session::default() });
        }
        for (p, by) in [("a.txt", "aaaaaaaa"), ("b.txt", "bbbbbbbb"), ("c.txt", "cccccccc")] {
            st.put_claim(Claim { path: p.into(), by: by.into(), owner: own, edits: 1, at: 900, touched: 990, ..Claim::default() });
        }
        assert!(blocker(&st, "b.txt", "aaaaaaaa", &ctx, own).is_some());
        assert!(blocker(&st, "a.txt", "aaaaaaaa", &ctx, own).is_none(), "own");
        assert!(blocker(&st, "free.txt", "aaaaaaaa", &ctx, own).is_none());
        assert_eq!(rules::wait_chain(&st, "aaaaaaaa", "bbbbbbbb"), None);
        // b waits for c's file, c waits for a's file: a waiting for b closes the ring
        st.sessions.get_mut("b-s").unwrap().blocked.insert("bbbbbbbb".into(), Blocked { path: "c.txt".into(), holder: "cccccccc".into(), at: 999 });
        st.sessions.get_mut("c-s").unwrap().blocked.insert("cccccccc".into(), Blocked { path: "a.txt".into(), holder: "aaaaaaaa".into(), at: 999 });
        let chain = rules::wait_chain(&st, "aaaaaaaa", "bbbbbbbb").expect("a ring");
        // simplicity/S9: the real edges, not "bbbbbbbb is waiting for a.txt"
        let text = msg::deadlock("b.txt", "bbbbbbbb", &chain, "aaaaaaaa", None);
        assert!(text.contains("bbbbbbbb is waiting for c.txt (held by cccccccc), cccccccc is waiting for a.txt (yours)"), "{text}");
        // a dead holder's claim is no blocker
        st.claim_mut("b.txt").unwrap().owner = Some(Owner { pid: 11, start: 1 });
        assert!(blocker(&st, "b.txt", "aaaaaaaa", &ctx, own).is_none());
        // a finished subagent's file passes to its family
        st.put_claim(Claim { path: "s.txt".into(), by: "aaaaaaaa:11111111".into(), owner: own, edits: 1, ..Claim::default() });
        st.sessions.get_mut("a-s").unwrap().subs.insert("aaaaaaaa:11111111".into(), Sub { seen: 1000, done: true, busy: 0 });
        assert!(blocker(&st, "s.txt", "aaaaaaaa", &ctx, own).is_none());
        let line = holder_line(&st, st.claim("c.txt").unwrap(), &ctx);
        assert_eq!(line, "cccccccc (process alive, working; 1 edit uncommitted; claimed 1m ago, last active 0s ago)");
    }

    #[test]
    fn stuck_markers_are_cleared_only_when_their_process_is_gone() {
        let mut st = State::default();
        let live = commit_marker();
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let dead = format!("commit:{}:1", gone.id());
        gone.wait().unwrap();
        let put = |st: &mut State, p: &str, pending: &str| {
            st.put_claim(Claim { path: p.into(), by: "a".into(), pending: Some(pending.into()), reason: Some(WAIT.into()), ..Claim::default() })
        };
        put(&mut st, "live", &live);
        put(&mut st, "dead", &dead);
        put(&mut st, "pkg", &format!("pkg:{}:1", dead.split(':').nth(1).unwrap()));
        put(&mut st, "edit", "toolu_1");
        put(&mut st, "old", "commit"); // v3.0's bare marker reads as a tool call
        let mut got = unstick(&mut st);
        got.sort();
        assert_eq!(got, ["dead", "pkg"]);
        assert_eq!(st.claim("live").unwrap().pending.as_deref(), Some(live.as_str()));
        assert_eq!(st.claim("dead").unwrap().reason.as_deref(), Some(WAIT), "a marker leaves `reason` alone");
        assert_eq!(st.claim("edit").unwrap().pending.as_deref(), Some("toolu_1"), "tool calls are the hooks' business");
        assert_eq!(marker_kind(st.claim("live").unwrap()), Some("commit"));
        assert_eq!(marker_kind(st.claim("edit").unwrap()), None);
    }

    fn temp_repo() -> Repo {
        let base = std::env::temp_dir().join(format!("al-cli-repo-{}-{}", sys::pid(), sys::now_ms()));
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::write(base.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let top = std::fs::canonicalize(&base).unwrap();
        Repo { git_dir: top.join(".git"), top }
    }

    #[test]
    fn settling_a_commit() {
        let repo = temp_repo();
        let marker = commit_marker();
        let c = |p: &str, pending: Option<&str>, edits: u32| Claim {
            path: p.into(),
            by: "aaaaaaaa".into(),
            edits,
            pending: pending.map(str::to_string),
            reason: (p == "clean").then(|| WAIT.to_string()),
            ..Claim::default()
        };
        let fill = |st: &mut State| {
            st.claims.clear();
            st.put_claim(c("clean", Some(&marker), 1));
            st.put_claim(c("dirty", Some(&marker), 1));
            st.put_claim(c("new", Some(&marker), 0));
            st.put_claim(c("written", None, 2)); // an own edit replaced the marker and landed in the commit
            st.put_claim(c("edited", None, 2)); // … or landed after it
            st.put_claim(c("flight", Some("toolu_9"), 2)); // … or is still in flight
        };
        let names = ["clean", "dirty", "new", "written", "edited", "flight"];
        let marks = || names.iter().map(|p| Mark { path: p.to_string(), created: *p == "new" }).collect::<Vec<_>>();
        state::transact(&repo, fill).unwrap();
        let mut m = Marked { repo: &repo, me: "aaaaaaaa", marker: marker.clone(), marks: marks(), armed: true };
        // git confirms all but "dirty" and "edited" clean
        let clean: BTreeSet<String> = ["clean", "new", "written", "flight"].iter().map(|s| s.to_string()).collect();
        let (released, kept) = m.settle(Some(&clean), true);
        // gitcli/GIT-7: "written" is clean after the commit: released, not "changed again"
        assert_eq!(released, ["clean", "new", "written"]);
        assert_eq!(kept, ["dirty", "edited", "flight"]);
        let st = state::read(&repo).unwrap();
        let d = st.claim("dirty").unwrap();
        assert_eq!((d.pending.as_deref(), d.edits), (None, 1));
        assert_eq!(st.claim("flight").unwrap().pending.as_deref(), Some("toolu_9"));
        // a failed commit (or a panic: Drop) clears the markers and undoes created claims; reasons stay
        state::transact(&repo, fill).unwrap();
        drop(Marked { repo: &repo, me: "aaaaaaaa", marker: marker.clone(), marks: marks(), armed: true });
        let st = state::read(&repo).unwrap();
        assert!(st.claim("new").is_none());
        assert_eq!(st.claim("clean").unwrap().reason.as_deref(), Some(WAIT));
        assert!(st.claims.values().all(|c| c.pending.as_deref() != Some(marker.as_str())));
        let _ = std::fs::remove_dir_all(&repo.top);
    }

    #[test]
    fn tables_align() {
        let t = table(&["A", "BB"], &[vec!["xyz".into(), "1".into()], vec!["é".into(), "22".into()]]);
        assert_eq!(t, ["  A    BB", "  xyz  1", "  é    22"]);
    }

    #[test]
    fn help_lists_every_command() {
        for c in CMDS {
            assert!(!c.about.is_empty() && !c.about.contains('\n'), "{}", c.name);
        }
        assert!(find("help").is_some() && find("hold").is_none() && find("purge-all").is_none());
    }
}
