//! Shared helpers for black-box tests: throwaway git repos, a hermetic
//! environment, and runners for the real binary as a hook, a CLI and a shim.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

pub const BIN: &str = env!("CARGO_BIN_EXE_agent-lock");

pub const SA: &str = "aaaaaaaa-1111-4111-8111-111111111111";
pub const SB: &str = "bbbbbbbb-2222-4222-8222-222222222222";
pub const SC: &str = "cccccccc-3333-4333-8333-333333333333";

static N: AtomicUsize = AtomicUsize::new(0);

pub fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("al3-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(p).unwrap()
}

/// A pid that is certainly dead (a finished child).
pub fn dead_pid() -> u32 {
    let mut c = Command::new("true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

pub struct Repo {
    pub base: PathBuf,
    pub dir: PathBuf,
    pub git_dir: PathBuf,
    pub origin: Option<PathBuf>,
    pub home: PathBuf,
    pub data: PathBuf,
}

/// Environment for every child: nothing inherited but PATH, plus hermetic git.
pub fn env_for(r: &Repo) -> Vec<(String, String)> {
    let path = std::env::var("PATH").unwrap_or_default();
    vec![
        ("PATH".into(), path),
        ("HOME".into(), r.home.display().to_string()),
        ("TMPDIR".into(), std::env::temp_dir().display().to_string()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_AUTHOR_NAME".into(), "Test".into()),
        ("GIT_AUTHOR_EMAIL".into(), "test@example.com".into()),
        ("GIT_COMMITTER_NAME".into(), "Test".into()),
        ("GIT_COMMITTER_EMAIL".into(), "test@example.com".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GIT_EDITOR".into(), "true".into()),
        ("CLAUDE_PLUGIN_DATA".into(), r.data.display().to_string()),
    ]
}

pub fn git(r: &Repo, cwd: &Path, args: &[&str]) -> String {
    let o = Command::new("git").args(args).current_dir(cwd).env_clear().envs(env_for(r)).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Make no file shared in `r`, so another agent's claim on a manifest blocks
/// (the repo's `.agent-lock-shared`, kept out of git status).
pub fn no_shared(r: &Repo) {
    std::fs::write(r.dir.join(".agent-lock-shared"), "!*\n").unwrap();
    let ex = r.git_dir.join("info/exclude");
    std::fs::create_dir_all(ex.parent().unwrap()).unwrap();
    let mut t = std::fs::read_to_string(&ex).unwrap_or_default();
    t.push_str("/.agent-lock-shared\n");
    std::fs::write(ex, t).unwrap();
}

pub fn write(dir: &Path, rel: &str, content: &str) -> PathBuf {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, content).unwrap();
    p
}

pub fn make_repo(remote: bool, files: &[(&str, &str)]) -> Repo {
    let base = tmp("repo");
    let dir = base.join("work");
    std::fs::create_dir_all(&dir).unwrap();
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let data = base.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mut r = Repo { git_dir: dir.join(".git"), dir: dir.clone(), base: base.clone(), origin: None, home, data };
    git(&r, &dir, &["init", "-q", "-b", "main"]);
    let files: Vec<(&str, &str)> = if files.is_empty() { vec![("README.md", "hello\n")] } else { files.to_vec() };
    for (f, c) in files {
        write(&dir, f, c);
    }
    git(&r, &dir, &["add", "-A"]);
    git(&r, &dir, &["commit", "-q", "-m", "init"]);
    if remote {
        let origin = base.join("origin.git");
        git(&r, &base, &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
        git(&r, &dir, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&r, &dir, &["push", "-q", "-u", "origin", "main"]);
        r.origin = Some(origin);
    }
    r
}

/// Another checkout of origin ("another machine").
pub fn clone(r: &Repo, name: &str) -> PathBuf {
    let c = r.base.join(name);
    git(r, &r.base, &["clone", "-q", "-b", "main", r.origin.as_ref().unwrap().to_str().unwrap(), c.to_str().unwrap()]);
    c
}

pub struct Run {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn out(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
    /// Parsed stdout JSON (hooks), or Null.
    pub fn json(&self) -> serde_free::Json {
        serde_free::parse(&self.stdout)
    }
    pub fn decision(&self) -> Option<String> {
        let j = self.json();
        j.get("hookSpecificOutput").get("permissionDecision").as_str().map(str::to_string)
            .or_else(|| j.get("decision").as_str().map(str::to_string))
    }
    pub fn reason(&self) -> String {
        let j = self.json();
        j.get("hookSpecificOutput").get("permissionDecisionReason").as_str()
            .or_else(|| j.get("reason").as_str())
            .unwrap_or("").to_string()
    }
    pub fn context(&self) -> String {
        self.json().get("hookSpecificOutput").get("additionalContext").as_str().unwrap_or("").to_string()
    }
}

pub fn run_bin(r: &Repo, argv0: Option<&Path>, args: &[&str], cwd: &Path, extra: &[(&str, String)], input: &str) -> Run {
    let exe: PathBuf = argv0.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(BIN));
    let mut cmd = Command::new(exe);
    cmd.args(args).current_dir(cwd).env_clear().envs(env_for(r));
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    Run { status: o.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&o.stdout).into(), stderr: String::from_utf8_lossy(&o.stderr).into() }
}

/// Run `agent-lock hook <event>` with a JSON payload; owner = `pid` ($CLAUDE_PID).
pub fn hook(r: &Repo, event: &str, payload: &str, pid: u32) -> Run {
    run_bin(r, None, &["hook", event], &r.dir, &[("CLAUDE_PID", pid.to_string())], payload)
}

/// Run the CLI as the main agent of `session` (or no session when None).
pub fn cli(r: &Repo, cwd: &Path, args: &[&str], session: Option<&str>, pid: u32) -> Run {
    let mut env = vec![("CLAUDE_PID", pid.to_string())];
    if let Some(s) = session {
        env.push(("CLAUDE_CODE_SESSION_ID", s.to_string()));
    }
    run_bin(r, None, args, cwd, &env, "")
}

/// Payload builder: `{"session_id":…, "cwd":…, …extra}` as JSON text.
pub fn payload(r: &Repo, session: &str, extra: &str) -> String {
    let cwd = r.dir.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    if extra.is_empty() {
        format!(r#"{{"session_id":"{session}","cwd":"{cwd}","transcript_path":"/dev/null"}}"#)
    } else {
        format!(r#"{{"session_id":"{session}","cwd":"{cwd}","transcript_path":"/dev/null",{extra}}}"#)
    }
}

/// `"tool_name":"Edit","tool_input":{"file_path":…},"tool_use_id":…`
pub fn edit(path: &Path, tool_use_id: &str) -> String {
    format!(r#""hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{{"file_path":"{}","old_string":"a","new_string":"b"}},"tool_use_id":"{tool_use_id}""#, path.display())
}

pub fn bash(command: &str, tool_use_id: &str) -> String {
    let c = command.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
    format!(r#""hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{{"command":"{c}"}},"tool_use_id":"{tool_use_id}""#)
}

pub fn state(r: &Repo) -> serde_free::Json {
    serde_free::parse(&std::fs::read_to_string(r.git_dir.join("agent-lock.json")).unwrap_or_default())
}

/// The agent holding `rel`, from the state file.
pub fn holder(r: &Repo, rel: &str) -> Option<String> {
    state(r).get("claims").get(rel).get("by").as_str().map(str::to_string)
}

/// A tiny JSON reader for assertions (tests can't use the crate's private modules).
pub mod serde_free {
    #[derive(Clone, Debug, PartialEq)]
    pub enum Json { Null, Bool(bool), Num(f64), Str(String), Arr(Vec<Json>), Obj(Vec<(String, Json)>) }
    static NULL: Json = Json::Null;
    impl Json {
        pub fn get(&self, k: &str) -> &Json {
            match self { Json::Obj(m) => m.iter().rev().find(|(x, _)| x == k).map(|(_, v)| v).unwrap_or(&NULL), _ => &NULL }
        }
        pub fn as_str(&self) -> Option<&str> { if let Json::Str(s) = self { Some(s) } else { None } }
        pub fn as_u64(&self) -> Option<u64> { if let Json::Num(n) = self { Some(*n as u64) } else { None } }
        pub fn as_bool(&self) -> Option<bool> { if let Json::Bool(b) = self { Some(*b) } else { None } }
        pub fn arr(&self) -> &[Json] { if let Json::Arr(a) = self { a } else { &[] } }
        pub fn keys(&self) -> Vec<String> { if let Json::Obj(m) = self { m.iter().map(|(k, _)| k.clone()).collect() } else { vec![] } }
        pub fn is_null(&self) -> bool { matches!(self, Json::Null) }
    }
    pub fn parse(s: &str) -> Json {
        let b = s.trim().as_bytes();
        if b.is_empty() { return Json::Null; }
        let mut i = 0;
        val(b, &mut i).unwrap_or(Json::Null)
    }
    /// The whole input must be one JSON value (surrounding whitespace aside).
    pub fn strict(s: &str) -> Option<Json> {
        let b = s.as_bytes();
        let mut i = 0;
        let v = val(b, &mut i)?;
        ws(b, &mut i);
        (i == b.len()).then_some(v)
    }
    fn ws(b: &[u8], i: &mut usize) { while *i < b.len() && (b[*i] as char).is_whitespace() { *i += 1; } }
    fn at(b: &[u8], i: usize) -> Option<u8> { b.get(i).copied() }
    fn lit(b: &[u8], i: &mut usize, word: &[u8], v: Json) -> Option<Json> {
        if b.get(*i..*i + word.len())? != word { return None; }
        *i += word.len();
        Some(v)
    }
    fn val(b: &[u8], i: &mut usize) -> Option<Json> {
        ws(b, i);
        match at(b, *i)? {
            b'{' => { *i += 1; let mut m = vec![]; ws(b, i); if at(b, *i)? == b'}' { *i += 1; return Some(Json::Obj(m)); }
                loop { ws(b, i); let k = if let Json::Str(s) = val(b, i)? { s } else { return None }; ws(b, i);
                    if at(b, *i)? != b':' { return None; } *i += 1; let v = val(b, i)?; m.push((k, v)); ws(b, i);
                    match at(b, *i)? { b',' => *i += 1, b'}' => { *i += 1; return Some(Json::Obj(m)); } _ => return None } } }
            b'[' => { *i += 1; let mut a = vec![]; ws(b, i); if at(b, *i)? == b']' { *i += 1; return Some(Json::Arr(a)); }
                loop { a.push(val(b, i)?); ws(b, i); match at(b, *i)? { b',' => *i += 1, b']' => { *i += 1; return Some(Json::Arr(a)); } _ => return None } } }
            b'"' => { *i += 1; let mut s = String::new();
                loop { let c = at(b, *i)?; *i += 1;
                    match c { b'"' => return Some(Json::Str(s)),
                        b'\\' => { let e = at(b, *i)?; *i += 1; match e { b'n' => s.push('\n'), b't' => s.push('\t'), b'r' => s.push('\r'), b'u' => { let h = std::str::from_utf8(b.get(*i..*i + 4)?).ok()?; *i += 4; s.push(char::from_u32(u32::from_str_radix(h, 16).ok()?).unwrap_or('?')); } x => s.push(x as char) } }
                        _ => { let start = *i - 1; let mut end = *i; while end < b.len() && b[end] != b'"' && b[end] != b'\\' { end += 1; } s.push_str(std::str::from_utf8(&b[start..end]).ok()?); *i = end; } } } }
            b't' => lit(b, i, b"true", Json::Bool(true)),
            b'f' => lit(b, i, b"false", Json::Bool(false)),
            b'n' => lit(b, i, b"null", Json::Null),
            _ => { let start = *i; while *i < b.len() && (b[*i] == b'-' || b[*i] == b'.' || b[*i] == b'e' || b[*i] == b'E' || b[*i] == b'+' || b[*i].is_ascii_digit()) { *i += 1; }
                if *i == start { return None; }
                std::str::from_utf8(&b[start..*i]).ok()?.parse().ok().map(Json::Num) }
        }
    }
}
