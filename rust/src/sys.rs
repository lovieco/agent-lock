//! Operating-system facts: time, process liveness, repo discovery, paths,
//! signals, shell text. No child processes here (start times come from one
//! system call: `proc_pidinfo` on macOS, `/proc/<pid>/stat` on Linux), except
//! `git_spelling` for a non-ASCII path on macOS.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

// ── processes ─────────────────────────────────────────────────────────────

/// A process identity that survives pid reuse: pid + its start time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub pid: u32,
    pub start: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Live {
    Alive,
    Dead,
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn getpid() -> i32;
    fn getuid() -> u32;
    fn signal(sig: i32, handler: usize) -> usize;
}

const ESRCH: i32 = 3;

pub fn pid() -> u32 {
    unsafe { getpid() as u32 }
}

pub fn uid() -> u32 {
    unsafe { getuid() }
}

fn pid_exists(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let r = unsafe { kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() != Some(ESRCH)
}

/// A process's start time and whether it has exited but is not reaped yet
/// (a zombie). None: no such process, or nothing could read it.
#[cfg(target_os = "macos")]
fn proc_info(pid: u32) -> Option<(u64, bool)> {
    bsd_start(pid).map(|s| (s, false)).or_else(|| kinfo(pid)) // proc_pidinfo reads neither zombies nor other users' processes
}

/// sysctl {CTL_KERN, KERN_PROC, KERN_PROC_PID}: a struct kinfo_proc, whose
/// extern_proc starts with p_starttime (struct timeval) and has p_stat at
/// byte 36 and p_pid at byte 40 (LP64). It covers any process.
#[cfg(target_os = "macos")]
fn kinfo(pid: u32) -> Option<(u64, bool)> {
    unsafe extern "C" {
        fn sysctl(name: *mut i32, namelen: u32, oldp: *mut std::ffi::c_void, oldlenp: *mut usize, newp: *mut std::ffi::c_void, newlen: usize) -> i32;
    }
    const SZOMB: u8 = 5;
    let mut mib = [1, 14, 1, i32::try_from(pid).ok()?];
    let mut buf = [0u8; 1024]; // sizeof(struct kinfo_proc) is 648
    let mut len = buf.len();
    if unsafe { sysctl(mib.as_mut_ptr(), 4, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) } != 0 || len < 44 {
        return None;
    }
    let (sec, usec, p) = (i64::from_ne_bytes(buf[0..8].try_into().ok()?), i32::from_ne_bytes(buf[8..12].try_into().ok()?), i32::from_ne_bytes(buf[40..44].try_into().ok()?));
    if p != mib[3] || sec < 0 || usec < 0 {
        return None;
    }
    Some((sec as u64 * 1_000_000 + usec as u64, buf[36] == SZOMB))
}

#[cfg(target_os = "macos")]
fn bsd_start(pid: u32) -> Option<u64> {
    // struct proc_bsdinfo from <sys/proc_info.h>; pbi_start_tvsec/usec at the end.
    #[repr(C)]
    struct ProcBsdInfo {
        head: [u32; 11],   // flags, status, xstatus, pid, ppid, uid, gid, ruid, rgid, svuid, svgid
        rfu_1: u32,
        comm: [u8; 16],
        name: [u8; 32],
        mid: [u32; 5],     // nfiles, pgid, pjobc, e_tdev, e_tpgid
        nice: i32,
        start_tvsec: u64,
        start_tvusec: u64,
    }
    unsafe extern "C" {
        fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut std::ffi::c_void, size: i32) -> i32;
    }
    const PROC_PIDTBSDINFO: i32 = 3;
    let mut info = std::mem::MaybeUninit::<ProcBsdInfo>::zeroed();
    let size = std::mem::size_of::<ProcBsdInfo>() as i32;
    let n = unsafe { proc_pidinfo(pid as i32, PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    if n != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    if info.head[3] != pid {
        return None;
    }
    Some(info.start_tvsec * 1_000_000 + info.start_tvusec)
}

#[cfg(target_os = "linux")]
fn proc_info(pid: u32) -> Option<(u64, bool)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 3 (state) and 22 (starttime); the command name (field 2) may contain spaces and ')'.
    let mut rest = stat[stat.rfind(')')? + 1..].split_whitespace();
    let state = rest.next()?;
    Some((rest.nth(18)?.parse().ok()?, matches!(state, "Z" | "X")))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn proc_info(_pid: u32) -> Option<(u64, bool)> {
    None
}

/// When the process started; None when it is gone, a zombie or unreadable.
pub fn start_time(pid: u32) -> Option<u64> {
    proc_info(pid).filter(|&(_, zombie)| !zombie).map(|(s, _)| s)
}

pub fn owner_of(pid: u32) -> Option<Owner> {
    if !pid_exists(pid) {
        return None;
    }
    Some(Owner { pid, start: start_time(pid)? })
}

/// The Claude Code process this hook or command belongs to (`$CLAUDE_PID`).
pub fn claude_owner() -> Option<Owner> {
    let pid: u32 = std::env::var("CLAUDE_PID").ok()?.trim().parse().ok()?;
    owner_of(pid)
}

/// This process, for script-owned claims and long-running locks.
pub fn self_owner() -> Option<Owner> {
    owner_of(pid())
}

/// Start 0 means "start unknown": then only the pid is checked.
pub fn liveness(o: &Owner) -> Live {
    if !pid_exists(o.pid) {
        return Live::Dead;
    }
    match proc_info(o.pid) {
        Some((_, true)) => Live::Dead, // exited, not reaped yet
        Some((s, _)) if o.start != 0 && s != o.start => Live::Dead, // pid reused
        _ => Live::Alive,
    }
}

// ── signals ───────────────────────────────────────────────────────────────

pub(crate) const SIG_IGN: usize = 1;

fn set_signal(sig: i32, handler: usize) -> usize {
    match unsafe { signal(sig, handler) } {
        usize::MAX => 0, // SIG_ERR: restore the default
        prev => prev,
    }
}

/// Ignore `sig` (children inherit it); returns the previous disposition.
pub(crate) fn signal_ignore(sig: i32) -> usize {
    set_signal(sig, SIG_IGN)
}

/// Call `f` on `sig`; returns the previous disposition.
pub(crate) fn signal_catch(sig: i32, f: extern "C" fn(i32)) -> usize {
    set_signal(sig, f as usize)
}

pub(crate) fn signal_restore(sig: i32, prev: usize) {
    set_signal(sig, prev);
}

/// kill(2): async-signal-safe, so a signal handler may forward with it.
pub(crate) fn signal_send(pid: i32, sig: i32) {
    unsafe { kill(pid, sig) };
}

// ── text ──────────────────────────────────────────────────────────────────

/// Quote for a POSIX shell (only when needed).
pub fn quote(s: &str) -> String {
    let safe = !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./_-".contains(&b));
    if safe { s.to_string() } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

/// `1 file`, `3 files`.
pub fn plural(n: usize, one: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") }
}

// ── repos ─────────────────────────────────────────────────────────────────

/// A git worktree: `top` holds the files, `git_dir` is its (per-worktree) git dir.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    pub top: PathBuf,
    pub git_dir: PathBuf,
}

impl Repo {
    pub fn state_file(&self) -> PathBuf {
        self.git_dir.join("agent-lock.json")
    }

    /// Path relative to `top`, '/'-separated; None when outside the worktree.
    pub fn rel(&self, abs: &Path) -> Option<String> {
        let r = abs.strip_prefix(&self.top).ok()?;
        let s = r.to_str()?.replace('\\', "/");
        if s.is_empty() { None } else { Some(s) }
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        self.top.join(rel)
    }

    /// `core.ignorecase` from the repo config (the common dir's for worktrees).
    pub fn ignore_case(&self) -> bool {
        let common = std::fs::read_to_string(self.git_dir.join("commondir"))
            .ok()
            .map(|c| self.git_dir.join(c.trim()))
            .unwrap_or_else(|| self.git_dir.clone());
        let cfg = std::fs::read_to_string(common.join("config")).unwrap_or_default();
        let mut in_core = false;
        for line in cfg.lines() {
            let l = line.trim();
            if l.starts_with('[') {
                in_core = l.to_ascii_lowercase().starts_with("[core");
            } else if in_core {
                let l = l.to_ascii_lowercase().replace(' ', "");
                if l.starts_with("ignorecase=") {
                    return matches!(&l["ignorecase=".len()..], "true" | "yes" | "on" | "1");
                }
            }
        }
        false
    }
}

fn read_gitfile(dot_git: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dot_git).ok()?;
    let line = text.lines().find(|l| l.starts_with("gitdir:"))?;
    let p = PathBuf::from(line["gitdir:".len()..].trim());
    let p = if p.is_absolute() { p } else { dot_git.parent()?.join(p) };
    std::fs::canonicalize(p).ok()
}

/// The worktree containing `dir` (an existing directory, canonical). None
/// outside a repo or inside a `.git` directory.
pub fn find_repo(dir: &Path) -> Option<Repo> {
    if dir.components().any(|c| c.as_os_str() == ".git") {
        return None;
    }
    let mut d = Some(dir);
    while let Some(cur) = d {
        let dot = cur.join(".git");
        if let Ok(meta) = std::fs::metadata(&dot) {
            let git_dir = if meta.is_dir() {
                dot
            } else {
                read_gitfile(&dot)?
            };
            if !git_dir.join("HEAD").exists() {
                return None;
            }
            return Some(Repo { top: cur.to_path_buf(), git_dir });
        }
        d = cur.parent();
    }
    None
}

/// Lexically normalize (resolve `.` and `..`) without touching the disk.
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonical form of a path that may not exist yet. An existing leaf is
/// resolved too: realpath gives its one on-disk spelling (on macOS whatever
/// case or Unicode form was typed), and a symlink leaf keeps its own name,
/// spelled as its directory entry. Of a missing path the deepest existing
/// ancestor is resolved (symlinks, /tmp → /private/tmp), the rest kept.
pub fn canonical(p: &Path, cwd: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let abs = normalize(&if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) });
    let Some(name) = abs.file_name().map(|n| n.to_os_string()) else { return abs };
    let mut parent = abs.parent().map(Path::to_path_buf).unwrap_or_default();
    match std::fs::symlink_metadata(&abs) {
        Ok(m) if !m.file_type().is_symlink() => {
            if let Ok(real) = std::fs::canonicalize(&abs) {
                return real;
            }
        }
        Ok(m) => {
            if let Ok(dir) = std::fs::canonicalize(&parent) {
                let same: Vec<std::ffi::OsString> = std::fs::read_dir(&dir)
                    .map(|d| d.flatten().filter(|e| e.metadata().is_ok_and(|x| x.dev() == m.dev() && x.ino() == m.ino())).map(|e| e.file_name()).collect())
                    .unwrap_or_default();
                let name = if same.contains(&name) { name } else { same.into_iter().next().unwrap_or(name) };
                return dir.join(name);
            }
        }
        Err(_) => {}
    }
    let mut tail: Vec<std::ffi::OsString> = vec![name];
    loop {
        if let Ok(real) = std::fs::canonicalize(&parent) {
            let mut out = real;
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        match (parent.file_name().map(|n| n.to_os_string()), parent.parent()) {
            (Some(n), Some(up)) => {
                tail.push(n);
                parent = up.to_path_buf();
            }
            _ => return abs,
        }
    }
}

