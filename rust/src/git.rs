//! Git: thin helpers over the real git binary (never a shim), and the
//! operations agent-lock performs: commit own files, sync, push, recovery.
//!
//! Read-only calls pass --no-optional-locks so they never take index.lock.
//! Pathspecs are always `:(literal)` entries (queries add `icase` under
//! core.ignorecase), on stdin (NUL-separated) where the command has
//! --pathspec-from-file, else as arguments in chunks (GIT_LITERAL_PATHSPECS
//! would leak into repo hooks).

use crate::sys::{self, Repo};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

pub struct Out {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn line(&self) -> String {
        self.text().trim().to_string()
    }
}

// Inherited variables that would redirect or reinterpret our git calls (agent-lock run from a git hook).
const STRIP_ENV: &[&str] = &[
    "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_PREFIX", "GIT_COMMON_DIR", "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS", "GIT_NOGLOB_PATHSPECS", "GIT_ICASE_PATHSPECS",
];

/// The one way to run the real git: in `dir`, without inherited variables
/// that would point it elsewhere (STRIP_ENV), never prompting. `env` adds
/// variables; `input` is fed on stdin. A death by signal n reads as 128+n.
pub fn run_in(dir: &Path, args: &[&str], env: &[(&str, &str)], input: Option<&[u8]>) -> io::Result<Out> {
    use std::os::unix::process::ExitStatusExt;
    let mut cmd = Command::new(git_path());
    cmd.current_dir(dir).args(args);
    for k in STRIP_ENV {
        cmd.env_remove(k);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0").env("GCM_INTERACTIVE", "never").env("GIT_EDITOR", ":").env("LC_ALL", "C").envs(env.iter().copied());
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    // Fed on a thread so a large input cannot deadlock against stdout.
    let feeder = input.map(|d| {
        let mut stdin = child.stdin.take().expect("piped");
        let d = d.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&d);
        })
    });
    let o = child.wait_with_output()?;
    if let Some(f) = feeder {
        let _ = f.join();
    }
    let status = o.status.code().unwrap_or_else(|| 128 + o.status.signal().unwrap_or(0));
    Ok(Out { status, stdout: o.stdout, stderr: String::from_utf8_lossy(&o.stderr).into_owned() })
}

/// `:(literal)<path>\0…` for --pathspec-from-file=- --pathspec-file-nul.
pub fn pathspecs(paths: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in paths {
        out.extend_from_slice(b":(literal)");
        out.extend_from_slice(p.as_bytes());
        out.push(0);
    }
    out
}

/// Pathspec arguments for status-like queries: `:(literal,icase)` under
/// core.ignorecase, so a path typed in another case still matches git's.
fn query_specs(repo: &Repo, paths: &[String]) -> Vec<String> {
    let magic = if repo.ignore_case() { ":(literal,icase)" } else { ":(literal)" };
    paths.iter().map(|p| format!("{magic}{p}")).collect()
}

/// `git <args> -- <query_specs>` over chunks of paths; stdout of each.
fn query(repo: &Repo, args: &[&str], paths: &[String]) -> io::Result<Vec<Out>> {
    let mut outs = Vec::new();
    for chunk in paths.chunks(CHUNK) {
        let specs = query_specs(repo, chunk);
        let mut a = args.to_vec();
        a.push("--");
        a.extend(specs.iter().map(String::as_str));
        let o = run_in(&repo.top, &a, &[], None)?;
        if !o.ok() {
            let what = args.iter().find(|a| !a.starts_with('-')).unwrap_or(&"");
            return Err(io::Error::other(format!("git {what} failed: {}", o.stderr.trim())));
        }
        outs.push(o);
    }
    Ok(outs)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub xy: String,      // porcelain v2 XY ('.' = unchanged), "??" untracked, "!!" ignored
    pub untracked: bool,
    pub ignored: bool,
    pub from: Option<String>, // rename source
}

const STATUS: [&str; 5] = ["--no-optional-locks", "status", "--porcelain=v2", "-z", "--untracked-files=all"];

/// `git status --porcelain=v2 -z --untracked-files=all [-- paths]`, keyed by
/// git's spelling of each path: compare through `State::key`. With `paths` =
/// Some(&[]) nothing is asked.
pub fn status(repo: &Repo, paths: Option<&[String]>) -> io::Result<BTreeMap<String, Entry>> {
    let mut out = BTreeMap::new();
    match paths {
        None => parse_status(&run_in(&repo.top, &STATUS, &[], None)?, &mut out)?,
        Some(p) => {
            for o in query(repo, &STATUS, p)? {
                parse_status(&o, &mut out)?;
            }
        }
    }
    Ok(out)
}

fn parse_status(o: &Out, out: &mut BTreeMap<String, Entry>) -> io::Result<()> {
    if !o.ok() {
        return Err(io::Error::other(format!("git status failed: {}", o.stderr.trim())));
    }
    let text = String::from_utf8_lossy(&o.stdout);
    let mut recs = text.split('\0').filter(|r| !r.is_empty());
    while let Some(r) = recs.next() {
        let (kind, rest) = r.split_at(1);
        match kind {
            "1" => {
                // 1 XY sub mH mI mW hH hI path
                let f: Vec<&str> = rest.trim_start().splitn(8, ' ').collect();
                if f.len() == 8 {
                    out.insert(f[7].to_string(), Entry { xy: f[0].to_string(), ..Entry::default() });
                }
            }
            "2" => {
                // 2 XY sub mH mI mW hH hI Xscore path \0 origPath
                let f: Vec<&str> = rest.trim_start().splitn(9, ' ').collect();
                let orig = recs.next().map(str::to_string);
                if f.len() == 9 {
                    out.insert(f[8].to_string(), Entry { xy: f[0].to_string(), from: orig.clone(), ..Entry::default() });
                    if let Some(o) = orig {
                        out.entry(o).or_insert(Entry { xy: "D.".into(), ..Entry::default() });
                    }
                }
            }
            "u" => {
                let f: Vec<&str> = rest.trim_start().splitn(10, ' ').collect();
                if f.len() == 10 {
                    out.insert(f[9].to_string(), Entry { xy: f[0].to_string(), ..Entry::default() });
                }
            }
            "?" => {
                let p = rest.strip_prefix(' ').unwrap_or(rest).to_string();
                match out.get_mut(&p) {
                    Some(e) => e.untracked = true, // e.g. `git rm --cached` + still on disk
                    None => {
                        out.insert(p, Entry { xy: "??".into(), untracked: true, ..Entry::default() });
                    }
                }
            }
            "!" => {
                let p = rest.strip_prefix(' ').unwrap_or(rest).to_string();
                match out.get_mut(&p) {
                    Some(e) => e.ignored = true,
                    None => {
                        out.insert(p, Entry { xy: "!!".into(), ignored: true, ..Entry::default() });
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The subset of `paths` with changes vs HEAD (modified, added, deleted,
/// untracked), in git's spelling: compare through `State::key`. Never decide a
/// release on its absence: see `clean`.
pub fn dirty(repo: &Repo, paths: &[String]) -> io::Result<BTreeSet<String>> {
    Ok(status(repo, Some(paths))?.into_iter().filter(|(_, e)| !e.ignored).map(|(p, _)| p).collect())
}

/// The subset of `paths` git positively reports clean vs HEAD: no status
/// entry under any spelling of it, and git knows the path under this very
/// spelling (tracked, or ignored) or nothing is there at all (gone from disk,
/// index and HEAD). A spelling git does not confirm stays out, so callers that
/// release only these never drop a dirty claim. Any error: nothing is clean.
pub fn clean(repo: &Repo, paths: &[String]) -> BTreeSet<String> {
    let fold = repo.ignore_case();
    let key = |p: &str| if fold { p.to_lowercase() } else { p.to_string() };
    // NFC and NFD spellings of one name share their ASCII letters: a dirty
    // entry blocks every path with its skeleton (a superset, so fail safe).
    let skel = |p: &str| -> String { p.chars().filter(char::is_ascii).map(|c| if fold { c.to_ascii_lowercase() } else { c }).collect() };
    let mut st = BTreeMap::new();
    let mut args = STATUS.to_vec();
    args.push("--ignored=matching");
    let Ok(outs) = query(repo, &args, paths) else { return BTreeSet::new() };
    for o in &outs {
        if parse_status(o, &mut st).is_err() {
            return BTreeSet::new();
        }
    }
    let dirty: BTreeSet<String> = st.iter().filter(|(_, e)| e.xy != "!!").map(|(p, _)| skel(p)).collect();
    let ignored: BTreeSet<String> = st.iter().filter(|(_, e)| e.xy == "!!").map(|(p, _)| key(p)).collect();
    let (mut out, mut ask) = (BTreeSet::new(), Vec::new());
    for p in paths.iter().filter(|p| !dirty.contains(&skel(p))) {
        if ignored.contains(&key(p)) || std::fs::symlink_metadata(repo.abs(p)).is_err() {
            out.insert(p.clone());
        } else {
            ask.push(p.clone());
        }
    }
    if !ask.is_empty() {
        let Ok(outs) = query(repo, &["ls-files", "-z", "--cached"], &ask) else { return out };
        let known: BTreeSet<String> = outs.iter().flat_map(|o| zsplit(&o.stdout)).map(|p| key(&p)).collect();
        out.extend(ask.into_iter().filter(|p| known.contains(&key(p))));
    }
    out
}

/// Committer time of refs/stash (a hook that stashed moves it).
pub fn stash_time(repo: &Repo) -> Option<u64> {
    let o = run_in(&repo.top, &["--no-optional-locks", "log", "-1", "--format=%ct", "refs/stash"], &[], None).ok()?;
    if o.ok() { o.line().parse().ok() } else { None }
}

/// git's (major, minor), probed once per process.
pub fn version() -> Option<(u32, u32)> {
    static V: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let o = run_in(Path::new("/"), &["--version"], &[], None).ok()?;
        let t = o.line();
        let mut it = t.split_whitespace().nth(2)?.split('.');
        Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
    })
}

#[cfg(test)]
pub(crate) mod helper_tests {
    use super::*;

    // Hermetic git for the setup calls (passed explicitly: tests run in parallel threads).
    const ENV: &[(&str, &str)] = &[
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("HOME", "/nonexistent-agent-lock-test"),
        ("XDG_CONFIG_HOME", "/nonexistent-agent-lock-test"),
        ("GIT_AUTHOR_NAME", "Test"),
        ("GIT_AUTHOR_EMAIL", "test@example.com"),
        ("GIT_COMMITTER_NAME", "Test"),
        ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ];

    pub(crate) fn git(r: &Repo, args: &[&str]) -> String {
        let o = run_in(&r.top, args, ENV, None).unwrap();
        assert!(o.ok(), "git {args:?}: {}", o.stderr);
        o.text()
    }

    /// A repo with one commit of `files` (written byte-exact, so a test may
    /// store an NFD name on disk).
    pub(crate) fn repo(files: &[(&str, &str)]) -> Repo {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let top = std::env::temp_dir().join(format!("al3-gith-{}-{n}", sys::pid()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(&top).unwrap();
        let top = std::fs::canonicalize(top).unwrap();
        let r = Repo { git_dir: top.join(".git"), top };
        git(&r, &["init", "-q", "-b", "main"]);
        for (f, c) in files {
            let p = r.abs(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, c).unwrap();
        }
        git(&r, &["add", "-A"]);
        git(&r, &["commit", "-q", "--allow-empty", "-m", "init"]);
        r
    }

    fn s(a: &[&str]) -> Vec<String> {
        a.iter().map(|x| x.to_string()).collect()
    }

    pub(crate) const NFC: &str = "caf\u{e9}.txt";
    pub(crate) const NFD: &str = "cafe\u{301}.txt";

    #[test]
    fn status_and_dirty_match_a_path_typed_in_another_case() {
        // gitcli/GIT-2: ':(literal)A.TXT' never matches git's a.txt
        let r = repo(&[("a.txt", "a\n")]);
        std::fs::write(r.abs("a.txt"), "changed\n").unwrap();
        if r.ignore_case() {
            assert_eq!(dirty(&r, &s(&["A.TXT"])).unwrap(), BTreeSet::from(["a.txt".to_string()]));
            assert!(status(&r, Some(&s(&["A.TXT"]))).unwrap().contains_key("a.txt"));
        }
        assert_eq!(dirty(&r, &s(&["a.txt"])).unwrap().len(), 1);
    }

    #[test]
    fn clean_is_only_what_git_confirms() {
        // state/F1 fail safe: a release needs git's positive word
        let r = repo(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("gone.txt", "g\n"), (".gitignore", "*.log\n")]);
        std::fs::write(r.abs("b.txt"), "changed\n").unwrap();
        std::fs::write(r.abs("x.log"), "log\n").unwrap();
        std::fs::write(r.abs("new.txt"), "new\n").unwrap();
        git(&r, &["rm", "-q", "gone.txt"]);
        git(&r, &["commit", "-q", "-m", "rm"]);
        let all = s(&["a.txt", "b.txt", "x.log", "gone.txt", "new.txt", "never.txt"]);
        assert_eq!(clean(&r, &all), BTreeSet::from(s(&["a.txt", "x.log", "gone.txt", "never.txt"]).into_iter().collect::<BTreeSet<_>>()));
        if r.ignore_case() {
            assert!(clean(&r, &s(&["A.TXT"])).contains("A.TXT"), "git confirms it under ignorecase");
            assert!(clean(&r, &s(&["B.TXT"])).is_empty());
        }
        assert!(clean(&r, &[]).is_empty());
    }

    #[test]
    fn clean_never_confirms_another_unicode_spelling() {
        // state/F1, gitcli/GIT-1: git prints NFC, the claim may hold NFD
        if !cfg!(target_os = "macos") {
            return;
        }
        let r = repo(&[(NFD, "x\n")]); // stored NFD on disk, as Finder would
        assert_eq!(git(&r, &["ls-files", "-z", NFD]), format!("{NFC}\0"), "git precomposes");
        assert!(clean(&r, &s(&[NFC])).contains(NFC));
        assert!(clean(&r, &s(&[NFD])).is_empty(), "not git's spelling: not confirmed");
        std::fs::write(r.abs(NFD), "changed\n").unwrap();
        assert!(clean(&r, &s(&[NFC, NFD])).is_empty());
        assert!(!dirty(&r, &s(&[NFD])).unwrap().contains(NFD), "dirty names git's spelling, so only clean can decide");
    }

    #[test]
    fn run_in_ignores_inherited_repo_variables() {
        // simplicity/S3: agent-lock started from a git hook inherits GIT_DIR / GIT_INDEX_FILE
        if let Some(top) = std::env::var_os("AGENT_LOCK_TEST_TOP") {
            let o = run_in(Path::new(&top), &["rev-parse", "--show-toplevel"], &[], None).unwrap();
            assert!(o.ok(), "{}", o.stderr);
            assert_eq!(Path::new(&o.line()), Path::new(&top));
            return;
        }
        let r = repo(&[("a.txt", "a\n")]);
        let o = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "git::helper_tests::run_in_ignores_inherited_repo_variables", "--test-threads=1"])
            .env("AGENT_LOCK_TEST_TOP", &r.top)
            .env("GIT_DIR", "/nonexistent-agent-lock-test")
            .env("GIT_INDEX_FILE", "/nonexistent-agent-lock-test/index")
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
        assert!(String::from_utf8_lossy(&o.stdout).contains("1 passed"));
    }

    #[test]
    fn run_in_reports_a_signal_death_as_128_plus_n() {
        let r = repo(&[]);
        let o = run_in(&r.top, &["-c", "alias.boom=!kill -9 $PPID", "boom"], &[], None).unwrap();
        assert_eq!(o.status, 128 + 9);
    }
}

// ── operations: commit, sync, push, recovery ──────────────────────────────
//
// commit_paths commits exactly the given paths. Hooks run first, outside
// locks.git, on a private index (HEAD + those paths): a write-tree gate
// refuses anything a hook staged beyond them, and a before/after snapshot of
// refs/stash and of every other dirty file reports a hook that clobbered
// other agents' work. Then, under locks.git, `git commit --only --no-verify`.
//
// sync fetches into the tracking ref (never FETCH_HEAD), refuses a rewritten
// upstream unless accepted, replays local commits with merge-tree +
// commit-tree, and installs the new HEAD + index as a compare-and-swap under
// <gitdir>/index.lock: hard-linked index copy, HEAD re-checked, two-way
// read-tree, update-ref with the old value. locks.sync records the install
// (H, tip, paths, index copy, lock inode) before it starts, so an install
// cut short by a kill is rolled back or finished by the next operation.
//
// push pushes HEAD's oid with a lease on the last integrated upstream tip,
// classifies --porcelain output, and on a race syncs and retries.

use crate::rules;
use crate::state::{self, OpLock, State, SyncLock, Upstream};
use crate::sys::{Live, Owner};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MIN_VERSION: (u32, u32) = (2, 40);
const REMOTE: &str = "origin";
const GIT_WAIT: Duration = if cfg!(test) { Duration::from_millis(1500) } else { Duration::from_secs(60) };
const GIT_ABANDON: u64 = 600; // a locks.git holder alive but silent this long is hung
const INDEX_LOCK_WAIT: Duration = if cfg!(test) { Duration::from_millis(1500) } else { Duration::from_secs(10) };
const CHUNK: usize = 500;
const HASH_MAX: u64 = 16 << 20; // bigger files are compared by lstat only
const SCRIPT_MAX: u64 = 256 << 10;
const PUSH_TRIES: u32 = 5;
const PUSH_RETRY: &[&str] = &["fetch first", "non-fast-forward", "stale info", "cannot lock ref", "failed to update ref", "incorrect old value provided"];

type Res<T> = Result<T, String>;

/// One repo plus extra environment for every git call (tests pass a hermetic config).
#[derive(Clone, Copy)]
struct G<'a> {
    repo: &'a Repo,
    env: &'a [(&'a str, &'a str)],
}

impl<'a> G<'a> {
    fn new(repo: &'a Repo) -> G<'a> {
        G { repo, env: &[] }
    }

    fn exec(&self, args: &[&str], env: &[(&str, &str)], input: Option<&[u8]>) -> io::Result<Out> {
        let env: Vec<(&str, &str)> = self.env.iter().chain(env).copied().collect();
        run_in(&self.repo.top, args, &env, input)
    }

    fn run(&self, args: &[&str]) -> io::Result<Out> {
        self.exec(args, &[], None)
    }

    fn ro(&self, args: &[&str]) -> io::Result<Out> {
        let mut a = vec!["--no-optional-locks"];
        a.extend_from_slice(args);
        self.exec(&a, &[], None)
    }

    /// Trimmed stdout of a read-only call that succeeded.
    fn get(&self, args: &[&str]) -> Option<String> {
        self.ro(args).ok().filter(Out::ok).map(|o| o.line())
    }
}

/// The git program itself (resolved once: see `sys::git_bin`).
fn git_path() -> &'static Path {
    static P: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    P.get_or_init(sys::git_bin)
}

fn said(o: &Out) -> String {
    format!("{}{}", o.stderr, String::from_utf8_lossy(&o.stdout)).trim().to_string()
}

fn must(r: io::Result<Out>, what: &str) -> Res<Out> {
    match r {
        Ok(o) if o.ok() => Ok(o),
        Ok(o) => Err(format!("git {what} failed: {}", said(&o))),
        Err(e) => Err(format!("git {what}: {e}")),
    }
}

fn zsplit(b: &[u8]) -> Vec<String> {
    b.split(|c| *c == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
}

fn literal(paths: &[String]) -> Vec<String> {
    paths.iter().map(|p| format!(":(literal){p}")).collect()
}

/// `git <args> -- :(literal)<p>…` over chunks of paths; stdout of each.
fn per_chunk(g: G, args: &[&str], env: &[(&str, &str)], paths: &[String], what: &str) -> Res<Vec<Out>> {
    let mut outs = Vec::new();
    for c in paths.chunks(CHUNK) {
        let specs = literal(c);
        let mut a: Vec<&str> = args.to_vec();
        a.push("--");
        a.extend(specs.iter().map(String::as_str));
        outs.push(must(g.exec(&a, env, None), what)?);
    }
    Ok(outs)
}

fn rels(paths: &[String]) -> String {
    let mut s = paths.iter().take(10).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > 10 {
        s.push_str(&format!(" and {} more", paths.len() - 10));
    }
    s
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}

fn lexists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

fn ino(p: &Path) -> Option<u64> {
    std::fs::symlink_metadata(p).ok().map(|m| m.ino())
}

fn unlink(p: &Path) {
    let _ = std::fs::remove_file(p);
}

/// Remove `p` only while it is still the file we created (same inode).
fn unlink_if(p: &Path, want: Option<u64>) -> bool {
    if want.is_some() && ino(p) == want {
        return std::fs::remove_file(p).is_ok();
    }
    false
}

fn lock_of(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

fn tag() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(sys::now_ms());
    format!("{}-{:010x}", sys::pid(), h.finish() & 0xff_ffff_ffff)
}

fn valid_rel(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('/') && !p.contains('\0') && !p.split('/').any(|c| c == ".." || c == ".git")
}

/// Removes files when dropped.
struct Litter(Vec<PathBuf>);

impl Drop for Litter {
    fn drop(&mut self) {
        for p in &self.0 {
            unlink(p);
        }
    }
}

fn need_version() -> Res<()> {
    match version() {
        Some(v) if v >= MIN_VERSION => Ok(()),
        Some((a, b)) => Err(format!("agent-lock needs git >= {}.{} (found {a}.{b})", MIN_VERSION.0, MIN_VERSION.1)),
        None => Err(format!("cannot run git ({})", git_path().display())),
    }
}

// Test-only stage hooks: a test can act (or "die") at a named point.
#[cfg(test)]
type StageHook = Option<Box<dyn FnMut(&str)>>;

#[cfg(test)]
thread_local! {
    static AT: std::cell::RefCell<StageHook> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn at(stage: &str) {
    let f = AT.with(|h| h.borrow_mut().take());
    if let Some(mut f) = f {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(stage)));
        AT.with(|h| *h.borrow_mut() = Some(f));
        if let Err(p) = r {
            std::panic::resume_unwind(p);
        }
    }
}

#[cfg(not(test))]
#[inline(always)]
fn at(_stage: &str) {}

// ── signals ───────────────────────────────────────────────────────────────

const SIGNALS: [i32; 3] = [1, 2, 15]; // SIGHUP, SIGINT, SIGTERM
static QUIET: std::sync::Mutex<(usize, [usize; 3])> = std::sync::Mutex::new((0, [0; 3]));

/// SIGINT/SIGTERM/SIGHUP ignored while alive (children inherit it), so a
/// Ctrl-C cannot cut an install in half. Only SIGKILL can: see `recover`.
struct Quiet;

fn quiet() -> Quiet {
    let mut q = QUIET.lock().unwrap_or_else(|e| e.into_inner());
    if q.0 == 0 {
        for (i, s) in SIGNALS.iter().enumerate() {
            q.1[i] = sys::signal_ignore(*s);
        }
    }
    q.0 += 1;
    Quiet
}

impl Drop for Quiet {
    fn drop(&mut self) {
        let mut q = QUIET.lock().unwrap_or_else(|e| e.into_inner());
        q.0 -= 1;
        if q.0 == 0 {
            for (i, s) in SIGNALS.iter().enumerate() {
                sys::signal_restore(*s, q.1[i]);
            }
        }
    }
}

// ── read-only queries ─────────────────────────────────────────────────────

fn rev(g: G, spec: &str) -> Option<String> {
    g.get(&["rev-parse", "-q", "--verify", &format!("{spec}^{{commit}}")])
}

fn head_ref(g: G) -> Option<String> {
    g.get(&["symbolic-ref", "-q", "HEAD"])
}

fn is_ancestor(g: G, a: &str, b: &str) -> bool {
    a == b || g.ro(&["merge-base", "--is-ancestor", a, b]).is_ok_and(|o| o.ok())
}

/// HEAD's oid (None while unborn) and the ref it points at (None when detached).
fn head_info(g: G) -> (Option<String>, Option<String>) {
    match g.ro(&["rev-parse", "HEAD", "--symbolic-full-name", "HEAD"]) {
        Ok(o) if o.ok() => {
            let t = o.text();
            let mut l = t.lines();
            (l.next().map(str::to_string), l.next().filter(|r| r.starts_with("refs/")).map(str::to_string))
        }
        _ => (None, head_ref(g)),
    }
}

fn has_commit(g: G, oid: &str) -> bool {
    g.ro(&["cat-file", "-e", &format!("{oid}^{{commit}}")]).is_ok_and(|o| o.ok())
}

fn empty_tree(g: G) -> String {
    g.get(&["hash-object", "-t", "tree", "/dev/null"]).unwrap_or_else(|| "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into())
}

/// Porcelain v2 status with renames split (a `git mv` is `D.` + `A.`); with
/// `paths` (`query_specs`: any case under core.ignorecase), ignored files are
/// reported too.
fn status_of(g: G, paths: Option<&[String]>, ignored: bool) -> Res<BTreeMap<String, Entry>> {
    let mut map = BTreeMap::new();
    let mut base = vec!["--no-optional-locks", "status", "--porcelain=v2", "-z", "--untracked-files=all", "--no-renames"];
    match paths {
        None => {
            let o = g.exec(&base, &[], None).map_err(|e| e.to_string())?;
            parse_status(&o, &mut map).map_err(|e| e.to_string())?;
        }
        Some(p) => {
            if ignored {
                base.push("--ignored=matching");
            }
            for c in p.chunks(CHUNK) {
                let specs = query_specs(g.repo, c);
                let mut a = base.clone();
                a.push("--");
                a.extend(specs.iter().map(String::as_str));
                let o = g.exec(&a, &[], None).map_err(|e| e.to_string())?;
                parse_status(&o, &mut map).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(map)
}

struct Ent {
    rel: String,
    a: Option<(String, String)>, // (mode, oid) before; None = absent
    b: Option<(String, String)>, // after
}

/// Every path that differs between two trees, both sides.
fn tree_diff(g: G, from: &str, to: &str) -> Res<Vec<Ent>> {
    let o = must(g.ro(&["diff-tree", "-r", "-z", "--no-renames", from, to]), "diff-tree")?;
    let toks: Vec<&[u8]> = o.stdout.split(|c| *c == 0).collect();
    let side = |m: &str, oid: &str| if m.bytes().all(|c| c == b'0') { None } else { Some((m.to_string(), oid.to_string())) };
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < toks.len() {
        let meta = String::from_utf8_lossy(toks[i]);
        let f: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        if !meta.starts_with(':') || f.len() < 5 {
            break;
        }
        out.push(Ent { rel: String::from_utf8_lossy(toks[i + 1]).into_owned(), a: side(f[0], f[2]), b: side(f[1], f[3]) });
        i += 2;
    }
    Ok(out)
}

/// Stage-0 entries of the shared index for these paths whose worktree file
/// git manages (skip-worktree entries of a sparse checkout left out): path →
/// (mode, oid).
fn index_entries(g: G, paths: &[String]) -> Res<BTreeMap<String, (String, String)>> {
    let mut m = BTreeMap::new();
    for o in per_chunk(g, &["--no-optional-locks", "ls-files", "-s", "-t", "-z"], &[], paths, "ls-files")? {
        for t in zsplit(&o.stdout) {
            let Some((meta, path)) = t.split_once('\t') else { continue };
            // "<tag> <mode> <oid> <stage>"; tag S = skip-worktree
            let f: Vec<&str> = meta.split(' ').collect();
            if f.len() == 4 && f[0] != "S" && f[3] == "0" {
                m.insert(path.to_string(), (f[1].to_string(), f[2].to_string()));
            }
        }
    }
    Ok(m)
}

/// `index.lock … exists` from a concurrent git process: back off ≤ 10 s.
fn retry_index_lock(f: impl Fn() -> io::Result<Out>) -> io::Result<Out> {
    let deadline = Instant::now() + INDEX_LOCK_WAIT;
    let mut pause = Duration::from_millis(25);
    loop {
        let r = f()?;
        if r.ok() || !r.stderr.contains("index.lock") || Instant::now() >= deadline {
            return Ok(r);
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(500));
    }
}

// ── locks.git ─────────────────────────────────────────────────────────────

/// Serializes agent-lock's own commits, syncs and pushes in one checkout.
/// Released on drop; a dead holder's entry is free (and pruned).
struct GitLock<'a> {
    repo: &'a Repo,
    me: Option<Owner>,
}

fn lock_git<'a>(g: &G<'a>, by: &str, what: &str) -> Res<GitLock<'a>> {
    let me = sys::self_owner();
    let deadline = Instant::now() + GIT_WAIT;
    let mut pause = Duration::from_millis(20);
    loop {
        let now = sys::now();
        let r = state::transact(g.repo, |st| {
            if let Some(l) = &st.locks.git {
                let free = (me.is_some() && l.owner == me)
                    || l.owner.is_some_and(|o| sys::liveness(&o) == Live::Dead)
                    || now.saturating_sub(l.since) > GIT_ABANDON;
                if !free {
                    return Some(l.clone());
                }
            }
            st.locks.git = Some(OpLock { by: by.to_string(), owner: me, since: now, what: what.to_string() });
            None
        });
        match r {
            Ok(None) => return Ok(GitLock { repo: g.repo, me }),
            Ok(Some(l)) if Instant::now() >= deadline => {
                let pid = l.owner.map(|o| o.pid.to_string()).unwrap_or_else(|| "?".into());
                return Err(format!(
                    "another agent-lock {} (by {}, pid {pid}) has been running for {}s; retry shortly",
                    if l.what.is_empty() { "git operation" } else { &l.what },
                    l.by,
                    now.saturating_sub(l.since)
                ));
            }
            Ok(Some(_)) => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(250));
            }
            Err(e) => return Err(format!("cannot update {}: {e}", g.repo.state_file().display())),
        }
    }
}

impl Drop for GitLock<'_> {
    fn drop(&mut self) {
        let me = self.me;
        let _ = state::transact(self.repo, |st| {
            if st.locks.git.as_ref().is_some_and(|l| l.owner == me) {
                st.locks.git = None;
            }
        });
    }
}

/// The git dir shared by all worktrees (refs, hooks).
fn common_dir(repo: &Repo) -> PathBuf {
    std::fs::read_to_string(repo.git_dir.join("commondir")).map(|c| repo.git_dir.join(c.trim())).unwrap_or_else(|_| repo.git_dir.clone())
}

// ── hooks: presence, dangerous scripts, damage ────────────────────────────

/// The hooks directory, and whether git would run a pre-commit or
/// commit-msg hook (from it, or configured as `hook.*`). One git call.
fn hooks(g: G) -> (PathBuf, bool) {
    let text = g.get(&["config", "--get-regexp", r"^(core\.hookspath|hook\..*)$"]).unwrap_or_default();
    let (mut path, mut configured) = (None, false);
    for l in text.lines() {
        let (k, v) = l.split_once(' ').unwrap_or((l, ""));
        if k == "core.hookspath" {
            path = Some(v.to_string());
        } else {
            configured |= k.starts_with("hook.");
        }
    }
    let dir = match path {
        Some(p) => match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
            (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
            _ => g.repo.top.join(p),
        },
        None => common_dir(g.repo).join("hooks"),
    };
    let present = configured || ["pre-commit", "commit-msg"].iter().any(|n| sys::is_executable(&dir.join(n)));
    (dir, present)
}

const STASH: &str = "runs `git stash` (hides other agents' changes)";
const CHECKOUT: &str = "runs `git checkout -- .` (reverts the worktree)";
const RESET: &str = "runs `git reset --hard`";
const PRE_COMMIT_COM: &str = "pre-commit.com framework (stashes unstaged changes during the hook)";
const LINT_STAGED: &str = "runs lint-staged without --no-stash (stashes unstaged changes)";

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn ends_word(w: &str, name: &str) -> bool {
    w.strip_suffix(name).is_some_and(|pre| pre.chars().last().is_none_or(|c| !word_char(c)))
}

fn starts_word(w: &str, name: &str) -> bool {
    w.strip_prefix(name).is_some_and(|rest| rest.chars().next().is_none_or(|c| !word_char(c)))
}

fn has_word(w: &str, name: &str) -> bool {
    w.match_indices(name).any(|(i, _)| {
        w[..i].chars().last().is_none_or(|c| !word_char(c)) && w[i + name.len()..].chars().next().is_none_or(|c| !word_char(c))
    })
}

fn unquote(w: &str) -> &str {
    for q in ['"', '\''] {
        if w.len() >= 2 && w.starts_with(q) && w.ends_with(q) {
            return &w[1..w.len() - 1];
        }
    }
    w
}

/// Shell-ish words of a script. Comments count too: a hook that mentions
/// `git stash` is refused (the agent runs the checks itself) rather than risked.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        for w in line.split(|c: char| c.is_whitespace() || ";&|()<>{}`".contains(c)) {
            let w = w.trim_matches(|c| c == '"' || c == '\'');
            if !w.is_empty() {
                out.push(w.to_string());
            }
        }
    }
    out
}

