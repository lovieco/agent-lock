//! git and package-manager shims. SessionStart links git and the package
//! managers to this binary in a folder first on the agents' PATH; argv[0]
//! picks the shim. A shim sees the real arguments, after the shell, xargs,
//! find -exec or a git alias produced them, so nothing has to be guessed from
//! command text.
//!
//! git: safe subcommands exec the real git at once, without reading the state.
//! The rest read the state (shared lock) and are refused only while another
//! agent id holds a live claim: one stderr line, exit 1, like a failing git.
//! git puts its exec-path first on PATH for everything it runs, so `!` aliases
//! and hooks never reach the shim: a `!` alias's git calls are judged from its
//! text before it runs; hooks stay unguarded (as in v2).
//! Identity: a subagent's Bash inherits its parent's AGENT_LOCK_AGENT_ID, so
//! a main id stands for its whole family. While a subagent may be running,
//! the main agent's own files count as another agent's for whole-tree git,
//! and a package install never blocks on a file its own family holds.
//! Package managers: dependency changes take `locks.pkg` and claim the
//! manifests and lockfiles they rewrite; installs and syncs take only the
//! lock; anything else runs straight through. Once the tool starts, the lock
//! and the claims' markers belong to its process, not the shim's.
//! Fail open: any error runs the real tool unchanged.
//!
//! `AGENT_LOCK_PKG_WAIT` (seconds, default 1200) bounds the wait for `locks.pkg`.

use crate::git;
use crate::ids;
use crate::lockfiles;
use crate::msg::{self, as_flag};
use crate::rules::{self, Ask, Conflict, Ctx, Got, Held, Marker};
use crate::state::{self, Claim, OpLock, Orphan, State};
use crate::sys::{self, Live, Owner, Repo};
use std::ffi::{OsStr, OsString};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

pub const NAMES: &[&str] = &["git", "npm", "pnpm", "yarn", "bun", "cargo", "uv", "poetry", "go", "bundle", "bundler"];

pub fn handles(name: &str) -> bool {
    NAMES.contains(&name)
}

enum Plan {
    Exec,
    Deny(String),
    Pkg(Pkg),
}

/// Set on everything a shim runs: "<pid>:<hops>" for the tool it execs (the
/// same process), "^<pid>:1" for the one it spawns (its child).
const HOP: &str = "AGENT_LOCK_SHIM";

/// Shim hops so far. The mark counts only in that very process: then the
/// "real" tool was itself a shim (a copy of agent-lock installed under the
/// tool's name), and this same command was judged already. Whatever the tool
/// runs in turn is another process and starts again at 0.
fn hops() -> u32 {
    let Ok(v) = std::env::var(HOP) else { return 0 };
    let (who, mark) = match v.strip_prefix('^') {
        Some(m) => (std::os::unix::process::parent_id(), m),
        None => (sys::pid(), v.as_str()),
    };
    match mark.split_once(':') {
        Some((pid, n)) if pid.parse() == Ok(who) => n.parse().unwrap_or(0),
        _ => 0,
    }
}

fn agent_lock_named(p: &Path) -> bool {
    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n == "agent-lock" || n.starts_with("agent-lock-"))
}

/// The first `name` on PATH after this binary's own entry, skipping agent-lock
/// links: shim copies reached from each other walk PATH forward, so they never
/// bounce between each other.
fn after_me(name: &str) -> Option<PathBuf> {
    let me = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok());
    let dirs: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH")?).collect();
    let canon = |c: &Path| std::fs::canonicalize(c).ok();
    let from = dirs.iter().position(|d| me.is_some() && canon(&d.join(name)) == me).map_or(0, |i| i + 1);
    dirs[from..].iter().map(|d| d.join(name)).find(|c| {
        let r = canon(c);
        sys::is_executable(c) && r != me && !r.as_deref().is_some_and(agent_lock_named)
    })
}

/// The tool the shim runs: the next one on PATH (git without the macOS
/// xcrun trampoline, so a shimmed git is no slower than a direct one).
fn real(name: &str, hops: u32) -> Option<PathBuf> {
    let t = if hops > 0 { after_me(name)? } else { sys::real_tool(name)? };
    Some(if name == "git" { sys::untrampoline(t) } else { t })
}

fn not_found(name: &str) -> i32 {
    eprintln!("agent-lock: {name}: command not found (no real {name} on PATH after the agent-lock shims)");
    127
}

pub fn run(name: &str, args: &[String]) -> i32 {
    run_as(name, args, None, args)
}

/// Arguments that are not all UTF-8 (a Latin-1 commit message, a byte-named
/// path): judge a lossy copy and, for git, also a copy where each such
/// argument is `.`, as if it named the whole tree; refuse when either is
/// refused. The tool always gets the raw arguments.
pub fn run_os(name: &str, raw: &[OsString]) -> i32 {
    let lossy: Vec<String> = raw.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let whole: Vec<String> = raw.iter().zip(&lossy).map(|(r, l)| if r.to_str().is_some() { l.clone() } else { ".".to_string() }).collect();
    run_as(name, &lossy, Some(&whole), raw)
}

fn run_as<S: AsRef<OsStr>>(name: &str, args: &[String], alt: Option<&[String]>, raw: &[S]) -> i32 {
    let hops = hops();
    let Some(real) = real(name, hops) else { return not_found(name) };
    if hops > 0 {
        return exec(&real, raw, hops); // the shim that ran this one judged it
    }
    std::panic::set_hook(Box::new(|_| {})); // a bug here must never print over the tool's output
    let plan_of = |a: &[String]| std::panic::catch_unwind(|| if name == "git" { git_plan(a) } else { pkg_plan(name, a) }).unwrap_or(Plan::Exec);
    let mut plan = plan_of(args);
    if let (Plan::Exec, Some(alt), "git") = (&plan, alt, name) {
        plan = plan_of(alt);
    }
    match plan {
        Plan::Exec => exec(&real, raw, hops),
        Plan::Deny(why) => {
            eprintln!("{why}");
            1
        }
        Plan::Pkg(p) => p.run(&real, raw),
    }
}

fn exec<S: AsRef<OsStr>>(real: &Path, args: &[S], hops: u32) -> i32 {
    let err = Command::new(real).args(args).env(HOP, format!("{}:{}", sys::pid(), hops + 1)).exec();
    eprintln!("agent-lock: cannot run {}: {err}", real.display());
    if err.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 }
}

fn disabled() -> bool {
    std::env::var("CLAUDE_FILE_LOCK").is_ok_and(|v| v.trim() == "0")
}

fn env_on(k: &str) -> bool {
    std::env::var(k).is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

fn cut(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// `a, b, c and 3 more`
fn names(items: &[String]) -> String {
    let mut out: Vec<String> = items.iter().take(3).map(|s| one_line(s)).collect();
    if items.len() > 3 {
        let last = out.pop().unwrap_or_default();
        out.push(format!("{last} and {} more", items.len() - 3));
    }
    out.join(", ")
}

/// `a.txt by bbbbbbbb, b.txt by cccccccc and 3 more`
fn list(claims: &[&Claim]) -> String {
    names(&claims.iter().map(|c| format!("{} by {}", c.path, c.by)).collect::<Vec<_>>())
}

// ── git: the command line ─────────────────────────────────────────────────

/// git's own commands (`git --list-cmds=builtins,main`): git never expands an
/// alias that shadows one, so only other names are looked up.
const BUILTINS: &[&str] = &[
    "add", "am", "annotate", "apply", "archive", "backfill", "bisect", "blame", "branch", "bugreport", "bundle",
    "cat-file", "check-attr", "check-ignore", "check-mailmap", "check-ref-format", "checkout", "checkout-index",
    "cherry", "cherry-pick", "citool", "clean", "clone", "column", "commit", "commit-graph", "commit-tree", "config",
    "count-objects", "credential", "credential-cache", "credential-store", "daemon", "describe", "diagnose", "diff",
    "diff-files", "diff-index", "diff-pairs", "diff-tree", "difftool", "fast-export", "fast-import", "fetch",
    "fetch-pack", "filter-branch", "fmt-merge-msg", "for-each-ref", "for-each-repo", "format-patch", "fsck",
    "fsck-objects", "gc", "get-tar-commit-id", "grep", "gui", "hash-object", "help", "history", "hook",
    "http-backend", "http-fetch", "http-push", "imap-send", "index-pack", "init", "init-db", "instaweb",
    "interpret-trailers", "last-modified", "log", "ls-files", "ls-remote", "ls-tree", "mailinfo", "mailsplit",
    "maintenance", "merge", "merge-base", "merge-file", "merge-index", "merge-octopus", "merge-one-file",
    "merge-ours", "merge-recursive", "merge-recursive-ours", "merge-recursive-theirs", "merge-resolve",
    "merge-subtree", "merge-tree", "mergetool", "mktag", "mktree", "multi-pack-index", "mv", "name-rev", "notes",
    "p4", "pack-objects", "pack-redundant", "pack-refs", "patch-id", "pickaxe", "prune", "prune-packed", "pull",
    "push", "quiltimport", "range-diff", "read-tree", "rebase", "receive-pack", "reflog", "refs", "remote",
    "remote-ext", "remote-fd", "repack", "replace", "replay", "repo", "request-pull", "rerere", "reset", "restore",
    "rev-list", "rev-parse", "revert", "rm", "scalar", "send-email", "send-pack", "shell", "shortlog", "show",
    "show-branch", "show-index", "show-ref", "sparse-checkout", "stage", "stash", "status", "stripspace",
    "submodule", "subtree", "svn", "switch", "symbolic-ref", "tag", "unpack-file", "unpack-objects",
    "update-index", "update-ref", "update-server-info", "upload-archive", "upload-pack", "var", "verify-commit",
    "verify-pack", "verify-tag", "version", "whatchanged", "worktree", "write-tree",
];

/// Builtins that can sweep other agents' uncommitted work; every other builtin is safe.
const CHECKED: &[&str] = &[
    "add", "stage", "commit", "stash", "reset", "rm", "mv", "checkout", "restore", "clean", "switch", "pull", "rebase",
    "merge", "cherry-pick", "revert", "am", "filter-branch", "checkout-index", "bisect", "sparse-checkout", "read-tree",
];

/// Pathspec-wide options (`--literal-pathspecs`, GIT_GLOB_PATHSPECS, …).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Magic {
    literal: bool,
    glob: bool,
    noglob: bool,
    icase: bool,
}

/// `git [global options] <sub> <args>`. Paths are relative to the process cwd.
#[derive(Debug, Default, PartialEq)]
struct Call {
    dir: PathBuf,
    git_dir: Option<PathBuf>,
    work_tree: Option<PathBuf>,
    config: Vec<(String, String)>,
    magic: Magic,
    sub: Option<String>,
    args: Vec<String>,
}

fn take(args: &[String], i: &mut usize, inline: Option<&str>) -> Option<String> {
    if let Some(v) = inline {
        return Some(v.to_string());
    }
    *i += 1;
    args.get(*i).cloned()
}

/// None: an option git rejects or one that only prints (let git answer).
fn parse_call(args: &[String]) -> Option<Call> {
    let mut c = Call::default();
    let mut i = 0;
    while i < args.len() {
        let t = args[i].as_str();
        let (name, inline) = match t.split_once('=') {
            Some((n, v)) if t.starts_with("--") => (n, Some(v)),
            _ => (t, None),
        };
        match name {
            "-C" => {
                let d = take(args, &mut i, None)?;
                if !d.is_empty() {
                    c.dir = c.dir.join(d);
                }
            }
            "-c" => {
                let kv = take(args, &mut i, None)?;
                let (k, v) = kv.split_once('=').unwrap_or((&kv, "true"));
                c.config.push((k.to_string(), v.to_string()));
            }
            "--config-env" => {
                let kv = take(args, &mut i, inline)?;
                if let Some((k, Ok(v))) = kv.rsplit_once('=').map(|(k, var)| (k, std::env::var(var))) {
                    c.config.push((k.to_string(), v));
                }
            }
            "--git-dir" => c.git_dir = Some(c.dir.join(take(args, &mut i, inline)?)),
            "--work-tree" => c.work_tree = Some(c.dir.join(take(args, &mut i, inline)?)),
            "--namespace" | "--super-prefix" | "--attr-source" => {
                take(args, &mut i, inline)?;
            }
            "--exec-path" if inline.is_none() => return Some(Call { sub: None, ..c }), // prints
            "--version" | "-v" | "--help" | "-h" | "--html-path" | "--man-path" | "--info-path" => {
                return Some(Call { sub: None, ..c });
            }
            "--exec-path" | "--list-cmds" => {}
            "--literal-pathspecs" => c.magic.literal = true,
            "--glob-pathspecs" => c.magic.glob = true,
            "--noglob-pathspecs" => c.magic.noglob = true,
            "--icase-pathspecs" => c.magic.icase = true,
            "-p" | "--paginate" | "-P" | "--no-pager" | "--bare" | "--no-replace-objects" | "--no-lazy-fetch"
            | "--no-optional-locks" | "--no-advice" => {}
            _ if t.starts_with('-') => return None,
            _ => {
                c.sub = Some(t.to_string());
                c.args = args[i + 1..].to_vec();
                return Some(c);
            }
        }
        i += 1;
    }
    Some(c)
}

/// Where a git call acts: its worktree, canonical cwd, and the cwd's path in it.
struct Place {
    repo: Repo,
    base: PathBuf, // where git started; Call paths are relative to it
    cwd: PathBuf,
    prefix: String,
}

/// `base`: the directory git starts in (the shell's cwd; the top for a `!` alias).
fn place(c: &Call, base: &Path) -> Option<Place> {
    let cwd = std::fs::canonicalize(base.join(&c.dir)).ok()?;
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(|v| cwd.join(v));
    let work_tree = c.work_tree.as_ref().map(|w| base.join(w)).or_else(|| env("GIT_WORK_TREE"));
    let git_dir = c.git_dir.as_ref().map(|g| base.join(g)).or_else(|| env("GIT_DIR"));
    let loc = match (work_tree, git_dir) {
        (Some(w), _) => w,
        (None, Some(g)) => g.parent()?.to_path_buf(),
        _ => cwd.clone(),
    };
    let repo = sys::find_repo(&std::fs::canonicalize(loc).ok()?)?;
    let prefix = repo.rel(&cwd).unwrap_or_default(); // "" at the top or outside it
    Some(Place { repo, base: base.to_path_buf(), cwd, prefix })
}

/// git's split_cmdline for alias values: whitespace, '…', "…", backslash.
fn split_cmdline(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut word = false;
    let mut quote: Option<char> = None;
    let mut it = s.chars();
    while let Some(ch) = it.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => cur.push(c),
            (_, '\\') => cur.push(it.next()?),
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(ch);
                word = true;
            }
            (None, c) if c.is_whitespace() => {
                if word || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                word = false;
            }
            (None, c) => cur.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if word || !cur.is_empty() {
        out.push(cur);
    }
    Some(out)
}

