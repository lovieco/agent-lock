//! The plugin as Claude Code runs it: hooks.json's command lines through
//! `sh -c`, the POSIX launcher at bin/agent-lock, the native binary in
//! bin/native/. A temp plugin root mirrors the final layout.
mod common;
use common::*;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

const EDIT_TOOLS: &str = "Write|Edit|MultiEdit|NotebookEdit";
const SUB: &str = "a1111111122222222"; // → aaaaaaaa:11111111

/// The plugin files at the repo root: hooks/hooks.json and bin/agent-lock (the launcher).
fn plugin_src(f: &str) -> PathBuf {
    let rel = match f {
        "hooks.json" => "hooks/hooks.json",
        "agent-lock" => "bin/agent-lock",
        other => other,
    };
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(rel)
}

/// This machine's binary name in bin/native/.
fn native_name() -> String {
    let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "x86_64" };
    if cfg!(target_os = "macos") { format!("agent-lock-darwin-{arch}") } else { format!("agent-lock-linux-{arch}") }
}

fn chmod_x(p: &Path) {
    assert!(Command::new("chmod").args(["755", p.to_str().unwrap()]).status().unwrap().success());
}

/// <root>/bin/agent-lock (the launcher) and <root>/bin/native/<native> (a copy of the binary).
fn plugin_root(r: &Repo, native: Option<&str>) -> PathBuf {
    let root = r.base.join("plugin");
    std::fs::create_dir_all(root.join("bin/native")).unwrap();
    std::fs::copy(plugin_src("agent-lock"), root.join("bin/agent-lock")).unwrap();
    chmod_x(&root.join("bin/agent-lock"));
    if let Some(n) = native {
        std::fs::copy(BIN, root.join("bin/native").join(n)).unwrap();
        chmod_x(&root.join("bin/native").join(n));
    }
    root
}

fn sh(r: &Repo, cmd: &str, env: &[(&str, String)], input: &str) -> Run {
    run_bin(r, Some(Path::new("/bin/sh")), &["-c", cmd], &r.dir, env, input)
}

/// (event, matcher, command) for every hook in hooks.json.
fn entries() -> Vec<(String, Option<String>, String)> {
    let j = serde_free::strict(&std::fs::read_to_string(plugin_src("hooks.json")).unwrap()).expect("hooks.json is JSON");
    let mut out = Vec::new();
    for event in j.get("hooks").keys() {
        for group in j.get("hooks").get(&event).arr() {
            let matcher = group.get("matcher").as_str().map(str::to_string);
            for h in group.get("hooks").arr() {
                assert_eq!(h.get("type").as_str(), Some("command"), "{event}");
                assert_eq!(h.get("timeout").as_u64(), Some(10), "{event}: timeout in seconds");
                out.push((event.clone(), matcher.clone(), h.get("command").as_str().unwrap().to_string()));
            }
        }
    }
    out
}

/// SessionStart goes through the launcher (it refreshes the shims dir's link to this
/// plugin version); every other hook execs that link directly, falling back to the launcher.
const FAST: &str = "a=\"$CLAUDE_PLUGIN_DATA/shims/core/agent-lock\"; [ -x \"$a\" ] || a=\"${CLAUDE_PLUGIN_ROOT}/bin/agent-lock\"; exec \"$a\" hook ";

fn hook_name(cmd: &str) -> &str {
    cmd.strip_prefix("\"${CLAUDE_PLUGIN_ROOT}/bin/agent-lock\" hook ")
        .filter(|n| *n == "session-start")
        .or_else(|| cmd.strip_prefix(FAST).filter(|n| *n != "session-start"))
        .unwrap_or_else(|| panic!("unexpected command line: {cmd}"))
}

#[test]
fn hooks_fall_back_to_the_launcher_before_session_start_made_the_link() {
    let r = make_repo(false, &[("x.ts", "x\n")]);
    let root = plugin_root(&r, Some(&native_name()));
    assert!(!r.data.join("shims/core/agent-lock").exists());
    let cmd = entries().into_iter().find(|(e, m, _)| e == "PreToolUse" && m.as_deref() == Some(EDIT_TOOLS)).unwrap().2;
    let env = [("CLAUDE_PLUGIN_ROOT", root.display().to_string()), ("CLAUDE_PID", std::process::id().to_string())];
    let o = sh(&r, &cmd, &env, &payload(&r, SA, &edit(&r.dir.join("x.ts"), "t1")));
    assert_eq!((o.status, o.stderr.as_str()), (0, ""), "{}", o.out());
    assert_eq!(holder(&r, "x.ts").as_deref(), Some("aaaaaaaa"), "claimed through the launcher");
}

