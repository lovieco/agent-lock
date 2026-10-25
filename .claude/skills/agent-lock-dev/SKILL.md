---
name: agent-lock-dev
description: How to develop the agent-lock repository itself. Covers the Rust crate in rust/ (one std-only binary that acts as hooks, shims and CLI), the plugin files (hooks/hooks.json, bin/agent-lock, bin/native, skills/agent-lock), the docs site and install.sh. Use whenever you edit, build, test, benchmark or release anything in the agent-lock repo, rebuild the binaries in bin/native, or change an agent-facing message.
---

# Developing agent-lock

## Working in this checkout

- agent-lock is built with agent-lock, so other agents may share this checkout
  on `main`. When the plugin is active in your session (your context has an
  `[agent-lock]` line with your agent id), follow `skills/agent-lock/SKILL.md`:
  commit with `agent-lock commit`, run no whole-tree git, and treat BLOCKED as
  a cue to do other work.
- Never edit, create or delete `.git/agent-lock.json` or `.git/index.lock`
  here. Try hooks and shims in a throwaway repo instead (see below).
- After an edit to `skills/agent-lock/SKILL.md`, the project hook
  `.claude/hooks/skill-sync-prompt.mjs` asks whether to copy the file to
  `~/.claude/skills/agent-lock/SKILL.md`. Copy it only when the user says yes.

## Layout

| Path | What it is |
|---|---|
| `.claude-plugin/plugin.json`, `marketplace.json` | Plugin manifest (version 0.3.0) and the `lovieco` marketplace (no version of its own) |
| `hooks/hooks.json` | SessionStart runs `bin/agent-lock hook session-start`. Every other hook execs the link that SessionStart keeps at `$CLAUDE_PLUGIN_DATA/shims/core/agent-lock`, and falls back to the launcher |
| `bin/agent-lock` | POSIX sh launcher. It execs `bin/native/agent-lock-darwin` or `agent-lock-darwin-<arch>` on macOS, and `agent-lock-linux-<arch>` on Linux. With no binary it prints one line and exits 0 (fail open) |
| `bin/native/` | Committed release binaries, written by CI: `agent-lock-darwin` (universal), `agent-lock-linux-x86_64`, `agent-lock-linux-arm64` |
| `skills/agent-lock/SKILL.md` | What agents load. It quotes messages from `rust/src/msg.rs` and the shim |
| `rust/` | The crate: `src/`, `tests/`, `build.sh` |
| `docs/` | Vite marketing site. The copy is in `docs/src/data.yaml`, rendered by `docs/src/main.js` |
| `install.sh` | Installs the CLI for a human's shell: clones to `~/.agent-lock-core` and symlinks `bin/agent-lock` onto PATH. No Node |
| `SPEC.md`, `README.md`, `TODO.md` | Spec, user docs, roadmap |

The v2 Node implementation was deleted. A backup is at
`.git/agent-lock-v2-backup.tar.gz`.

`rust/src`:

| Module | Role |
|---|---|
| `main.rs` | Dispatch: an argv[0] in `shim::NAMES` goes to `shim::run`, `hook <event>` to `hooks::run`, anything else to `cli::run` |
| `json.rs` | Minimal JSON parser and printer |
| `sys.rs` | Time, pid liveness (pid + start time), repo discovery, git-spelling paths, `real_tool`, `data_dir` |
| `ids.rs` | Agent ids: `<sid8>` for a main agent, `<sid8>:<sub8>` for a subagent; `caller()` resolves `--as`, then `$AGENT_LOCK_AGENT_ID`, then `$CLAUDE_CODE_SESSION_ID` |
| `state.rs` | The state file `<gitdir>/agent-lock.json`: `transact` (exclusive flock, one write) and `read` (shared flock) |
| `rules.rs` | Pure claim rules: acquire, stale, hand-over, waits and deadlock (`TTL` 600 s, `UNWRITTEN_GRACE` 45 min, `SUB_IDLE` 30 min) |
| `hooks.rs` | Claude Code hooks: session-start, subagent-start, prompt, pre-edit, post-edit, edit-failed, pre-bash, post-bash, post-agent, subagent-stop, stop, session-end |
| `msg.rs` | Every agent-facing hook sentence |
| `git.rs` | git helpers, `commit_paths`, `sync`, `push`, `recover` |
| `shim.rs` | The git shim and the package-manager shims (npm, pnpm, yarn, bun, cargo, uv, poetry, go, bundle, bundler) |
| `cli.rs` | CLI commands (`agent-lock help` lists them) |
| `lockfiles.rs` | Manifest and lockfile pairs |

## Rules for the code

- std only. No crates, ever: `[dependencies]` in `rust/Cargo.toml` stays
  empty. `unsafe` is limited to the small `extern "C"` declarations in
  `sys.rs`.
- Fail open. A hook that errors exits 0 with no output. A shim that errors
  runs the real tool unchanged. The launcher exits 0 when it finds no binary.
  Entry points use `catch_unwind`.
- Each state change is one `state::transact`: open, flock, read, change, one
  write plus truncate. Never hold a transaction while git or a package manager
  runs.
- Hot paths spawn no processes. Safe git subcommands exec the real git without
  reading the state.
- Agent-facing text lives in `msg.rs`, or in the shim's one-line refusals.
  Keep it short and specific, write ids as `a1b2c3d4`, and in copy-paste
  commands use absolute shell-quoted paths, `--as <id>` for subagents and
  `-C <repo>` when the shell is elsewhere. When you change a message, update
  `skills/agent-lock/SKILL.md` too, since it quotes them.
- Every behavior gets a test. Keep each test under about 10 s.

## Toolchain and tests

On this machine cargo is not on PATH, and `rust-objcopy` is broken, so rustc
must not strip:

```sh
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export CARGO_PROFILE_RELEASE_STRIP=false
cd rust
cargo test --locked                 # 267 pass, 1 ignored benchmark (268 in all)
cargo build --release --locked
```

- `rust/tests/common/mod.rs` holds the helpers: `make_repo`, `clone`, `git`,
  `write`, `hook`, `cli`, `run_bin`, `payload`, `edit`, `bash`, `state`,
  `holder`, `dead_pid`, and the sessions `SA`, `SB` and `SC`. In tests,
  `std::process::id()` is a live owner and `dead_pid()` a dead one.
- Black-box suites: `cli.rs`, `hooks.rs`, `shim.rs`, `git.rs`, `e2e.rs`
  (multi-agent scenarios with a bare remote), `plugin.rs` (hooks.json, the
  launcher, the env file, build.sh as POSIX sh) and `smoke.rs`.
- The helpers set `GIT_EDITOR=true` and `GIT_TERMINAL_PROMPT=0`. Never run
  interactive commands, and keep each shell command under 90 s.

## Trying it by hand

`bin/agent-lock selftest` runs the real hooks, shims and CLI in a throwaway
repo with a bare remote: 12 steps, under half a second.

To drive a hook yourself, work in a throwaway repo and point the data
directory at a scratch folder, so the real one is untouched. `CLAUDE_PID=1`
gives the session a live owner.

```sh
export CLAUDE_PLUGIN_DATA=$TMPDIR/al-data CLAUDE_PID=1
s=aaaaaaaa-1111-2222-3333-444444444444
printf '{"session_id":"%s","cwd":"%s","hook_event_name":"SessionStart","source":"startup"}' "$s" "$PWD" |
  CLAUDE_ENV_FILE=$TMPDIR/al-env /path/to/agent-lock/bin/agent-lock hook session-start
```

The env file then holds `AGENT_LOCK_AGENT_ID`, the shim PATH and
`GIT_OPTIONAL_LOCKS=0`. The shims are in `$CLAUDE_PLUGIN_DATA/shims/core`
(git, agent-lock) and `$CLAUDE_PLUGIN_DATA/shims/<tool>`.

## Refreshing bin/native

`rust/build.sh` builds release binaries and installs them as
`bin/native/agent-lock-<os>-<arch>`. It installs by rename, so hooks that are
running the old binary keep their copy.

- Default targets: this machine, plus the other macOS arch when its standard
  library is installed (`rustup target add x86_64-apple-darwin`). With both
  macOS slices it also writes the universal `agent-lock-darwin` (lipo).
- On macOS it strips with `/usr/bin/strip` and signs ad hoc with `codesign`.
- Environment: `TARGETS="<rust triples>"`, `CARGO_CMD="<the whole build
  command>"` (default `cargo build`; CI uses `cargo zigbuild`), `OUT=<dir>`.
  To cross-build the Linux binaries on a Mac:
  `pip install cargo-zigbuild` in a venv, `rustup target add
  x86_64-unknown-linux-musl aarch64-unknown-linux-musl`, then run build.sh with
  `TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl"`
  `CARGO_CMD="cargo zigbuild"` `OUT=<scratch dir>`. Never write them into
  `bin/native/` by hand: CI owns the committed files.
- Check the result with `bin/agent-lock version` and `bin/agent-lock selftest`.
- The launcher tries `agent-lock-darwin` before `agent-lock-darwin-<arch>`.
  CI's committed universal binary therefore shadows a local arm64-only build
  in `bin/native/` (git ignores that file). To run your own build, execute
  `rust/target/release/agent-lock` directly, or install the x86_64 target so
  build.sh also writes a universal `agent-lock-darwin`.

## CI: .github/workflows/native.yml

It runs on a push to `main` that touches `rust/**` or the workflow, and on
manual dispatch.

- `macos-14`: runs `cargo test --locked`, then builds the universal
  `agent-lock-darwin`.
- `ubuntu-24.04`: runs `cargo test --locked`, then builds static musl
  `agent-lock-linux-x86_64` and `agent-lock-linux-arm64` with cargo-zigbuild.
- `commit`: installs the three binaries into `bin/native/` and pushes
  `native: agent-lock binaries for <sha> [skip ci]` to `main`.

It ran first on 2026-10-25 and passes. After it pushes its binaries commit, bring that in with
`agent-lock sync`.

## Plugin checks

- `claude plugin validate .claude-plugin/plugin.json` passes today with one
  warning: the root `CLAUDE.md` is not loaded as plugin context (expected: it
  is this repo's index). `claude plugin validate .claude-plugin/marketplace.json`
  passes clean.
- The version appears in `rust/Cargo.toml` (and `Cargo.lock`) and
  `.claude-plugin/plugin.json`. Bump them together. Keep it out of
  `marketplace.json`: `plugin.json` wins, and a second copy drifts.
- Claude Code installs a new plugin version only when `plugin.json`'s version
  changes. Bump it after CI has committed the binaries for that source, never
  in the same push: a user who installs in between gets the version without
  the Linux and universal macOS binaries, and `claude plugin update` then
  reports it as current.
- To try the plugin, load it for one session with `claude --plugin-dir .`.
  Don't install it from a local path (`--marketplace <this checkout>`): that
  copies the whole folder into the plugin cache, `rust/target` included
  (about 2 GB). From GitHub only tracked files are copied.
- To test a real install without touching your own setup, give it a scratch
  config dir:
  `CLAUDE_CONFIG_DIR=$TMPDIR/al-cfg claude plugin install agent-lock --marketplace lovieco/agent-lock`.

## Docs site

`cd docs && npm install && npm run dev` (also `build`, `preview`). Edit the
copy in `docs/src/data.yaml`, not in `index.html`. Keep the Lovie.co
breadcrumb and footer links.