enum Expansion {
    Git(Call, String),                          // the builtin call, the first alias text
    Shell(String, Vec<String>, Vec<(String, String)>), // a `!` alias: text, args, config
}

/// Follow `alias.<sub>` until a builtin or a `!` alias. None: not an alias or
/// unresolvable — git runs as asked.
fn expand(c: &Call, p: &Place) -> Option<Expansion> {
    let mut sub = c.sub.clone()?;
    let mut args = c.args.clone();
    let mut config = c.config.clone();
    let mut magic = c.magic;
    let mut text = String::new();
    for _ in 0..8 {
        if BUILTINS.contains(&sub.as_str()) {
            let call = Call { dir: c.dir.clone(), git_dir: c.git_dir.clone(), work_tree: c.work_tree.clone(), config, magic, sub: Some(sub), args };
            return Some(Expansion::Git(call, text));
        }
        let key = format!("alias.{sub}");
        let found = match config.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(&key)) {
            Some((_, v)) => v.clone(),
            None => {
                let mut a: Vec<String> = Vec::new();
                if let Some(g) = &c.git_dir {
                    a.extend(["--git-dir".into(), p.base.join(g).display().to_string()]);
                }
                a.extend(["config".into(), "--get".into(), key.to_ascii_lowercase()]);
                let a: Vec<&str> = a.iter().map(String::as_str).collect();
                let o = git::run_in(&p.cwd, &a, &[], None).ok()?;
                if !o.ok() {
                    return None;
                }
                o.text().trim_end_matches('\n').to_string()
            }
        };
        let found = found.trim();
        if found.is_empty() {
            return None;
        }
        if let Some(sh) = found.strip_prefix('!') {
            return Some(Expansion::Shell(sh.to_string(), args, config));
        }
        if text.is_empty() {
            text = found.to_string();
        }
        let words = split_cmdline(found)?;
        let mut k = 0;
        while k < words.len() {
            match words[k].as_str() {
                "-c" if k + 1 < words.len() => {
                    let kv = &words[k + 1];
                    let (a, b) = kv.split_once('=').unwrap_or((kv, "true"));
                    config.push((a.to_string(), b.to_string()));
                    k += 1;
                }
                "--literal-pathspecs" => magic.literal = true,
                "--glob-pathspecs" => magic.glob = true,
                "--noglob-pathspecs" => magic.noglob = true,
                "--icase-pathspecs" => magic.icase = true,
                "-p" | "-P" | "--paginate" | "--no-pager" | "--no-replace-objects" | "--no-optional-locks"
                | "--no-advice" | "--bare" => {}
                _ => break,
            }
            k += 1;
        }
        let next = words.get(k)?.clone();
        args = words[k + 1..].iter().cloned().chain(args).collect();
        sub = next;
    }
    None // alias loop: git reports it
}

/// The simple commands of a `!` alias. git puts its exec-path first on that
/// shell's PATH, so its git calls never reach the shim: they are judged here.
/// `;`, `&`, `|`, newlines, braces and parens split; quotes are respected.
/// `$@`, `$*` and `$1`… become the alias's arguments (git also appends them);
/// any other `$…` word is unknown and becomes `:/`, the whole tree.
fn shell_commands(text: &str, args: &[String]) -> Vec<Vec<String>> {
    fn end(cmd: &mut Vec<String>, cur: &mut String, word: &mut bool, dynamic: &mut bool, args: &[String]) {
        if *word || !cur.is_empty() {
            let w = std::mem::take(cur);
            let pos = w.strip_prefix('$').and_then(|n| n.parse::<usize>().ok()).filter(|n| *n > 0);
            match () {
                _ if *dynamic && matches!(w.as_str(), "$@" | "$*" | "${@}" | "${*}") => cmd.extend(args.iter().cloned()),
                _ if *dynamic && pos.is_some() => cmd.extend(args.get(pos.unwrap_or(1) - 1).cloned()),
                _ if *dynamic => cmd.push(":/".into()),
                _ => cmd.push(w),
            }
        }
        *word = false;
        *dynamic = false;
    }
    let mut cmds: Vec<Vec<String>> = vec![Vec::new()];
    let (mut cur, mut word, mut dynamic, mut quote) = (String::new(), false, false, None::<char>);
    let mut it = text.chars();
    while let Some(ch) = it.next() {
        let cmd = cmds.last_mut().expect("one");
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => cur.push(c),
            (_, '\\') => cur.extend(it.next()),
            (_, '$' | '`') => {
                dynamic = true;
                cur.push(ch);
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(ch);
                word = true;
            }
            (None, ';' | '&' | '|' | '\n' | '{' | '}' | '(' | ')') => {
                end(cmd, &mut cur, &mut word, &mut dynamic, args);
                if !cmd.is_empty() {
                    cmds.push(Vec::new());
                }
            }
            (None, c) if c.is_whitespace() => end(cmd, &mut cur, &mut word, &mut dynamic, args),
            (None, c) => cur.push(c),
        }
    }
    end(cmds.last_mut().expect("one"), &mut cur, &mut word, &mut dynamic, args);
    cmds.retain(|c| !c.is_empty());
    if let Some(last) = cmds.last_mut() {
        last.extend(args.iter().cloned());
    }
    cmds
}

/// `git …` (after assignments, wrappers like `command`/`env` and shell
/// keywords like `then`/`do`) → its argv.
fn git_argv(words: &[String]) -> Option<&[String]> {
    let k = words.iter().position(|w| {
        let assign = w.split_once('=').is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        !assign
            && !matches!(
                w.as_str(),
                "command" | "exec" | "env" | "nohup" | "time" | "builtin" | "if" | "then" | "else" | "elif" | "while" | "until" | "do" | "!"
            )
    })?;
    let w = &words[k];
    (w == "git" || w.ends_with("/git")).then(|| &words[k + 1..])
}

/// The fallback for a `!` alias command that is not a plain git call (`sh -c
/// '…'`, `xargs git …`, `eval git …`): does any word, quoted text included,
/// run git with a checked subcommand, or with one unknowable here (an alias,
/// a variable)?
fn mentions_checked(words: &[String]) -> bool {
    let toks: Vec<&str> = words.iter().flat_map(|w| w.split(|c: char| c.is_whitespace() || ";&|()`'\"{}".contains(c))).filter(|t| !t.is_empty()).collect();
    toks.iter().enumerate().any(|(i, t)| {
        if !(*t == "git" || t.ends_with("/git")) {
            return false;
        }
        let mut j = i + 1;
        while let Some(o) = toks.get(j).filter(|o| o.starts_with('-')) {
            j += if matches!(*o, "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env") { 2 } else { 1 };
        }
        toks.get(j).is_some_and(|s| CHECKED.contains(s) || !BUILTINS.contains(s))
    })
}

// ── git: classification ───────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
enum Hint {
    Commit,
    Sync,
    Reset,
    Branch,
    Stash(u32), // the stash@{n} named
}

#[derive(Debug, PartialEq)]
enum Scope {
    Repo,
    Specs(Vec<String>),
}

#[derive(Debug, PartialEq)]
struct CommitOp {
    whole: bool, // no paths, -a, -i or -p: the whole shared index
    all: bool,   // -a: every tracked change, staged or not
    amend: bool,
    scope: Scope,
    msgs: Vec<(bool, String)>, // (from a file, value)
}

#[derive(Debug, PartialEq)]
enum Kind {
    Safe,
    Sweep(Hint),
    Scoped(Scope),
    Clean(Scope),
    Commit(CommitOp),
}

#[derive(Debug, Default)]
struct Opts {
    flags: Vec<String>,
    vals: Vec<(String, String)>,
    pos: Vec<String>,
    dd: Option<usize>, // positionals before `--`
}

impl Opts {
    fn has(&self, names: &[&str]) -> bool {
        self.flags.iter().any(|f| names.contains(&f.as_str()))
    }
    fn help(&self) -> bool {
        self.has(&["h", "help"])
    }
    fn after_dd(&self) -> &[String] {
        self.dd.map(|i| &self.pos[i..]).unwrap_or(&[])
    }
}

fn canon_long(n: &str, long: &[&str], names: &[&str]) -> String {
    if long.contains(&n) || names.contains(&n) {
        return n.to_string();
    }
    let mut it = long.iter().chain(names).filter(|x| x.starts_with(n));
    match (it.next(), it.next()) {
        (Some(x), None) => x.to_string(), // git's unique-prefix rule
        _ => n.to_string(),
    }
}

/// Parse subcommand args. `short`: letters taking a value (attached or next
/// arg); `opt`: letters whose value can only be attached; `long`: long options
/// taking a value; `names`: other long flags (for unique prefixes).
fn opts(args: &[String], short: &str, opt: &str, long: &[&str], names: &[&str]) -> Opts {
    let mut o = Opts::default();
    let mut i = 0;
    while i < args.len() {
        let t = &args[i];
        if o.dd.is_some() || t == "-" || !t.starts_with('-') {
            o.pos.push(t.clone());
        } else if t == "--" {
            o.dd = Some(o.pos.len());
        } else if let Some(body) = t.strip_prefix("--") {
            let (n, v) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (body, None),
            };
            let name = canon_long(n, long, names);
            if let Some(v) = v {
                o.vals.push((name.clone(), v.to_string()));
            } else if long.contains(&name.as_str()) && i + 1 < args.len() {
                i += 1;
                o.vals.push((name.clone(), args[i].clone()));
            }
            o.flags.push(name);
        } else {
            let chars: Vec<char> = t[1..].chars().collect();
            for (j, ch) in chars.iter().enumerate() {
                o.flags.push(ch.to_string());
                if short.contains(*ch) {
                    let rest: String = chars[j + 1..].iter().collect();
                    if !rest.is_empty() {
                        o.vals.push((ch.to_string(), rest));
                    } else if i + 1 < args.len() {
                        i += 1;
                        o.vals.push((ch.to_string(), args[i].clone()));
                    }
                    break;
                }
                if opt.contains(*ch) {
                    break;
                }
            }
        }
        i += 1;
    }
    o
}

fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// A revision that moves the shared branch (`reset <rev>`).
fn rev_move(t: &str) -> bool {
    t != "HEAD"
        && t != "@"
        && (t.contains(['~', '^'])
            || (t.len() >= 7 && t.len() <= 40 && t.bytes().all(|b| b.is_ascii_hexdigit()))
            || matches!(t, "ORIG_HEAD" | "FETCH_HEAD" | "MERGE_HEAD")
            || ["origin/", "upstream/", "refs/"].iter().any(|p| t.starts_with(p))
            || t.contains("@{"))
}

/// Forms that finish or undo an operation in progress and keep others' files.
fn recovery(sub: &str) -> &'static [&'static str] {
    match sub {
        "rebase" => &["--quit", "--continue", "--edit-todo", "--show-current-patch"],
        "merge" => &["--abort", "--quit"],
        "cherry-pick" | "revert" => &["--abort", "--quit", "--continue"],
        "am" => &["--abort", "--quit", "--show-current-patch"],
        _ => &[],
    }
}