/// Why a hook script rewrites the shared worktree.
fn danger(text: &str) -> Vec<&'static str> {
    let w = words(text);
    let mut why = Vec::new();
    let mut lint = false;
    for i in 0..w.len() {
        if ends_word(&w[i], "git") {
            let mut j = i + 1;
            while let Some(x) = w.get(j) {
                match x.as_str() {
                    "-c" | "-C" => j += 2,
                    x if x.starts_with("--") => j += 1,
                    _ => break,
                }
            }
            if w.get(j).is_some_and(|x| starts_word(x, "stash")) {
                why.push(STASH);
            }
        }
        if ends_word(&w[i], "checkout") {
            let mut j = i + 1;
            if w.get(j).is_some_and(|x| x == "-f") {
                j += 1;
            }
            if w.get(j).is_some_and(|x| x == "--") {
                j += 1;
            }
            if w.get(j).is_some_and(|x| x == ".") {
                why.push(CHECKOUT);
            }
        }
        if ends_word(&w[i], "reset") && w.get(i + 1).is_some_and(|x| starts_word(x, "--hard")) {
            why.push(RESET);
        }
        lint |= has_word(&w[i], "lint-staged");
    }
    if text.contains("File generated by pre-commit") {
        why.push(PRE_COMMIT_COM);
    }
    if lint && !text.contains("--no-stash") {
        why.push(LINT_STAGED);
    }
    let mut seen = BTreeSet::new();
    why.retain(|r| seen.insert(*r));
    why
}

/// package.json scripts a hook runs (`npm run x`, `yarn x`, …), one level.
fn scripts_called(text: &str, pkg: &crate::json::Json) -> String {
    let scripts = pkg.get("scripts");
    let w = words(text);
    let mut found = Vec::new();
    for i in 0..w.len() {
        if !["npm", "pnpm", "yarn", "bun"].iter().any(|m| ends_word(&w[i], m)) {
            continue;
        }
        let mut j = i + 1;
        if w.get(j).is_some_and(|x| x == "run" || x == "run-script") {
            j += 1;
        }
        if w.get(j).is_some_and(|x| x == "-s" || x == "--silent") {
            j += 1;
        }
        if let Some(s) = w.get(j).and_then(|n| scripts.get(n).str()) {
            found.push(s.to_string());
        }
    }
    found.join("\n")
}

fn read_small(p: &Path) -> Option<String> {
    let m = std::fs::metadata(p).ok()?;
    if !m.is_file() || m.len() > SCRIPT_MAX {
        return None;
    }
    std::fs::read_to_string(p).ok()
}

/// The recipe of `make [-C dir] [-f file] [target]` (default: the first target).
fn make_recipe(args: &[String], top: &Path) -> Option<(String, String)> {
    let (mut dir, mut file, mut target) = (top.to_path_buf(), None::<String>, None::<String>);
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-C" | "--directory" => {
                i += 1;
                dir = dir.join(args.get(i).map(String::as_str).unwrap_or("."));
            }
            "-f" | "--file" | "--makefile" => {
                i += 1;
                file = args.get(i).cloned();
            }
            _ if !a.starts_with('-') && !a.contains('=') && target.is_none() => target = Some(a.to_string()),
            _ => {}
        }
        i += 1;
    }
    let all = format!("{}{}{}", dir.display(), file.as_deref().unwrap_or(""), target.as_deref().unwrap_or(""));
    if all.contains('$') || all.contains('`') {
        return None;
    }
    let names: Vec<String> = std::fs::read_dir(&dir).ok()?.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect();
    let mf = match file {
        Some(f) => dir.join(f),
        None => dir.join(["GNUmakefile", "makefile", "Makefile"].iter().find(|n| names.iter().any(|x| x == *n))?),
    };
    let text = read_small(&mf)?;
    let (mut body, mut on, mut first) = (Vec::new(), false, None::<String>);
    for l in text.lines() {
        if on && l.starts_with('\t') {
            body.push(l[1..].to_string());
            continue;
        }
        if on && l.trim().is_empty() {
            continue;
        }
        on = false;
        // a rule line: `targets: prereqs [; recipe]` (not `x := y` or `x ::= y`)
        let Some(c) = l.find(':') else { continue };
        let names = &l[..c];
        if names.is_empty() || names.starts_with(['\t', '#', '=']) || names.contains(['#', '=']) {
            continue;
        }
        let rest = l[c + 1..].strip_prefix(':').unwrap_or(&l[c + 1..]);
        if rest.starts_with('=') {
            continue;
        }
        let names: Vec<&str> = names.split_whitespace().collect();
        if first.is_none() {
            first = names.iter().find(|n| !n.starts_with('.')).map(|n| n.to_string());
        }
        let want = target.clone().or_else(|| first.clone());
        if want.is_some_and(|t| names.contains(&t.as_str())) {
            on = true;
            if let Some((_, r)) = rest.split_once(';') {
                body.push(r.to_string());
            }
        }
    }
    if body.is_empty() {
        return None;
    }
    Some((format!("{}:{}", mf.display(), target.or(first).unwrap_or_default()), body.join("\n")))
}

/// Scripts and make targets a hook hands off to, one level deep: `sh x.sh`,
/// `exec ./x`, `. lib.sh`, `node check.js`, `scripts/x.sh`, `make lint`.
fn delegated(text: &str, top: &Path) -> Vec<(String, String)> {
    const PREFIX: &[&str] = &["exec", "command", "env", "nohup", "time", "then", "do", "else", "if", "!"];
    const RUNNERS: &[&str] =
        &["sh", "bash", "zsh", "dash", "ksh", "source", ".", "node", "python", "python3", "ruby", "perl", "bun", "deno", "tsx", "ts-node"];
    let mut found: Vec<(String, String)> = Vec::new();
    let assign = |w: &str| {
        w.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    };
    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        let line = line.replace("&&", ";").replace("||", ";");
        for cmd in line.split(|c: char| ";&|(){}`".contains(c)) {
            let w: Vec<String> = cmd.split_whitespace().map(|x| unquote(x).to_string()).collect();
            let (mut i, mut runner) = (0, false);
            loop {
                while i < w.len() && assign(&w[i]) {
                    i += 1;
                }
                if i < w.len() && PREFIX.contains(&w[i].as_str()) {
                    i += 1;
                    continue;
                }
                if i < w.len() && RUNNERS.contains(&w[i].as_str()) {
                    runner = true;
                    i += 1;
                    while i < w.len() && w[i].starts_with('-') {
                        i += 1;
                    }
                    continue;
                }
                break;
            }
            let Some(word) = w.get(i) else { continue };
            if !runner && (word == "make" || word == "gmake") {
                if let Some(m) = make_recipe(&w[i + 1..], top) {
                    found.push(m);
                }
                continue;
            }
            if !(runner || word.contains('/')) || word.contains(['$', '`', '*', '?']) {
                continue;
            }
            let f = top.join(word);
            if let Some(t) = read_small(&f) {
                found.push((f.display().to_string(), t));
            }
        }
    }
    found
}

/// Hooks that would rewrite the shared worktree: `<hook>: <why>` each.
fn dangerous_hooks(g: G, dir: &Path) -> Vec<String> {
    let top = &g.repo.top;
    let husky = top.join(".husky");
    let pkg = std::fs::read_to_string(top.join("package.json")).ok().and_then(|t| crate::json::parse(&t).ok()).unwrap_or(crate::json::Json::Null);
    let mut found = Vec::new();
    for name in ["pre-commit", "commit-msg"] {
        let mut files: Vec<(PathBuf, String)> = Vec::new();
        let file = dir.join(name);
        let text = if sys::is_executable(&file) { std::fs::read_to_string(&file).unwrap_or_default() } else { String::new() };
        if sys::is_executable(&file) {
            files.push((file.clone(), text.clone()));
        }
        if dir.starts_with(&husky) || text.contains("husky") {
            let hf = husky.join(name);
            if hf != file
                && let Ok(t) = std::fs::read_to_string(&hf)
            {
                files.push((hf, t));
            }
        }
        if let Some(v4) = pkg.at(&["husky", "hooks", name]).str()
            && text.contains("husky")
        {
            files.push((top.join("package.json"), v4.to_string()));
        }
        for (_, t) in files {
            let own = format!("{t}\n{}", scripts_called(&t, &pkg));
            let mut seen = BTreeSet::new();
            for r in danger(&own) {
                seen.insert(r);
                found.push(format!("{name}: {r}"));
            }
            for (via, vt) in delegated(&own, top) {
                for r in danger(&vt) {
                    if seen.insert(r) {
                        let at = Path::new(&via).strip_prefix(top).map(|p| p.display().to_string()).unwrap_or(via.clone());
                        found.push(format!("{name}: {r} via {at}"));
                    }
                }
            }
        }
    }
    found
}

/// Content fingerprint for hook-damage detection.
#[derive(Clone, Debug, Default, PartialEq)]
struct Fp {
    missing: bool,
    mode: u32,
    size: u64,
    mtime: i128,
    ino: u64,
    hash: Option<u64>,
    link: Option<std::ffi::OsString>,
}