#[test]
fn hooks_json_routes_every_event_to_its_hook() {
    let got: BTreeSet<(String, String, String)> =
        entries().iter().map(|(e, m, c)| (e.clone(), m.clone().unwrap_or_default(), hook_name(c).to_string())).collect();
    let want: BTreeSet<(String, String, String)> = [
        ("SessionStart", "startup|resume|clear|compact", "session-start"),
        ("SubagentStart", "", "subagent-start"),
        ("UserPromptSubmit", "", "prompt"),
        ("PreToolUse", EDIT_TOOLS, "pre-edit"),
        ("PreToolUse", "Bash", "pre-bash"),
        ("PostToolUse", EDIT_TOOLS, "post-edit"),
        ("PostToolUse", "Bash", "post-bash"),
        ("PostToolUse", "Agent|Task|Workflow", "post-agent"),
        ("PostToolUseFailure", EDIT_TOOLS, "edit-failed"),
        ("PostToolUseFailure", "Bash", "post-bash"),
        ("PermissionDenied", EDIT_TOOLS, "edit-failed"),
        ("PermissionDenied", "Bash", "post-bash"),
        ("SubagentStop", "", "subagent-stop"),
        ("Stop", "", "stop"),
        ("SessionEnd", "", "session-end"),
    ]
    .iter()
    .map(|(e, m, n)| (e.to_string(), m.to_string(), n.to_string()))
    .collect();
    assert_eq!(got, want);
    assert_eq!(entries().len(), 15, "no event is routed twice");
}

/// Claude Code's choice: the hooks of `event` whose matcher names `tool`.
struct Plugin<'r> {
    r: &'r Repo,
    root: PathBuf,
    entries: Vec<(String, Option<String>, String)>,
    used: std::cell::RefCell<BTreeSet<(String, Option<String>)>>,
}

impl Plugin<'_> {
    fn fire(&self, event: &str, tool: Option<&str>, session: &str, pid: u32, extra: &str, env: &[(&str, String)]) -> Run {
        let hit: Vec<&(String, Option<String>, String)> = self
            .entries
            .iter()
            .filter(|(e, m, _)| e == event && m.as_deref().is_none_or(|m| tool.is_none_or(|t| m.split('|').any(|x| x == t))))
            .collect();
        assert_eq!(hit.len(), 1, "{event} {tool:?}: exactly one hook");
        let (e, m, cmd) = hit[0];
        self.used.borrow_mut().insert((e.clone(), m.clone()));
        let mut all = vec![("CLAUDE_PLUGIN_ROOT", self.root.display().to_string()), ("CLAUDE_PID", pid.to_string())];
        all.extend(env.iter().cloned());
        let o = sh(self.r, cmd, &all, &payload(self.r, session, extra));
        assert_eq!((o.status, o.stderr.as_str()), (0, ""), "{event} {tool:?}");
        assert!(o.stdout.is_empty() || serde_free::strict(&o.stdout).is_some(), "{event}: {}", o.stdout);
        o
    }
}