/// Repo + repo-relative path (git's spelling) for a file a tool is about to
/// touch. A symlink to a file in a worktree means that file: an edit through
/// the link writes it, so two agents editing it by either name must meet on
/// one claim.
pub fn locate(file: &Path, cwd: &Path) -> Option<(Repo, String)> {
    let abs = canonical(file, cwd);
    if std::fs::symlink_metadata(&abs).is_ok_and(|m| m.file_type().is_symlink())
        && let Ok(target) = std::fs::canonicalize(&abs)
        && target.is_file()
        && let Some(found) = target.parent().and_then(find_repo).filter(|r| !target.starts_with(&r.git_dir))
        && let Some(rel) = found.rel(&target)
    {
        let rel = git_spelling(&found, &rel);
        return Some((found, rel));
    }
    let repo = find_repo(abs.parent()?)?;
    if abs.starts_with(&repo.git_dir) {
        return None;
    }
    let rel = git_spelling(&repo, &repo.rel(&abs)?);
    Some((repo, rel))
}

/// git's spelling of a repo-relative path, so claim keys match what git
/// prints. On macOS git precomposes Unicode (NFC) while the disk may hold NFD
/// names (Finder, Cocoa, unzip), so a non-ASCII path is asked of git: its one
/// index or untracked entry, else the deepest existing directory as git spells
/// it plus the rest as typed (ask again once a new file exists). ASCII paths
/// and other systems: unchanged, no process.
pub fn git_spelling(repo: &Repo, rel: &str) -> String {
    if !cfg!(target_os = "macos") || rel.is_ascii() {
        return rel.to_string();
    }
    let ask = |dir: &Path, args: &[&str]| crate::git::run_in(dir, args, &[], None).ok().filter(|o| o.ok()).map(|o| o.stdout);
    let spec = format!("{}{rel}", if repo.ignore_case() { ":(literal,icase)" } else { ":(literal)" });
    if let Some(out) = ask(&repo.top, &["ls-files", "-z", "--cached", "--others", "--", &spec]) {
        let hits: std::collections::BTreeSet<&[u8]> = out.split(|b| *b == 0).filter(|s| !s.is_empty()).collect();
        if hits.len() == 1
            && let Some(Ok(one)) = hits.first().map(|h| std::str::from_utf8(h))
        {
            return one.to_string();
        }
    }
    let parts: Vec<&str> = rel.split('/').collect();
    let mut n = parts.len() - 1;
    while n > 0 && !repo.top.join(parts[..n].join("/")).is_dir() {
        n -= 1;
    }
    let dir = parts[..n].join("/");
    if dir.is_ascii() {
        return rel.to_string();
    }
    match ask(&repo.top.join(&dir), &["rev-parse", "--show-prefix"]).and_then(|o| String::from_utf8(o).ok()) {
        Some(pre) if pre.ends_with("/\n") => format!("{}{}", pre.trim_end_matches('\n'), parts[n..].join("/")),
        _ => rel.to_string(),
    }
}