fn fingerprint(p: &Path, prev: Option<&Fp>) -> Fp {
    let Ok(m) = std::fs::symlink_metadata(p) else { return Fp { missing: true, ..Fp::default() } };
    let mut fp = Fp { mode: m.mode(), size: m.len(), mtime: m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128, ino: m.ino(), ..Fp::default() };
    if let Some(pr) = prev
        && !pr.missing
        && (pr.mode, pr.size, pr.mtime, pr.ino) == (fp.mode, fp.size, fp.mtime, fp.ino)
    {
        return pr.clone();
    }
    if m.file_type().is_symlink() {
        fp.link = std::fs::read_link(p).ok().map(PathBuf::into_os_string);
    } else if m.is_file() && m.len() <= HASH_MAX {
        fp.hash = std::fs::read(p).ok().map(|b| {
            use std::hash::Hasher;
            let mut h = std::hash::DefaultHasher::new();
            h.write(&b);
            h.finish()
        });
    }
    fp
}

fn same_file(a: &Fp, b: &Fp) -> bool {
    if a == b {
        return true;
    }
    if a.missing || b.missing {
        return a.missing && b.missing;
    }
    if a.mode != b.mode {
        return false;
    }
    if a.link.is_some() || b.link.is_some() {
        return a.link == b.link;
    }
    match (a.hash, b.hash) {
        (Some(x), Some(y)) => x == y,
        _ => (a.size, a.mtime, a.ino) == (b.size, b.mtime, b.ino),
    }
}

/// A claim's record: any edit, commit, release or claim by its owner changes it.
fn mark(st: &Option<State>, p: &str) -> Option<String> {
    let c = st.as_ref()?.claim(p)?;
    Some(format!("{}|{}|{}|{}|{:?}", c.by, c.at, c.touched, c.edits, c.pending))
}

fn committing(st: &Option<State>, p: &str) -> bool {
    st.as_ref().and_then(|s| s.claim(p)).and_then(|c| c.pending.as_deref()).is_some_and(|m| m.starts_with("commit:"))
}

/// Another agent that holds `p` has a Bash call in flight: it may be
/// rewriting its own file (a formatter, codegen) while our hooks run.
fn owner_busy(st: &Option<State>, p: &str, me: &str) -> bool {
    let Some(st) = st else { return false };
    st.claim(p).is_some_and(|c| c.by != me && st.session_of(&c.by).is_some_and(|s| s.bash.values().any(|b| b.agent == c.by)))
}

struct Snap {
    stash: Option<String>,
    files: Vec<(String, Fp, Option<String>, bool)>, // path, fingerprint, claim mark, untracked
    busy: BTreeSet<String>,                          // owner_busy when the hooks started
}

fn snapshot(g: G, want: &BTreeSet<String>, key: &dyn Fn(&str) -> String, me: &str) -> Snap {
    let st = state::read(g.repo).ok();
    let files: Vec<_> = status_of(g, None, false)
        .unwrap_or_default()
        .into_iter()
        .filter(|(p, _)| !want.contains(&key(p)))
        .map(|(p, e)| {
            let fp = fingerprint(&g.repo.abs(&p), None);
            let m = mark(&st, &p);
            (p, fp, m, e.xy == "??")
        })
        .collect();
    let busy = files.iter().filter(|f| owner_busy(&st, &f.0, me)).map(|f| f.0.clone()).collect();
    Snap { stash: rev(g, "refs/stash"), files, busy }
}

struct Clob {
    files: Vec<String>,
    stash: Option<String>,
    prev: Option<String>,
}

fn clobbered(g: G, snap: &Snap, me: &str) -> Option<Clob> {
    let stash = rev(g, "refs/stash");
    let st = state::read(g.repo).ok();
    let files: Vec<String> = snap
        .files
        .iter()
        .filter(|(p, fp, m, untracked)| {
            let now = fingerprint(&g.repo.abs(p), Some(fp));
            if same_file(fp, &now) {
                return false;
            }
            // nobody's untracked file (a cache, a log) counts only when it vanished
            if *untracked && m.is_none() && !now.missing {
                return false;
            }
            // its owner claimed, edited, committed or released it meanwhile, or
            // ran Bash meanwhile (`agent-lock mine -z | xargs -0 -r <formatter>`);
            // or it is in another agent's commit, whose own hooks may reformat it
            mark(&st, p) == *m && !committing(&st, p) && !snap.busy.contains(p) && !owner_busy(&st, p, me)
        })
        .map(|t| t.0.clone())
        .collect();
    if stash == snap.stash && files.is_empty() {
        return None;
    }
    Some(Clob { files, stash, prev: snap.stash.clone() })
}

fn clobber_message(c: &Clob, failed: Option<&str>) -> String {
    let list = rels(&c.files);
    let who = match failed {
        Some(h) => format!("the repo's {h} hook (which also failed)"),
        None => "the repo's commit hooks".to_string(),
    };
    if c.stash != c.prev {
        return match &c.stash {
            Some(oid) => format!(
                "{who} ran `git stash` and did not restore it: stash@{{0}} ({}) now holds {}. Ask the user to run `git stash pop`; do not run it yourself and do not retry this commit (the hook would do it again). Run the checks yourself and commit with --no-verify.",
                short(oid),
                if list.is_empty() { "stashed changes".to_string() } else { format!("{list}: other agents' uncommitted work, gone from the worktree") }
            ),
            None => format!(
                "{who} dropped a git stash (refs/stash {} is gone){}. Ask the user before doing anything else.",
                c.prev.as_deref().map(short).unwrap_or(""),
                if list.is_empty() { String::new() } else { format!(" and changed {list}") }
            ),
        };
    }
    format!(
        "files outside your commit changed while {who} ran, with no edit recorded by an owner: {list}. A hook rewrote or reverted them. Do not retry blindly: ask the user, or run the checks yourself and commit with --no-verify."
    )
}

// ── commit ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommitOut {
    pub ok: bool,
    pub sha: Option<String>,
    pub committed: Vec<String>, // repo-relative, as git spells them
    /// None = committed; "nothing" (ok) or a refusal: hook-staged, hook-clobbered,
    /// hook-failed, dangerous-hooks, case-rename, staged-untrack, commit-failed, busy.
    pub code: Option<&'static str>,
    pub message: String,
    /// The paths the code is about (staged by a hook, clobbered, untracked, renamed, …).
    pub extra: Vec<String>,
    pub skipped: Vec<String>, // asked for but unchanged or ignored
    pub hook_output: String,
    pub recovery: Option<String>, // an interrupted sync recovered first
}

impl CommitOut {
    fn fail(code: &'static str, message: impl Into<String>, extra: Vec<String>) -> CommitOut {
        CommitOut { code: Some(code), message: message.into(), extra, ..CommitOut::default() }
    }

    fn nothing(skipped: Vec<String>) -> CommitOut {
        CommitOut { ok: true, code: Some("nothing"), message: "nothing to commit".into(), skipped, ..CommitOut::default() }
    }
}

fn cf(e: String) -> CommitOut {
    CommitOut::fail("commit-failed", e, Vec::new())
}

/// Commit exactly `paths` (repo-relative) and nothing else, as `agent`; see the module notes.
pub fn commit_paths(repo: &Repo, agent: &str, paths: &[String], message: &str, no_verify: bool, dry_run: bool, refuse_dangerous: bool) -> CommitOut {
    commit_in(G::new(repo), agent, paths, message, no_verify, dry_run, refuse_dangerous)
}

fn commit_in(g: G, me: &str, paths: &[String], message: &str, no_verify: bool, dry_run: bool, refuse: bool) -> CommitOut {
    if let Err(e) = need_version() {
        return cf(e);
    }
    let mut want: Vec<String> = Vec::new();
    for p in paths {
        if !valid_rel(p) {
            return cf(format!("not a path inside the worktree: {p}"));
        }
        if !want.contains(p) {
            want.push(p.clone());
        }
    }
    if !dry_run && message.trim().is_empty() {
        return cf("empty commit message".into());
    }
    // an interrupted sync's leftovers first, so the hooks see the real worktree
    let early = if dry_run {
        None
    } else {
        match recover_in(g) {
            Rec::Busy(m) => return CommitOut::fail("busy", m, Vec::new()),
            Rec::Done(m) => Some(m),
            Rec::None => None,
        }
    };
    let mut out = match commit_flow(g, me, &want, message, no_verify, dry_run, refuse) {
        Ok(o) | Err(o) => o,
    };
    if out.recovery.is_none() {
        out.recovery = early;
    }
    out
}

#[allow(clippy::result_large_err)] // both arms are the report
fn commit_flow(g: G, me: &str, want: &[String], message: &str, no_verify: bool, dry_run: bool, refuse: bool) -> Result<CommitOut, CommitOut> {
    let repo = g.repo;
    let fold = repo.ignore_case();
    let key = move |p: &str| if fold { p.to_lowercase() } else { p.to_string() };
    let st = status_of(g, Some(want), true).map_err(cf)?;
    // every spelling git reports per key: two only for a case-only rename
    let mut by_key: BTreeMap<String, Vec<(&String, &Entry)>> = BTreeMap::new();
    for (p, e) in &st {
        by_key.entry(key(p)).or_default().push((p, e));
    }
    // `git mv Foo foo` is a staged D + A pair under one key: a path-limited
    // commit cannot express it on a case-insensitive filesystem
    let renames: Vec<(String, String)> = by_key
        .values()
        .filter_map(|v| {
            let del = v.iter().find(|x| x.1.xy.starts_with('D'))?;
            let add = v.iter().find(|x| x.0 != del.0 && !x.1.xy.starts_with(['D', '.', '?', '!']))?;
            Some((del.0.clone(), add.0.clone()))
        })
        .collect();
    if !renames.is_empty() {
        let list = renames.iter().map(|(a, b)| format!("{a} → {b}")).collect::<Vec<_>>().join(", ");
        let msg = format!("case-only renames can't be committed path-limited on a case-insensitive filesystem ({list}); ask the user, or commit when no other agent is active. Nothing was committed.");
        return Err(CommitOut::fail("case-rename", msg, renames.into_iter().flat_map(|(a, b)| [a, b]).collect()));
    }
    // git's spelling of a path and its entry
    let ent = |p: &str| by_key.get(&key(p)).and_then(|v| v.iter().find(|x| x.0 == p).or(v.first()).copied());
    let xy = |p: &str| ent(p).map(|x| x.1.xy.as_str()).unwrap_or("");
    let (mut skipped, mut cands, mut untrack) = (Vec::new(), Vec::new(), Vec::new());
    for p in want {
        match ent(p) {
            None => skipped.push(p.clone()),
            Some((q, e)) if e.xy.starts_with('D') && lexists(&repo.abs(q)) => untrack.push(q.clone()),
            Some((_, e)) if e.ignored && !e.xy.starts_with('D') => skipped.push(p.clone()),
            Some((q, _)) => cands.push(q.clone()),
        }
    }
    if !untrack.is_empty() {
        let msg = format!(
            "{} {} staged for removal from git (`git rm --cached`) but still on disk: a path-limited commit would re-add the current content. Nothing was committed and the removal stays staged. Ask the user, or commit it with plain `git commit` when no other agent is active.",
            rels(&untrack),
            if untrack.len() == 1 { "is" } else { "are" }
        );
        return Err(CommitOut { skipped, ..CommitOut::fail("staged-untrack", msg, untrack) });
    }
    if cands.is_empty() {
        return Ok(CommitOut::nothing(skipped));
    }
    let (hooks_dir, hooks) = if no_verify { (PathBuf::new(), false) } else { hooks(g) };
    if hooks && refuse {
        let bad = dangerous_hooks(g, &hooks_dir);
        if !bad.is_empty() {
            let msg = format!(
                "repo hooks rewrite the shared worktree ({}) and other agents have uncommitted changes. Run the checks yourself, then commit with --no-verify.",
                bad.join("; ")
            );
            return Err(CommitOut { skipped, ..CommitOut::fail("dangerous-hooks", msg, Vec::new()) });
        }
    }

    // Without hooks, status already says exactly what a commit of these paths
    // changes, unless a path has both staged and unstaged changes (its
    // worktree may equal HEAD): then, and with hooks, a preflight decides.
    let precise = cands.iter().all(|p| xy(p) == "??" || xy(p).contains('.'));
    let preflight = hooks || !precise;
    // ---- preflight on a private index: HEAD + exactly our paths ----
    let head = if preflight { rev(g, "HEAD") } else { None };
    let base = if preflight { head.clone().unwrap_or_else(|| empty_tree(g)) } else { String::new() };
    let t = tag();
    let p_idx = repo.git_dir.join(format!("agent-lock-commit-{t}.idx"));
    let m_file = repo.git_dir.join(format!("agent-lock-commit-{t}.msg"));
    let _msg_file = Litter(vec![m_file.clone()]);
    let text = if message.ends_with('\n') { message.to_string() } else { format!("{message}\n") };
    std::fs::write(&m_file, text).map_err(|e| cf(format!("cannot write {}: {e}", m_file.display())))?;
    let m_str = m_file.to_string_lossy().into_owned();
    let mut hook_output = String::new();
    let wanted: BTreeSet<String> = want.iter().chain(&cands).map(|p| key(p)).collect();
    let committed: Vec<String> = if !preflight {
        cands.clone()
    } else {
        let _idx = Litter(vec![p_idx.clone(), lock_of(&p_idx)]);
        let p_str = p_idx.to_string_lossy().into_owned();
        let penv = [("GIT_INDEX_FILE", p_str.as_str())];
        let rt: Vec<&str> = match &head {
            Some(h) => vec!["read-tree", h],
            None => vec!["read-tree", "--empty"],
        };
        must(g.exec(&rt, &penv, None), "read-tree").map_err(cf)?;
        // a path in neither HEAD nor the worktree has nothing to commit
        let gone: Vec<String> = cands.iter().filter(|p| !lexists(&repo.abs(p))).cloned().collect();
        let mut in_head = BTreeSet::new();
        if !gone.is_empty() && head.is_some() {
            for o in per_chunk(g, &["--no-optional-locks", "ls-files", "-z"], &penv, &gone, "ls-files").map_err(cf)? {
                in_head.extend(zsplit(&o.stdout));
            }
        }
        let add: Vec<String> = cands.iter().filter(|p| lexists(&repo.abs(p)) || in_head.contains(*p)).cloned().collect();
        if !add.is_empty() {
            must(g.exec(&["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"], &penv, Some(&pathspecs(&add))), "add").map_err(cf)?;
        }
        if hooks && !dry_run {
            let snap = snapshot(g, &wanted, &key, me);
            let mut failed: Option<(&str, String)> = None;
            for (hook, extra) in [("pre-commit", vec![]), ("commit-msg", vec!["--", m_str.as_str()])] {
                let mut a = vec!["hook", "run", "--ignore-missing", hook];
                a.extend(extra);
                let r = g.exec(&a, &penv, None).map_err(|e| cf(e.to_string()))?;
                let said = said(&r);
                if !said.is_empty() {
                    hook_output.push_str(&said);
                    hook_output.push('\n');
                }
                if !r.ok() {
                    failed = Some((hook, said));
                    break;
                }
            }
            // checked whether the hooks passed or failed: a failing hook that
            // stashed and never popped is the common way peers' work disappears
            if let Some(c) = clobbered(g, &snap, me) {
                let msg = clobber_message(&c, failed.as_ref().map(|f| f.0));
                let hook_output = hook_output.trim().to_string();
                return Err(CommitOut { skipped, hook_output, ..CommitOut::fail("hook-clobbered", msg, c.files) });
            }
            if let Some((hook, text)) = failed {
                let hook_output = hook_output.trim().to_string();
                return Err(CommitOut { skipped, hook_output, ..CommitOut::fail("hook-failed", format!("{hook} hook failed:\n{text}"), Vec::new()) });
            }
        }
        let tree = must(g.exec(&["write-tree"], &penv, None), "write-tree").map_err(cf)?.line();
        let o = must(g.ro(&["diff-tree", "-r", "-z", "--name-only", "--no-renames", &base, &tree]), "diff-tree").map_err(cf)?;
        let changed = zsplit(&o.stdout);
        let extra: Vec<String> = changed.iter().filter(|p| !wanted.contains(&key(p))).cloned().collect();
        if !extra.is_empty() {
            let msg = format!("pre-commit hook staged files you don't own: {}. Nothing was committed; fix the hook or re-run with --no-verify.", rels(&extra));
            return Err(CommitOut { skipped, hook_output: hook_output.trim().to_string(), ..CommitOut::fail("hook-staged", msg, extra) });
        }
        changed
    };
    let done: BTreeSet<String> = committed.iter().map(|p| key(p)).collect();
    skipped.extend(cands.iter().filter(|p| !done.contains(&key(p))).cloned());
    let hook_output = hook_output.trim().to_string();
    if committed.is_empty() {
        return Ok(CommitOut { hook_output, ..CommitOut::nothing(skipped) });
    }
    if dry_run {
        return Ok(CommitOut { ok: true, committed, skipped, ..CommitOut::default() });
    }
    at("preflight-done");

    // ---- the real commit, under locks.git: the shared index is touched by git only ----
    let _lock = lock_git(&g, me, "commit").map_err(|m| CommitOut { skipped: skipped.clone(), ..CommitOut::fail("busy", m, Vec::new()) })?;
    let recovery = match recover_in(g) {
        Rec::Busy(m) => return Err(CommitOut { skipped, ..CommitOut::fail("busy", m, Vec::new()) }),
        Rec::Done(m) => Some(m),
        Rec::None => None,
    };
    let now = if preflight { rev(g, "HEAD") } else { None };
    if preflight && now != head {
        // another agent committed or synced while our hooks ran: fine, unless it touched our paths
        let to = now.clone().unwrap_or_else(|| empty_tree(g));
        let o = must(g.ro(&["diff-tree", "-r", "-z", "--name-only", "--no-renames", &base, &to]), "diff-tree").map_err(cf)?;
        let touched: Vec<String> = zsplit(&o.stdout).into_iter().filter(|p| done.contains(&key(p))).collect();
        if !touched.is_empty() {
            let msg = format!("HEAD moved while the hooks ran and changed {}; nothing was committed, re-run", rels(&touched));
            return Err(CommitOut { skipped, recovery, ..CommitOut::fail("commit-failed", msg, touched) });
        }
    }
    let ita: Vec<String> = committed.iter().filter(|p| xy(p) == "??").cloned().collect();
    let reset_ita = || {
        if !ita.is_empty() {
            let _ = retry_index_lock(|| g.exec(&["reset", "-q", "--pathspec-from-file=-", "--pathspec-file-nul"], &[], Some(&pathspecs(&ita))));
        }
    };
    let failure = |r: io::Result<Out>, what: &str| match r {
        Ok(o) if o.ok() => None,
        Ok(o) => Some(if said(&o).is_empty() { format!("git {what} exited {}", o.status) } else { said(&o) }),
        Err(e) => Some(format!("git {what}: {e}")),
    };
    if !ita.is_empty() {
        let r = retry_index_lock(|| g.exec(&["add", "-N", "--pathspec-from-file=-", "--pathspec-file-nul"], &[], Some(&pathspecs(&ita))));
        if let Some(e) = failure(r, "add -N") {
            reset_ita();
            return Err(CommitOut { skipped, recovery, ..cf(e) });
        }
    }
    let r = retry_index_lock(|| {
        g.exec(&["commit", "--only", "--no-verify", "-q", "-F", &m_str, "--pathspec-from-file=-", "--pathspec-file-nul"], &[], Some(&pathspecs(&committed)))
    });
    if let Some(e) = failure(r, "commit") {
        reset_ita();
        if ["nothing to commit", "nothing added to commit", "no changes added to commit"].iter().any(|x| e.contains(x)) {
            // someone committed exactly this content meanwhile
            skipped.extend(committed);
            return Ok(CommitOut { recovery, hook_output, ..CommitOut::nothing(skipped) });
        }
        return Err(CommitOut { skipped, recovery, hook_output, ..cf(e) });
    }
    Ok(CommitOut { ok: true, sha: rev(g, "HEAD"), committed, skipped, hook_output, recovery, ..CommitOut::default() })
}

// ── upstream record ───────────────────────────────────────────────────────

fn upstream_oid(g: G, branch: &str) -> Option<String> {
    state::read(g.repo).ok()?.upstream.filter(|u| u.remote == REMOTE && u.branch == branch).map(|u| u.oid)
}

fn set_upstream(g: G, branch: &str, oid: &str) {
    let _ = state::transact(g.repo, |st| {
        st.upstream = Some(Upstream { remote: REMOTE.into(), branch: branch.into(), oid: oid.into() });
    });
}

fn current_branch(g: G) -> Option<String> {
    head_ref(g)?.strip_prefix("refs/heads/").map(str::to_string)
}

// ── sync ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SyncOut {
    /// up-to-date | no-upstream | fast-forward | rebased | conflict | blocked | rewritten | busy | error
    pub status: &'static str,
    pub files: Vec<String>,
    pub blocked_by: Vec<(String, String)>, // (path, holder) for claims covering upstream's changes
    pub message: String,
    pub head: Option<String>,
    pub replayed: usize,
    pub skipped: usize,         // local commits already upstream
    pub dropped: Vec<String>,   // commits a rewritten upstream removed
    pub recovery: Option<String>,
}