fn as_sub(rest: String) -> String {
    format!(r#""agent_id":"{SUB}","agent_type":"general-purpose",{rest}"#)
}

#[test]
fn a_session_through_hooks_json_and_the_launcher() {
    let r = make_repo(false, &[("x.ts", "x\n"), ("y.ts", "y\n"), ("z.ts", "z\n")]);
    let root = plugin_root(&r, Some(&native_name()));
    let p = Plugin { r: &r, root: root.clone(), entries: entries(), used: Default::default() };
    let me = std::process::id();
    let b = Command::new("sleep").arg("60").spawn().unwrap();
    let bp = b.id();
    let (x, y, z) = (r.dir.join("x.ts"), r.dir.join("y.ts"), r.dir.join("z.ts"));

    let envf = r.base.join("env-a");
    std::fs::write(&envf, "").unwrap();
    let start = r#""hook_event_name":"SessionStart","source":"startup""#;
    let o = p.fire("SessionStart", None, SA, me, start, &[("CLAUDE_ENV_FILE", envf.display().to_string())]);
    assert!(o.context().contains("Your agent id is aaaaaaaa"), "{}", o.stdout);
    p.fire("SessionStart", None, SB, bp, start, &[]);
    let text = std::fs::read_to_string(&envf).unwrap();
    assert!(text.contains("export AGENT_LOCK_AGENT_ID=aaaaaaaa\n"), "{text}");
    let shims = r.data.join("shims");
    assert!(text.contains(&format!("export PATH=\"{}/core:$_alp$PATH\"", shims.display())), "{text}");
    let native = std::fs::canonicalize(root.join("bin/native").join(native_name())).unwrap();
    for n in ["core/git", "core/agent-lock", "npm/npm", "pnpm/pnpm", "yarn/yarn", "bun/bun"] {
        assert_eq!(std::fs::canonicalize(shims.join(n)).unwrap(), native, "{n} links to the plugin's binary");
    }
    p.fire("SubagentStart", None, SA, me, &as_sub(r#""hook_event_name":"SubagentStart""#.into()), &[]);
    p.fire("UserPromptSubmit", None, SA, me, r#""hook_event_name":"UserPromptSubmit","prompt":"go""#, &[]);

    // edits: claim, block, release on failure or denial
    let o = p.fire("PreToolUse", Some("Edit"), SA, me, &edit(&x, "a1"), &[]);
    assert_ne!(o.decision().as_deref(), Some("deny"), "{}", o.stdout);
    std::fs::write(&x, "x by A\n").unwrap();
    p.fire("PostToolUse", Some("Edit"), SA, me, &edit(&x, "a1").replace("PreToolUse", "PostToolUse"), &[]);
    assert_eq!(state(&r).get("claims").get("x.ts").get("edits").as_u64(), Some(1));
    let d = p.fire("PreToolUse", Some("Write"), SB, bp, &edit(&x, "b1").replace("\"Edit\"", "\"Write\""), &[]);
    assert_eq!(d.decision().as_deref(), Some("deny"));
    assert!(d.reason().contains("BLOCKED: x.ts is claimed by aaaaaaaa"), "{}", d.reason());
    p.fire("PreToolUse", Some("Edit"), SB, bp, &edit(&y, "b2"), &[]);
    assert_eq!(holder(&r, "y.ts").as_deref(), Some("bbbbbbbb"));
    p.fire("PermissionDenied", Some("Edit"), SB, bp, &edit(&y, "b2").replace("PreToolUse", "PermissionDenied"), &[]);
    assert_eq!(holder(&r, "y.ts"), None, "a denied edit leaves no claim");
    p.fire("PreToolUse", Some("Edit"), SA, me, &as_sub(edit(&z, "s1")), &[]);
    assert_eq!(holder(&r, "z.ts").as_deref(), Some("aaaaaaaa:11111111"));
    let failed = edit(&z, "s1").replace("PreToolUse", "PostToolUseFailure");
    p.fire("PostToolUseFailure", Some("Edit"), SA, me, &as_sub(format!(r#"{failed},"error":"old_string not found""#)), &[]);
    assert_eq!(holder(&r, "z.ts"), None, "a failed first edit leaves no claim");

    // Bash: identity check for a subagent, then the three post hooks
    let o = p.fire("PreToolUse", Some("Bash"), SA, me, &as_sub(bash("agent-lock commit -m x", "s2")), &[]);
    assert!(o.reason().contains("agent-lock commit --as aaaaaaaa:11111111 -m x"), "{}", o.stdout);
    p.fire("PermissionDenied", Some("Bash"), SA, me, &as_sub(bash("agent-lock commit -m x", "s2").replace("PreToolUse", "PermissionDenied")), &[]);
    assert!(p.fire("PreToolUse", Some("Bash"), SA, me, &bash("ls", "a2"), &[]).stdout.is_empty());
    let post = bash("ls", "a2").replace("PreToolUse", "PostToolUse");
    p.fire("PostToolUse", Some("Bash"), SA, me, &format!(r#"{post},"tool_response":{{"stdout":"x.ts\n","stderr":""}}"#), &[]);
    p.fire("PreToolUse", Some("Bash"), SA, me, &bash("false", "a3"), &[]);
    let fail = bash("false", "a3").replace("PreToolUse", "PostToolUseFailure");
    p.fire("PostToolUseFailure", Some("Bash"), SA, me, &format!(r#"{fail},"error":"exit 1""#), &[]);

    // a background launch outlives the turn
    let launch = r#""hook_event_name":"PostToolUse","tool_name":"Agent","tool_input":{"prompt":"x","run_in_background":true},"tool_use_id":"a9","tool_response":{"status":"async_launched","agentId":"a5555555566666666"}"#;
    p.fire("PostToolUse", Some("Agent"), SA, me, launch, &[]);
    let idx = serde_free::parse(&std::fs::read_to_string(r.data.join("sessions.json")).unwrap());
    assert_eq!(idx.get(SA).get("bg").arr()[0].as_str(), Some("aaaaaaaa:55555555"), "{idx:?}");

    // the shims and the CLI the session start linked
    let env = vec![
        ("PATH", format!("{}:{}", shims.join("core").display(), std::env::var("PATH").unwrap())),
        ("AGENT_LOCK_AGENT_ID", "bbbbbbbb".to_string()),
    ];
    let o = sh(&r, "git add -A", &env, "");
    assert_eq!(o.status, 1, "{}", o.out());
    assert!(o.stderr.contains("x.ts by aaaaaaaa"), "{}", o.stderr);
    let o = sh(&r, "agent-lock mine", &[("PATH", env[0].1.clone()), ("AGENT_LOCK_AGENT_ID", "aaaaaaaa".into())], "");
    assert_eq!((o.status, o.stdout.trim()), (0, x.to_str().unwrap()), "{}", o.out());

    // end of turn and of sessions
    p.fire("SubagentStop", None, SA, me, &as_sub(r#""hook_event_name":"SubagentStop","stop_hook_active":false"#.into()), &[]);
    let s = p.fire("Stop", None, SA, me, r#""hook_event_name":"Stop","stop_hook_active":false"#, &[]);
    assert_eq!(s.decision().as_deref(), Some("block"), "B is active: {}", s.stdout);
    assert!(s.reason().contains("x.ts"), "{}", s.reason());
    p.fire("SessionEnd", None, SB, bp, r#""hook_event_name":"SessionEnd","reason":"logout""#, &[]);
    assert_eq!(state(&r).get("sessions").get(SB).get("phase").as_str(), Some("ended"), "{:?}", state(&r).get("sessions").get(SB));

    let used = p.used.borrow().clone();
    let all: BTreeSet<(String, Option<String>)> = p.entries.iter().map(|(e, m, _)| (e.clone(), m.clone())).collect();
    assert_eq!(used, all, "every hook in hooks.json ran");
    let mut b = b;
    let _ = b.kill();
    let _ = b.wait();
}

/// The env file as the Bash tool sources it: git and agent-lock are always
/// shimmed, a package manager only where the shell finds the real one, so
/// `command -v pnpm` stays false on a machine without pnpm.
#[test]
fn the_env_file_shims_only_installed_package_managers() {
    let r = make_repo(false, &[("x.ts", "x\n")]);
    let root = plugin_root(&r, Some(&native_name()));
    let envf = r.base.join("env");
    std::fs::write(&envf, "").unwrap();
    let start = payload(&r, SA, r#""hook_event_name":"SessionStart","source":"startup""#);
    let env = [("CLAUDE_PLUGIN_ROOT", root.display().to_string()), ("CLAUDE_ENV_FILE", envf.display().to_string())];
    sh(&r, "\"${CLAUDE_PLUGIN_ROOT}/bin/agent-lock\" hook session-start", &env, &start);
    let text = std::fs::read_to_string(&envf).unwrap();
    assert_eq!(text.lines().filter(|l| l.contains("export PATH=")).count(), 1, "{text}");
    let fake = r.base.join("fakebin");
    write(&fake, "npm", "#!/bin/sh\necho real npm\n");
    chmod_x(&fake.join("npm"));
    let shims = r.data.join("shims");
    for shell in ["/bin/sh", "/bin/bash", "/bin/zsh", "/bin/dash"].into_iter().filter(|s| Path::new(s).exists()) {
        let script = format!(
            ". {}; for t in git agent-lock npm pnpm yarn bun; do command -v $t || echo \"no $t\"; done; echo \"[$_alp$_alt]\"",
            envf.display()
        );
        let o = run_bin(&r, Some(Path::new(shell)), &["-c", &script], &r.dir, &[("PATH", format!("{}:/nonexistent", fake.display()))], "");
        let want = [
            shims.join("core/git").display().to_string(),
            shims.join("core/agent-lock").display().to_string(),
            shims.join("npm/npm").display().to_string(),
            "no pnpm".into(),
            "no yarn".into(),
            "no bun".into(),
            "[]".into(), // its loop variables are gone
        ];
        assert_eq!(o.stdout.lines().collect::<Vec<_>>(), want, "{shell}: {}", o.out());
    }
}

#[test]
fn the_launcher_fails_open_without_a_binary() {
    let r = make_repo(false, &[("x.ts", "x\n")]);
    let root = plugin_root(&r, None);
    let env = [("CLAUDE_PLUGIN_ROOT", root.display().to_string())];
    let o = sh(&r, "\"${CLAUDE_PLUGIN_ROOT}/bin/agent-lock\" hook pre-edit", &env, &payload(&r, SA, &edit(&r.dir.join("x.ts"), "t1")));
    assert_eq!((o.status, o.stdout.as_str()), (0, ""), "{}", o.out());
    assert!(o.stderr.starts_with("agent-lock: no native binary for this machine") && o.stderr.lines().count() == 1, "{}", o.stderr);
    assert!(!r.git_dir.join("agent-lock.json").exists(), "nothing ran");
}

#[test]
fn the_launcher_picks_the_binary_and_passes_arguments_through() {
    let r = make_repo(false, &[]);
    let root = plugin_root(&r, None);
    let launcher = root.join("bin/agent-lock");
    let text = std::fs::read_to_string(&launcher).unwrap();
    assert!(text.starts_with("#!/bin/sh\n") && text.lines().count() <= 15, "{text}");
    assert!(Command::new("sh").args(["-n", launcher.to_str().unwrap()]).status().unwrap().success());
    let fake = |name: &str, says: &str| {
        let p = root.join("bin/native").join(name);
        std::fs::write(&p, format!("#!/bin/sh\necho {says}\nprintf '<%s>' \"$@\"\n")).unwrap();
        chmod_x(&p);
    };
    // macOS: no process under bash (HOSTTYPE), /usr/bin/uname under other shells; Linux: `uname -m`
    let path = if cfg!(target_os = "macos") { "/nonexistent".to_string() } else { "/usr/bin:/bin".to_string() };
    let run = |via: &str| {
        let o = run_bin(&r, Some(Path::new(via)), &[launcher.to_str().unwrap(), "hook", "a b", ""], &r.dir, &[("PATH", path.clone())], "");
        assert_eq!(o.status, 0, "{via}: {}", o.out());
        o.stdout
    };
    let shells: Vec<&str> = ["/bin/sh", "/bin/dash", "/bin/bash"].into_iter().filter(|s| Path::new(s).exists()).collect();
    fake(&native_name(), "arch");
    for s in &shells {
        assert_eq!(run(s), "arch\n<hook><a b><>", "{s}");
    }
    if cfg!(target_os = "macos") {
        fake("agent-lock-darwin", "universal");
        for s in &shells {
            assert_eq!(run(s), "universal\n<hook><a b><>", "{s}: the universal binary comes first");
        }
    }
    // invoked by a relative path, as `sh bin/agent-lock`
    let o = run_bin(&r, Some(Path::new("/bin/sh")), &["bin/agent-lock", "version"], &root, &[("PATH", path.clone())], "");
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.starts_with(if cfg!(target_os = "macos") { "universal\n" } else { "arch\n" }), "{}", o.stdout);
}

#[test]
fn the_launcher_runs_the_real_binary() {
    let r = make_repo(false, &[]);
    let root = plugin_root(&r, Some(&native_name()));
    let o = sh(&r, "\"$CLAUDE_PLUGIN_ROOT/bin/agent-lock\" version", &[("CLAUDE_PLUGIN_ROOT", root.display().to_string())], "");
    assert_eq!(o.status, 0, "{}", o.out());
    assert!(o.stdout.starts_with("agent-lock 0."), "{}", o.stdout);
}

#[test]
fn the_build_script_is_valid_posix_sh() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("build.sh");
    for sh in ["/bin/sh", "/bin/dash"].into_iter().filter(|s| Path::new(s).exists()) {
        assert!(Command::new(sh).arg("-n").arg(&script).status().unwrap().success(), "{sh} -n build.sh");
    }
}