/// What `git <sub> <args>` does to the worktree. `probe(arg)`: None when no
/// such path exists (relative to git's cwd), else Some(is_dir).
fn classify(sub: &str, args: &[String], probe: &dyn Fn(&str) -> Option<bool>) -> Kind {
    let pathish = |t: &str| {
        t == "." || t == ".." || t.starts_with("./") || t.starts_with("../") || t.starts_with(':') || has_glob(t) || probe(t).is_some()
    };
    let first = args.first().map(String::as_str);
    let first_help = matches!(first, Some("-h" | "--help"));
    let specs = |p: &[String]| Kind::Scoped(Scope::Specs(p.to_vec()));
    match sub {
        "add" | "stage" => {
            let a = opts(args, "", "", &["pathspec-from-file", "chmod"], &[
                "all", "update", "dry-run", "patch", "interactive", "edit", "intent-to-add", "force", "no-all",
                "ignore-removal", "verbose", "renormalize", "refresh", "ignore-errors", "ignore-missing", "sparse",
                "pathspec-file-nul",
            ]);
            if a.help() || a.has(&["n", "dry-run", "refresh"]) {
                Kind::Safe
            } else if a.has(&["pathspec-from-file"]) {
                Kind::Scoped(Scope::Repo)
            } else if !a.pos.is_empty() {
                specs(&a.pos)
            } else if a.has(&["A", "all", "u", "update", "p", "patch", "i", "interactive", "e", "edit", "renormalize"]) {
                Kind::Scoped(Scope::Repo)
            } else {
                Kind::Safe
            }
        }
        "commit" => {
            let a = opts(args, "mFCct", "uS", &[
                "message", "file", "reuse-message", "reedit-message", "fixup", "squash", "author", "date", "template",
                "trailer", "pathspec-from-file", "cleanup",
            ], &[
                "all", "include", "only", "amend", "patch", "interactive", "dry-run", "no-verify", "allow-empty",
                "allow-empty-message", "signoff", "verbose", "quiet", "edit", "no-edit", "reset-author", "status",
                "no-status", "short", "porcelain", "long", "branch", "null", "untracked-files", "gpg-sign",
                "no-gpg-sign", "pathspec-file-nul", "verify", "no-post-rewrite",
            ]);
            if a.help() || a.has(&["dry-run"]) {
                return Kind::Safe;
            }
            let from_file = a.has(&["pathspec-from-file"]);
            let bare = a.pos.is_empty() && !from_file;
            let msgs = a
                .vals
                .iter()
                .filter_map(|(k, v)| match k.as_str() {
                    "m" | "message" => Some((false, v.clone())),
                    "F" | "file" => Some((true, v.clone())),
                    _ => None,
                })
                .collect();
            Kind::Commit(CommitOp {
                whole: bare || a.has(&["a", "all", "i", "include", "p", "patch", "interactive"]),
                all: a.has(&["a", "all"]),
                amend: a.has(&["amend"]),
                scope: if bare || from_file { Scope::Repo } else { Scope::Specs(a.pos.clone()) },
                msgs,
            })
        }
        "stash" => {
            let verb = match first {
                _ if first_help => return Kind::Safe,
                Some(v) if !v.starts_with('-') => v,
                _ => "push",
            };
            if matches!(verb, "list" | "show" | "create" | "store") {
                return Kind::Safe;
            }
            if matches!(verb, "pop" | "apply" | "drop" | "clear") {
                let r = args[1..].iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or("0");
                return Kind::Sweep(Hint::Stash(r.strip_prefix("stash@{").and_then(|r| r.strip_suffix('}')).unwrap_or(r).parse().unwrap_or(0)));
            }
            if verb == "push" {
                let rest = if first == Some("push") { &args[1..] } else { args };
                let a = opts(rest, "m", "", &["message", "pathspec-from-file"], &[
                    "staged", "keep-index", "include-untracked", "all", "patch", "quiet", "pathspec-file-nul",
                ]);
                if !a.pos.is_empty() && !a.has(&["S", "staged", "pathspec-from-file"]) {
                    return specs(&a.pos);
                }
            }
            Kind::Sweep(Hint::Commit)
        }
        "reset" => {
            let a = opts(args, "", "", &["pathspec-from-file"], &[
                "hard", "merge", "keep", "soft", "mixed", "patch", "quiet", "no-refresh", "recurse-submodules",
                "intent-to-add", "pathspec-file-nul",
            ]);
            if a.help() {
                return Kind::Safe;
            }
            if a.has(&["hard", "merge", "keep"]) {
                return Kind::Sweep(Hint::Commit);
            }
            if a.dd.is_some() || a.pos.len() > 1 || a.has(&["pathspec-from-file", "p", "patch"]) {
                return Kind::Safe; // index only
            }
            match a.pos.first() {
                None => Kind::Safe,
                Some(r) if rev_move(r) => Kind::Sweep(Hint::Reset),
                Some(r) if r == "HEAD" || r == "@" || pathish(r) => Kind::Safe,
                Some(_) => Kind::Sweep(Hint::Reset), // `git reset <branch>` moves the shared branch
            }
        }
        "rm" => {
            let a = opts(args, "", "", &["pathspec-from-file"], &[
                "cached", "force", "dry-run", "quiet", "ignore-unmatch", "sparse", "pathspec-file-nul",
            ]);
            if a.help() || a.has(&["n", "dry-run"]) {
                Kind::Safe
            } else if a.has(&["pathspec-from-file"]) {
                Kind::Scoped(Scope::Repo)
            } else {
                specs(&a.pos)
            }
        }
        "mv" => {
            let a = opts(args, "", "", &[], &["force", "dry-run", "verbose", "sparse"]);
            if a.help() || a.has(&["n", "dry-run"]) || a.pos.len() < 2 {
                return Kind::Safe;
            }
            let (dst, srcs) = a.pos.split_last().expect("two or more");
            let mut all = srcs.to_vec();
            if probe(dst) == Some(true) {
                let base = dst.trim_end_matches('/');
                all.extend(srcs.iter().map(|s| format!("{base}/{}", s.trim_end_matches('/').rsplit('/').next().unwrap_or(s))));
            } else {
                all.push(dst.clone());
            }
            Kind::Scoped(Scope::Specs(all))
        }
        "checkout" => {
            let a = opts(args, "bB", "", &["orphan", "pathspec-from-file", "conflict"], &[
                "detach", "force", "patch", "ours", "theirs", "merge", "quiet", "progress", "no-track", "track",
                "guess", "no-guess", "overlay", "no-overlay", "ignore-other-worktrees", "recurse-submodules",
                "pathspec-file-nul",
            ]);
            if a.help() {
                return Kind::Safe;
            }
            if a.has(&["b", "B", "orphan", "detach"]) {
                return Kind::Sweep(Hint::Branch);
            }
            if a.has(&["pathspec-from-file"]) {
                return Kind::Scoped(Scope::Repo);
            }
            if a.dd.is_some() {
                let after = a.after_dd();
                return if !after.is_empty() {
                    specs(after)
                } else if a.pos.is_empty() {
                    Kind::Safe
                } else {
                    Kind::Sweep(Hint::Branch) // `git checkout <branch> --`
                };
            }
            let Some(first) = a.pos.first() else {
                return if a.has(&["f", "force", "m", "merge"]) {
                    Kind::Sweep(Hint::Commit) // discards local changes
                } else if a.has(&["p", "patch"]) {
                    Kind::Scoped(Scope::Repo)
                } else {
                    Kind::Safe
                };
            };
            if first == "-" {
                Kind::Sweep(Hint::Branch)
            } else if pathish(first) {
                specs(&a.pos)
            } else if a.pos.len() > 1 {
                specs(&a.pos[1..]) // <rev> <paths>
            } else {
                Kind::Sweep(Hint::Branch)
            }
        }
        "restore" => {
            let a = opts(args, "s", "", &["source", "pathspec-from-file", "conflict"], &[
                "staged", "worktree", "patch", "ours", "theirs", "merge", "quiet", "progress", "overlay",
                "no-overlay", "ignore-unmerged", "ignore-skip-worktree-bits", "recurse-submodules", "pathspec-file-nul",
            ]);
            if a.help() || (a.has(&["S", "staged"]) && !a.has(&["W", "worktree"])) {
                Kind::Safe // index only
            } else if a.has(&["pathspec-from-file"]) {
                Kind::Scoped(Scope::Repo)
            } else if !a.pos.is_empty() {
                specs(&a.pos)
            } else if a.has(&["p", "patch"]) {
                Kind::Scoped(Scope::Repo)
            } else {
                Kind::Safe
            }
        }
        "clean" => {
            let a = opts(args, "e", "", &["exclude"], &["dry-run", "force", "interactive", "quiet"]);
            if a.help() || a.has(&["n", "dry-run"]) || !a.has(&["f", "force", "i", "interactive"]) {
                Kind::Safe // clean.requireForce
            } else if a.pos.is_empty() {
                Kind::Clean(Scope::Specs(vec![".".into()]))
            } else {
                Kind::Clean(Scope::Specs(a.pos))
            }
        }
        "rebase" | "merge" | "cherry-pick" | "revert" | "am" => {
            // The recovery flag must come first: `git merge -m --abort` merges.
            if first_help || first.is_some_and(|f| recovery(sub).contains(&f)) { Kind::Safe } else { Kind::Sweep(Hint::Sync) }
        }
        "pull" => if first_help { Kind::Safe } else { Kind::Sweep(Hint::Sync) },
        "switch" => if first_help { Kind::Safe } else { Kind::Sweep(Hint::Branch) },
        "filter-branch" | "checkout-index" => if first_help { Kind::Safe } else { Kind::Sweep(Hint::Commit) },
        "bisect" => match first {
            None | Some("log" | "visualize" | "view" | "help" | "-h" | "--help") => Kind::Safe,
            _ => Kind::Sweep(Hint::Commit),
        },
        "sparse-checkout" => match first {
            None | Some("list" | "-h" | "--help") => Kind::Safe,
            _ => Kind::Sweep(Hint::Commit),
        },
        "read-tree" => {
            let u = |t: &String| t == "--reset" || (t.len() > 1 && !t.starts_with("--") && t.starts_with('-') && t.contains('u'));
            if args.iter().any(u) { Kind::Sweep(Hint::Commit) } else { Kind::Safe }
        }
        _ => Kind::Safe,
    }
}

// ── git: pathspecs ────────────────────────────────────────────────────────

/// One pathspec, repo-relative. `pat` None: outside the worktree (git refuses it).
#[derive(Clone, Debug, PartialEq)]
struct Spec {
    pat: Option<String>,
    glob: bool,
    pathname: bool, // :(glob): `*` stays within a directory, `**` crosses
    icase: bool,
    exclude: bool,
}

/// None: magic we cannot evaluate (attr:…, unknown): treat as the whole repo.
fn spec(text: &str, prefix: &str, top: &Path, m: Magic) -> Option<Spec> {
    let (mut s, mut from_top, mut exclude, mut literal, mut glob, mut icase) = (text, false, false, false, false, m.icase);
    if !m.literal {
        if let Some(rest) = s.strip_prefix(":(") {
            let j = rest.find(')')?;
            for w in rest[..j].split(',') {
                match w.trim() {
                    "top" => from_top = true,
                    "exclude" => exclude = true,
                    "literal" => literal = true,
                    "glob" => glob = true,
                    "icase" => icase = true,
                    "" => {}
                    _ => return None,
                }
            }
            s = &rest[j + 1..];
        } else if let Some(rest) = s.strip_prefix(':') {
            let b = rest.as_bytes();
            let mut k = 0;
            while k < b.len() && matches!(b[k], b'/' | b'!' | b'^') {
                if b[k] == b'/' { from_top = true } else { exclude = true }
                k += 1;
            }
            if b.get(k) == Some(&b':') {
                k += 1;
            }
            s = &rest[k..];
        }
    }
    let literal = m.literal || literal || (m.noglob && !glob);
    let pat = if s.starts_with('/') { abs_rel(s, top) } else { join_norm(if from_top { "" } else { prefix }, s) };
    Some(Spec { pat, glob: !literal && has_glob(s), pathname: glob || m.glob, icase, exclude })
}

/// Lexical `base/rel` ('.' and '..' resolved like git does); None above the top.
fn join_norm(base: &str, rel: &str) -> Option<String> {
    let mut out: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            p => out.push(p),
        }
    }
    Some(out.join("/"))
}

/// An absolute pathspec made repo-relative; the literal head is canonicalized.
fn abs_rel(s: &str, top: &Path) -> Option<String> {
    let parts: Vec<&str> = s.split('/').collect();
    let k = parts.iter().position(|p| has_glob(p)).unwrap_or(parts.len());
    let head = sys::canonical(Path::new(&parts[..k].join("/")), Path::new("/"));
    let rel = if head == top { String::new() } else { head.strip_prefix(top).ok()?.to_str()?.to_string() };
    join_norm(&rel, &parts[k..].join("/"))
}