impl SyncOut {
    fn new(status: &'static str, message: impl Into<String>) -> SyncOut {
        SyncOut { status, message: message.into(), ..SyncOut::default() }
    }

    pub fn ok(&self) -> bool {
        matches!(self.status, "up-to-date" | "no-upstream" | "fast-forward" | "rebased")
    }
}

/// Bring upstream (origin/<branch>) into HEAD without touching anyone's
/// uncommitted work. `agent` is recorded as the holder of locks.git/locks.sync.
pub fn sync(repo: &Repo, agent: &str, accept_rewrite: bool) -> SyncOut {
    sync_in(G::new(repo), agent, accept_rewrite)
}

fn sync_in(g: G, agent: &str, accept: bool) -> SyncOut {
    let Some(branch) = current_branch(g) else { return SyncOut::new("error", "HEAD is detached; check out a branch first") };
    if let Err(e) = need_version() {
        return SyncOut::new("error", e);
    }
    let _lock = match lock_git(&g, agent, "sync") {
        Ok(l) => l,
        Err(m) => return SyncOut::new("busy", m),
    };
    sync_locked(g, agent, &branch, accept)
}

fn sync_locked(g: G, agent: &str, branch: &str, accept: bool) -> SyncOut {
    let recovery = match recover_in(g) {
        Rec::Busy(m) => return SyncOut::new("busy", m),
        Rec::Done(m) => Some(m),
        Rec::None => None,
    };
    let mut out = match sync_steps(g, agent, branch, accept) {
        Ok(o) | Err(o) => o,
    };
    out.recovery = recovery;
    out
}

fn rev_list(g: G, args: &[String]) -> Res<Vec<(String, Vec<String>)>> {
    let mut a = vec!["rev-list", "--reverse", "--topo-order", "--parents"];
    a.extend(args.iter().map(String::as_str));
    let o = must(g.ro(&a), "rev-list")?;
    Ok(o.text()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut it = l.split(' ').map(str::to_string);
            (it.next().unwrap_or_default(), it.collect())
        })
        .collect())
}

/// Re-create commit `oid` with `tree` on `parent`, keeping author, encoding
/// and the exact message bytes.
fn recommit(g: G, oid: &str, tree: &str, parent: &str, sign: bool) -> Res<String> {
    let raw = must(g.ro(&["cat-file", "commit", oid]), "cat-file")?.stdout;
    let cut = raw.windows(2).position(|w| w == b"\n\n");
    let header = String::from_utf8_lossy(&raw[..cut.unwrap_or(raw.len())]).into_owned();
    let msg: &[u8] = cut.map(|c| &raw[c + 2..]).unwrap_or(&[]);
    let mut env: Vec<(&str, String)> = Vec::new();
    if let Some(a) = header.lines().find_map(|l| l.strip_prefix("author "))
        && let Some(gt) = a.rfind('>')
        && let Some(lt) = a[..gt].rfind(" <")
    {
        env.push(("GIT_AUTHOR_NAME", a[..lt].to_string()));
        env.push(("GIT_AUTHOR_EMAIL", a[lt + 2..gt].to_string()));
        env.push(("GIT_AUTHOR_DATE", format!("@{}", a[gt + 1..].trim())));
    }
    let enc = header.lines().find_map(|l| l.strip_prefix("encoding ")).map(|e| format!("i18n.commitEncoding={e}"));
    let mut args: Vec<&str> = Vec::new();
    if let Some(e) = &enc {
        args.extend(["-c", e.as_str()]);
    }
    args.extend(["commit-tree", tree, "-p", parent]);
    if sign {
        args.push("-S");
    }
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    Ok(must(g.exec(&args, &env, Some(msg)), "commit-tree")?.line())
}

enum Replay {
    Done { tip: String, replayed: usize, skipped: usize },
    Conflict { commit: String, files: Vec<String> },
}

fn replay(g: G, commits: &[(String, Vec<String>)], onto: &str) -> Res<Replay> {
    let sign = g.get(&["config", "--bool", "commit.gpgSign"]).as_deref() == Some("true");
    let (mut base, mut replayed, mut skipped) = (onto.to_string(), 0, 0);
    for (oid, parents) in commits {
        if parents.len() != 1 {
            return Err(format!(
                "local commit {} is a {} commit; agent-lock sync only replays linear history. Ask the user.",
                short(oid),
                if parents.is_empty() { "root" } else { "merge" }
            ));
        }
        let mb = format!("--merge-base={}", parents[0]);
        let r = g.ro(&["merge-tree", "--write-tree", "-z", "--name-only", "--no-messages", &mb, &base, oid]).map_err(|e| e.to_string())?;
        if r.status == 1 {
            let mut files: Vec<String> = zsplit(&r.stdout).into_iter().skip(1).collect();
            files.dedup();
            return Ok(Replay::Conflict { commit: oid.clone(), files });
        }
        if !r.ok() {
            return Err(format!("git merge-tree failed: {}", said(&r)));
        }
        let tree = zsplit(&r.stdout).into_iter().next().unwrap_or_default();
        if g.get(&["rev-parse", &format!("{base}^{{tree}}")]).as_deref() == Some(tree.as_str()) {
            skipped += 1; // already upstream
            continue;
        }
        base = recommit(g, oid, &tree, &base, sign)?;
        replayed += 1;
    }
    Ok(Replay::Done { tip: base, replayed, skipped })
}

#[allow(clippy::result_large_err)] // both arms are the report
fn sync_steps(g: G, agent: &str, branch: &str, accept: bool) -> Result<SyncOut, SyncOut> {
    let repo = g.repo;
    let err = |m: String| SyncOut::new("error", m);
    let tracking = format!("refs/remotes/{REMOTE}/{branch}");
    // the tracking ref before this fetch is a rewrite baseline too: the
    // upstream record is missing on a first sync and stale after a plain pull
    let before = rev(g, &tracking);
    let spec = format!("+refs/heads/{branch}:{tracking}");
    let f = g.run(&["fetch", "--quiet", "--no-write-fetch-head", "--no-auto-maintenance", REMOTE, &spec]).map_err(|e| err(e.to_string()))?;
    if !f.ok() {
        if f.stderr.contains("couldn't find remote ref") {
            return Ok(SyncOut { head: rev(g, "HEAD"), ..SyncOut::new("no-upstream", format!("{REMOTE} has no branch {branch} yet; push will create it")) });
        }
        return Err(err(format!("git fetch {REMOTE} failed: {}", said(&f))));
    }
    let u = rev(g, &tracking).ok_or_else(|| err(format!("cannot resolve {tracking} after fetch")))?;
    let rec = upstream_oid(g, branch);
    let mut lost: Vec<String> = Vec::new();
    for b in [rec.clone(), before].into_iter().flatten() {
        if !lost.contains(&b) && !is_ancestor(g, &b, &u) {
            lost.push(b);
        }
    }
    if !lost.is_empty() && !accept {
        // the fetch just moved the tracking ref past the evidence: keep it
        if rec.as_ref().is_none_or(|r| !lost.contains(r)) {
            set_upstream(g, branch, &lost[0]);
        }
        let mut known: Vec<String> = lost.iter().filter(|b| has_commit(g, b)).cloned().collect();
        let dropped: Vec<String> = if known.is_empty() {
            Vec::new()
        } else {
            known.push(format!("^{u}"));
            rev_list(g, &known).map_err(err)?.into_iter().map(|c| c.0).collect()
        };
        let msg = format!(
            "upstream history was rewritten ({} is no longer in {REMOTE}/{branch}{}). Ask the user; `agent-lock sync --accept-rewrite` drops the removed commits and keeps local work.",
            short(&lost[0]),
            if dropped.is_empty() { String::new() } else { format!("; {} commit(s) were removed", dropped.len()) }
        );
        return Ok(SyncOut { dropped, head: rev(g, "HEAD"), ..SyncOut::new("rewritten", msg) });
    }
    let mut exclude = vec![format!("^{u}")];
    exclude.extend(lost.iter().filter(|b| has_commit(g, b)).map(|b| format!("^{b}")));

    for _round in 0..5 {
        let h = rev(g, "HEAD");
        if h.as_deref() == Some(u.as_str()) && exclude.len() == 1 {
            set_upstream(g, branch, &u);
            return Ok(SyncOut { head: h, ..SyncOut::new("up-to-date", "up to date") });
        }
        let mut commits = Vec::new();
        if let Some(hh) = &h {
            let mut a = vec![hh.clone()];
            a.extend(exclude.iter().cloned());
            commits = rev_list(g, &a).map_err(err)?;
            // ahead of U: nothing to do, unless accepting a rewrite drops some of H
            if is_ancestor(g, &u, hh) && (exclude.len() == 1 || commits.len() == rev_list(g, &[hh.clone(), format!("^{u}")]).map_err(err)?.len()) {
                set_upstream(g, branch, &u);
                let msg = if *hh == u { "up to date".to_string() } else { format!("up to date ({} local commit(s) not pushed)", commits.len()) };
                return Ok(SyncOut { head: h.clone(), ..SyncOut::new("up-to-date", msg) });
            }
        }
        // no local commits to keep (H is an ancestor of U, or all were dropped): U itself
        let (mut tip, mut replayed, mut skipped) = (u.clone(), 0, 0);
        if !commits.is_empty() {
            match replay(g, &commits, &u).map_err(|m| SyncOut { head: h.clone(), ..err(m) })? {
                Replay::Conflict { commit, files } => {
                    let msg = format!(
                        "local commit {} conflicts with {REMOTE}/{branch} in {}; nothing was changed. Ask the user (or resolve in a quiet checkout).",
                        short(&commit),
                        rels(&files)
                    );
                    return Ok(SyncOut { files, head: h.clone(), ..SyncOut::new("conflict", msg) });
                }
                Replay::Done { tip: t, replayed: r, skipped: s } => (tip, replayed, skipped) = (t, r, s),
            }
        }
        let base = h.clone().unwrap_or_else(|| empty_tree(g));
        let ents = tree_diff(g, &base, &tip).map_err(err)?;
        let d: Vec<String> = ents.iter().map(|e| e.rel.clone()).collect();
        // untracked or ignored local files in the way (read-tree refuses the
        // first and would silently overwrite the second)
        let mut local = Vec::new();
        for o in per_chunk(g, &["--no-optional-locks", "ls-files", "-z", "-o"], &[], &d, "ls-files").map_err(err)? {
            local.extend(zsplit(&o.stdout));
        }
        if !local.is_empty() {
            let msg = format!("blocked by local file(s) that {REMOTE}/{branch} would overwrite: {}", rels(&local));
            return Ok(SyncOut { files: local, head: h, ..SyncOut::new("blocked", msg) });
        }
        // read-tree treats a missing worktree file as up to date and would
        // silently recreate a tracked file someone deleted
        let gone: Vec<String> = ents.iter().filter(|e| e.b.is_some() && !lexists(&repo.abs(&e.rel))).map(|e| e.rel.clone()).collect();
        let deleted: Vec<String> = if gone.is_empty() { Vec::new() } else { index_entries(g, &gone).map_err(err)?.into_keys().collect() };
        if !deleted.is_empty() {
            let it = if deleted.len() == 1 { "it" } else { "them" };
            let msg = format!(
                "blocked by local deletion of {}: {REMOTE}/{branch} changes {it}, so sync would bring {it} back. Commit the deletion first (or restore the file).",
                rels(&deleted)
            );
            return Ok(SyncOut { files: deleted, head: h, ..SyncOut::new("blocked", msg) });
        }
        match install(g, agent, branch, h.as_deref(), &tip, &ents) {
            Install::Retry => continue,
            Install::Busy(m) => return Ok(SyncOut { head: h, ..SyncOut::new("busy", m) }),
            Install::Blocked { files, by, message } => return Ok(SyncOut { files, blocked_by: by, head: h, ..SyncOut::new("blocked", message) }),
            Install::Done => {
                set_upstream(g, branch, &u);
                let already = if skipped > 0 { format!(" ({skipped} local commit(s) already upstream)") } else { String::new() };
                let (status, msg) = if replayed > 0 {
                    ("rebased", format!("replayed {replayed} local commit(s) onto {REMOTE}/{branch}{already}"))
                } else {
                    ("fast-forward", format!("fast-forwarded to {REMOTE}/{branch}{already}"))
                };
                return Ok(SyncOut { files: d, head: Some(tip), replayed, skipped, ..SyncOut::new(status, msg) });
            }
        }
    }
    Err(err("HEAD kept moving during sync; retry".into()))
}

fn not_uptodate(stderr: &str) -> Vec<String> {
    let mut files = Vec::new();
    for l in stderr.lines() {
        if let Some(rest) = l.strip_prefix("error: Entry '") {
            if let Some(end) = rest.find("' not uptodate").or_else(|| rest.find("' would be overwritten")) {
                files.push(rest[..end].to_string());
            }
        } else if let Some(p) = l.strip_prefix('\t') {
            files.push(p.trim().to_string());
        }
    }
    files.dedup();
    files
}

enum Install {
    Done,
    Retry,
    Busy(String),
    Blocked { files: Vec<String>, by: Vec<(String, String)>, message: String },
}

fn clear_sync(g: G, me: Option<Owner>, tip: &str) {
    let _ = state::transact(g.repo, |st| {
        if st.locks.sync.as_ref().is_some_and(|l| l.owner == me && l.tip == tip) {
            st.locks.sync = None;
        }
    });
}

/// §9 step 5: HEAD + index as a compare-and-swap. locks.sync (taken in the
/// same transaction that checks the claims) stops edits to the paths and
/// records what recover() needs; <gitdir>/index.lock stops raw git writers;
/// HEAD is re-read under it; the index copy is a hard link (keeps mtime, so
/// racy-git detection still sees same-second edits) that git replaces via
/// P.lock + rename, so the live index inode is never written.
fn install(g: G, agent: &str, branch: &str, h: Option<&str>, tip: &str, ents: &[Ent]) -> Install {
    let repo = g.repo;
    let lock_path = repo.git_dir.join("index.lock");
    let p = repo.git_dir.join(format!("agent-lock-sync-{}.index", tag()));
    let body = body_of(&p);
    let p_str = p.to_string_lossy().into_owned();
    let me = sys::self_owner();
    let head = h.unwrap_or("").to_string();
    let marker = crate::json::object([
        ("agent-lock", "sync".into()),
        ("by", agent.into()),
        ("pid", sys::pid().into()),
        ("start", me.map(|o| o.start).into()),
        ("head", head.as_str().into()),
        ("tip", tip.into()),
        ("index_tmp", p_str.as_str().into()),
    ]);
    if let Err(e) = std::fs::write(&body, format!("{}\n", marker.to_string())) {
        return Install::Busy(format!("cannot write {}: {e}", body.display()));
    }
    let body_ino = ino(&body);
    let paths: Vec<String> = ents.iter().map(|e| e.rel.clone()).collect();
    let _quiet = quiet(); // until the install settles
    let now = sys::now();
    let t1 = state::transact(repo, |st| {
        if let Some(l) = &st.locks.sync {
            return Err(Some(l.by.clone()));
        }
        let ctx = rules::Ctx::real(now);
        let by: Vec<(String, String)> = paths
            .iter()
            .filter_map(|p| {
                let c = st.claim(p)?;
                let live = c.edits > 0 || c.pending.is_some() || rules::stale(st, c, &ctx, None).is_none();
                live.then(|| (p.clone(), c.by.clone()))
            })
            .collect();
        if !by.is_empty() {
            return Ok(by);
        }
        st.locks.sync = Some(SyncLock {
            by: agent.to_string(),
            owner: me,
            since: now,
            head: head.clone(),
            tip: tip.to_string(),
            paths: paths.clone(),
            index_tmp: Some(p_str.clone()),
            lock_ino: body_ino,
        });
        Ok(Vec::new())
    });
    match t1 {
        Ok(Ok(by)) if by.is_empty() => {}
        Ok(Ok(by)) => {
            unlink(&body);
            let list = by.iter().map(|(p, a)| format!("{p} ({a})")).collect::<Vec<_>>().join(", ");
            let message = format!("blocked by {list}: they have uncommitted work in files {REMOTE}/{branch} changes");
            return Install::Blocked { files: by.iter().map(|b| b.0.clone()).collect(), by, message };
        }
        Ok(Err(holder)) => {
            unlink(&body);
            return Install::Busy(format!("another agent-lock sync (by {}) is installing an upstream update; retry shortly", holder.unwrap_or_default()));
        }
        Err(e) => {
            unlink(&body);
            return Install::Busy(format!("cannot update {}: {e}", repo.state_file().display()));
        }
    }
    at("locked");
    // O_EXCL-equivalent: link a fully written file, so the lock is never empty
    let deadline = Instant::now() + INDEX_LOCK_WAIT;
    let mut pause = Duration::from_millis(10);
    let got = loop {
        match std::fs::hard_link(&body, &lock_path) {
            Ok(()) => break true,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && Instant::now() < deadline => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(200));
            }
            Err(_) => break false,
        }
    };
    unlink(&body);
    if !got {
        clear_sync(g, me, tip);
        return Install::Busy(format!("{} has been held by another git process; retry shortly (never delete it)", lock_path.display()));
    }
    at("index-locked");
    let settle = |rolled: bool| {
        unlink(&p);
        unlink(&lock_of(&p));
        unlink_if(&lock_path, body_ino);
        clear_sync(g, me, tip);
        if rolled {
            refresh_index(g);
        }
    };
    let (now, at_ref) = head_info(g);
    if now.as_deref() != h || at_ref.as_deref() != Some(format!("refs/heads/{branch}").as_str()) {
        settle(false);
        return Install::Retry;
    }
    if let Err(e) = std::fs::hard_link(repo.git_dir.join("index"), &p)
        && e.kind() != io::ErrorKind::NotFound
    {
        settle(false);
        return Install::Busy(format!("cannot copy the index: {e}"));
    }
    // Two-way merge H → tip (checkout semantics): refuses any path that
    // differs between H and tip and has local index/worktree changes or would
    // clobber an untracked file, and keeps other agents' staged entries.
    let base = h.map(str::to_string).unwrap_or_else(|| empty_tree(g));
    let penv = [("GIT_INDEX_FILE", p_str.as_str())];
    let read_tree = || g.exec(&["read-tree", "-m", "-u", &base, tip], &penv, None).unwrap_or_else(|e| Out { status: -1, stdout: Vec::new(), stderr: e.to_string() });
    let refused = |r: &Out| (1..=128).contains(&r.status) && (r.stderr.contains("not uptodate") || r.stderr.contains("would be overwritten"));
    let mut r = read_tree();
    if refused(&r) {
        // read-tree never refreshes, so a file rewritten with the same content
        // looks "not uptodate": refresh the copy's stat data (as checkout
        // does) and try once more; a refusal wrote nothing
        let _ = g.exec(&["update-index", "-q", "--refresh"], &penv, None);
        r = read_tree();
    }
    at("after-read-tree");
    if !r.ok() {
        if refused(&r) {
            settle(false); // refused up front: it wrote nothing
            let files = not_uptodate(&r.stderr);
            let on = if files.is_empty() { String::new() } else { format!(" in {}", rels(&files)) };
            return Install::Blocked { files, by: Vec::new(), message: format!("blocked by local changes{on}: {}", said(&r)) };
        }
        // stopped half-way (a failing filter, a signal): put back what it wrote
        at("read-tree-failed");
        let rolled = rollback(g, h, tip, Some(ents)).unwrap_or_default();
        settle(!rolled.is_empty());
        let how = if r.status >= 128 { "was interrupted by a signal" } else { "failed" };
        return Install::Busy(format!("git read-tree {how}; nothing was changed: {}", said(&r)));
    }
    let zero = "0".repeat(tip.len());
    let msg = format!("agent-lock sync: {}", short(tip));
    let r = g.exec(&["update-ref", "-m", &msg, "HEAD", tip, h.unwrap_or(&zero)], &penv, None);
    if !r.as_ref().is_ok_and(|o| o.ok()) {
        // a ref writer that bypasses index.lock moved HEAD: put the files back
        let back = g.exec(&["read-tree", "-m", "-u", tip, &base], &penv, None);
        let rolled = if back.is_ok_and(|o| o.ok()) { Vec::new() } else { rollback(g, h, tip, Some(ents)).unwrap_or_default() };
        settle(!rolled.is_empty());
        let why = r.map(|o| said(&o)).unwrap_or_else(|e| e.to_string());
        return Install::Busy(format!("HEAD moved while sync was installing; nothing was changed, retry: {why}"));
    }
    at("after-update-ref");
    if let Err(e) = std::fs::rename(&p, repo.git_dir.join("index")) {
        // HEAD has moved: leave locks.sync for recover(), which finishes the install
        return Install::Busy(format!("could not install the new index ({e}); the next agent-lock git command finishes it"));
    }
    at("between-renames");
    unlink(&lock_of(&p));
    unlink_if(&lock_path, body_ino);
    clear_sync(g, me, tip);
    Install::Done
}

/// The file whose hard link becomes our index.lock.
fn body_of(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(".body");
    PathBuf::from(s)
}