pub fn cwd() -> PathBuf {
    std::env::current_dir().ok().and_then(|d| std::fs::canonicalize(d).ok()).unwrap_or_else(|| PathBuf::from("."))
}

/// File stamp for detecting writes: (mtime in ns, size). None when missing.
pub fn stamp(p: &Path) -> Option<(u128, u64)> {
    let m = std::fs::symlink_metadata(p).ok()?;
    let t = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    Some((t, m.len()))
}

// ── executables ───────────────────────────────────────────────────────────

pub fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// First `name` on PATH that is not agent-lock itself (skips the shims dir).
/// Any agent-lock binary counts, not only this one: shims linked to another
/// copy (a plugin's and a cargo-installed one) must never exec each other.
pub fn real_tool(name: &str) -> Option<PathBuf> {
    let me = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok());
    let ours = |p: &Path| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n == "agent-lock" || n.starts_with("agent-lock-"));
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if !is_executable(&cand) {
            continue;
        }
        let real = std::fs::canonicalize(&cand).ok();
        if real.is_some() && (real == me || real.as_deref().is_some_and(ours)) {
            continue;
        }
        return Some(cand);
    }
    None
}

/// macOS: /usr/bin/git is an xcrun trampoline that adds ~5 ms to every call;
/// the developer directory's git (the one xcrun would pick) is the same
/// program without it. Anything else is returned unchanged.
pub fn untrampoline(git: PathBuf) -> PathBuf {
    if !cfg!(target_os = "macos") || git != Path::new("/usr/bin/git") {
        return git;
    }
    let dirs = std::env::var_os("DEVELOPER_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .chain(std::fs::read_link("/var/db/xcode_select_link").ok())
        .chain(["/Library/Developer/CommandLineTools", "/Applications/Xcode.app/Contents/Developer"].map(PathBuf::from));
    dirs.map(|d| d.join("usr/bin/git")).find(|g| is_executable(g)).unwrap_or(git)
}

/// The real git, never a shim ($AGENT_LOCK_GIT overrides). Fallbacks cover a
/// PATH that only has shims. Resolved once per process.
pub fn git_bin() -> PathBuf {
    static GIT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    GIT.get_or_init(|| {
        if let Some(p) = std::env::var_os("AGENT_LOCK_GIT").filter(|p| !p.is_empty()) {
            return PathBuf::from(p);
        }
        untrampoline(
            real_tool("git")
                .or_else(|| ["/usr/bin/git", "/opt/homebrew/bin/git", "/usr/local/bin/git"].iter().map(PathBuf::from).find(|p| is_executable(p)))
                .unwrap_or_else(|| PathBuf::from("git")),
        )
    })
    .clone()
}

/// Where hooks keep machine-level files (session index, shim links).
pub fn data_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_PLUGIN_DATA") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => std::env::temp_dir().join(format!("agent-lock-{}", uid())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive_and_reuse_is_dead() {
        let me = self_owner().expect("own start time");
        assert_eq!(liveness(&me), Live::Alive);
        assert_eq!(liveness(&Owner { pid: me.pid, start: me.start + 1 }), Live::Dead);
        let gone = std::process::Command::new("true").spawn().unwrap();
        let pid = gone.id();
        let mut gone = gone;
        gone.wait().unwrap();
        assert_eq!(liveness(&Owner { pid, start: 1 }), Live::Dead);
    }

    #[test]
    fn canonical_keeps_missing_tail_and_leaf() {
        let t = std::env::temp_dir();
        let c = canonical(Path::new("no/such/dir/../file.txt"), &t);
        assert!(c.ends_with("no/such/file.txt"));
        assert!(c.starts_with(std::fs::canonicalize(&t).unwrap()));
    }

    /// Case-insensitive volume (the macOS default)?
    fn folds(dir: &Path) -> bool {
        std::fs::write(dir.join("probe"), "").unwrap();
        std::fs::symlink_metadata(dir.join("PROBE")).is_ok()
    }

    #[test]
    fn canonical_spells_an_existing_leaf_as_the_disk_does() {
        // state/F1, gitcli/GIT-2: the leaf used to be kept as typed
        let r = crate::git::helper_tests::repo(&[("Dd/Sub/f.txt", "x\n"), ("t.txt", "t\n")]);
        std::os::unix::fs::symlink("t.txt", r.abs("Lnk")).unwrap();
        assert_eq!(canonical(Path::new("Dd/Sub/f.txt"), &r.top), r.abs("Dd/Sub/f.txt"));
        assert_eq!(canonical(Path::new("Lnk"), &r.top), r.abs("Lnk"), "a symlink leaf is not resolved");
        if folds(&r.top) {
            assert_eq!(canonical(Path::new("dd/SUB/F.TXT"), &r.top), r.abs("Dd/Sub/f.txt"));
            assert_eq!(canonical(Path::new("lnk"), &r.top), r.abs("Lnk"));
            assert_eq!(canonical(Path::new("dd/new.txt"), &r.top), r.abs("Dd/new.txt"), "a new leaf keeps its name");
            assert_eq!(locate(&r.abs("DD/sub/F.txt"), &r.top).unwrap().1, "Dd/Sub/f.txt");
        }
    }

    #[test]
    fn locate_keys_every_unicode_spelling_by_gits() {
        // state/F1, gitcli/GIT-1: NFD on disk (Finder, unzip), NFC in git: one key, git's
        if !cfg!(target_os = "macos") {
            return;
        }
        use crate::git::helper_tests::{NFC, NFD};
        let ddir = "Cafe\u{301}-docs";
        let cdir = "Caf\u{e9}-docs";
        let r = crate::git::helper_tests::repo(&[(NFD, "x\n"), (&format!("{ddir}/notes.md"), "n\n")]);
        for typed in [NFC, NFD] {
            assert_eq!(locate(&r.abs(typed), &r.top).unwrap().1, NFC);
        }
        for d in [cdir, ddir] {
            assert_eq!(locate(&r.abs(&format!("{d}/notes.md")), &r.top).unwrap().1, format!("{cdir}/notes.md"));
            // a new file: git's spelling of its directory, the name as typed
            assert_eq!(locate(&r.abs(&format!("{d}/new.md")), &r.top).unwrap().1, format!("{cdir}/new.md"));
            assert_eq!(locate(&r.abs(&format!("{d}/sub/{NFD}")), &r.top).unwrap().1, format!("{cdir}/sub/{NFD}"));
        }
        std::fs::write(r.abs(&format!("{ddir}/{NFD}")), "new\n").unwrap();
        assert_eq!(git_spelling(&r, &format!("{ddir}/{NFD}")), format!("{cdir}/{NFC}"), "asked again once it exists");
        assert_eq!(git_spelling(&r, "plain/ascii.txt"), "plain/ascii.txt");
    }

    #[test]
    fn a_zombie_is_dead_and_another_users_process_is_judged_by_its_start() {
        let mut child = std::process::Command::new("cat").stdin(std::process::Stdio::piped()).spawn().unwrap();
        let o = owner_of(child.id()).expect("a running child");
        assert_eq!(liveness(&o), Live::Alive);
        drop(child.stdin.take()); // cat exits; nobody reaps it yet
        let t = std::time::Instant::now();
        while proc_info(o.pid).is_some_and(|(_, zombie)| !zombie) && t.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(liveness(&o), Live::Dead, "an exited, unreaped process");
        assert_eq!(owner_of(o.pid), None);
        child.wait().unwrap();
        #[cfg(target_os = "macos")]
        {
            // the kernel's process table: the same start time proc_pidinfo gives,
            // and it covers processes proc_pidinfo may not read (pid 1 is root's)
            assert_eq!(kinfo(pid()), Some((bsd_start(pid()).unwrap(), false)));
            let (start, zombie) = kinfo(1).expect("launchd");
            assert!(!zombie);
            assert_eq!(liveness(&Owner { pid: 1, start }), Live::Alive);
            assert_eq!(liveness(&Owner { pid: 1, start: start + 1 }), Live::Dead, "pid reused by another user");
        }
    }

    #[test]
    fn unknown_start_means_pid_only() {
        let me = self_owner().unwrap();
        assert_eq!(liveness(&Owner { pid: me.pid, start: 0 }), Live::Alive);
    }

    #[test]
    fn text_helpers() {
        assert_eq!(quote("a/b.txt"), "a/b.txt");
        assert_eq!(quote("it's here"), r"'it'\''s here'");
        assert_eq!(quote(""), "''");
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(2, "file"), "2 files");
    }

    #[test]
    fn normalize_lexically() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    }

    #[test]
    fn only_the_macos_git_trampoline_is_replaced() {
        assert_eq!(untrampoline(PathBuf::from("/opt/homebrew/bin/git")), PathBuf::from("/opt/homebrew/bin/git"));
        let g = untrampoline(PathBuf::from("/usr/bin/git"));
        if cfg!(target_os = "macos") && g != Path::new("/usr/bin/git") {
            assert!(g.ends_with("usr/bin/git") && is_executable(&g), "{}", g.display());
            let v = std::process::Command::new(&g).arg("--version").output().unwrap();
            assert!(String::from_utf8_lossy(&v.stdout).starts_with("git version"));
        } else {
            assert_eq!(g, PathBuf::from("/usr/bin/git"));
        }
    }
}
