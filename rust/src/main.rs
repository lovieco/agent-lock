//! agent-lock v3: one binary, three roles, picked by how it is called.
//!
//!   agent-lock hook <event>    Claude Code hooks (JSON on stdin, JSON out)
//!   git | npm | pnpm | yarn | bun | cargo | uv | poetry | go | bundle   shims (argv[0]): check, then run the real tool
//!   agent-lock <command>       the CLI (commit, push, sync, status, …)
//!
//! All coordination state for a checkout is one file, <gitdir>/agent-lock.json,
//! changed only inside a flock transaction (state.rs).

mod cli;
mod git;
mod hooks;
mod ids;
mod json;
mod lockfiles;
mod msg;
mod rules;
mod shared;
mod shim;
mod state;
mod sys;

use std::ffi::OsString;

fn main() {
    let raw: Vec<OsString> = std::env::args_os().collect();
    let name = raw.first().and_then(|a| std::path::Path::new(a).file_name()).and_then(|n| n.to_str()).unwrap_or("agent-lock").to_string();
    let utf8: Option<Vec<String>> = raw.iter().map(|a| a.to_str().map(str::to_string)).collect();
    if shim::handles(&name) && utf8.is_none() {
        std::process::exit(shim::run_os(&name, &raw[1..])); // judged from a lossy copy; the tool gets the raw bytes
    }
    let args = utf8.unwrap_or_else(|| raw.iter().map(|a| a.to_string_lossy().into_owned()).collect());
    let rest = args.get(1..).unwrap_or(&[]);
    let code = if shim::handles(&name) {
        shim::run(&name, rest)
    } else if args.get(1).map(String::as_str) == Some("hook") {
        hooks::run(args.get(2).map(String::as_str).unwrap_or(""))
    } else {
        cli::run(rest)
    };
    std::process::exit(code);
}