fn hash_paths(g: G, rels: &[String]) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let ok: Vec<&String> = rels.iter().filter(|p| !p.contains('\n') && !p.starts_with('"')).collect();
    for c in ok.chunks(CHUNK) {
        let input: String = c.iter().map(|p| format!("{p}\n")).collect();
        let Ok(o) = g.exec(&["--no-optional-locks", "hash-object", "--stdin-paths"], &[], Some(input.as_bytes())) else { continue };
        if !o.ok() {
            continue; // one vanished: leave this chunk alone
        }
        for (p, oid) in c.iter().zip(o.text().lines()) {
            m.insert((*p).clone(), oid.to_string());
        }
    }
    m
}

fn checkout_from(g: G, rev: &str, rels: &[String]) -> Res<()> {
    let tmp = g.repo.git_dir.join(format!("agent-lock-restore-{}.index", tag()));
    let _litter = Litter(vec![tmp.clone(), lock_of(&tmp)]);
    let t = tmp.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", t.as_str())];
    must(g.exec(&["read-tree", rev], &env, None), "read-tree")?;
    let input: Vec<u8> = rels.iter().flat_map(|p| p.bytes().chain([0])).collect();
    must(g.exec(&["checkout-index", "-f", "-z", "--stdin"], &env, Some(&input)), "checkout-index")?;
    Ok(())
}

fn is_file_mode(m: &str) -> bool {
    m == "100644" || m == "100755" || m == "120000"
}

/// Undo what an interrupted `read-tree -m -u H tip` wrote. A path goes back
/// to H only when its index entry is still H's and its worktree is exactly
/// tip's (tip's blob, or absent where tip removes it), or it is missing where
/// both have it (read-tree unlinks before writing; sync checked it existed).
/// Anything else, someone's edit, is left alone. Returns the paths put back.
fn rollback(g: G, h: Option<&str>, tip: &str, ents: Option<&[Ent]>) -> Res<Vec<String>> {
    let owned;
    let ents = match ents {
        Some(e) => e,
        None => {
            owned = tree_diff(g, &h.map(str::to_string).unwrap_or_else(|| empty_tree(g)), tip)?;
            &owned
        }
    };
    let filey = |s: &Option<(String, String)>| s.as_ref().is_some_and(|x| is_file_mode(&x.0));
    let all: Vec<&Ent> = ents.iter().filter(|e| (filey(&e.b) || e.b.is_none()) && (filey(&e.a) || e.a.is_none())).collect();
    if all.is_empty() {
        return Ok(Vec::new());
    }
    let idx = index_entries(g, &all.iter().map(|e| e.rel.clone()).collect::<Vec<_>>())?;
    let cand: Vec<&Ent> = all.into_iter().filter(|e| idx.get(&e.rel) == e.a.as_ref()).collect();
    let regular: Vec<String> = cand
        .iter()
        .filter(|e| e.b.as_ref().is_some_and(|b| b.0 != "120000") && std::fs::symlink_metadata(g.repo.abs(&e.rel)).is_ok_and(|m| m.is_file()))
        .map(|e| e.rel.clone())
        .collect();
    let hashes = hash_paths(g, &regular);
    let mut revert: Vec<&Ent> = Vec::new();
    for e in cand {
        let abs = g.repo.abs(&e.rel);
        let present = lexists(&abs);
        let hit = match &e.b {
            None => !present,
            Some(_) if !present => e.a.is_some(),
            Some((m, oid)) if m == "120000" => {
                let blob = g.ro(&["cat-file", "blob", oid]).ok().filter(Out::ok).map(|o| o.stdout);
                std::fs::read_link(&abs).ok().is_some_and(|l| blob.as_deref() == Some(l.as_os_str().as_encoded_bytes()))
            }
            Some((_, oid)) => hashes.get(&e.rel) == Some(oid),
        };
        if hit {
            revert.push(e);
        }
    }
    let restore: Vec<String> = revert.iter().filter(|e| e.a.is_some()).map(|e| e.rel.clone()).collect();
    if let (false, Some(h)) = (restore.is_empty(), h) {
        checkout_from(g, h, &restore)?;
    }
    for e in revert.iter().filter(|e| e.a.is_none()) {
        let abs = g.repo.abs(&e.rel);
        unlink(&abs);
        let mut d = abs.parent();
        while let Some(dir) = d.filter(|d| d.starts_with(&g.repo.top) && *d != g.repo.top) {
            if std::fs::remove_dir(dir).is_err() {
                break;
            }
            d = dir.parent();
        }
    }
    Ok(revert.iter().map(|e| e.rel.clone()).collect())
}

/// Restored files have new stat data, which `read-tree -m` reports as "not
/// uptodate": refresh the index (stat info only, like `git status` does).
fn refresh_index(g: G) {
    let _ = retry_index_lock(|| g.run(&["update-index", "-q", "--refresh"]));
}

// ── recovery ──────────────────────────────────────────────────────────────

enum Rec {
    None,
    Busy(String),
    Done(String),
}

/// Recover an install that a kill interrupted (locks.sync with a dead
/// owner): roll the worktree back, or finish an install that had already
/// moved HEAD, then remove only our own index.lock. Returns what was done;
/// None when nothing was left over (one shared read of the state, no child
/// process) or a live sync is still at work.
pub fn recover(repo: &Repo) -> Option<String> {
    match recover_in(G::new(repo)) {
        Rec::Done(m) => Some(m),
        _ => None,
    }
}

/// Our index.lock marker when the state file lost its locks.sync record.
pub(crate) fn marker_lock(lock_path: &Path) -> Option<SyncLock> {
    use std::io::Read;
    let mut buf = vec![0u8; 4096];
    let n = std::fs::File::open(lock_path).ok()?.read(&mut buf).ok()?;
    if n == 0 || buf[0] != b'{' {
        return None; // a real index starts with DIRC
    }
    let (j, _) = crate::json::parse_prefix(std::str::from_utf8(&buf[..n]).ok()?).ok()?;
    if j.get("agent-lock").str() != Some("sync") {
        return None;
    }
    let pid = j.get("pid").u64()? as u32;
    Some(SyncLock {
        by: j.get("by").str().unwrap_or("?").to_string(),
        owner: Some(Owner { pid, start: j.get("start").u64().unwrap_or(0) }),
        since: 0,
        head: j.get("head").str()?.to_string(),
        tip: j.get("tip").str()?.to_string(),
        paths: Vec::new(),
        index_tmp: j.get("index_tmp").str().map(str::to_string),
        lock_ino: ino(lock_path),
    })
}

/// The command lines of all processes; None when ps cannot run.
fn processes() -> Option<String> {
    let o = Command::new("ps").args(["-A", "-ww", "-o", "command="]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// A killed sync's orphaned child that may still be at work: `read-tree …
/// <tip>` writing the worktree, or `update-ref … HEAD <tip> <H>` moving HEAD.
/// `ps` (the process list) shows them; without it, a lock one of them would
/// hold (P.lock, HEAD.lock, the branch's ref lock) younger than GIT_ABANDON.
fn orphan_running(ps: Option<&str>, repo: &Repo, l: &SyncLock) -> Option<&'static str> {
    if let Some(ps) = ps {
        return ["read-tree", "update-ref"].into_iter().find(|k| ps.lines().any(|x| x.contains(k) && x.contains(&l.tip)));
    }
    let young = |p: &Path| {
        std::fs::metadata(p).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().map(|d| d.as_secs()).unwrap_or(0) < GIT_ABANDON)
    };
    if l.index_tmp.as_ref().is_some_and(|p| young(&lock_of(Path::new(p)))) {
        return Some("read-tree");
    }
    let head = std::fs::read_to_string(repo.git_dir.join("HEAD")).unwrap_or_default();
    let branch = head.trim().strip_prefix("ref: ").map(|r| lock_of(&common_dir(repo).join(r)));
    (young(&repo.git_dir.join("HEAD.lock")) || branch.is_some_and(|b| young(&b))).then_some("update-ref")
}

/// Undo `rollback`: those paths back to tip's content (or gone where tip
/// removes them), for an install whose HEAD moved to tip after all.
fn redo(g: G, h: Option<&str>, tip: &str, rels: &[String]) -> Res<()> {
    if rels.is_empty() {
        return Ok(());
    }
    let want: BTreeSet<&String> = rels.iter().collect();
    let ents = tree_diff(g, &h.map(str::to_string).unwrap_or_else(|| empty_tree(g)), tip)?;
    let (back, gone): (Vec<&Ent>, Vec<&Ent>) = ents.iter().filter(|e| want.contains(&e.rel)).partition(|e| e.b.is_some());
    let back: Vec<String> = back.iter().map(|e| e.rel.clone()).collect();
    if !back.is_empty() {
        checkout_from(g, tip, &back)?;
    }
    for e in gone {
        unlink(&g.repo.abs(&e.rel));
    }
    Ok(())
}

fn recover_in(g: G) -> Rec {
    let repo = g.repo;
    let lock_path = repo.git_dir.join("index.lock");
    let Ok(st) = state::read(repo) else { return Rec::None };
    let (lock, in_state) = match st.locks.sync {
        Some(l) => (l, true),
        None => match marker_lock(&lock_path) {
            Some(l) => (l, false),
            None => return Rec::None,
        },
    };
    let pid = lock.owner.map(|o| o.pid.to_string()).unwrap_or_else(|| "?".into());
    let who = format!("agent-lock sync (by {}, pid {pid})", lock.by);
    let dead = match lock.owner {
        Some(o) => sys::liveness(&o) == Live::Dead,
        None => sys::now().saturating_sub(lock.since) > GIT_ABANDON,
    };
    if !dead {
        return Rec::Busy(format!("an {who} is installing an upstream update; retry shortly"));
    }
    if let Some(what) = orphan_running(processes().as_deref(), repo, &lock) {
        return Rec::Busy(format!("an interrupted {who} left git {what} running; retry shortly"));
    }
    let p = lock.index_tmp.as_ref().map(PathBuf::from);
    // take it over first, so two recoverers never both act
    let me = sys::self_owner();
    let took = state::transact(repo, |st| {
        let same = match &st.locks.sync {
            Some(l) => in_state && l.since == lock.since && l.tip == lock.tip && l.owner == lock.owner,
            None => !in_state,
        };
        if same {
            st.locks.sync = Some(SyncLock { by: format!("recover:{}", lock.by), owner: me, since: sys::now(), ..lock.clone() });
            Ok(())
        } else {
            Err(st.locks.sync.is_some())
        }
    });
    match took {
        Ok(Ok(())) => {}
        Ok(Err(true)) => return Rec::Busy("another agent-lock command is recovering an interrupted sync; retry shortly".into()),
        _ => return Rec::None,
    }
    let head = rev(g, "HEAD");
    // our lock: the inode we linked, still holding our marker (an inode can be reused)
    let ours = lock.lock_ino.is_some() && ino(&lock_path) == lock.lock_ino && marker_lock(&lock_path).is_some_and(|m| m.tip == lock.tip);
    let index = repo.git_dir.join("index");
    let moved = |h: Option<String>| lock.tip != lock.head && h.as_deref() == Some(lock.tip.as_str());
    // HEAD is at tip: P is the finished index, install it
    let finish = || {
        let r = match &p {
            Some(p) if p.is_file() && ours => std::fs::rename(p, &index),
            Some(p) if p.is_file() && !lexists(&lock_path) => std::fs::hard_link(p, &lock_path).and_then(|_| std::fs::rename(&lock_path, &index)),
            Some(p) if p.is_file() => Err(io::Error::other("index.lock is held by another git process")),
            _ => Ok(()), // already renamed into place
        };
        match r {
            Ok(()) => "finished its install".to_string(),
            Err(e) => format!("could not finish its install ({e}); review `git status`"),
        }
    };
    let mut files = Vec::new();
    let how = if moved(head.clone()) {
        finish() // killed after update-ref
    } else {
        // read-tree may have written only once its index copy existed
        let wrote = p.as_ref().is_some_and(|p| lexists(p) || lexists(&lock_of(p)));
        let h = (!lock.head.is_empty()).then_some(lock.head.as_str());
        let mut trouble = String::new();
        if wrote && (ours || head.as_deref().unwrap_or("") == lock.head) {
            match rollback(g, h, &lock.tip, None) {
                Ok(f) => files = f,
                Err(e) => trouble = format!("; could not check the worktree ({}), review `git status`", e.lines().next().unwrap_or("")),
            }
        }
        at("rolled-back");
        if moved(rev(g, "HEAD")) {
            // its update-ref landed after all (past ps's look): forward again, then finish
            match redo(g, h, &lock.tip, &files) {
                Ok(()) => format!("{} (its git update-ref landed meanwhile)", finish()),
                Err(e) => format!("its git update-ref moved HEAD meanwhile and the worktree could not be brought forward ({e}); review `git status`"),
            }
        } else {
            let n = files.len();
            format!("{}{trouble}", if n > 0 { format!("rolled back {n} file(s) it had already written") } else { "nothing needed undoing".to_string() })
        }
    };
    if let Some(p) = &p {
        unlink(p);
        unlink(&lock_of(p));
        unlink(&body_of(p));
    }
    let removed = ours && unlink_if(&lock_path, lock.lock_ino);
    let tip = lock.tip.clone();
    clear_sync(g, me, &tip);
    if !files.is_empty() {
        refresh_index(g);
    }
    Rec::Done(format!("recovered from an interrupted {who}: {how}{}", if removed { "; removed the index.lock it left behind" } else { "" }))
}

// ── push ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PushOut {
    /// pushed | up-to-date | rejected | error | busy, or the sync status that stopped it
    /// (conflict | blocked | rewritten)
    pub status: &'static str,
    pub message: String,
    pub attempts: u32,
    pub head: Option<String>,
    pub files: Vec<String>,
    pub blocked_by: Vec<(String, String)>,
    pub recovery: Option<String>,
}

impl PushOut {
    fn new(status: &'static str, message: impl Into<String>, attempts: u32) -> PushOut {
        PushOut { status, message: message.into(), attempts, ..PushOut::default() }
    }

    pub fn ok(&self) -> bool {
        matches!(self.status, "pushed" | "up-to-date")
    }
}

/// Publish HEAD to origin/<branch>; on a race, sync and retry (≤ 5 attempts).
pub fn push(repo: &Repo, agent: &str, accept_rewrite: bool) -> PushOut {
    push_in(G::new(repo), agent, accept_rewrite)
}

fn push_in(g: G, agent: &str, accept: bool) -> PushOut {
    let Some(branch) = current_branch(g) else { return PushOut::new("error", "HEAD is detached; check out a branch first", 0) };
    if let Err(e) = need_version() {
        return PushOut::new("error", e, 0);
    }
    let _lock = match lock_git(&g, agent, "push") {
        Ok(l) => l,
        Err(m) => return PushOut::new("busy", m, 0),
    };
    let recovery = match recover_in(g) {
        Rec::Busy(m) => return PushOut::new("busy", m, 0),
        Rec::Done(m) => Some(m),
        Rec::None => None,
    };
    PushOut { recovery, ..push_steps(g, agent, &branch, accept) }
}

/// The porcelain line for `dst`: (flag, summary, line).
fn push_line(stdout: &str, dst: &str) -> Option<(char, String, String)> {
    stdout.lines().find_map(|l| {
        let mut it = l.splitn(3, '\t');
        let (flag, refs, summary) = (it.next()?, it.next()?, it.next()?);
        let mut f = flag.chars();
        let (Some(c), None) = (f.next(), f.next()) else { return None };
        refs.ends_with(&format!(":{dst}")).then(|| (c, summary.to_string(), l.to_string()))
    })
}

fn jitter(attempt: u32) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(attempt);
    let base = 2000u64.min(50 << attempt);
    Duration::from_millis(base / 2 + h.finish() % base.max(1))
}