/// git's wildmatch: `*` crosses '/' unless `pathname` (then only `**` does).
/// A pathological pattern runs out of `budget` and counts as a match.
fn wild(p: &[u8], t: &[u8], pathname: bool, budget: &mut u32) -> bool {
    let (mut pi, mut ti) = (0, 0);
    while pi < p.len() {
        if *budget == 0 {
            return true;
        }
        *budget -= 1;
        match p[pi] {
            b'*' => {
                let mut np = pi;
                while np < p.len() && p[np] == b'*' {
                    np += 1;
                }
                let cross = !pathname || np - pi > 1;
                if pathname && np - pi > 1 && p.get(np) == Some(&b'/') && (pi == 0 || p[pi - 1] == b'/') && wild(&p[np + 1..], &t[ti..], pathname, budget) {
                    return true; // `**/` matches zero directories
                }
                if np == p.len() {
                    return cross || !t[ti..].contains(&b'/');
                }
                let mut k = ti;
                loop {
                    if wild(&p[np..], &t[k..], pathname, budget) {
                        return true;
                    }
                    if k == t.len() || (!cross && t[k] == b'/') {
                        return false;
                    }
                    k += 1;
                }
            }
            b'?' => {
                if ti >= t.len() || (pathname && t[ti] == b'/') {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            b'[' if class(&p[pi..], 0, false).is_some() => {
                let Some(&c) = t.get(ti) else { return false };
                let (ok, len) = class(&p[pi..], c, pathname).expect("checked");
                if !ok {
                    return false;
                }
                pi += len;
                ti += 1;
            }
            b'\\' if pi + 1 < p.len() => {
                if t.get(ti) != Some(&p[pi + 1]) {
                    return false;
                }
                pi += 2;
                ti += 1;
            }
            c => {
                if t.get(ti) != Some(&c) {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == t.len()
}

/// `[…]` at the start of `p` against `c`: (matches, length). None: no closing `]`.
fn class(p: &[u8], c: u8, pathname: bool) -> Option<(bool, usize)> {
    let mut i = 1;
    let neg = matches!(p.get(i), Some(b'!' | b'^'));
    if neg {
        i += 1;
    }
    let (mut hit, mut first) = (false, true);
    loop {
        let &x = p.get(i)?;
        if x == b']' && !first {
            break;
        }
        first = false;
        let lo = if x == b'\\' {
            i += 1;
            *p.get(i)?
        } else {
            x
        };
        if p.get(i + 1) == Some(&b'-') && p.get(i + 2).is_some_and(|&y| y != b']') {
            let mut j = i + 2;
            let mut hi = p[j];
            if hi == b'\\' {
                j += 1;
                hi = *p.get(j)?;
            }
            hit |= lo <= c && c <= hi;
            i = j + 1;
        } else {
            hit |= lo == c;
            i += 1;
        }
    }
    Some((hit != neg && !(pathname && c == b'/'), i + 1))
}

/// Does the pathspec reach `path` (the file, or a directory above it)?
fn reaches(s: &Spec, path: &str, fold: bool) -> bool {
    let Some(pat) = &s.pat else { return false };
    let ci = fold || s.icase;
    let (pat, p) = if ci { (pat.to_lowercase(), path.to_lowercase()) } else { (pat.clone(), path.to_string()) };
    let pat = pat.trim_end_matches('/');
    if pat.is_empty() || p == pat || (p.starts_with(pat) && p.as_bytes().get(pat.len()) == Some(&b'/')) {
        return true;
    }
    if !s.glob {
        return false;
    }
    let mut budget = 100_000;
    let mut end = p.len();
    loop {
        if wild(pat.as_bytes(), &p.as_bytes()[..end], s.pathname, &mut budget) {
            return true;
        }
        match p[..end].rfind('/') {
            Some(i) => end = i,
            None => return false,
        }
    }
}

// ── git: the decision ─────────────────────────────────────────────────────

/// Whose work a git call must spare.
struct Who<'s> {
    st: &'s State,
    me: String,
    others: Vec<&'s Claim>, // live claims of every other agent id
    shared: bool,           // `me` is a main id while a subagent may run under it
    mine: Vec<&'s Claim>,   // then: the main agent's own live claims
    multi: bool,            // another agent is at work (rules::multi_agent)
}

impl Who<'_> {
    fn all(&self) -> Vec<&Claim> {
        self.others.iter().chain(&self.mine).copied().collect()
    }
}

struct Judge<'c> {
    who: &'c Who<'c>,
    place: &'c Place,
    magic: Magic,
    label: String, // the command as typed
    note: String,  // " (alias for …)"
}

impl<'c> Judge<'c> {
    /// The claims `scope` reaches. The main agent's own count only when a
    /// pathspec sweeps over them (`.`, a directory, a glob), not when it names
    /// the file itself.
    fn covered(&self, scope: &Scope) -> Vec<&'c Claim> {
        let w = self.who;
        let texts = match scope {
            Scope::Repo => return w.all(),
            Scope::Specs(v) => v,
        };
        let (mut inc, mut exc, mut positive) = (Vec::new(), Vec::new(), false);
        for t in texts {
            let Some(mut s) = spec(t, &self.place.prefix, &self.place.repo.top, self.magic) else { return w.all() };
            if let Some(p) = s.pat.as_ref().filter(|p| !s.glob && !p.is_ascii()) {
                s.pat = Some(sys::git_spelling(&self.place.repo, p)); // claims are keyed by git's spelling (NFC on macOS)
            }
            if s.exclude {
                exc.push(s);
            } else {
                positive = true;
                inc.push(s);
            }
        }
        if !positive && exc.is_empty() {
            return Vec::new();
        }
        let fold = w.st.fold;
        let exact = |s: &Spec, p: &str| !s.glob && s.pat.as_deref().is_some_and(|x| if fold || s.icase { x.eq_ignore_ascii_case(p) } else { x == p });
        let hit = |c: &Claim, sweep_only: bool| {
            (!positive || inc.iter().any(|s| reaches(s, &c.path, fold) && !(sweep_only && exact(s, &c.path)))) && !exc.iter().any(|s| reaches(s, &c.path, fold))
        };
        w.others.iter().copied().filter(|c| hit(c, false)).chain(w.mine.iter().copied().filter(|c| hit(c, true))).collect()
    }

    fn commit_cmd(&self, msgs: &[(bool, String)]) -> String {
        let mut out = format!("agent-lock commit{}", as_flag(&self.who.me));
        let mut any = false;
        for (file, v) in msgs {
            if *file && v == "-" {
                continue; // a here-doc on stdin cannot be carried over
            }
            let v = if v.contains('\n') || v.chars().count() > 120 { "\"<msg>\"".to_string() } else { sys::quote(v) };
            out.push_str(if *file { " -F " } else { " -m " });
            out.push_str(&v);
            any = true;
        }
        if !any {
            out.push_str(" -m \"<msg>\"");
        }
        out
    }

    fn hint(&self, h: Hint) -> String {
        let as_ = as_flag(&self.who.me);
        match h {
            Hint::Commit => format!("Commit only your own files: agent-lock commit{as_} -m \"<msg>\" [-- <paths>]; to undo your own change, name the file: git checkout -- <file>."),
            Hint::Reset => format!("Moving the shared branch can drop commits other agents build on: make a new commit instead (agent-lock commit{as_} -m \"<msg>\"); to unstage, name the paths: git reset -- <paths>."),
            Hint::Branch => format!("This checkout is shared on one branch: stay on it and commit your own files with agent-lock commit{as_} -m \"<msg>\" (to restore a file, name it after --: git checkout -- <file>)."),
            Hint::Stash(n) => format!(
                "The stash stack is shared, so pop, apply and drop wait until the others commit; to get your stashed work back now: git stash show --name-only stash@{{{n}}}, then git checkout stash@{{{n}}} -- <your paths> (or git restore --source=stash@{{{n}}} -- <your paths>)."
            ),
            Hint::Sync => {
                let g = &self.place.repo.git_dir;
                let at = |p: &str| g.join(p).exists();
                let busy = if at("rebase-merge") || (at("rebase-apply") && !at("rebase-apply/applying")) {
                    Some("A rebase is in progress: resolve the conflicts, git add the resolved files and run git rebase --continue, or git rebase --quit to stop; --abort and --skip would discard their work, so ask the user first.")
                } else if at("rebase-apply") {
                    Some("A git am is in progress: git am --abort or --quit are allowed; --skip would discard their work, so ask the user first.")
                } else if at("MERGE_HEAD") {
                    Some("A merge is in progress: git merge --abort is allowed (it keeps their files); concluding it commits the whole shared index, so ask the user first.")
                } else if at("CHERRY_PICK_HEAD") {
                    Some("A cherry-pick is in progress: git cherry-pick --continue, --abort or --quit are allowed; ask the user before anything else.")
                } else if at("REVERT_HEAD") {
                    Some("A revert is in progress: git revert --continue, --abort or --quit are allowed; ask the user before anything else.")
                } else {
                    None
                };
                let detached = std::fs::read_to_string(g.join("HEAD")).is_ok_and(|h| !h.starts_with("ref:"));
                match busy {
                    Some(b) => b.to_string(),
                    None if detached => "HEAD is detached, so agent-lock sync cannot run here: ask the user before moving HEAD while other agents have uncommitted work.".into(),
                    None => format!("Get upstream changes with agent-lock sync{as_} (it never touches uncommitted files)."),
                }
            }
        }
    }

    /// Why the caller's own family shows up in the list.
    fn family_note(&self, hit: &[&Claim]) -> String {
        let w = self.who;
        if !w.shared || !hit.iter().any(|c| ids::family(&c.by) == w.me) {
            return String::new();
        }
        format!(
            " Subagents share this shell's agent id {}, so while one may run the main agent's files count as another agent's: a subagent names itself (AGENT_LOCK_AGENT_ID=<its id> git …, agent-lock commit --as <its id>).",
            w.me
        )
    }

    /// A whole-index commit while another agent is at work but holds no live
    /// claim: refused when it would take changes the caller has not claimed.
    fn stray(&self, c: &CommitOp, label: &str) -> Option<String> {
        let w = self.who;
        if !c.whole || !w.multi {
            return None;
        }
        let what: &[&str] = if c.all { &["HEAD"] } else { &["--cached"] };
        let mut a = vec!["--no-optional-locks", "diff", "--name-only", "--no-renames", "-z"];
        a.extend(what);
        let o = git::run_in(&self.place.repo.top, &a, &[], None).ok().filter(|o| o.ok())?;
        let stray: Vec<String> = o.text().split('\0').filter(|p| !p.is_empty() && w.st.claim(p).is_none_or(|x| x.by != w.me)).map(str::to_string).collect();
        if stray.is_empty() {
            return None;
        }
        Some(format!(
            "agent-lock: {label} would commit changes you have not claimed ({}) while another agent is at work. Commit only your own files: {} -- <your paths>",
            names(&stray),
            self.commit_cmd(&c.msgs)
        ))
    }

    /// The refusal for `kind`, or None to run git.
    fn verdict(&self, kind: &Kind) -> Option<String> {
        let label = format!("`{}`{}", self.label, self.note);
        let as_ = as_flag(&self.who.me);
        // (what it reaches, the refusal, the safe alternative)
        let (hit, head, tail) = match kind {
            Kind::Safe => return None,
            Kind::Sweep(h) => {
                let hit = self.who.all();
                if hit.is_empty() {
                    return None;
                }
                let head = format!("agent-lock: {label} acts on the whole shared checkout while other agents have uncommitted work ({}).", list(&hit));
                (hit, head, self.hint(*h))
            }
            Kind::Scoped(s) => {
                let hit = self.covered(s);
                if hit.is_empty() {
                    return None;
                }
                let head = format!("agent-lock: {label} covers files other agents are editing ({}).", list(&hit));
                (hit, head, format!("Name only your own files, or commit them with agent-lock commit{as_} -m \"<msg>\" -- <paths>."))
            }
            Kind::Clean(s) => {
                let top = &self.place.repo.top;
                let hit: Vec<&Claim> = self.covered(s).into_iter().filter(|c| top.join(&c.path).symlink_metadata().is_ok()).collect();
                if hit.is_empty() {
                    return None;
                }
                // ls-files has no --pathspec-from-file; the hits are few.
                let specs: Vec<String> = hit.iter().map(|c| format!(":(literal){}", c.path)).collect();
                let mut a = vec!["--no-optional-locks", "ls-files", "-z", "--"];
                a.extend(specs.iter().map(String::as_str));
                let o = git::run_in(top, &a, &[], None).ok().filter(|o| o.ok())?;
                let text = o.text();
                let tracked: Vec<&str> = text.split('\0').collect();
                let fold = self.who.st.fold;
                let untracked: Vec<&Claim> =
                    hit.into_iter().filter(|c| !tracked.iter().any(|t| if fold { t.eq_ignore_ascii_case(&c.path) } else { *t == c.path })).collect();
                if untracked.is_empty() {
                    return None;
                }
                let head = format!("agent-lock: {label} would delete untracked files other agents are editing ({}).", list(&untracked));
                (untracked, head, "Name only your own paths: git clean -f -- <paths>.".to_string())
            }
            Kind::Commit(c) if c.whole || c.amend => {
                let hit = self.who.all();
                if hit.is_empty() {
                    return self.stray(c, &label).map(|l| one_line(&l));
                }
                let head = format!(
                    "agent-lock: {label} commits the whole shared index (no paths, -a, -i, -p or --amend) while other agents have uncommitted work ({}).",
                    list(&hit)
                );
                let amend = if c.amend { " (never amend in a shared checkout: HEAD may be another agent's commit)" } else { "" };
                (hit, head, format!("Commit only your own files: {}{amend}", self.commit_cmd(&c.msgs)))
            }
            Kind::Commit(c) => {
                let hit = self.covered(&c.scope);
                if hit.is_empty() {
                    return None;
                }
                let head = format!("agent-lock: {label} names files other agents are editing ({}).", list(&hit));
                (hit, head, format!("Commit only your own files: {} -- <your paths>", self.commit_cmd(&c.msgs)))
            }
        };
        Some(one_line(&format!("{head}{} {tail}", self.family_note(&hit))))
    }
}

fn label(sub: &str, args: &[String]) -> String {
    let words: Vec<String> = std::iter::once("git".to_string()).chain(std::iter::once(sub.to_string())).chain(args.iter().map(|a| sys::quote(a))).collect();
    cut(one_line(&words.join(" ")), 60)
}

fn git_plan(args: &[String]) -> Plan {
    if disabled() {
        return Plan::Exec;
    }
    let Some(call) = parse_call(args) else { return Plan::Exec };
    let Some(sub) = call.sub.as_deref() else { return Plan::Exec };
    let builtin = BUILTINS.contains(&sub);
    if builtin && !CHECKED.contains(&sub) {
        return Plan::Exec; // the hot path: no state, no repo lookup
    }
    let Some(me) = ids::caller(None) else { return Plan::Exec };
    let Some(place) = std::env::current_dir().ok().and_then(|d| place(&call, &d)) else { return Plan::Exec };
    // A stuck lock: judge from the state as last written rather than not at all.
    let Some(st) = state::read(&place.repo).ok().or_else(|| state::peek(&place.repo)) else { return Plan::Exec };
    if st.off {
        return Plan::Exec;
    }
    let ctx = Ctx::real(sys::now());
    let others = rules::others(&st, &me, &ctx);
    // A subagent's Bash runs under its parent's id: while one may be running,
    // "me" may be that subagent, so the main agent's own work is at stake too.
    let shared = !ids::is_sub(&me) && rules::live_subs(&st, &me);
    let mine = if shared { st.claims.values().filter(|c| c.by == me && rules::stale(&st, c, &ctx, None).is_none()).collect() } else { Vec::new() };
    let multi = !others.is_empty() || rules::multi_agent(&st, &me, &ctx);
    if !multi && mine.is_empty() {
        return Plan::Exec;
    }
    let who = Who { st: &st, me, others, shared, mine, multi };
    let mut call = call;
    call.magic.literal |= env_on("GIT_LITERAL_PATHSPECS");
    call.magic.glob |= env_on("GIT_GLOB_PATHSPECS");
    call.magic.noglob |= env_on("GIT_NOGLOB_PATHSPECS");
    call.magic.icase |= env_on("GIT_ICASE_PATHSPECS");
    match judge(&call, &place, &who, "", 0) {
        Some(why) => Plan::Deny(why),
        None => Plan::Exec,
    }
}

/// A word `shell_commands` could not know (a variable, `$(…)`).
fn unknown(p: &Path) -> bool {
    p.to_str().is_some_and(|s| s.contains(":/"))
}

/// The refusal for one git call (an alias is followed), or None to run it.
fn judge(call: &Call, place: &Place, who: &Who, via: &str, depth: u32) -> Option<String> {
    let sub = call.sub.as_deref()?;
    let label = label(sub, &call.args);
    let (c, note) = if BUILTINS.contains(&sub) {
        (None, via.to_string())
    } else {
        match expand(call, place)? {
            Expansion::Git(c, text) => (Some(c), format!(" (alias for git {}){via}", cut(one_line(&text), 40))),
            Expansion::Shell(text, args, config) => {
                if depth > 2 {
                    return None;
                }
                let via = format!(" (run by the alias `{label}` = !{})", cut(one_line(&text), 40));
                let mut opaque = false; // a command that may run git in ways not followed here
                for words in shell_commands(&text, &args) {
                    let Some(argv) = git_argv(&words) else {
                        opaque |= mentions_checked(&words);
                        continue;
                    };
                    let Some(mut inner) = parse_call(argv) else { continue }; // git rejects it
                    if inner.sub.as_deref() == Some(":/") {
                        opaque = true; // `git $cmd`
                        continue;
                    }
                    if unknown(&inner.dir) || inner.git_dir.as_deref().is_some_and(unknown) || inner.work_tree.as_deref().is_some_and(unknown) {
                        (inner.dir, inner.git_dir, inner.work_tree) = (PathBuf::new(), None, None); // `-C "$X"`: judged at the top
                    }
                    inner.config.splice(0..0, config.iter().cloned());
                    let m = call.magic; // global pathspec options reach the alias's git through the environment
                    inner.magic = Magic { literal: m.literal || inner.magic.literal, glob: m.glob || inner.magic.glob, noglob: m.noglob || inner.magic.noglob, icase: m.icase || inner.magic.icase };
                    let Some(ip) = self::place(&inner, &place.repo.top) else { continue }; // `!` aliases run at the top
                    if ip.repo != place.repo {
                        continue;
                    }
                    if let Some(why) = judge(&inner, &ip, who, &via, depth + 1) {
                        return Some(why);
                    }
                }
                if !opaque {
                    return None;
                }
                let note = format!(" (= !{}, which runs git in a way agent-lock cannot follow)", cut(one_line(&text), 40));
                return Judge { who, place, magic: call.magic, label, note }.verdict(&Kind::Sweep(Hint::Commit));
            }
        }
    };
    let c = c.as_ref().unwrap_or(call);
    let cwd = place.cwd.clone();
    let probe = move |t: &str| cwd.join(t).symlink_metadata().ok().map(|m| m.is_dir());
    let kind = classify(c.sub.as_deref()?, &c.args, &probe);
    Judge { who, place, magic: c.magic, label, note }.verdict(&kind)
}

// ── package managers ──────────────────────────────────────────────────────

#[derive(Debug, Default, PartialEq)]
struct PkgOp {
    deps: bool,          // changes the dependency set: claim manifests + lockfiles
    dir: Option<String>, // directory option as written
    ws: Vec<String>,     // workspace selectors (names or paths)
    all_ws: bool,        // every workspace (-r, --workspaces)
    ws_root: bool,       // the workspace root's own manifest (pnpm -w, --include-workspace-root)
}

#[derive(Default)]
struct Scan {
    pos: Vec<String>,
    flags: Vec<String>,
    vals: Vec<(String, String)>,
    dir: Option<String>,
}

impl Scan {
    fn has(&self, names: &[&str]) -> bool {
        self.flags.iter().any(|f| names.contains(&f.as_str()))
    }
    fn val(&self, names: &[&str]) -> Option<&str> {
        self.vals.iter().find(|(k, _)| names.contains(&k.as_str())).map(|(_, v)| v.as_str())
    }
    fn all(&self, names: &[&str]) -> Vec<String> {
        self.vals.iter().filter(|(k, _)| names.contains(&k.as_str())).map(|(_, v)| v.clone()).collect()
    }
    fn pos(&self, i: usize) -> Option<&str> {
        self.pos.get(i).map(String::as_str)
    }
}

/// Positionals and options. `value`: options taking the next argument; `dir`:
/// its value is the directory to run in; `file`: a file whose directory is.
fn scan(args: &[String], value: &[&str], dir: &[&str], file: &[&str]) -> Scan {
    let mut s = Scan::default();
    let mut dd = false;
    let mut i = 0;
    while i < args.len() {
        let t = &args[i];
        if dd || !t.starts_with('-') || t == "-" {
            s.pos.push(t.clone());
        } else if t == "--" {
            dd = true;
        } else {
            let (name, mut v) = match t.split_once('=') {
                Some((n, v)) if t.starts_with("--") => (n.to_string(), Some(v.to_string())),
                _ => (t.clone(), None),
            };
            let n = name.as_str();
            if v.is_none() && (value.contains(&n) || dir.contains(&n) || file.contains(&n)) {
                i += 1;
                v = args.get(i).cloned();
            }
            if let Some(v) = v {
                if dir.contains(&n) {
                    s.dir = Some(v.clone());
                } else if file.contains(&n) {
                    s.dir = Some(Path::new(&v).parent().map(|p| p.display().to_string()).filter(|p| !p.is_empty()).unwrap_or_else(|| ".".into()));
                }
                s.vals.push((name.clone(), v));
            }
            s.flags.push(name);
        }
        i += 1;
    }
    s
}

fn verb(table: &[(&'static str, &[&str])], v: Option<&str>) -> Option<&'static str> {
    let v = v?;
    table.iter().find(|(k, alias)| *k == v || alias.contains(&v)).map(|(k, _)| *k)
}

const NPM: &[(&str, &[&str])] = &[
    ("install", &["i", "in", "ins", "inst", "insta", "instal", "isnt", "isnta", "isntal", "isntall", "add"]),
    ("install-test", &["it"]),
    ("uninstall", &["un", "unlink", "remove", "rm", "r"]),
    ("update", &["up", "upgrade", "udpate"]),
    ("ci", &["clean-install", "ic", "install-clean", "isntall-clean"]),
    ("install-ci-test", &["cit"]),
    ("dedupe", &["ddp"]),
    ("prune", &[]),
    ("audit", &[]),
];
const PNPM: &[(&str, &[&str])] = &[
    ("install", &["i"]),
    ("install-test", &["it"]),
    ("add", &[]),
    ("remove", &["rm", "uninstall", "un"]),
    ("update", &["up", "upgrade"]),
    ("dedupe", &[]),
    ("import", &[]),
];
const YARN: &[(&str, &[&str])] =
    &[("install", &[]), ("add", &[]), ("remove", &[]), ("upgrade", &["up"]), ("upgrade-interactive", &[]), ("dedupe", &[]), ("import", &[])];
const BUN: &[(&str, &[&str])] = &[("install", &["i"]), ("add", &["a"]), ("remove", &["rm"]), ("update", &[])];
const CARGO: &[(&str, &[&str])] = &[("add", &[]), ("remove", &["rm"]), ("update", &["up"]), ("generate-lockfile", &[])];

/// Is `<manager> <args>` a dependency operation (v2's pkg.cjs)? None: pass
/// straight through.
fn classify_pkg(manager: &str, args: &[String]) -> Option<PkgOp> {
    let op = |deps, s: Scan| Some(PkgOp { deps, dir: s.dir, ..PkgOp::default() });
    match manager {
        "npm" => {
            let s = scan(args, &[
                "--registry", "--tag", "--loglevel", "--cache", "--userconfig", "--globalconfig", "--omit", "--include",
                "--install-strategy", "--save-prefix", "--before", "--otp", "-w", "--workspace", "--access", "--auth-type",
                "--script-shell", "--location",
            ], &["--prefix", "-C"], &[]);
            let sub = verb(NPM, s.pos(0))?;
            if s.has(&["-g", "--global"]) || s.val(&["--location"]) == Some("global") {
                return None;
            }
            let deps = match sub {
                "install" | "install-test" => (s.pos.len() > 1 && !s.has(&["--no-save"])) || s.has(&["--package-lock-only"]),
                "uninstall" | "update" | "dedupe" => true,
                "audit" if s.pos(1) == Some("fix") => true,
                "audit" => return None,
                _ => false,
            };
            let (ws, all_ws, ws_root) = (s.all(&["-w", "--workspace"]), s.has(&["--workspaces", "-ws"]), s.has(&["--include-workspace-root"]));
            Some(PkgOp { deps, dir: s.dir, ws, all_ws, ws_root })
        }
        "pnpm" => {
            let s = scan(args, &[
                "--filter", "-F", "--filter-prod", "--reporter", "--loglevel", "--store-dir", "--virtual-store-dir",
                "--workspace-concurrency", "--test-pattern", "--changed-files-ignore-pattern",
            ], &["-C", "--dir"], &[]);
            if s.has(&["-g", "--global"]) {
                return None;
            }
            let sub = if s.pos.is_empty() {
                if s.has(&["-v", "--version", "-h", "--help"]) {
                    return None;
                }
                "install"
            } else {
                verb(PNPM, s.pos(0))?
            };
            let deps = if sub == "install" || sub == "install-test" { s.pos.len() > 1 || s.has(&["--fix-lockfile", "--lockfile-only"]) } else { true };
            let ws = s.all(&["--filter", "-F", "--filter-prod"]);
            let (all_ws, ws_root) = (s.has(&["-r", "--recursive"]), s.has(&["-w", "--workspace-root", "--include-workspace-root"]));
            Some(PkgOp { deps, dir: s.dir, ws, all_ws, ws_root })
        }
        "yarn" => {
            let s = scan(args, &[
                "--modules-folder", "--cache-folder", "--mutex", "--network-timeout", "--registry", "--global-folder",
                "--link-folder", "--network-concurrency", "--proxy", "--https-proxy",
            ], &["--cwd"], &[]);
            if s.pos.is_empty() {
                if s.has(&["-v", "--version", "-h", "--help"]) {
                    return None;
                }
                return op(false, s); // bare `yarn` installs
            }
            if s.pos[0] == "workspace" && s.pos.len() >= 3 {
                let at = args.iter().position(|a| a == "workspace")? + 2;
                let mut inner = classify_pkg("yarn", &args[at..])?;
                inner.ws.push(s.pos[1].clone());
                if inner.dir.is_none() {
                    inner.dir = s.dir;
                }
                return Some(inner);
            }
            let sub = verb(YARN, s.pos(0))?;
            op(sub != "install" || s.pos.len() > 1, s)
        }
        "bun" => {
            let s = scan(args, &[
                "--backend", "--cache-dir", "--registry", "--config", "-c", "--network-concurrency", "--concurrent-scripts",
                "--filter",
            ], &["--cwd"], &[]);
            let sub = verb(BUN, s.pos(0))?;
            if s.has(&["-g", "--global"]) {
                return None;
            }
            let deps = sub != "install" || s.pos.len() > 1;
            Some(PkgOp { deps, ws: s.all(&["--filter"]), dir: s.dir, ..PkgOp::default() })
        }
        "uv" => {
            let s = scan(args, &[
                "--python", "-p", "--package", "--index", "--default-index", "--index-url", "-i", "--extra-index-url",
                "--find-links", "-f", "--group", "--extra", "--optional", "--cache-dir", "--config-file", "--color",
                "--resolution", "--prerelease", "--exclude-newer", "--with", "--upgrade-package", "-P",
                "--reinstall-package", "--refresh-package", "--index-strategy", "--keyring-provider", "--link-mode",
                "--script", "--bounds", "--branch", "--tag", "--rev", "--marker", "-m", "--requirements", "-r",
                "--constraints", "-c", "--python-platform", "--only-group", "--no-group", "--target", "--prefix",
            ], &["--directory", "--project"], &[]);
            match (s.pos(0)?, s.pos(1)) {
                ("add" | "remove" | "lock", _) => op(true, s),
                ("sync", _) | ("pip", Some("install" | "uninstall" | "sync")) => op(false, s),
                _ => None,
            }
        }
        "poetry" => {
            let s = scan(args, &["--group", "-G", "--extras", "-E", "--source", "--python", "--with", "--without", "--only", "--platform"], &[
                "-C", "--directory", "-P", "--project",
            ], &[]);
            match s.pos(0)? {
                "add" | "remove" | "lock" | "update" => op(true, s),
                "install" | "sync" => op(false, s),
                _ => None,
            }
        }
        "cargo" => {
            let args = if args.first().is_some_and(|a| a.starts_with('+')) { &args[1..] } else { args }; // +toolchain
            let s = scan(args, &[
                "-p", "--package", "--features", "-F", "--target", "--registry", "--rename", "--git", "--branch", "--tag",
                "--rev", "--path", "--precise", "-Z", "--config", "--color", "--target-dir", "--lockfile-path",
            ], &[], &["--manifest-path"]);
            verb(CARGO, s.pos(0))?;
            op(true, s)
        }
        "go" => {
            let s = scan(args, &["-modfile", "-mod"], &["-C"], &[]);
            match (s.pos(0)?, s.pos(1)) {
                ("get", _) | ("mod", Some("tidy" | "edit")) => op(true, s),
                ("mod", Some("vendor")) => op(false, s),
                _ => None,
            }
        }
        "bundle" | "bundler" => {
            let s = scan(args, &[
                "--path", "--jobs", "-j", "--retry", "--without", "--with", "--group", "-g", "--version", "-v", "--source",
                "-s", "--git", "--branch", "--ref", "--github", "--glob", "--require", "--binstubs", "--shebang",
                "--standalone", "--trust-policy", "--target-rbconfig",
            ], &[], &["--gemfile"]);
            match s.pos(0) {
                None if s.has(&["--version", "-v", "--help", "-h"]) => None,
                None | Some("install" | "i") => op(false, s), // bare `bundle` installs
                Some("add" | "update" | "remove" | "lock") => op(true, s),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Nearest `file` (a manifest name) at or above `dir`, not above `top`.
fn manifest_for(dir: &Path, top: &Path, file: &str) -> Option<PathBuf> {
    let mut d = dir;
    if !d.starts_with(top) {
        return None;
    }
    loop {
        let p = d.join(file);
        if p.exists() {
            return Some(p);
        }
        if d == top {
            return None;
        }
        d = d.parent()?;
    }
}

fn package_name(dir: &Path) -> Option<String> {
    let j = crate::json::parse(&std::fs::read_to_string(dir.join("package.json")).ok()?).ok()?;
    j.get("name").str().map(str::to_string)
}

/// `packages:` of pnpm-workspace.yaml: a block or a flow list of globs.
fn yaml_packages(y: &str) -> Vec<String> {
    let unquote = |s: &str| s.trim().trim_matches(['\'', '"']).to_string();
    let (mut out, mut inside) = (Vec::new(), false);
    for line in y.lines() {
        let t = line.split(" #").next().unwrap_or("").trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if !line.starts_with([' ', '\t', '-']) {
            inside = false;
            if let Some(rest) = t.strip_prefix("packages:").map(str::trim) {
                match rest.strip_prefix('[') {
                    Some(flow) => out.extend(flow.trim_end_matches(']').split(',').map(unquote).filter(|s| !s.is_empty())),
                    None => inside = true,
                }
            }
        } else if let Some(item) = t.strip_prefix('-').filter(|_| inside) {
            out.push(unquote(item));
        }
    }
    out
}

/// The workspace globs a directory declares: pnpm-workspace.yaml, or the
/// package.json `workspaces` (an array, or `{packages: […]}`).
fn ws_globs(d: &Path) -> Option<Vec<String>> {
    if let Ok(y) = std::fs::read_to_string(d.join("pnpm-workspace.yaml")) {
        return Some(yaml_packages(&y));
    }
    let j = crate::json::parse(&std::fs::read_to_string(d.join("package.json")).ok()?).ok()?;
    let w = j.get("workspaces");
    if w.is_null() {
        return None;
    }
    let list = if w.arr().is_empty() { w.get("packages").arr() } else { w.arr() };
    Some(list.iter().filter_map(|x| x.str().map(str::to_string)).collect())
}

/// Directories under `base` matching glob segments (`*` one level, `**` any
/// depth; dot directories and node_modules never match).
fn glob_dirs(base: &Path, segs: &[&str], out: &mut Vec<PathBuf>, budget: &mut u32) {
    let Some((seg, rest)) = segs.split_first() else {
        out.push(base.to_path_buf());
        return;
    };
    let subdirs = |budget: &mut u32| -> Vec<(String, PathBuf)> {
        let Ok(rd) = std::fs::read_dir(base) else { return Vec::new() };
        rd.flatten()
            .filter_map(|e| {
                let n = e.file_name().into_string().ok()?;
                (!n.starts_with('.') && n != "node_modules" && e.path().is_dir()).then(|| (n, e.path()))
            })
            .take_while(|_| {
                *budget = budget.saturating_sub(1);
                *budget > 0
            })
            .collect()
    };
    match *seg {
        "**" => {
            glob_dirs(base, rest, out, budget);
            for (_, d) in subdirs(budget) {
                glob_dirs(&d, segs, out, budget);
            }
        }
        "" | "." => glob_dirs(base, rest, out, budget),
        s if has_glob(s) => {
            for (n, d) in subdirs(budget) {
                if wild(s.as_bytes(), n.as_bytes(), true, &mut 10_000) {
                    glob_dirs(&d, rest, out, budget);
                }
            }
        }
        s => {
            let d = base.join(s);
            if d.is_dir() {
                glob_dirs(&d, rest, out, budget);
            }
        }
    }
}

/// Workspace packages: (directory, package name).
type Pkgs = Vec<(PathBuf, Option<String>)>;

/// The workspace root at or above `dir` and its packages.
fn workspaces(dir: &Path, top: &Path) -> Option<(PathBuf, Pkgs)> {
    let mut d = dir;
    let globs = loop {
        if let Some(g) = ws_globs(d) {
            break g;
        }
        if d == top || !d.starts_with(top) {
            return None;
        }
        d = d.parent()?;
    };
    let (mut found, mut gone, mut budget) = (Vec::new(), Vec::new(), 5000);
    for g in &globs {
        let (neg, g) = match g.strip_prefix('!') {
            Some(r) => (true, r),
            None => (false, g.as_str()),
        };
        let segs: Vec<&str> = g.trim_start_matches("./").trim_end_matches('/').split('/').collect();
        glob_dirs(d, &segs, if neg { &mut gone } else { &mut found }, &mut budget);
    }
    found.sort();
    found.dedup();
    let pkgs = found.into_iter().filter(|p| !gone.contains(p) && p.join("package.json").is_file()).map(|p| {
        let name = package_name(&p);
        (p, name)
    });
    Some((d.to_path_buf(), pkgs.collect()))
}

/// The packages a workspace selector picks: a directory (a package, or every
/// package under it) or a package name (`@scope/` optional). None: a glob, a
/// graph or exclusion selector, or nothing matches.
fn pick(sel: &str, cwd: &Path, root: &Path, pkgs: &Pkgs) -> Option<Vec<PathBuf>> {
    let s = sel.strip_prefix('{').and_then(|s| s.strip_suffix('}')).unwrap_or(sel);
    if s.is_empty() || has_glob(s) || s.contains("...") || s.starts_with(['!', '^']) {
        return None;
    }
    for base in [cwd, root] {
        let d = sys::normalize(&base.join(s));
        if d.join("package.json").is_file() && d.starts_with(root) {
            return Some(vec![d]);
        }
        let under: Vec<PathBuf> = pkgs.iter().filter(|(p, _)| d.is_dir() && p.starts_with(&d)).map(|(p, _)| p.clone()).collect();
        if !under.is_empty() {
            return Some(under);
        }
    }
    let named: Vec<PathBuf> = pkgs
        .iter()
        .filter(|(_, n)| n.as_deref().is_some_and(|n| n == s || n.strip_prefix('@').and_then(|n| n.split_once('/')).is_some_and(|(_, n)| n == s)))
        .map(|(p, _)| p.clone())
        .collect();
    (!named.is_empty()).then_some(named)
}

/// The manifests a workspace-targeted operation rewrites: each selected
/// package's (never the root's unless asked for); every package's for -r, a
/// glob or a selector that does not resolve. No workspaces: the nearest one.
fn ws_manifests(dir: &Path, top: &Path, op: &PkgOp) -> Vec<PathBuf> {
    let nearest = || manifest_for(dir, top, "package.json").into_iter().collect();
    let Some((root, pkgs)) = workspaces(dir, top) else { return nearest() };
    let mut out = Vec::new();
    if op.ws_root {
        out.push(root.clone());
    }
    let picked: Option<Vec<PathBuf>> = if op.all_ws { None } else { op.ws.iter().map(|s| pick(s, dir, &root, &pkgs)).collect::<Option<Vec<_>>>().map(|v| v.concat()) };
    match picked {
        Some(v) => out.extend(v),
        None => out.extend(pkgs.into_iter().map(|(p, _)| p)),
    }
    if out.is_empty() {
        return nearest();
    }
    out.into_iter().map(|d| d.join("package.json")).collect()
}

struct Pkg {
    repo: Repo,
    me: String,
    files: Vec<String>, // repo-relative manifests + lockfiles to claim
    what: String,
}

/// A file the run works on.
struct Took {
    path: String,
    by: String,    // whose claim the change counts for
    created: bool, // claimed by this run (dropped again if unchanged)
    orphan: bool,  // another agent's uncommitted leftovers: forgotten once the claim keeps a change
}

enum Take {
    Off,
    Nested,
    Busy(OpLock),
    Blocked(String, Held),
    Got { took: Vec<Took>, notes: Vec<String>, since: u64 },
}

const PKG_LOCK_MAX: u64 = 1200;

fn pkg_plan(name: &str, args: &[String]) -> Plan {
    if disabled() {
        return Plan::Exec;
    }
    let Some(op) = classify_pkg(name, args) else { return Plan::Exec };
    let Some((manifest, _)) = lockfiles::manager(name) else { return Plan::Exec };
    let Some(me) = ids::caller(None) else { return Plan::Exec };
    let cwd = sys::cwd();
    let dir = op.dir.as_deref().map(|d| sys::canonical(Path::new(d), &cwd)).unwrap_or(cwd);
    let mut existing = dir.as_path();
    while !existing.is_dir() {
        let Some(up) = existing.parent() else { return Plan::Exec };
        existing = up;
    }
    let Some(repo) = std::fs::canonicalize(existing).ok().and_then(|d| sys::find_repo(&d)) else { return Plan::Exec };
    let mut files: Vec<String> = Vec::new();
    if op.deps {
        let manifests = if op.ws.is_empty() && !op.all_ws && !op.ws_root {
            vec![manifest_for(&dir, &repo.top, manifest).unwrap_or_else(|| dir.join(manifest))]
        } else {
            ws_manifests(&dir, &repo.top, &op)
        };
        for m in manifests {
            let locks = lockfiles::lockfiles_for(&m, &repo.top, Some(name));
            for rel in std::iter::once(m).chain(locks).filter_map(|p| repo.rel(&p)) {
                let rel = sys::git_spelling(&repo, &rel);
                if !files.contains(&rel) {
                    files.push(rel);
                }
            }
        }
    }
    let what = cut(one_line(&std::iter::once(name.to_string()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ")), 100);
    Plan::Pkg(Pkg { repo, me, files, what })
}

fn held_by_other(l: &OpLock, now: u64, mine: Option<Owner>) -> bool {
    match l.owner {
        Some(o) => Some(o) != mine && sys::liveness(&o) == Live::Alive,
        None => now.saturating_sub(l.since) < PKG_LOCK_MAX,
    }
}

/// What a lifecycle script's own install finds in AGENT_LOCK_PKG_HELD.
fn held_mark(l: &OpLock) -> String {
    format!("{}:{}", l.since, l.by)
}

fn marker_of(o: Option<Owner>) -> String {
    rules::proc_marker("pkg", o.unwrap_or(Owner { pid: sys::pid(), start: 0 }))
}

impl Pkg {
    fn take(&self, st: &mut State, owner: Option<Owner>, claude: Option<Owner>, outer: Option<&str>, now: u64) -> Take {
        if st.off {
            return Take::Off;
        }
        if let Some(l) = &st.locks.pkg {
            if outer == Some(held_mark(l).as_str()) && l.owner.is_some_and(|o| sys::liveness(&o) == Live::Alive) {
                return Take::Nested; // a lifecycle script of the install we are inside
            }
            if held_by_other(l, now, owner) {
                return Take::Busy(l.clone());
            }
        }
        let before = st.clone();
        let marker = marker_of(owner);
        st.locks.pkg = Some(OpLock { by: self.me.clone(), owner, since: now, what: self.what.clone() });
        let ask = Ask {
            agent: self.me.clone(),
            session: st.session_of(&self.me).map(|s| s.id.clone()),
            owner: claude,
            pending: Some(marker.clone()),
            made_by: Some("pkg".into()),
            reason: None,
        };
        let ctx = Ctx::real(now);
        let (mut took, mut notes) = (Vec::new(), Vec::new());
        for f in &self.files {
            // A main id may be any agent of its family (a subagent's Bash runs
            // under it): a file the family holds stays with its holder.
            let family = st.claim(f).is_some_and(|c| {
                !ids::is_sub(&self.me)
                    && c.by != self.me
                    && ids::family(&c.by) == self.me
                    && rules::conflict(st, c, &self.me) == Conflict::Block
                    && c.pending.as_deref().is_none_or(|p| rules::marker(p) == Marker::Tool || rules::marker_dead(p))
            });
            if family {
                let c = st.claim_mut(f).expect("present");
                if c.pending.as_deref().is_none_or(rules::marker_dead) {
                    c.pending = Some(marker.clone());
                }
                took.push(Took { path: f.clone(), by: c.by.clone(), created: false, orphan: false });
                continue;
            }
            let (created, orphan) = match rules::acquire(st, f, &ask, &ctx) {
                Ok(Got::Created) => match st.orphans.get(&st.key(f)).filter(|o| o.from != self.me) {
                    Some(o) => {
                        notes.push(msg::orphan_note(f, &self.repo.top, &o.from));
                        (true, true)
                    }
                    None => (true, false),
                },
                Ok(Got::TookOver { from, why }) if from.edits > 0 => {
                    // Its uncommitted work stays in the file: remembered until committed.
                    let k = st.key(f);
                    st.orphans.insert(k, Orphan { path: f.clone(), from: from.by.clone(), at: now });
                    notes.push(msg::takeover_note(&self.repo.abs(f), f, &self.repo.top, &from.by, why.says(), from.edits, &self.me));
                    (false, false)
                }
                Ok(Got::TookOver { .. }) => (true, false),
                Ok(_) => (false, false),
                Err(h) => {
                    *st = before; // all or nothing: neither the lock nor any claim
                    return Take::Blocked(f.clone(), h);
                }
            };
            took.push(Took { path: f.clone(), by: self.me.clone(), created, orphan });
        }
        Take::Got { took, notes, since: now }
    }

    fn run<S: AsRef<OsStr>>(self, real: &Path, args: &[S]) -> i32 {
        let owner = sys::self_owner();
        let claude = sys::claude_owner();
        let outer = std::env::var("AGENT_LOCK_PKG_HELD").ok();
        let wait = std::env::var("AGENT_LOCK_PKG_WAIT").ok().and_then(|v| Duration::try_from_secs_f64(v.trim().parse().ok()?).ok());
        let deadline = Instant::now().checked_add(wait.unwrap_or(Duration::from_secs(PKG_LOCK_MAX)));
        let mut told = false;
        let (took, since) = loop {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                state::transact(&self.repo, |st| self.take(st, owner, claude, outer.as_deref(), sys::now()))
            }));
            match r {
                Ok(Ok(Take::Got { took, notes, since })) => {
                    for n in notes {
                        eprintln!("{n}");
                    }
                    break (took, since);
                }
                Ok(Ok(Take::Busy(l))) => {
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        eprintln!("agent-lock: gave up waiting for `{}` ({}) to finish; retry later.", l.what, l.by);
                        return 1;
                    }
                    if !told {
                        let pid = l.owner.map(|o| format!(", pid {}", o.pid)).unwrap_or_default();
                        eprintln!("agent-lock: waiting for `{}` ({}{pid}) to finish before `{}`…", l.what, l.by, self.what);
                        told = true;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                Ok(Ok(Take::Blocked(f, h))) => {
                    let abs = sys::quote(&self.repo.abs(&f).display().to_string());
                    let sub = if ids::is_sub(&self.me) { "" } else { " (a subagent adds --as <its id>)" };
                    eprintln!(
                        "agent-lock: `{}` rewrites {f}, which {} is editing (uncommitted). Prefer other work; to wait: agent-lock wait{} --claim {abs} --timeout 540 with run_in_background{sub}.",
                        self.what,
                        h.claim.by,
                        as_flag(&self.me)
                    );
                    return 1;
                }
                _ => return exec(real, args, 0), // off, nested, or any failure: run as asked
            }
        };
        let before: Vec<_> = took.iter().map(|t| sys::stamp(&self.repo.abs(&t.path))).collect();
        let mut cmd = Command::new(real);
        cmd.args(args).env("AGENT_LOCK_PKG_HELD", format!("{since}:{}", self.me)).env(HOP, format!("^{}:1", sys::pid()));
        let (mut holder, mut marker) = (owner, marker_of(owner));
        let code = match cmd.spawn() {
            Ok(mut child) => {
                forward_signals(child.id());
                // From here the lock and the markers follow the tool itself, so a
                // shim killed on its own frees nothing while the install runs.
                if let Some(o) = sys::owner_of(child.id()) {
                    let moved = rules::proc_marker("pkg", o);
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        state::transact(&self.repo, |st| {
                            let Some(l) = st.locks.pkg.as_mut().filter(|l| l.by == self.me && l.owner == owner) else { return false };
                            l.owner = Some(o);
                            for t in &took {
                                if let Some(c) = st.claim_mut(&t.path).filter(|c| c.pending.as_deref() == Some(marker.as_str())) {
                                    c.pending = Some(moved.clone());
                                }
                            }
                            true
                        })
                    }));
                    if matches!(r, Ok(Ok(true))) {
                        (holder, marker) = (Some(o), moved);
                    }
                }
                let status = child.wait();
                CHILD.store(0, Ordering::Relaxed); // reaped: its pid may be reused
                match status {
                    Ok(s) => s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)),
                    Err(_) => 1,
                }
            }
            Err(e) => {
                eprintln!("agent-lock: cannot run {}: {e}", real.display());
                127
            }
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state::transact(&self.repo, |st| {
                if st.locks.pkg.as_ref().is_some_and(|l| l.by == self.me && l.owner == holder) {
                    st.locks.pkg = None;
                }
                let now = sys::now();
                for (t, stamp) in took.iter().zip(&before) {
                    let changed = sys::stamp(&self.repo.abs(&t.path)) != *stamp;
                    let Some(c) = st.claim_mut(&t.path).filter(|c| c.holds(&t.by)) else { continue };
                    if c.pending.as_deref() == Some(marker.as_str()) {
                        c.pending = None;
                    }
                    if changed {
                        c.edits += 1;
                        c.touched = now;
                        if t.orphan {
                            let k = st.key(&t.path);
                            st.orphans.remove(&k); // the note was shown; the change is the claim's now
                        }
                    } else if t.created && c.edits == 0 {
                        st.take_claim(&t.path);
                    }
                }
            })
        }));
        code
    }
}

static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(sig: i32) {
    let pid = CHILD.load(Ordering::Relaxed);
    if pid > 0 {
        sys::signal_send(pid, sig);
    }
}

/// Like system(3), ^C and ^\ reach the tool through the process group and the
/// shim settles after it. A TERM, HUP or ALRM aimed at the shim alone is
/// passed on: the tool stops, and the shim still releases what it holds.
fn forward_signals(child: u32) {
    CHILD.store(child as i32, Ordering::Relaxed);
    sys::signal_ignore(2); // SIGINT
    sys::signal_ignore(3); // SIGQUIT
    for sig in [1, 14, 15] {
        sys::signal_catch(sig, forward); // SIGHUP, SIGALRM, SIGTERM
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    /// Classify with a fake worktree: `files` exist, names ending in '/' are dirs.
    fn k(cmd: &str, files: &[&str]) -> Kind {
        let a = v(cmd);
        let probe = |t: &str| {
            let t = t.trim_end_matches('/');
            files.iter().find(|f| f.trim_end_matches('/') == t).map(|f| f.ends_with('/'))
        };
        classify(&a[0], &a[1..], &probe)
    }

    fn sp(v: &[&str]) -> Kind {
        Kind::Scoped(Scope::Specs(v.iter().map(|s| s.to_string()).collect()))
    }

    use Kind::*;

    #[test]
    fn global_options() {
        let c = parse_call(&v("-C a -C b -c alias.x=status --no-pager --literal-pathspecs add -A")).unwrap();
        assert_eq!(c.dir, PathBuf::from("a/b"));
        assert_eq!(c.config, vec![("alias.x".to_string(), "status".to_string())]);
        assert!(c.magic.literal);
        assert_eq!(c.sub.as_deref(), Some("add"));
        assert_eq!(c.args, v("-A"));
        let c = parse_call(&v("-C /abs --git-dir=.git --work-tree w status")).unwrap();
        assert_eq!(c.dir, PathBuf::from("/abs"));
        assert_eq!(c.git_dir, Some(PathBuf::from("/abs/.git")));
        assert_eq!(c.work_tree, Some(PathBuf::from("/abs/w")));
        assert_eq!(parse_call(&v("--version")).unwrap().sub, None);
        assert_eq!(parse_call(&v("--exec-path")).unwrap().sub, None);
        assert_eq!(parse_call(&v("--exec-path=/x log")).unwrap().sub.as_deref(), Some("log"));
        assert!(parse_call(&v("--bogus add")).is_none(), "git rejects it");
        assert!(parse_call(&v("-C")).is_none());
        assert_eq!(parse_call(&[String::new(), "add".into()]).unwrap().sub.as_deref(), Some(""));
    }

    #[test]
    fn add_rules() {
        assert_eq!(k("add -A", &[]), Scoped(Scope::Repo));
        assert_eq!(k("add --all", &[]), Scoped(Scope::Repo));
        assert_eq!(k("add -u", &[]), Scoped(Scope::Repo));
        assert_eq!(k("add --up", &[]), Scoped(Scope::Repo), "unique prefix of --update");
        assert_eq!(k("add -A -- own.txt", &[]), sp(&["own.txt"]));
        assert_eq!(k("add .", &[]), sp(&["."]));
        assert_eq!(k("add -n .", &[]), Safe);
        assert_eq!(k("add", &[]), Safe);
        assert_eq!(k("add -h", &[]), Safe);
        assert_eq!(k("stage :/", &[]), sp(&[":/"]));
        assert_eq!(k("add --pathspec-from-file=f", &[]), Scoped(Scope::Repo));
    }

    #[test]
    fn commit_rules() {
        let c = |cmd: &str| match k(cmd, &[]) {
            Commit(c) => c,
            other => panic!("{cmd}: {other:?}"),
        };
        assert!(c("commit -m x").whole);
        assert!(c("commit -am x").whole, "combined short flags");
        assert_eq!(c("commit -am x").msgs, vec![(false, "x".to_string())]);
        assert!(c("commit -amx").whole);
        assert_eq!(c("commit -amx").msgs, vec![(false, "x".to_string())]);
        assert!(c("commit -i -m x -- a").whole);
        assert!(c("commit --amend --no-edit -- a").amend);
        let p = c("commit -m x -- own.txt");
        assert!(!p.whole && !p.amend);
        assert_eq!(p.scope, Scope::Specs(v("own.txt")));
        assert!(!c("commit -m -a own.txt").whole, "-a is the message here");
        assert_eq!(c("commit -F - own.txt").msgs, vec![(true, "-".to_string())]);
        assert_eq!(c("commit --pathspec-from-file=f -m x").scope, Scope::Repo);
        assert_eq!(k("commit --dry-run", &[]), Safe);
        assert!(matches!(k("commit -m -h", &[]), Commit(_)), "-h consumed as the message is not help");
        assert_eq!(k("commit -h", &[]), Safe);
    }

    #[test]
    fn stash_reset_rm_mv() {
        assert_eq!(k("stash", &[]), Sweep(Hint::Commit));
        assert_eq!(k("stash -m wip", &[]), Sweep(Hint::Commit));
        assert_eq!(k("stash pop", &[]), Sweep(Hint::Stash(0)));
        assert_eq!(k("stash apply --index stash@{2}", &[]), Sweep(Hint::Stash(2)));
        assert_eq!(k("stash drop 1", &[]), Sweep(Hint::Stash(1)));
        assert_eq!(k("stash list", &[]), Safe);
        assert_eq!(k("stash show -p", &[]), Safe);
        assert_eq!(k("stash push -m w own.txt", &[]), sp(&["own.txt"]));
        assert_eq!(k("stash -- own.txt", &[]), sp(&["own.txt"]));
        assert_eq!(k("stash push --staged own.txt", &[]), Sweep(Hint::Commit));
        assert_eq!(k("reset --hard", &[]), Sweep(Hint::Commit));
        assert_eq!(k("reset --keep HEAD~1", &[]), Sweep(Hint::Commit));
        assert_eq!(k("reset", &[]), Safe);
        assert_eq!(k("reset -q", &[]), Safe);
        assert_eq!(k("reset HEAD", &[]), Safe);
        assert_eq!(k("reset HEAD~1", &[]), Sweep(Hint::Reset));
        assert_eq!(k("reset --soft abc1234", &[]), Sweep(Hint::Reset));
        assert_eq!(k("reset origin/main", &[]), Sweep(Hint::Reset));
        assert_eq!(k("reset main", &[]), Sweep(Hint::Reset), "a branch, not a path");
        assert_eq!(k("reset own.txt", &["own.txt"]), Safe, "an existing path is a path");
        assert_eq!(k("reset HEAD~1 -- own.txt", &[]), Safe);
        assert_eq!(k("reset HEAD own.txt", &[]), Safe);
        assert_eq!(k("rm --cached a.txt", &[]), sp(&["a.txt"]));
        assert_eq!(k("rm -n a.txt", &[]), Safe);
        assert_eq!(k("mv a.txt b.txt", &[]), sp(&["a.txt", "b.txt"]));
        assert_eq!(k("mv a.txt src", &["src/"]), sp(&["a.txt", "src/a.txt"]));
    }

    #[test]
    fn checkout_restore_clean() {
        assert_eq!(k("checkout -- .", &[]), sp(&["."]));
        assert_eq!(k("checkout .", &[]), sp(&["."]));
        assert_eq!(k("checkout own.txt", &["own.txt"]), sp(&["own.txt"]));
        assert_eq!(k("checkout main", &[]), Sweep(Hint::Branch));
        assert_eq!(k("checkout main -- a.txt", &[]), sp(&["a.txt"]));
        assert_eq!(k("checkout main a.txt", &[]), sp(&["a.txt"]));
        assert_eq!(k("checkout main --", &[]), Sweep(Hint::Branch));
        assert_eq!(k("checkout --", &[]), Safe);
        assert_eq!(k("checkout -b x", &[]), Sweep(Hint::Branch));
        assert_eq!(k("checkout -qb x", &[]), Sweep(Hint::Branch), "combined short flags");
        assert_eq!(k("checkout --orphan x", &[]), Sweep(Hint::Branch));
        assert_eq!(k("checkout --detach", &[]), Sweep(Hint::Branch));
        assert_eq!(k("checkout -", &[]), Sweep(Hint::Branch));
        assert_eq!(k("checkout -f", &[]), Sweep(Hint::Commit));
        assert_eq!(k("checkout", &[]), Safe);
        assert_eq!(k("checkout '*.ts'", &[]), sp(&["'*.ts'"]));
        assert_eq!(k("restore .", &[]), sp(&["."]));
        assert_eq!(k("restore --staged .", &[]), Safe);
        assert_eq!(k("restore -SW .", &[]), sp(&["."]));
        assert_eq!(k("restore -s HEAD~1 a.txt", &[]), sp(&["a.txt"]));
        assert_eq!(k("clean -n", &[]), Safe);
        assert_eq!(k("clean -d", &[]), Safe, "clean.requireForce");
        assert_eq!(k("clean -fd", &[]), Clean(Scope::Specs(v("."))));
        assert_eq!(k("clean -f -e x src", &[]), Clean(Scope::Specs(v("src"))));
    }

    #[test]
    fn history_and_recovery() {
        for s in ["pull", "pull --rebase", "rebase main", "rebase --abort", "rebase --skip", "merge x", "cherry-pick abc", "revert abc", "am p.patch", "merge -m --abort"] {
            assert_eq!(k(s, &[]), Sweep(Hint::Sync), "{s}");
        }
        for s in ["merge --abort", "merge --quit", "rebase --quit", "rebase --continue", "cherry-pick --abort", "cherry-pick --continue", "revert --quit", "am --abort", "pull --help"] {
            assert_eq!(k(s, &[]), Safe, "{s}");
        }
        assert_eq!(k("switch x", &[]), Sweep(Hint::Branch));
        assert_eq!(k("bisect start", &[]), Sweep(Hint::Commit));
        assert_eq!(k("bisect log", &[]), Safe);
        assert_eq!(k("sparse-checkout list", &[]), Safe);
        assert_eq!(k("sparse-checkout set a", &[]), Sweep(Hint::Commit));
        assert_eq!(k("read-tree -mu HEAD", &[]), Sweep(Hint::Commit));
        assert_eq!(k("read-tree HEAD", &[]), Safe);
        assert_eq!(k("checkout-index -a", &[]), Sweep(Hint::Commit));
        assert_eq!(k("status", &[]), Safe);
        assert_eq!(k("log", &[]), Safe);
    }

    fn sp1(t: &str, prefix: &str) -> Spec {
        spec(t, prefix, Path::new("/r"), Magic::default()).unwrap()
    }

    #[test]
    fn pathspec_magic() {
        assert_eq!(sp1(".", "sub").pat.as_deref(), Some("sub"));
        assert_eq!(sp1("..", "sub").pat.as_deref(), Some(""));
        assert_eq!(sp1("../..", "sub").pat, None, "outside the worktree");
        assert_eq!(sp1(":/", "sub").pat.as_deref(), Some(""));
        assert_eq!(sp1(":/a.txt", "sub").pat.as_deref(), Some("a.txt"));
        assert_eq!(sp1(":(top)a", "sub").pat.as_deref(), Some("a"));
        let x = sp1(":!b.txt", "");
        assert!(x.exclude && x.pat.as_deref() == Some("b.txt"));
        assert!(sp1(":^b", "").exclude);
        assert!(sp1(":(exclude,top)b", "s").exclude);
        assert!(sp1("*.ts", "").glob);
        assert!(!sp1(":(literal)*.ts", "").glob);
        assert!(sp1(":(glob)src/*", "").pathname);
        assert!(sp1(":(icase)A", "").icase);
        assert!(spec(":(attr:x)a", "", Path::new("/r"), Magic::default()).is_none());
        let lit = spec(":(exclude)a", "", Path::new("/r"), Magic { literal: true, ..Magic::default() }).unwrap();
        assert!(!lit.exclude && lit.pat.as_deref() == Some(":(exclude)a"), "--literal-pathspecs disables magic");
        assert!(!spec("*.ts", "", Path::new("/r"), Magic { noglob: true, ..Magic::default() }).unwrap().glob);
        assert_eq!(sp1("dir/", "").pat.as_deref(), Some("dir"));
    }

    #[test]
    fn reach() {
        let r = |t: &str, p: &str| reaches(&sp1(t, ""), p, false);
        assert!(r(".", "a/b.txt"));
        assert!(r("a", "a/b.txt"));
        assert!(!r("a", "ab/c.txt"));
        assert!(r("a/b.txt", "a/b.txt"));
        assert!(r("*.txt", "a/b.txt"), "git's * crosses /");
        assert!(!r(":(glob)*.txt", "a/b.txt"));
        assert!(r(":(glob)**/*.txt", "a/b.txt"));
        assert!(r(":(glob)a/*", "a/b/c.txt"), "a matched directory covers its files");
        assert!(r("a/[bc].txt", "a/c.txt"));
        assert!(!r("a/[!bc].txt", "a/c.txt"));
        assert!(r("a?b", "a/b"), "? crosses / without :(glob)");
        assert!(!r(":(literal)*.txt", "a.txt"));
        assert!(r(":(literal)*.txt", "*.txt"));
        assert!(reaches(&sp1("A.TXT", ""), "a.txt", true), "core.ignorecase");
        assert!(reaches(&sp1(":(icase)A.TXT", ""), "a.txt", false));
        let mut budget = 100_000;
        assert!(wild(b"*a*a*a*a*a*a*a*a*b", &[b'a'; 60], false, &mut budget) || budget == 0);
    }

    #[test]
    fn alias_words() {
        assert_eq!(split_cmdline("checkout -q").unwrap(), v("checkout -q"));
        assert_eq!(split_cmdline(r#"commit -m "a b" 'c d' e\ f"#).unwrap(), vec!["commit", "-m", "a b", "c d", "e f"]);
        assert_eq!(split_cmdline("log ''").unwrap(), vec!["log", ""]);
        assert!(split_cmdline("x 'open").is_none());
    }

    #[test]
    fn bang_alias_commands() {
        let cmds = shell_commands("git add -A && git commit -m 'wip it'; echo done", &[]);
        assert_eq!(cmds, vec![v("git add -A"), vec!["git".into(), "commit".into(), "-m".into(), "wip it".into()], v("echo done")]);
        let cmds = shell_commands(r#"f() { git checkout "$@"; }; f"#, &v("-- ."));
        assert_eq!(cmds[1], v("git checkout -- ."), "$@ is the alias's arguments");
        assert_eq!(cmds.last().unwrap(), &v("f -- ."), "git appends them too");
        assert_eq!(shell_commands("git checkout $1", &v("main"))[0], v("git checkout main main"));
        assert_eq!(shell_commands("git checkout $BRANCH", &[])[0], v("git checkout :/"), "unknown words cover the tree");
        assert_eq!(shell_commands("echo '$x'", &[])[0], v("echo $x"));
        assert_eq!(git_argv(&v("GIT_EDITOR=true command /usr/bin/git add .")).unwrap(), &v("add .")[..]);
        assert!(git_argv(&v("echo git")).is_none());
        assert_eq!(git_argv(&v("then git stash")).unwrap(), &v("stash")[..], "shell keywords");
        assert_eq!(git_argv(&v("do git checkout -- :/")).unwrap(), &v("checkout -- :/")[..]);
        // the fallback for what is not a plain git call
        let m = |s: &str| mentions_checked(&shell_commands(s, &[])[0]);
        assert!(m("sh -c 'git checkout -- .'"));
        assert!(m("bash -c \"git -C x reset --hard\""));
        assert!(m("xargs -0 git checkout --"));
        assert!(m("eval git stash"));
        assert!(m("sh -c 'git \"$@\"'"), "a git subcommand it cannot know");
        assert!(m("sh -c 'git co .'"), "maybe an alias");
        assert!(!m("sh -c 'git status && git log -1'"));
        assert!(!m("echo git"));
        assert!(!m("for f in a b"));
    }

    #[test]
    fn pkg_classes() {
        let p = |m: &str, a: &str| classify_pkg(m, &v(a));
        let d = |m: &str, a: &str| p(m, a).map(|o| o.deps);
        assert_eq!(d("npm", "install x"), Some(true));
        assert_eq!(d("npm", "i -D x"), Some(true));
        assert_eq!(d("npm", "add x"), Some(true));
        assert_eq!(d("npm", "install"), Some(false));
        assert_eq!(d("npm", "install --no-save x"), Some(false));
        assert_eq!(d("npm", "install --package-lock-only"), Some(true));
        assert_eq!(d("npm", "ci"), Some(false));
        assert_eq!(d("npm", "rm x"), Some(true));
        assert_eq!(d("npm", "update"), Some(true));
        assert_eq!(d("npm", "audit fix"), Some(true));
        assert_eq!(p("npm", "audit"), None);
        assert_eq!(p("npm", "run test"), None);
        assert_eq!(p("npm", "install -g x"), None);
        assert_eq!(p("npm", "install --location=global x"), None);
        assert_eq!(p("npm", "--prefix sub install x").unwrap().dir.as_deref(), Some("sub"));
        let o = p("npm", "install -w ./pkgs/a --workspace=web x").unwrap();
        assert_eq!((o.dir, o.ws), (None, v("./pkgs/a web")));
        assert!(p("npm", "update --workspaces --include-workspace-root").is_some_and(|o| o.all_ws && o.ws_root));
        assert_eq!(d("pnpm", ""), Some(false), "bare pnpm = install");
        assert_eq!(d("pnpm", "add x"), Some(true));
        assert_eq!(d("pnpm", "i"), Some(false));
        assert_eq!(p("pnpm", "--filter ./a add x -F web").unwrap().ws, v("./a web"));
        assert!(p("pnpm", "-r update").is_some_and(|o| o.all_ws && o.deps));
        assert!(p("pnpm", "add -w x").is_some_and(|o| o.ws_root));
        assert_eq!(p("pnpm", "exec tsc"), None);
        assert_eq!(d("yarn", ""), Some(false), "bare yarn installs");
        assert_eq!(p("yarn", "--version"), None);
        assert_eq!(d("yarn", "add x"), Some(true));
        assert_eq!(p("yarn", "workspace web add x").map(|o| (o.deps, o.ws)), Some((true, v("web"))));
        assert_eq!(p("yarn", "build"), None);
        assert_eq!(d("bun", "add x"), Some(true));
        assert_eq!(d("bun", "install"), Some(false));
        assert_eq!(p("bun", "run x"), None);
        assert_eq!(p("bun", "add x --filter web").unwrap().ws, v("web"));
        // v2's other managers: dependency verbs claim, installs only lock, the rest runs straight through
        for (m, a, deps) in [
            ("cargo", "add serde", Some(true)),
            ("cargo", "+nightly update -p x", Some(true)),
            ("cargo", "rm x", Some(true)),
            ("cargo", "build --release", None),
            ("cargo", "test", None),
            ("uv", "add requests", Some(true)),
            ("uv", "lock", Some(true)),
            ("uv", "sync", Some(false)),
            ("uv", "pip install x", Some(false)),
            ("uv", "run pytest", None),
            ("poetry", "add x", Some(true)),
            ("poetry", "install", Some(false)),
            ("poetry", "run x", None),
            ("go", "get example.com/x", Some(true)),
            ("go", "mod tidy", Some(true)),
            ("go", "mod vendor", Some(false)),
            ("go", "build ./...", None),
            ("go", "mod download", None),
            ("bundle", "", Some(false)),
            ("bundle", "add rails", Some(true)),
            ("bundler", "install", Some(false)),
            ("bundle", "exec rake", None),
            ("bundle", "--version", None),
        ] {
            assert_eq!(d(m, a), deps, "{m} {a}");
        }
        assert_eq!(p("cargo", "add x --manifest-path crates/y/Cargo.toml").unwrap().dir.as_deref(), Some("crates/y"));
        assert_eq!(p("cargo", "update --manifest-path Cargo.toml").unwrap().dir.as_deref(), Some("."));
        assert_eq!(p("go", "get -C sub x").unwrap().dir.as_deref(), Some("sub"));
        assert_eq!(p("uv", "add --directory py x").unwrap().dir.as_deref(), Some("py"));
        assert_eq!(p("bundle", "add x --gemfile app/Gemfile").unwrap().dir.as_deref(), Some("app"));
        for n in ["cargo", "uv", "poetry", "go", "bundle", "bundler"] {
            assert!(handles(n) && lockfiles::manager(n).is_some(), "{n}");
        }
    }

    #[test]
    fn manifests_and_workspaces() {
        let base = std::env::temp_dir().join(format!("al-shim-{}-{}", sys::pid(), sys::now_ms()));
        let top = base.join("r");
        for d in ["pkgs/a/src", "pkgs/b", "pkgs/.hidden", "apps/web", "apps/deep/x/y", "tools"] {
            std::fs::create_dir_all(top.join(d)).unwrap();
        }
        let w = |rel: &str, body: &str| std::fs::write(top.join(rel), body).unwrap();
        w("package.json", r#"{"name":"root","workspaces":["pkgs/*","apps/**","!apps/deep/x"]}"#);
        w("pkgs/a/package.json", r#"{"name":"@acme/a"}"#);
        w("pkgs/b/package.json", r#"{"name":"b"}"#);
        w("pkgs/.hidden/package.json", "{}");
        w("apps/web/package.json", r#"{"name":"@acme/web"}"#);
        w("apps/deep/x/y/package.json", r#"{"name":"y"}"#);
        w("tools/package.json", r#"{"name":"tools"}"#);
        assert_eq!(manifest_for(&top.join("pkgs/a/src"), &top, "package.json").unwrap(), top.join("pkgs/a/package.json"));
        assert_eq!(manifest_for(&base, &top, "package.json"), None, "not above the top");
        let (root, pkgs) = workspaces(&top.join("pkgs/a/src"), &top).unwrap();
        assert_eq!(root, top);
        let dirs: Vec<PathBuf> = pkgs.iter().map(|(p, _)| p.strip_prefix(&top).unwrap().to_path_buf()).collect();
        assert_eq!(dirs, ["apps/deep/x/y", "apps/web", "pkgs/a", "pkgs/b"].map(PathBuf::from), "globs, dot dirs skipped; only the exact exclusion");
        let m = |op: PkgOp, cwd: &str| -> Vec<String> {
            ws_manifests(&top.join(cwd), &top, &op).iter().map(|p| p.strip_prefix(&top).unwrap().display().to_string()).collect()
        };
        let sel = |v: &[&str]| PkgOp { deps: true, ws: v.iter().map(|s| s.to_string()).collect(), ..PkgOp::default() };
        assert_eq!(m(sel(&["pkgs/b"]), ""), ["pkgs/b/package.json"], "npm -w <path>");
        assert_eq!(m(sel(&["./b"]), "pkgs"), ["pkgs/b/package.json"], "relative to the cwd");
        assert_eq!(m(sel(&["{./pkgs/a}"]), ""), ["pkgs/a/package.json"], "pnpm braces");
        assert_eq!(m(sel(&["@acme/web"]), ""), ["apps/web/package.json"], "by name");
        assert_eq!(m(sel(&["web", "b"]), ""), ["apps/web/package.json", "pkgs/b/package.json"], "the scope is optional");
        assert_eq!(m(sel(&["pkgs"]), ""), ["pkgs/a/package.json", "pkgs/b/package.json"], "npm: a parent directory selects its packages");
        let every = ["apps/deep/x/y/package.json", "apps/web/package.json", "pkgs/a/package.json", "pkgs/b/package.json"];
        assert_eq!(m(sel(&["@acme/*"]), ""), every, "a glob: all of them");
        assert_eq!(m(sel(&["web..."]), ""), every, "a graph selector: all of them");
        assert_eq!(m(sel(&["nope"]), ""), every, "unresolvable: all of them");
        assert_eq!(m(PkgOp { all_ws: true, ..PkgOp::default() }, "pkgs/a"), every, "-r");
        assert_eq!(m(PkgOp { ws_root: true, ..PkgOp::default() }, "pkgs/a"), ["package.json"], "pnpm -w");
        w("pnpm-workspace.yaml", "# ws\npackages:\n  - 'pkgs/*'   # libs\n  - \"tools\"\ncatalog:\n  - x\n");
        assert_eq!(m(PkgOp { all_ws: true, ..PkgOp::default() }, ""), ["pkgs/a/package.json", "pkgs/b/package.json", "tools/package.json"], "pnpm-workspace.yaml wins");
        assert_eq!(yaml_packages("packages: ['a/*', \"b\"]\n"), ["a/*", "b"]);
        assert_eq!(yaml_packages("packages:\n- a\n- b/**\nother: 1\n"), ["a", "b/**"]);
        std::fs::remove_file(top.join("pnpm-workspace.yaml")).unwrap();
        w("package.json", "{}");
        assert_eq!(m(sel(&["b"]), "pkgs/a/src"), ["pkgs/a/package.json"], "no workspaces: the nearest manifest");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn messages_are_one_line() {
        let c = |p: &str, by: &str| Claim { path: p.into(), by: by.into(), ..Claim::default() };
        let cs = [c("a\nb", "x"), c("b", "y"), c("c", "z"), c("d", "w")];
        let refs: Vec<&Claim> = cs.iter().collect();
        assert_eq!(list(&refs), "a b by x, b by y, c by z and 1 more");
        assert_eq!(sys::quote("it's"), "'it'\\''s'");
        assert_eq!(sys::quote("a.txt"), "a.txt");
        assert_eq!(label("commit", &v("-m x")), "git commit -m x");
        assert!(label("add", &["x".repeat(100)]).chars().count() <= 60);
    }
}