fn push_steps(g: G, agent: &str, branch: &str, accept: bool) -> PushOut {
    let dst = format!("refs/heads/{branch}");
    let mut no_lease = false;
    let mut last = String::new();
    for attempt in 1..=PUSH_TRIES {
        let Some(oid) = rev(g, "HEAD") else { return PushOut::new("error", "nothing to push (no commits yet)", attempt - 1) };
        // Lease on the last integrated upstream tip, only when it is in our
        // history (so it can never force a non-fast-forward): a moved or
        // rolled-back upstream refuses the push and routes it through sync.
        let rec = if no_lease { None } else { upstream_oid(g, branch) };
        let lease = rec.filter(|r| is_ancestor(g, r, &oid)).map(|r| format!("--force-with-lease={dst}:{r}"));
        let refspec = format!("{oid}:{dst}");
        let mut a = vec!["push", "--porcelain"];
        if let Some(l) = &lease {
            a.push(l);
        }
        a.extend([REMOTE, refspec.as_str()]);
        let r = match g.run(&a) {
            Ok(r) => r,
            Err(e) => return PushOut::new("error", e.to_string(), attempt),
        };
        let p = push_line(&r.text(), &dst);
        match p {
            Some((flag, _, _)) if r.ok() && flag != '!' => {
                set_upstream(g, branch, &oid);
                let out = if flag == '=' {
                    PushOut::new("up-to-date", format!("{REMOTE}/{branch} is already at {}", short(&oid)), attempt)
                } else {
                    PushOut::new("pushed", format!("pushed {} to {REMOTE}/{branch}", short(&oid)), attempt)
                };
                return PushOut { head: Some(oid), ..out };
            }
            None => {
                let m = said(&r);
                return PushOut::new("error", if m.is_empty() { format!("git push exited {}", r.status) } else { m }, attempt);
            }
            Some((_, summary, line)) if !PUSH_RETRY.iter().any(|x| summary.contains(x)) => {
                return PushOut::new("rejected", format!("{line}\n{}", r.stderr.trim()).trim().to_string(), attempt);
            }
            Some((_, _, line)) => last = line,
        }
        if attempt == PUSH_TRIES {
            break;
        }
        let s = sync_locked(g, agent, branch, accept);
        if matches!(s.status, "conflict" | "blocked" | "rewritten" | "error" | "busy") {
            return PushOut { files: s.files, blocked_by: s.blocked_by, head: s.head, ..PushOut::new(s.status, s.message, attempt) };
        }
        if s.status == "no-upstream" {
            no_lease = true;
        }
        std::thread::sleep(jitter(attempt));
    }
    PushOut::new("rejected", format!("gave up after {PUSH_TRIES} attempts: {last}"), PUSH_TRIES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Claim;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Hermetic git for every call (passed explicitly: tests run in parallel threads).
    const ENV: &[(&str, &str)] = &[
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_ATTR_NOSYSTEM", "1"),
        ("HOME", "/nonexistent-agent-lock-test"),
        ("XDG_CONFIG_HOME", "/nonexistent-agent-lock-test"),
        ("GIT_AUTHOR_NAME", "Test"),
        ("GIT_AUTHOR_EMAIL", "test@example.com"),
        ("GIT_COMMITTER_NAME", "Test"),
        ("GIT_COMMITTER_EMAIL", "test@example.com"),
        ("GIT_EDITOR", "true"),
    ];
    const ME: &str = "aaaaaaaa";
    const B: &str = "bbbbbbbb";

    struct T {
        base: PathBuf,
        repo: Repo,
        origin: PathBuf,
    }

    impl Drop for T {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                let _ = std::fs::remove_dir_all(&self.base);
            }
        }
    }

    fn g(r: &Repo) -> G<'_> {
        G { repo: r, env: ENV }
    }

    fn try_git(r: &Repo, args: &[&str]) -> Out {
        g(r).run(args).unwrap()
    }

    fn git(r: &Repo, args: &[&str]) -> String {
        let o = try_git(r, args);
        assert!(o.ok(), "git {args:?}: {}", said(&o));
        let t = o.text();
        t.strip_suffix('\n').unwrap_or(&t).to_string()
    }

    fn write(r: &Repo, rel: &str, text: &str) {
        let p = r.abs(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(r: &Repo, rel: &str) -> String {
        std::fs::read_to_string(r.abs(rel)).unwrap_or_else(|_| "<missing>".into())
    }

    fn s(a: &[&str]) -> Vec<String> {
        a.iter().map(|x| x.to_string()).collect()
    }

    fn hook(r: &Repo, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = r.git_dir.join("hooks").join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn at_dir(dir: &Path) -> Repo {
        Repo { top: dir.to_path_buf(), git_dir: dir.join(".git") }
    }

    static N: AtomicUsize = AtomicUsize::new(0);

    /// A repo (main, one commit, pushed to a bare origin). Every repo's first
    /// commit is unique, so no two tests ever share an oid.
    fn setup(files: &[(&str, &str)]) -> T {
        let base = std::env::temp_dir().join(format!("al3-git-{}-{}", sys::pid(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let base = std::fs::canonicalize(base).unwrap();
        let b = at_dir(&base);
        git(&b, &["init", "-q", "-b", "main", "work"]);
        git(&b, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
        let repo = at_dir(&base.join("work"));
        write(&repo, "id.txt", &format!("{}\n", base.display()));
        for (f, c) in files {
            write(&repo, f, c);
        }
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        let origin = base.join("origin.git");
        git(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&repo, &["push", "-q", "-u", "origin", "main"]);
        T { base, repo, origin }
    }

    /// "Another machine": a clone that commits and pushes to origin.
    fn upstream(t: &T, files: &[(&str, Option<&str>)], msg: &str) -> String {
        let dir = t.base.join("up");
        if !dir.exists() {
            git(&at_dir(&t.base), &["clone", "-q", "-b", "main", t.origin.to_str().unwrap(), "up"]);
        }
        let up = at_dir(&dir);
        git(&up, &["fetch", "-q", "origin"]);
        git(&up, &["reset", "-q", "--hard", "origin/main"]);
        for (f, c) in files {
            match c {
                Some(c) => write(&up, f, c),
                None => {
                    let _ = std::fs::remove_file(up.abs(f));
                }
            }
        }
        git(&up, &["add", "-A", "-f"]);
        git(&up, &["commit", "-q", "-m", msg]);
        git(&up, &["push", "-q", "origin", "main"]);
        git(&up, &["rev-parse", "HEAD"])
    }

    fn head(r: &Repo) -> String {
        git(r, &["rev-parse", "HEAD"])
    }

    fn porcelain(r: &Repo) -> Vec<String> {
        let mut v: Vec<String> =
            git(r, &["status", "--porcelain", "--untracked-files=all"]).lines().map(str::to_string).filter(|l| !l.is_empty()).collect();
        v.sort();
        v
    }

    fn staged(r: &Repo) -> String {
        git(r, &["diff", "--cached", "--name-only"])
    }

    fn leftovers(r: &Repo) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&r.git_dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with("agent-lock-") && n != "agent-lock.json" || n == "index.lock")
            .collect();
        v.sort();
        v
    }

    fn st(r: &Repo) -> State {
        state::read(r).unwrap()
    }

    fn dead() -> Owner {
        let mut c = Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        Owner { pid, start: 1 }
    }

    /// A live process that is not this one (the test runner's parent).
    fn other_alive() -> Owner {
        sys::owner_of(std::os::unix::process::parent_id()).expect("parent alive")
    }

    fn claim(r: &Repo, path: &str, by: &str, owner: Option<Owner>, edits: u32, pending: Option<&str>) {
        let now = sys::now();
        state::transact(r, |st| {
            st.put_claim(Claim {
                path: path.into(),
                by: by.into(),
                owner,
                at: now,
                touched: now,
                edits,
                pending: pending.map(str::to_string),
                ..Claim::default()
            })
        })
        .unwrap();
    }

    fn on_stage(f: impl FnMut(&str) + 'static) {
        AT.with(|h| *h.borrow_mut() = Some(Box::new(f)));
    }

    fn no_stage() {
        AT.with(|h| *h.borrow_mut() = None);
    }

    /// Run `f` and "die" (unwind without any cleanup code) at `stage`; then
    /// make the sync's owner dead, as a SIGKILL would.
    fn killed_at(r: &Repo, stage: &'static str, f: impl FnOnce()) {
        on_stage(move |s| {
            if s == stage {
                panic!("simulated kill at {stage}");
            }
        });
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        no_stage();
        assert!(res.is_err(), "never reached {stage}");
        let d = dead();
        state::transact(r, |st| {
            if let Some(l) = st.locks.sync.as_mut() {
                l.owner = Some(d);
            }
        })
        .unwrap();
        assert!(st(r).locks.sync.is_some(), "the kill left locks.sync behind");
    }

    fn commit(r: &Repo, paths: &[&str], msg: &str) -> CommitOut {
        commit_in(g(r), ME, &s(paths), msg, false, false, false)
    }

    fn do_sync(r: &Repo) -> SyncOut {
        sync_in(g(r), ME, false)
    }

    // ── commit ────────────────────────────────────────────────────────────

    #[test]
    fn commit_is_scoped_and_leaves_others_staged_and_unstaged_work() {
        let t = setup(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
        let r = &t.repo;
        let before = head(r);
        write(r, "a.txt", "a2\n");
        write(r, "b.txt", "b2 (other agent, unstaged)\n");
        write(r, "c.txt", "c2 (other agent, staged)\n");
        git(r, &["add", "c.txt"]);
        write(r, "c.txt", "c3 (other agent, unstaged on top)\n");
        let out = commit(r, &["a.txt"], "A: change a");
        assert!(out.ok && out.code.is_none(), "{out:?}");
        assert_eq!(out.committed, ["a.txt"]);
        assert_eq!(out.sha.as_deref(), Some(head(r).as_str()));
        assert_eq!(git(r, &["rev-parse", "HEAD^"]), before);
        assert_eq!(git(r, &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]), "a.txt");
        assert_eq!(git(r, &["log", "-1", "--format=%B"]), "A: change a\n");
        assert_eq!(porcelain(r), [" M b.txt", "MM c.txt"]);
        assert_eq!(git(r, &["show", ":c.txt"]), "c2 (other agent, staged)");
        assert_eq!(read(r, "c.txt"), "c3 (other agent, unstaged on top)\n");
        assert!(leftovers(r).is_empty(), "{:?}", leftovers(r));
        assert!(st(r).locks.git.is_none(), "locks.git released");
    }

    #[test]
    fn commit_new_deleted_renamed_spaced_glob_unicode_and_exec_bit() {
        let t = setup(&[("old.txt", "o\n"), ("gone.txt", "g\n"), ("mv-src.txt", "m\n"), ("script.sh", "echo hi\n"), ("*.md", "lit\n"), ("a.md", "a\n")]);
        let r = &t.repo;
        write(r, "dir/new file.txt", "new\n");
        write(r, "[x]?.txt", "x\n");
        write(r, "ünï cödé.txt", "u\n");
        std::fs::remove_file(r.abs("old.txt")).unwrap();
        git(r, &["rm", "-q", "gone.txt"]);
        git(r, &["mv", "mv-src.txt", "mv-dst.txt"]);
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(r.abs("script.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        write(r, "*.md", "lit2\n");
        write(r, "a.md", "other agent\n");
        let mine = ["dir/new file.txt", "[x]?.txt", "ünï cödé.txt", "old.txt", "gone.txt", "mv-src.txt", "mv-dst.txt", "script.sh", "*.md"];
        let message = "subject line\n\nbody line\n# not a comment\n";
        let out = commit(r, &mine, message);
        assert!(out.ok, "{out:?}");
        let mut got = out.committed.clone();
        got.sort();
        let mut want = s(&mine);
        want.sort();
        assert_eq!(got, want);
        assert_eq!(git(r, &["log", "-1", "--format=%B"]), message);
        assert!(git(r, &["ls-tree", "HEAD", "script.sh"]).starts_with("100755 "));
        let tree = git(r, &["ls-tree", "-r", "-z", "--name-only", "HEAD"]);
        let tree: Vec<&str> = tree.split('\0').collect();
        for f in ["old.txt", "gone.txt", "mv-src.txt"] {
            assert!(!tree.contains(&f), "{f} still in HEAD");
        }
        for f in ["mv-dst.txt", "dir/new file.txt", "[x]?.txt", "ünï cödé.txt", "*.md"] {
            assert!(tree.contains(&f), "{f} not in HEAD: {tree:?}");
        }
        assert_eq!(porcelain(r), [" M a.md"], "only the other agent's file is left");
    }

    #[test]
    fn commit_on_an_unborn_head() {
        let t = setup(&[]);
        let r = at_dir(&t.base.join("fresh"));
        std::fs::create_dir_all(&r.top).unwrap();
        git(&r, &["init", "-q", "-b", "main"]);
        write(&r, "mine.txt", "m\n");
        write(&r, "other.txt", "o\n");
        let out = commit(&r, &["mine.txt"], "first");
        assert!(out.ok, "{out:?}");
        assert_eq!(git(&r, &["ls-tree", "--name-only", "HEAD"]), "mine.txt");
        assert_eq!(porcelain(&r), ["?? other.txt"]);
    }

    #[test]
    fn nothing_to_commit_and_dry_run() {
        let t = setup(&[("a.txt", "a\n"), (".gitignore", "*.log\n")]);
        let r = &t.repo;
        let before = head(r);
        write(r, "x.log", "x\n");
        let out = commit(r, &["a.txt", "x.log", "missing.txt"], "m");
        assert_eq!((out.ok, out.code), (true, Some("nothing")), "{out:?}");
        assert_eq!(out.skipped, ["a.txt", "x.log", "missing.txt"]);
        write(r, "a.txt", "a2\n");
        write(r, "n.txt", "n\n");
        hook(r, "pre-commit", "echo ran > hook-ran.txt");
        let out = commit_in(g(r), ME, &s(&["a.txt", "n.txt"]), "", false, true, false);
        assert!(out.ok && out.sha.is_none(), "{out:?}");
        assert_eq!(out.committed, ["a.txt", "n.txt"]);
        assert_eq!(head(r), before);
        assert_eq!(porcelain(r), [" M a.txt", "?? n.txt"]);
        assert!(!r.abs("hook-ran.txt").exists(), "a dry run never runs hooks");
        assert!(leftovers(r).is_empty());
        assert_eq!(commit(r, &["a.txt"], "  ").code, Some("commit-failed"), "empty message");
        assert_eq!(commit(r, &["../x"], "m").code, Some("commit-failed"), "outside the worktree");
    }

    #[test]
    fn pre_commit_sees_exactly_head_plus_own_paths() {
        let t = setup(&[("a.txt", "a\n"), ("c.txt", "c\n")]);
        let r = &t.repo;
        write(r, "a.txt", "a2\n");
        write(r, "c.txt", "c2\n");
        git(r, &["add", "c.txt"]);
        let seen = t.base.join("seen");
        hook(r, "pre-commit", &format!("git diff --cached --name-only > '{}'", seen.display()));
        let out = commit(r, &["a.txt"], "m");
        assert!(out.ok, "{out:?}");
        assert_eq!(std::fs::read_to_string(&seen).unwrap(), "a.txt\n");
        assert_eq!(porcelain(r), ["M  c.txt"]);
    }

    #[test]
    fn hook_that_stages_others_is_hook_staged_and_changes_nothing() {
        let t = setup(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
        let r = &t.repo;
        let before = head(r);
        write(r, "a.txt", "a2\n");
        write(r, "b.txt", "b2 (other agent)\n");
        hook(r, "pre-commit", "git add -u");
        let out = commit(r, &["a.txt"], "m");
        assert_eq!(out.code, Some("hook-staged"), "{out:?}");
        assert_eq!(out.extra, ["b.txt"]);
        assert!(out.message.contains("b.txt"));
        assert_eq!(head(r), before);
        assert_eq!(staged(r), "");
        assert_eq!(porcelain(r), [" M a.txt", " M b.txt"]);
        assert!(leftovers(r).is_empty());
    }

    #[test]
    fn failing_pre_commit_leaves_nothing_staged() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let before = head(r);
        write(r, "a.txt", "a2\n");
        write(r, "n.txt", "n\n");
        hook(r, "pre-commit", "echo 'lint failed' >&2; exit 1");
        let out = commit(r, &["a.txt", "n.txt"], "m");
        assert_eq!(out.code, Some("hook-failed"), "{out:?}");
        assert!(out.message.contains("lint failed") && out.hook_output.contains("lint failed"));
        assert_eq!(head(r), before);
        assert_eq!(staged(r), "");
        assert_eq!(porcelain(r), [" M a.txt", "?? n.txt"], "no intent-to-add left behind");
    }

    #[test]
    fn commit_msg_hook_may_rewrite_the_message() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        write(r, "a.txt", "a2\n");
        hook(r, "commit-msg", r#"printf "%s\n\nSigned-off-by: Hook <h@x>\n" "$(head -n1 "$1") [checked]" > "$1""#);
        let out = commit(r, &["a.txt"], "original");
        assert!(out.ok, "{out:?}");
        assert_eq!(git(r, &["log", "-1", "--format=%B"]), "original [checked]\n\nSigned-off-by: Hook <h@x>\n");
    }

    #[test]
    fn failing_commit_resets_intent_to_add() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let before = head(r);
        write(r, "a.txt", "a2\n");
        write(r, "n.txt", "n\n");
        hook(r, "prepare-commit-msg", "exit 1");
        let out = commit(r, &["a.txt", "n.txt"], "m");
        assert_eq!(out.code, Some("commit-failed"), "{out:?}");
        assert_eq!(head(r), before);
        assert_eq!(staged(r), "");
        assert_eq!(porcelain(r), [" M a.txt", "?? n.txt"]);
        assert!(st(r).locks.git.is_none());
    }

    #[test]
    fn dangerous_hooks_refused_when_asked_and_no_verify_commits() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let before = head(r);
        write(r, "a.txt", "a2\n");
        hook(r, "pre-commit", "# would run: git stash --keep-index\nexit 0");
        let out = commit_in(g(r), ME, &s(&["a.txt"]), "m", false, false, true);
        assert_eq!(out.code, Some("dangerous-hooks"), "{out:?}");
        assert!(out.message.contains("stash") && out.message.contains("--no-verify"));
        assert_eq!(head(r), before);
        assert!(commit_in(g(r), ME, &s(&["a.txt"]), "m", true, false, true).ok, "--no-verify");
        write(r, "a.txt", "a3\n");
        assert!(commit_in(g(r), ME, &s(&["a.txt"]), "m", false, false, false).ok, "not refused: the hooks run");
    }

    #[test]
    fn stash_hook_that_fails_is_hook_clobbered_naming_files_and_stash() {
        let t = setup(&[("b.txt", "b base\n"), ("scripts/check.sh", "set -e\ngit stash push -q --keep-index\nfalse\ngit stash pop -q\n")]);
        let r = &t.repo;
        let before = head(r);
        write(r, "a.txt", "a\n");
        write(r, "b.txt", "B precious uncommitted work\n");
        write(r, "u.txt", "B untracked\n");
        hook(r, "pre-commit", "exec sh scripts/check.sh");
        let out = commit(r, &["a.txt"], "A");
        assert_eq!(out.code, Some("hook-clobbered"), "{out:?}");
        assert_eq!(out.extra, ["b.txt"]);
        let stash = git(r, &["rev-parse", "refs/stash"]);
        assert!(out.message.contains("stash@{0}") && out.message.contains(&stash[..12]), "{}", out.message);
        assert!(out.message.contains("b.txt") && out.message.contains("git stash pop"));
        assert!(out.message.contains("pre-commit hook (which also failed)"));
        assert_eq!(head(r), before);
        assert_eq!(staged(r), "");
        assert_eq!(read(r, "b.txt"), "b base\n", "reported, never repaired over a peer");
        assert!(leftovers(r).is_empty());
    }

    #[test]
    fn clean_stash_pop_is_fine_but_a_rewritten_peer_file_is_reported() {
        let t = setup(&[("b.txt", "b base\n")]);
        let r = &t.repo;
        write(r, "a.txt", "a\n");
        write(r, "b.txt", "B work\n");
        hook(r, "pre-commit", "git stash push -q --keep-index && git stash pop -q");
        let out = commit(r, &["a.txt"], "A");
        assert!(out.ok, "{out:?}");
        assert_eq!(read(r, "b.txt"), "B work\n");
        assert_eq!(git(r, &["stash", "list"]), "");
        write(r, "a.txt", "a2\n");
        hook(r, "pre-commit", r#"printf "formatted\n" > b.txt"#);
        let out = commit(r, &["a.txt"], "A2");
        assert_eq!(out.code, Some("hook-clobbered"), "{out:?}");
        assert_eq!(out.extra, ["b.txt"]);
        assert!(out.message.contains("changed while") && !out.message.contains("stash@"));
        assert_eq!(git(r, &["log", "-1", "--format=%s"]), "A");
    }

    #[test]
    fn hook_damage_ignores_owner_activity_commits_in_flight_and_unclaimed_untracked() {
        let t = setup(&[("b.txt", "b base\n"), ("c.txt", "c base\n")]);
        let r = &t.repo;
        let gg = g(r);
        write(r, "b.txt", "B work\n");
        write(r, "c.txt", "C work\n");
        write(r, ".lintcache", "old\n");
        write(r, "new.ts", "B new file\n");
        claim(r, "b.txt", B, Some(other_alive()), 1, None);
        claim(r, "c.txt", "cccccccc", Some(other_alive()), 1, Some(&rules::proc_marker("commit", other_alive())));
        claim(r, "new.ts", B, Some(other_alive()), 1, None);
        let want: BTreeSet<String> = [String::from("a.txt")].into();
        let key = |p: &str| p.to_string();
        let snap = snapshot(gg, &want, &key, ME);
        // b: its owner edits it (the claim record changes); c: in its owner's commit
        write(r, "b.txt", "B work, next edit\n");
        claim(r, "b.txt", B, Some(other_alive()), 2, None);
        write(r, "c.txt", "C formatted by C's own hooks\n");
        // an unclaimed untracked cache rewritten: nobody's work
        write(r, ".lintcache", "new\n");
        assert!(clobbered(gg, &snap, ME).is_none());
        // the same rewrite with no claim activity counts
        let snap = snapshot(gg, &want, &key, ME);
        write(r, "b.txt", "clobbered\n");
        write(r, "new.ts", "formatted\n");
        std::fs::remove_file(r.abs(".lintcache")).unwrap();
        let c = clobbered(gg, &snap, ME).expect("damage");
        assert_eq!(c.files, [".lintcache", "b.txt", "new.ts"]);
        assert!(c.stash == c.prev);
        // same content rewritten (new mtime) is not damage
        let snap = snapshot(gg, &want, &key, ME);
        std::thread::sleep(Duration::from_millis(5));
        write(r, "b.txt", "clobbered\n");
        assert!(clobbered(gg, &snap, ME).is_none());
    }

    #[test]
    fn staged_untrack_is_refused_and_stays_staged() {
        let t = setup(&[("config.env", "TOKEN=placeholder\n")]);
        let r = &t.repo;
        let before = head(r);
        git(r, &["rm", "-q", "--cached", "config.env"]);
        write(r, "config.env", "TOKEN=real-secret\n");
        let out = commit(r, &["config.env"], "untrack");
        assert_eq!(out.code, Some("staged-untrack"), "{out:?}");
        assert_eq!(out.extra, ["config.env"]);
        assert_eq!(head(r), before);
        assert_eq!(git(r, &["diff", "--cached", "--name-status"]), "D\tconfig.env");
        // the same for a file that is ignored by now
        write(r, ".gitignore", "config.env\n");
        let out = commit(r, &["config.env"], "untrack");
        assert_eq!(out.code, Some("staged-untrack"), "{out:?}");
    }

    #[test]
    fn case_only_rename_is_refused() {
        let t = setup(&[("Foo.txt", "f\n")]);
        let r = &t.repo;
        if !r.ignore_case() {
            return; // case-sensitive filesystem
        }
        let before = head(r);
        git(r, &["mv", "Foo.txt", "foo.txt"]);
        let out = commit(r, &["Foo.txt", "foo.txt"], "rename");
        assert_eq!(out.code, Some("case-rename"), "{out:?}");
        assert_eq!(out.extra, ["Foo.txt", "foo.txt"]);
        assert_eq!(head(r), before);
    }

    #[test]
    fn a_case_only_rename_named_by_either_spelling_is_a_case_rename() {
        // gitcli/GIT-9: the CLI passes one spelling per folded key; it read as `git rm --cached`
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        if !r.ignore_case() {
            return;
        }
        git(r, &["mv", "a.txt", "A.txt"]);
        for one in ["a.txt", "A.txt"] {
            let out = commit(r, &[one], "rename");
            assert_eq!(out.code, Some("case-rename"), "{one}: {out:?}");
            assert_eq!(out.extra, ["a.txt", "A.txt"]);
        }
    }

    #[test]
    fn a_peer_rewriting_its_own_file_through_bash_is_not_hook_damage() {
        // gitcli/GIT-4: `agent-lock mine -z | xargs -0 -r <formatter>` while our hooks run
        use crate::state::{BashSnap, Session};
        let t = setup(&[("b.txt", "b base\n"), ("c.txt", "c base\n")]);
        let r = &t.repo;
        let gg = g(r);
        write(r, "b.txt", "B work\n");
        write(r, "c.txt", "my other work\n");
        claim(r, "b.txt", B, Some(other_alive()), 1, None);
        claim(r, "c.txt", ME, sys::self_owner(), 1, None);
        // a Bash call in flight for B, and for us (this commit runs in it)
        state::transact(r, |st| {
            for (id, agent) in [("sb", B), ("sa", ME)] {
                let snap = BashSnap { agent: agent.into(), at: sys::now(), files: Vec::new() };
                let bash = BTreeMap::from([(format!("toolu_{id}"), snap)]);
                st.sessions.insert(id.into(), Session { id: id.into(), agent: agent.into(), seen: sys::now(), bash, ..Session::default() });
            }
        })
        .unwrap();
        let want: BTreeSet<String> = [String::from("a.txt")].into();
        let key = |p: &str| p.to_string();
        let snap = snapshot(gg, &want, &key, ME);
        write(r, "b.txt", "B work, formatted\n");
        assert!(clobbered(gg, &snap, ME).is_none(), "B rewrote its own file");
        // B's Bash ended before the hooks did: still B's
        let snap = snapshot(gg, &want, &key, ME);
        state::transact(r, |st| st.sessions.get_mut("sb").unwrap().bash.clear()).unwrap();
        write(r, "b.txt", "B work, formatted twice\n");
        assert!(clobbered(gg, &snap, ME).is_none());
        // no Bash of B's: the hooks did it; and our own Bash never excuses our other files
        let snap = snapshot(gg, &want, &key, ME);
        write(r, "b.txt", "clobbered\n");
        write(r, "c.txt", "clobbered\n");
        assert_eq!(clobbered(gg, &snap, ME).expect("damage").files, ["b.txt", "c.txt"]);
    }

    #[test]
    fn head_moving_onto_our_paths_while_hooks_ran_fails_and_commits_nothing() {
        let t = setup(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
        let r = &t.repo;
        hook(r, "pre-commit", "true");
        write(r, "a.txt", "a2\n");
        let rr = r.clone();
        on_stage(move |s| {
            if s == "preflight-done" {
                git(&rr, &["commit", "-q", "-m", "raw", "--", "a.txt"]);
            }
        });
        let out = commit(r, &["a.txt"], "mine");
        no_stage();
        assert_eq!(out.code, Some("commit-failed"), "{out:?}");
        assert!(out.message.contains("HEAD moved"));
        assert_eq!(git(r, &["log", "-1", "--format=%s"]), "raw");
        // without hooks (no preflight) the same race is just nothing left to commit
        std::fs::remove_file(r.git_dir.join("hooks/pre-commit")).unwrap();
        write(r, "a.txt", "a2b\n");
        let rr = r.clone();
        on_stage(move |s| {
            if s == "preflight-done" {
                git(&rr, &["commit", "-q", "-m", "raw 2", "--", "a.txt"]);
            }
        });
        let out = commit(r, &["a.txt"], "mine");
        no_stage();
        assert_eq!((out.ok, out.code), (true, Some("nothing")), "{out:?}");
        assert_eq!(git(r, &["log", "-1", "--format=%s"]), "raw 2");
        // a move that does not touch our paths is fine
        write(r, "a.txt", "a3\n");
        write(r, "b.txt", "b2\n");
        let rr = r.clone();
        on_stage(move |s| {
            if s == "preflight-done" {
                git(&rr, &["commit", "-q", "-m", "raw b", "--", "b.txt"]);
            }
        });
        let out = commit(r, &["a.txt"], "mine");
        no_stage();
        assert!(out.ok, "{out:?}");
        assert_eq!(git(r, &["log", "-2", "--format=%s"]), "mine\nraw b");
        assert_eq!(git(r, &["show", "HEAD:a.txt"]), "a3");
    }

    #[test]
    fn locks_git_waits_for_a_live_holder_and_takes_a_dead_ones() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        write(r, "a.txt", "a2\n");
        let hold = |o: Owner| {
            state::transact(r, |st| st.locks.git = Some(OpLock { by: B.into(), owner: Some(o), since: sys::now(), what: "push".into() })).unwrap()
        };
        hold(other_alive());
        let t0 = Instant::now();
        let out = commit(r, &["a.txt"], "m");
        assert_eq!(out.code, Some("busy"), "{out:?}");
        assert!(out.message.contains("push") && out.message.contains(B), "{}", out.message);
        assert!(t0.elapsed() >= GIT_WAIT);
        assert_eq!(do_sync(r).status, "busy");
        hold(dead());
        assert!(commit(r, &["a.txt"], "m").ok);
        assert!(st(r).locks.git.is_none());
    }

    #[test]
    fn commit_and_sync_wait_for_a_foreign_index_lock() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let u = upstream(&t, &[("u.txt", Some("u\n"))], "up");
        let lock = r.git_dir.join("index.lock");
        let release = |ms: u64| {
            std::fs::write(&lock, "").unwrap();
            let l = lock.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ms));
                std::fs::remove_file(l).unwrap();
            })
        };
        write(r, "n.txt", "new\n");
        let j = release(300);
        let out = commit(r, &["n.txt"], "m");
        j.join().unwrap();
        assert!(out.ok, "{out:?}");
        let j = release(300);
        let out = do_sync(r);
        j.join().unwrap();
        assert_eq!(out.status, "rebased", "{out:?}");
        assert_eq!(git(r, &["rev-parse", "HEAD^"]), u);
        // held for good: sync gives up and leaves it alone
        std::fs::write(&lock, "").unwrap();
        upstream(&t, &[("v.txt", Some("v\n"))], "up 2");
        let out = do_sync(r);
        assert_eq!(out.status, "busy", "{out:?}");
        assert!(out.message.contains("never delete it"));
        assert!(lock.exists() && st(r).locks.sync.is_none());
        std::fs::remove_file(&lock).unwrap();
    }

    #[test]
    fn dangerous_hook_scanner() {
        assert_eq!(danger("git stash -q"), [STASH]);
        for body in ["git -C . stash push -q", "git --no-pager stash", "/usr/bin/git -c core.quotepath=off stash"] {
            assert_eq!(danger(body), [STASH], "{body}");
        }
        assert_eq!(danger("git checkout -- ."), [CHECKOUT]);
        assert_eq!(danger("git checkout -f ."), [CHECKOUT]);
        assert!(danger("git checkout -- src").is_empty());
        assert_eq!(danger("git reset --hard HEAD"), [RESET]);
        assert!(danger("git reset --soft HEAD~1; git stashed").is_empty());
        assert_eq!(danger("npx lint-staged"), [LINT_STAGED]);
        assert!(danger("npx lint-staged --no-stash").is_empty());
        assert_eq!(danger("# File generated by pre-commit: https://pre-commit.com"), [PRE_COMMIT_COM]);
        assert!(danger("npm test && cargo fmt --check").is_empty());

        let t = setup(&[]);
        let r = &t.repo;
        let gg = g(r);
        write(r, "scripts/check.sh", "set -e\ngit stash push -q --keep-index\nnpm test\ngit stash pop -q\n");
        hook(r, "pre-commit", "exec sh scripts/check.sh");
        let dangerous_hooks = |g: G| super::dangerous_hooks(g, &hooks(g).0);
        let hooks_present = |g: G| hooks(g).1;
        assert_eq!(dangerous_hooks(gg), [format!("pre-commit: {STASH} via scripts/check.sh")]);
        hook(r, "pre-commit", r#"./scripts/check.sh "$@""#);
        assert_eq!(dangerous_hooks(gg).len(), 1, "direct invocation");
        hook(r, "pre-commit", ". scripts/check.sh");
        assert_eq!(dangerous_hooks(gg).len(), 1, "sourced");
        write(r, "Makefile", "CC := cc\nlint:\n\techo lint\n.PHONY: precommit\nprecommit: lint\n\tgit stash -q --keep-index\n\t$(MAKE) test\n");
        hook(r, "pre-commit", "make -s precommit");
        let found = dangerous_hooks(gg);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("stash") && found[0].ends_with("Makefile:precommit"), "{found:?}");
        hook(r, "pre-commit", "make lint");
        assert!(dangerous_hooks(gg).is_empty(), "other make target");
        write(r, "scripts/check.sh", "npm test\n");
        hook(r, "pre-commit", "exec sh scripts/check.sh");
        assert!(dangerous_hooks(gg).is_empty(), "harmless script");
        // husky: core.hooksPath .husky/_ running lint-staged through an npm script
        std::fs::remove_file(r.git_dir.join("hooks/pre-commit")).unwrap();
        write(r, ".husky/_/pre-commit", "#!/bin/sh\n. \"$(dirname \"$0\")/h\"\n");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(r.abs(".husky/_/pre-commit"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        write(r, ".husky/pre-commit", "npm run precommit\n");
        write(r, "package.json", r#"{"scripts":{"precommit":"lint-staged"}}"#);
        git(r, &["config", "core.hooksPath", ".husky/_"]);
        assert!(hooks_present(gg));
        assert_eq!(dangerous_hooks(gg), [format!("pre-commit: {LINT_STAGED}")]);
        write(r, "package.json", r#"{"scripts":{"precommit":"lint-staged --no-stash"}}"#);
        assert!(dangerous_hooks(gg).is_empty());
        git(r, &["config", "--unset", "core.hooksPath"]);
        assert!(!hooks_present(gg));
    }

    // ── sync ──────────────────────────────────────────────────────────────

    #[test]
    fn sync_up_to_date_and_ahead_records_upstream() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let out = do_sync(r);
        assert_eq!((out.status, out.message.as_str()), ("up-to-date", "up to date"), "{out:?}");
        assert_eq!(st(r).upstream.map(|u| u.oid), Some(head(r)));
        write(r, "a.txt", "a2\n");
        assert!(commit(r, &["a.txt"], "local").ok);
        let out = do_sync(r);
        assert_eq!(out.status, "up-to-date");
        assert!(out.message.contains("1 local commit(s) not pushed"), "{}", out.message);
        assert_eq!(st(r).upstream.map(|u| u.oid), Some(git(r, &["rev-parse", "HEAD^"])));
    }

    #[test]
    fn sync_fast_forward_keeps_others_dirty_and_staged_files() {
        let t = setup(&[("x.txt", "x\n"), ("y.txt", "y\n"), ("z.txt", "z\n")]);
        let r = &t.repo;
        let u = upstream(&t, &[("x.txt", Some("x upstream\n")), ("new/n.txt", Some("n\n"))], "up");
        write(r, "y.txt", "y dirty, B's\n");
        claim(r, "y.txt", B, Some(other_alive()), 3, None);
        write(r, "z.txt", "z staged\n");
        git(r, &["add", "z.txt"]);
        let out = do_sync(r);
        assert_eq!(out.status, "fast-forward", "{out:?}");
        assert_eq!(out.head.as_deref(), Some(u.as_str()));
        assert_eq!(out.files, ["new/n.txt", "x.txt"]);
        assert_eq!(head(r), u);
        assert_eq!(read(r, "x.txt"), "x upstream\n");
        assert_eq!(read(r, "new/n.txt"), "n\n");
        assert_eq!(read(r, "y.txt"), "y dirty, B's\n");
        assert_eq!(porcelain(r), [" M y.txt", "M  z.txt"]);
        let s = st(r);
        assert!(s.locks.sync.is_none() && s.locks.git.is_none());
        assert_eq!(s.upstream.map(|u| u.oid), Some(u));
        assert!(leftovers(r).is_empty(), "{:?}", leftovers(r));
    }

    #[test]
    fn sync_replays_diverged_commits_keeping_authorship() {
        let t = setup(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
        let r = &t.repo;
        let u = upstream(&t, &[("b.txt", Some("b upstream\n"))], "up");
        write(r, "a.txt", "a local\n");
        git(r, &["add", "a.txt"]);
        let msg = "local: a\n\nbody  with  spaces\n";
        let o = g(r)
            .exec(
                &["commit", "-q", "--cleanup=verbatim", "-F", "-"],
                &[("GIT_AUTHOR_NAME", "Alice Example"), ("GIT_AUTHOR_EMAIL", "alice@example.com"), ("GIT_AUTHOR_DATE", "@1700000000 +0200")],
                Some(msg.as_bytes()),
            )
            .unwrap();
        assert!(o.ok(), "{}", said(&o));
        let out = do_sync(r);
        assert_eq!((out.status, out.replayed), ("rebased", 1), "{out:?}");
        assert_eq!(git(r, &["rev-parse", "HEAD^"]), u);
        assert_eq!(git(r, &["log", "-1", "--format=%an|%ae|%ad", "--date=raw"]), "Alice Example|alice@example.com|1700000000 +0200");
        assert_eq!(git(r, &["log", "-1", "--format=%B"]), msg);
        assert_eq!(read(r, "a.txt"), "a local\n");
        assert_eq!(read(r, "b.txt"), "b upstream\n");
        assert!(porcelain(r).is_empty());
    }

    #[test]
    fn sync_conflict_changes_nothing() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        upstream(&t, &[("a.txt", Some("upstream\n"))], "up");
        write(r, "a.txt", "local\n");
        assert!(commit(r, &["a.txt"], "local").ok);
        let before = head(r);
        let out = do_sync(r);
        assert_eq!(out.status, "conflict", "{out:?}");
        assert_eq!(out.files, ["a.txt"]);
        assert_eq!(head(r), before);
        assert_eq!(read(r, "a.txt"), "local\n");
        assert!(porcelain(r).is_empty());
        assert!(st(r).locks.sync.is_none());
    }

    #[test]
    fn sync_blocked_by_local_changes_claims_files_and_deletions() {
        let t = setup(&[("x.txt", "x\n"), ("y.txt", "y\n"), ("gone.txt", "g\n")]);
        let r = &t.repo;
        upstream(&t, &[("x.txt", Some("x up\n"))], "up 1");
        let before = head(r);
        // an unclaimed local change: read-tree refuses it
        write(r, "x.txt", "x local\n");
        let out = do_sync(r);
        assert_eq!(out.status, "blocked", "{out:?}");
        assert_eq!(out.files, ["x.txt"]);
        assert!(out.blocked_by.is_empty());
        assert_eq!((head(r), read(r, "x.txt")), (before.clone(), "x local\n".to_string()));
        // the same file claimed by a live agent: refused before anything runs
        claim(r, "x.txt", B, Some(other_alive()), 1, None);
        let out = do_sync(r);
        assert_eq!(out.status, "blocked");
        assert_eq!(out.blocked_by, [("x.txt".to_string(), B.to_string())]);
        assert!(out.message.contains("x.txt (bbbbbbbb)"), "{}", out.message);
        // a clean claim with a write in flight blocks too; a dead never-written one does not
        write(r, "x.txt", "x\n");
        claim(r, "x.txt", B, Some(other_alive()), 0, Some("toolu_1"));
        assert_eq!(do_sync(r).blocked_by.len(), 1);
        claim(r, "x.txt", B, Some(dead()), 0, None);
        assert_eq!(do_sync(r).status, "fast-forward");
        // a local deletion of a file upstream changes
        upstream(&t, &[("gone.txt", Some("g up\n"))], "up 2");
        std::fs::remove_file(r.abs("gone.txt")).unwrap();
        let out = do_sync(r);
        assert_eq!(out.status, "blocked", "{out:?}");
        assert_eq!(out.files, ["gone.txt"]);
        assert!(out.message.contains("local deletion"));
        assert!(!r.abs("gone.txt").exists());
        // upstream deleting the same file is fine
        upstream(&t, &[("gone.txt", None)], "up 3");
        assert_eq!(do_sync(r).status, "fast-forward");
        assert!(!r.abs("gone.txt").exists());
        assert!(porcelain(r).is_empty(), "{:?}", porcelain(r));
    }

    #[test]
    fn upstream_changes_outside_a_sparse_cone_are_no_local_deletion() {
        // gitcli/GIT-5: skip-worktree entries have no file on disk by design
        let t = setup(&[("in/i.txt", "i\n"), ("out/o.txt", "o\n")]);
        let r = &t.repo;
        git(r, &["sparse-checkout", "set", "in"]);
        assert!(!r.abs("out/o.txt").exists());
        let u = upstream(&t, &[("out/o.txt", Some("o up\n")), ("out/new.txt", Some("new\n")), ("in/i.txt", Some("i up\n"))], "up");
        let out = do_sync(r);
        assert_eq!(out.status, "fast-forward", "{out:?}");
        assert_eq!(head(r), u);
        assert_eq!(read(r, "in/i.txt"), "i up\n");
        assert!(!r.abs("out/o.txt").exists() && !r.abs("out/new.txt").exists(), "outside the cone stays out");
        assert!(porcelain(r).is_empty(), "{:?}", porcelain(r));
        // a real deletion inside the cone still blocks
        upstream(&t, &[("in/i.txt", Some("i up 2\n"))], "up 2");
        std::fs::remove_file(r.abs("in/i.txt")).unwrap();
        let out = do_sync(r);
        assert_eq!((out.status, out.files.as_slice()), ("blocked", &["in/i.txt".to_string()][..]), "{out:?}");
    }

    #[test]
    fn sync_blocked_by_untracked_or_ignored_file_in_the_way() {
        let t = setup(&[(".gitignore", "*.log\n")]);
        let r = &t.repo;
        upstream(&t, &[("new.txt", Some("upstream\n")), ("build.log", Some("tracked upstream\n"))], "up");
        write(r, "new.txt", "mine, untracked\n");
        let out = do_sync(r);
        assert_eq!(out.status, "blocked", "{out:?}");
        assert_eq!(out.files, ["new.txt"]);
        std::fs::remove_file(r.abs("new.txt")).unwrap();
        write(r, "build.log", "my ignored log\n");
        let out = do_sync(r);
        assert_eq!((out.status, out.files.clone()), ("blocked", s(&["build.log"])), "{out:?}");
        assert_eq!(read(r, "build.log"), "my ignored log\n");
    }

    #[test]
    fn raw_commit_racing_the_install_is_replayed_never_orphaned() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let u = upstream(&t, &[("up.txt", Some("u\n"))], "up");
        let rr = r.clone();
        let mut fired = false;
        on_stage(move |s| {
            if s == "locked" && !fired {
                fired = true;
                write(&rr, "raw.txt", "raw\n");
                git(&rr, &["add", "raw.txt"]);
                git(&rr, &["commit", "-q", "-m", "raw commit"]);
            }
        });
        let out = do_sync(r);
        no_stage();
        assert_eq!(out.status, "rebased", "{out:?}");
        assert_eq!(git(r, &["log", "-1", "--format=%s"]), "raw commit");
        assert_eq!(git(r, &["rev-parse", "HEAD^"]), u);
        assert!(porcelain(r).is_empty());
    }

    #[test]
    fn a_raw_git_write_fails_fast_while_sync_holds_index_lock() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let u = upstream(&t, &[("up.txt", Some("u\n"))], "up");
        let rr = r.clone();
        let refused = std::rc::Rc::new(std::cell::Cell::new(false));
        let seen = refused.clone();
        on_stage(move |s| {
            if s == "index-locked" {
                let o = try_git(&rr, &["commit", "-q", "--allow-empty", "-m", "racer"]);
                seen.set(!o.ok() && o.stderr.contains("index.lock"));
            }
        });
        let out = do_sync(r);
        no_stage();
        assert!(refused.get());
        assert_eq!((out.status, head(r)), ("fast-forward", u));
    }

    #[test]
    fn rewritten_upstream_is_refused_and_accept_rewrite_drops_the_removed_commit() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let u1 = head(r);
        let u2 = upstream(&t, &[("b.txt", Some("b\n"))], "will be dropped");
        assert_eq!(do_sync(r).status, "fast-forward");
        write(r, "l.txt", "local\n");
        assert!(commit(r, &["l.txt"], "local").ok);
        // the other machine force-pushes history without u2
        let up = at_dir(&t.base.join("up"));
        git(&up, &["reset", "-q", "--hard", &u1]);
        write(&up, "c.txt", "c\n");
        git(&up, &["add", "c.txt"]);
        git(&up, &["commit", "-q", "-m", "rewritten"]);
        git(&up, &["push", "-q", "-f", "origin", "main"]);
        let u3 = head(&up);
        let before = head(r);
        let out = do_sync(r);
        assert_eq!(out.status, "rewritten", "{out:?}");
        assert_eq!(out.dropped, std::slice::from_ref(&u2));
        assert_eq!(head(r), before);
        assert_eq!(do_sync(r).status, "rewritten", "still detected on the next sync");
        let out = sync_in(g(r), ME, true);
        assert_eq!((out.status, out.replayed), ("rebased", 1), "{out:?}");
        assert_eq!(git(r, &["rev-parse", "HEAD^"]), u3);
        assert!(!is_ancestor(g(r), &u2, &head(r)), "the removed commit is gone");
        assert_eq!(read(r, "l.txt"), "local\n");
        assert!(!r.abs("b.txt").exists());
    }

    #[test]
    fn rewrite_check_falls_back_to_the_old_tracking_ref() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let u1 = head(r);
        let u2 = upstream(&t, &[("b.txt", Some("b\n"))], "dropped");
        git(r, &["pull", "-q", "--ff-only", "origin", "main"]); // a human's plain pull: no upstream record
        assert!(st(r).upstream.is_none());
        let up = at_dir(&t.base.join("up"));
        git(&up, &["reset", "-q", "--hard", &u1]);
        git(&up, &["commit", "-q", "--allow-empty", "-m", "rewritten"]);
        git(&up, &["push", "-q", "-f", "origin", "main"]);
        let out = do_sync(r);
        assert_eq!(out.status, "rewritten", "{out:?}");
        assert_eq!(st(r).upstream.map(|u| u.oid), Some(u2), "the evidence is kept");
        // push must not re-publish the dropped commit either
        let p = push_in(g(r), ME, false);
        assert_eq!(p.status, "rewritten", "{p:?}");
    }

    #[test]
    fn already_applied_local_commit_is_skipped() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        write(r, "a.txt", "same change\n");
        assert!(commit(r, &["a.txt"], "local copy").ok);
        let u = upstream(&t, &[("a.txt", Some("same change\n"))], "upstream copy");
        let out = do_sync(r);
        assert_eq!((out.status, out.skipped, out.replayed), ("fast-forward", 1, 0), "{out:?}");
        assert_eq!(head(r), u);
    }

    #[test]
    fn merge_commits_no_upstream_detached_and_unborn() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        upstream(&t, &[("u.txt", Some("u\n"))], "up");
        git(r, &["checkout", "-q", "-b", "side"]);
        git(r, &["commit", "-q", "--allow-empty", "-m", "side"]);
        git(r, &["checkout", "-q", "main"]);
        git(r, &["commit", "-q", "--allow-empty", "-m", "main"]);
        git(r, &["merge", "-q", "--no-edit", "side"]);
        let before = head(r);
        let out = do_sync(r);
        assert_eq!(out.status, "error", "{out:?}");
        assert!(out.message.contains("merge commit"));
        assert_eq!(head(r), before);
        git(r, &["checkout", "-q", "-b", "feature"]);
        assert_eq!(do_sync(r).status, "no-upstream");
        git(r, &["checkout", "-q", "--detach"]);
        assert_eq!(do_sync(r).status, "error");
        assert_eq!(push_in(g(r), ME, false).status, "error");
        // an unborn HEAD fast-forwards and keeps local files
        let fresh = at_dir(&t.base.join("fresh"));
        std::fs::create_dir_all(&fresh.top).unwrap();
        git(&fresh, &["init", "-q", "-b", "main"]);
        git(&fresh, &["remote", "add", "origin", t.origin.to_str().unwrap()]);
        write(&fresh, "local.txt", "mine\n");
        let out = do_sync(&fresh);
        assert_eq!(out.status, "fast-forward", "{out:?}");
        assert_eq!(read(&fresh, "u.txt"), "u\n");
        assert_eq!(porcelain(&fresh), ["?? local.txt"]);
    }

    // ── push ──────────────────────────────────────────────────────────────

    #[test]
    fn push_fetch_first_or_stale_lease_syncs_and_retries() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let origin = at_dir(&t.origin);
        // no upstream record yet: a plain push, rejected (fetch first)
        let u = upstream(&t, &[("up.txt", Some("u\n"))], "up");
        write(r, "a.txt", "a2\n");
        assert!(commit(r, &["a.txt"], "local").ok);
        let p = push_in(g(r), ME, false);
        assert_eq!((p.status, p.attempts), ("pushed", 2), "{p:?}");
        assert_eq!(git(&origin, &["rev-parse", "main"]), head(r));
        assert_eq!(git(r, &["rev-parse", "HEAD^"]), u);
        assert_eq!(st(r).upstream.map(|x| x.oid), Some(head(r)));
        assert!(st(r).locks.git.is_none());
        assert_eq!(push_in(g(r), ME, false).status, "up-to-date");
        // with the record: the lease refuses (stale info), same recovery
        let u = upstream(&t, &[("up2.txt", Some("u\n"))], "up 2");
        write(r, "a.txt", "a3\n");
        assert!(commit(r, &["a.txt"], "local 2").ok);
        let p = push_in(g(r), ME, false);
        assert_eq!((p.status, p.attempts), ("pushed", 2), "{p:?}");
        assert_eq!(git(&origin, &["rev-parse", "main"]), head(r));
        assert_eq!(git(r, &["rev-parse", "HEAD^"]), u);
    }

    #[test]
    fn push_creates_a_missing_branch_and_reports_rejections_verbatim() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        git(r, &["checkout", "-q", "-b", "feature"]);
        write(r, "f.txt", "f\n");
        assert!(commit(r, &["f.txt"], "feature").ok);
        let p = push_in(g(r), ME, false);
        assert_eq!((p.status, p.attempts), ("pushed", 1), "{p:?}");
        assert_eq!(git(&at_dir(&t.origin), &["rev-parse", "feature"]), head(r));
        // a remote hook rejection is reported, not retried
        let hook = t.origin.join("hooks/pre-receive");
        std::fs::write(&hook, "#!/bin/sh\necho 'protected branch' >&2\nexit 1\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        write(r, "f.txt", "f2\n");
        assert!(commit(r, &["f.txt"], "feature 2").ok);
        let p = push_in(g(r), ME, false);
        assert_eq!((p.status, p.attempts), ("rejected", 1), "{p:?}");
        assert!(p.message.contains("pre-receive hook declined") && p.message.contains("protected branch"), "{}", p.message);
    }

    #[test]
    fn push_stops_on_conflict_and_on_a_rolled_back_upstream() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        upstream(&t, &[("a.txt", Some("theirs\n"))], "theirs");
        write(r, "a.txt", "mine\n");
        assert!(commit(r, &["a.txt"], "mine").ok);
        let p = push_in(g(r), ME, false);
        assert_eq!((p.status, p.attempts), ("conflict", 1), "{p:?}");
        assert_eq!(p.files, ["a.txt"]);
        // rolled back upstream: the lease refuses, sync says rewritten, nothing re-published
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let u1 = head(r);
        let u2 = upstream(&t, &[("b.txt", Some("b\n"))], "rolled back later");
        assert_eq!(do_sync(r).status, "fast-forward");
        git(&at_dir(&t.origin), &["update-ref", "refs/heads/main", &u1]);
        write(r, "l.txt", "l\n");
        assert!(commit(r, &["l.txt"], "local").ok);
        let p = push_in(g(r), ME, false);
        assert_eq!(p.status, "rewritten", "{p:?}");
        assert_eq!(git(&at_dir(&t.origin), &["rev-parse", "main"]), u1);
        assert!(is_ancestor(g(r), &u2, &head(r)));
    }

    #[test]
    fn porcelain_and_refusal_parsing() {
        let out = "To /x/origin.git\n!\tabc:refs/heads/main\t[rejected] (fetch first)\nDone\n";
        let (f, summary, _) = push_line(out, "refs/heads/main").unwrap();
        assert_eq!((f, summary.as_str()), ('!', "[rejected] (fetch first)"));
        assert!(push_line(out, "refs/heads/other").is_none());
        assert_eq!(push_line("=\tabc:refs/heads/main\t[up to date]\n", "refs/heads/main").unwrap().0, '=');
        let err = "error: Entry 'x.txt' not uptodate. Cannot merge.\nerror: Entry 'a b.txt' would be overwritten by merge. Cannot merge.\n";
        assert_eq!(not_uptodate(err), ["x.txt", "a b.txt"]);
        assert!(jitter(1) >= Duration::from_millis(50) && jitter(9) <= Duration::from_millis(3000));
    }

    // ── interrupted sync ──────────────────────────────────────────────────

    fn interrupted(stage: &'static str) -> (T, String, String) {
        let t = setup(&[("a.txt", "a\n"), ("m.txt", "m\n"), ("z.txt", "z\n"), ("old.txt", "old\n")]);
        let h = head(&t.repo);
        let u = upstream(&t, &[("a.txt", Some("a up\n")), ("m.txt", Some("m up\n")), ("z.txt", Some("z up\n")), ("n/new.txt", Some("new\n")), ("old.txt", None)], "up");
        let rr = t.repo.clone();
        killed_at(&t.repo, stage, move || {
            do_sync(&rr);
        });
        (t, h, u)
    }

    fn assert_at(r: &Repo, h: &str) {
        assert_eq!(head(r), h);
        assert!(porcelain(r).is_empty(), "{:?}", porcelain(r));
        assert!(leftovers(r).is_empty(), "{:?}", leftovers(r));
        assert!(st(r).locks.sync.is_none());
    }

    #[test]
    fn killed_after_read_tree_rolls_back_exactly() {
        let (t, h, u) = interrupted("after-read-tree");
        let r = &t.repo;
        assert_eq!(read(r, "a.txt"), "a up\n", "read-tree had written");
        assert!(r.git_dir.join("index.lock").exists());
        let m = recover(r).expect("recovered");
        assert!(m.contains("rolled back 5 file(s)") && m.contains("removed the index.lock"), "{m}");
        assert_eq!((read(r, "a.txt"), read(r, "old.txt")), ("a\n".into(), "old\n".into()));
        assert!(!r.abs("n").exists(), "an added file and its new directory are gone");
        assert_at(r, &h);
        assert!(recover(r).is_none(), "nothing left");
        assert_eq!(do_sync(r).status, "fast-forward");
        assert_at(r, &u);
    }

    #[test]
    fn killed_after_update_ref_or_between_renames_finishes_the_install() {
        for stage in ["after-update-ref", "between-renames"] {
            let (t, _h, u) = interrupted(stage);
            let r = &t.repo;
            let m = recover(r).expect("recovered");
            assert!(m.contains("finished its install"), "{stage}: {m}");
            assert_at(r, &u);
            assert_eq!(read(r, "m.txt"), "m up\n");
        }
    }

    #[test]
    fn killed_before_read_tree_undoes_nothing() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let h = head(r);
        upstream(&t, &[("a.txt", Some("a up\n"))], "up");
        let rr = r.clone();
        let r2 = r.clone();
        on_stage(move |s| {
            if s == "index-locked" {
                write(&r2, "a.txt", "a up\n"); // an unattributed edit that equals upstream's content
                panic!("simulated kill");
            }
        });
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| do_sync(&rr)));
        no_stage();
        assert!(res.is_err());
        let d = dead();
        state::transact(r, |st| st.locks.sync.as_mut().unwrap().owner = Some(d)).unwrap();
        let m = recover(r).expect("recovered");
        assert!(m.contains("nothing needed undoing") && m.contains("removed the index.lock"), "{m}");
        assert_eq!(read(r, "a.txt"), "a up\n", "not rolled back");
        assert_eq!(head(r), h);
        assert!(leftovers(r).is_empty(), "{:?}", leftovers(r));
    }

    #[test]
    fn an_orphaned_update_ref_keeps_recovery_busy_then_the_install_is_finished() {
        // gitcli/GIT-3: the sync was killed while its `update-ref HEAD tip H` still ran
        let (t, h, u) = interrupted("after-read-tree");
        let r = &t.repo;
        hook(r, "reference-transaction", "cat >/dev/null\n[ \"$1\" = prepared ] && sleep 2\nexit 0");
        let mut orphan = Command::new(git_path())
            .current_dir(&r.top)
            .envs(ENV.iter().copied())
            .args(["update-ref", "-m", "agent-lock sync", "HEAD", &u, &h])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let t0 = Instant::now();
        while !r.git_dir.join("HEAD.lock").exists() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        match recover_in(g(r)) {
            Rec::Busy(m) => assert!(m.contains("left git update-ref running"), "{m}"),
            _ => panic!("recovered under a running update-ref"),
        }
        assert_eq!(head(r), h);
        assert!(st(r).locks.sync.is_some(), "left for later");
        assert!(orphan.wait().unwrap().success());
        let m = recover(r).expect("recovered");
        assert!(m.contains("finished its install"), "{m}");
        assert_at(r, &u);
        assert_eq!(read(r, "a.txt"), "a up\n");
    }

    #[test]
    fn an_update_ref_landing_during_the_rollback_is_finished_not_undone() {
        // gitcli/GIT-3 backstop: HEAD re-read after the rollback
        let (t, h, u) = interrupted("after-read-tree");
        let r = &t.repo;
        let (rr, uu, hh) = (r.clone(), u.clone(), h.clone());
        on_stage(move |s| {
            if s == "rolled-back" {
                git(&rr, &["update-ref", "HEAD", &uu, &hh]);
            }
        });
        let m = recover(r).expect("recovered");
        no_stage();
        assert!(m.contains("finished its install") && m.contains("landed meanwhile"), "{m}");
        assert_at(r, &u);
        assert_eq!((read(r, "a.txt"), read(r, "n/new.txt")), ("a up\n".into(), "new\n".into()));
        assert!(!r.abs("old.txt").exists(), "upstream's deletion stands");
    }

    #[test]
    fn without_ps_a_fresh_lock_stands_for_a_running_orphan() {
        let t = setup(&[]);
        let r = &t.repo;
        let h = head(r);
        let p = r.git_dir.join("agent-lock-sync-x.index");
        let l = SyncLock { by: B.into(), owner: None, since: 0, head: h.clone(), tip: "f".repeat(40), paths: Vec::new(), index_tmp: Some(p.to_string_lossy().into()), lock_ino: None };
        assert_eq!(orphan_running(None, r, &l), None);
        let ps = format!("/usr/bin/git update-ref -m agent-lock sync: ffff HEAD {} {h}\n", l.tip);
        assert_eq!(orphan_running(Some(&ps), r, &l), Some("update-ref"));
        assert_eq!(orphan_running(Some("git status\n"), r, &l), None, "ps knows better than a stale lock");
        let old = std::time::SystemTime::now() - Duration::from_secs(GIT_ABANDON + 60);
        for (f, what) in [(r.git_dir.join("HEAD.lock"), "update-ref"), (r.git_dir.join("refs/heads/main.lock"), "update-ref"), (lock_of(&p), "read-tree")] {
            std::fs::write(&f, "").unwrap();
            assert_eq!(orphan_running(None, r, &l), Some(what), "{}", f.display());
            assert_eq!(orphan_running(Some(""), r, &l), None);
            std::fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
            assert_eq!(orphan_running(None, r, &l), None, "an abandoned lock blocks nothing: {}", f.display());
            std::fs::remove_file(&f).unwrap();
        }
    }

    /// A smudge filter that SIGKILLs read-tree while it checks out m.txt.
    fn killer_filter(t: &T) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let r = &t.repo;
        let arm = t.base.join("armed");
        let script = t.base.join("killer.sh");
        std::fs::write(&script, format!("#!/bin/sh\nif [ -f '{0}' ]; then rm -f '{0}'; kill -9 $PPID; sleep 1; fi\ncat\n", arm.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(r.git_dir.join("info")).unwrap();
        std::fs::write(r.git_dir.join("info/attributes"), "m.txt filter=killer\n").unwrap();
        git(r, &["config", "filter.killer.smudge", script.to_str().unwrap()]);
        git(r, &["config", "filter.killer.clean", "cat"]);
        std::fs::write(&arm, "").unwrap();
        arm
    }

    #[test]
    fn read_tree_killed_mid_checkout_is_rolled_back_in_process() {
        let t = setup(&[("a.txt", "a\n"), ("m.txt", "m\n"), ("z.txt", "z\n")]);
        let r = &t.repo;
        let h = head(r);
        let u = upstream(&t, &[("a.txt", Some("a up\n")), ("m.txt", Some("m up\n")), ("z.txt", Some("z up\n"))], "up");
        let arm = killer_filter(&t);
        let out = do_sync(r);
        assert_eq!(out.status, "busy", "{out:?}");
        assert!(out.message.contains("interrupted by a signal"), "{}", out.message);
        assert_eq!((read(r, "a.txt"), read(r, "m.txt"), read(r, "z.txt")), ("a\n".into(), "m\n".into(), "z\n".into()));
        assert_at(r, &h);
        assert!(!arm.exists(), "the filter fired");
        assert_eq!(do_sync(r).status, "fast-forward");
        assert_at(r, &u);
    }

    #[test]
    fn process_killed_with_its_read_tree_is_rolled_back_by_the_next_command() {
        let t = setup(&[("a.txt", "a\n"), ("m.txt", "m\n"), ("z.txt", "z\n")]);
        let r = &t.repo;
        let h = head(r);
        let u = upstream(&t, &[("a.txt", Some("a up\n")), ("m.txt", Some("m up\n")), ("z.txt", Some("z up\n"))], "up");
        let arm = killer_filter(&t);
        let rr = r.clone();
        killed_at(r, "read-tree-failed", move || {
            do_sync(&rr);
        });
        assert!(!arm.exists(), "the filter fired");
        assert_eq!(read(r, "a.txt"), "a up\n");
        // the next agent-lock git command recovers first, then does its job
        let out = do_sync(r);
        assert_eq!(out.status, "fast-forward", "{out:?}");
        let rec = out.recovery.expect("recovery note");
        assert!(rec.contains("rolled back") && rec.contains("pid"), "{rec}");
        assert_at(r, &u);
        let _ = h;
    }

    #[test]
    fn recover_leaves_live_syncs_and_foreign_locks_alone() {
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let h = head(r);
        let lock = |owner: Owner, ino: Option<u64>| SyncLock {
            by: B.into(),
            owner: Some(owner),
            since: sys::now(),
            head: h.clone(),
            tip: h.clone(),
            paths: s(&["a.txt"]),
            index_tmp: None,
            lock_ino: ino,
        };
        // a live sync: everyone waits
        state::transact(r, |st| st.locks.sync = Some(lock(other_alive(), None))).unwrap();
        assert!(recover(r).is_none());
        assert!(matches!(recover_in(g(r)), Rec::Busy(_)));
        write(r, "a.txt", "a2\n");
        assert_eq!(commit(r, &["a.txt"], "m").code, Some("busy"));
        assert_eq!(do_sync(r).status, "busy");
        assert_eq!(push_in(g(r), ME, false).status, "busy");
        assert!(st(r).locks.sync.is_some());
        // a dead one whose index.lock is now someone else's: only the record goes
        std::fs::write(r.git_dir.join("index.lock"), "DIRC foreign").unwrap();
        state::transact(r, |st| st.locks.sync = Some(lock(dead(), Some(1)))).unwrap();
        let m = recover(r).expect("recovered");
        assert!(!m.contains("removed the index.lock"), "{m}");
        assert!(r.git_dir.join("index.lock").exists(), "a foreign index.lock is never removed");
        assert!(st(r).locks.sync.is_none());
        std::fs::remove_file(r.git_dir.join("index.lock")).unwrap();
        // the same inode reused by another git's lock (other content): left alone
        std::fs::write(r.git_dir.join("index.lock"), "DIRC reused inode").unwrap();
        let reused = ino(&r.git_dir.join("index.lock"));
        state::transact(r, |st| st.locks.sync = Some(SyncLock { lock_ino: reused, ..lock(dead(), None) })).unwrap();
        assert!(recover(r).is_some());
        assert!(r.git_dir.join("index.lock").exists());
        std::fs::remove_file(r.git_dir.join("index.lock")).unwrap();
        // our lock marker survives a lost state file
        let marker = format!(r#"{{"agent-lock":"sync","by":"{B}","pid":{},"start":1,"head":"{h}","tip":"{h}","index_tmp":"/nonexistent"}}"#, dead().pid);
        std::fs::write(r.git_dir.join("index.lock"), marker).unwrap();
        let m = recover(r).expect("recovered from the marker");
        assert!(m.contains("removed the index.lock"), "{m}");
        assert!(!r.git_dir.join("index.lock").exists());
        assert!(commit(r, &["a.txt"], "m").ok);
    }

    /// `cargo test --release -- --ignored --nocapture git::tests::bench`
    #[test]
    #[ignore]
    fn bench() {
        fn median(mut v: Vec<Duration>) -> String {
            v.sort();
            format!("{:.2} ms", v[v.len() / 2].as_secs_f64() * 1000.0)
        }
        let t = setup(&[("a.txt", "a\n")]);
        let r = &t.repo;
        let (mut rec, mut com, mut up, mut ff, mut push) = (vec![], vec![], vec![], vec![], vec![]);
        for i in 0..15 {
            let t0 = Instant::now();
            assert!(recover(r).is_none());
            rec.push(t0.elapsed());
            write(r, "a.txt", &format!("a{i}\n"));
            let t0 = Instant::now();
            assert!(commit(r, &["a.txt"], "bench").ok);
            com.push(t0.elapsed());
            let t0 = Instant::now();
            assert_eq!(push_in(g(r), ME, false).status, "pushed");
            push.push(t0.elapsed());
            let t0 = Instant::now();
            assert_eq!(do_sync(r).status, "up-to-date");
            up.push(t0.elapsed());
            upstream(&t, &[(&format!("u{i}.txt"), Some("u\n"))], "up");
            let t0 = Instant::now();
            assert_eq!(do_sync(r).status, "fast-forward");
            ff.push(t0.elapsed());
        }
        eprintln!(
            "recover (nothing left over) {}, commit 1 file (no hooks) {}, push {}, sync up-to-date {}, sync fast-forward {}",
            median(rec), median(com), median(push), median(up), median(ff)
        );
    }

    #[test]
    fn status_of_splits_renames_and_reports_ignored() {
        let t = setup(&[("a.txt", "a\n"), (".gitignore", "*.log\n")]);
        let r = &t.repo;
        git(r, &["mv", "a.txt", "b.txt"]);
        write(r, "x.log", "x\n");
        let m = status_of(g(r), Some(&s(&["a.txt", "b.txt", "x.log"])), true).unwrap();
        assert_eq!(m["a.txt"].xy, "D.");
        assert_eq!(m["b.txt"].xy, "A.");
        assert!(m["x.log"].ignored);
        assert!(!status_of(g(r), None, false).unwrap().contains_key("x.log"));
    }

    #[test]
    fn dirty_takes_paths_as_literal_arguments() {
        let t = setup(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("*.txt", "star\n")]);
        let r = &t.repo;
        write(r, "a.txt", "changed\n");
        write(r, " lead.txt", "new\n");
        let paths = s(&["a.txt", "b.txt", "*.txt", " lead.txt", "gone.txt"]);
        let d = super::dirty(r, &paths).unwrap();
        assert_eq!(d.into_iter().collect::<Vec<_>>(), s(&[" lead.txt", "a.txt"]));
        let many: Vec<String> = (0..450).map(|i| format!("f{i}.txt")).chain(paths).collect();
        assert_eq!(super::dirty(r, &many).unwrap().len(), 2, "every chunk is asked");
        assert!(super::status(r, Some(&[])).unwrap().is_empty());
    }
}
