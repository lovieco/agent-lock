# agent-lock roadmap

What v0.3 leaves open, roughly in priority order. Each item lands with a test
(`cd rust && cargo test --locked`).

---

## Next

### Precise subagent identity in the shims

Today subagents share their session's Bash environment, so
`AGENT_LOCK_AGENT_ID` names the main agent, and the shims see the main id for
every agent in the session. Two workarounds follow from that:

- While a subagent may be running, the git shim counts the main agent's own
  files as another agent's. Whole-tree git is then refused for the main agent
  too.
- Subagents must pass `--as <id>` to agent-lock commands (the Bash hook
  enforces it), and prefix `AGENT_LOCK_AGENT_ID=<id>` to raw git and package
  manager commands.

Plan: while a subagent of the session may run, pre-bash records each
in-flight Bash call (agent id plus a hash of `tool_input.command`) next to the
existing `BashSnap`. The shim reads its ancestor shell's argv
(`sysctl KERN_PROCARGS2` on macOS, `/proc/<ppid>/cmdline` on Linux), matches it
against the session's in-flight commands, and acts as that agent. When the
match is ambiguous, it judges as the union of the candidates and refuses if
any of them would be refused. Then the main agent's files no longer count as
foreign, and the `--as` requirement can be relaxed.

### First run of the Linux and universal macOS binaries

`.github/workflows/native.yml` has never run. Only
`bin/native/agent-lock-darwin-arm64` exists, so on Linux and on Intel Macs the
launcher finds no binary and agent-lock stays inactive (it fails open: one
stderr line, exit 0). Shells other than bash (no `HOSTTYPE`) get the arch from
`/usr/bin/uname`, so an Intel Mac never execs the arm64 binary. Not yet tried on
an Intel Mac.

- Run the workflow (workflow_dispatch). Check that both test jobs pass, that
  `lipo -info` shows two slices in `agent-lock-darwin`, and that `file` and
  `version` work for both Linux binaries.
- Check that the commit job can push to `main` (branch protection, the
  `[skip ci]` commit).
- Run `bin/agent-lock selftest` with the committed binaries on Linux x86_64,
  Linux arm64 and an Intel Mac.
- Measure hook latency on Linux. On macOS arm64 a hook takes about 7.4 ms as
  Claude Code runs it.
- Decide whether to keep `agent-lock-darwin-arm64`. The launcher prefers the
  universal `agent-lock-darwin`, so the arm64 file is unused once that lands.

### `git stash apply` when the stash is disjoint

While other agents hold live claims, the shim refuses `git stash pop`,
`apply` and `drop`. The refusal names the way out
(`git stash show --name-only stash@{n}`, then
`git checkout stash@{n} -- <your paths>`), but an agent that stashed its own
paths cannot simply apply them.

Plan: allow `git stash apply <ref>`, which leaves the shared stack alone, when
the paths from `git stash show --name-only --include-untracked <ref>` are
disjoint from other agents' claims. Claim the applied paths for the caller.
Keep `pop` and `drop` refused, since they change the shared stack and can
consume another agent's entry.

### Commit edge: an NFD directory in a sparse checkout

Claim keys follow git's spelling (NFC, case), and sync skips skip-worktree
entries. The reported open edge is a path-limited `agent-lock commit` of a
file under a directory whose name is NFD on disk, in a sparse checkout. The
simple cases pass: on 2026-10-25, a tracked file and a new file under an NFD
directory both committed correctly, in cone and non-cone sparse checkouts.
Pin down the failing case with a test first (next to
`an_nfd_directory_and_a_miscased_leaf_are_one_claim_that_stop_and_commit_keep`
in `tests/e2e.rs`, or the sparse-cone sync test in `src/git.rs`), then fix it.

### Index-only changes to another agent's staged file

`git reset -- <path>` and `git restore --staged <path>` on another agent's
claimed file are allowed, and they undo that agent's staging. So is
`git update-index` (for example `--assume-unchanged`). `git rm --cached` is
already refused. No work is lost: the worktree keeps the content, and
`agent-lock commit` commits worktree content. But staging the holder did on
purpose (`git add -p`) is gone.

Plan: judge path-limited `reset`, `restore --staged` and `update-index` like
`rm --cached`, and refuse them when they name other agents' claimed files.

### Background-task heuristics (post-agent and Stop)

What still runs after a turn is read from fields Claude Code does not
document:

- post-agent (PostToolUse on `Agent|Task|Workflow`) reads
  `status: "async_launched"`, then `agentId` (a background subagent) or
  `taskId` (a workflow) from the tool response.
- Stop reads `background_tasks` entries of type `subagent`, `agent` or
  `local_agent`, and of type `workflow`, `local_workflow`, `teammate` or
  `in_process_teammate`.

When those fields are missing, heuristics take over. Without
`background_tasks`, a subagent with no tool call in flight and no hook for
30 min (`SUB_IDLE`) counts as finished. While a workflow may run,
UserPromptSubmit marks no unfinished subagent as finished (only subagents
already reported done are handed back). If a release renames a field, a background
subagent can be taken for finished, and its files pass to the main agent while
it still works.

Plan: pin the payload shapes with fixtures per Claude Code version, add a
`doctor` warning when a session's Stop payloads carry no `background_tasks`,
and move to a documented signal when Claude Code offers one.

## Later

- **Verify before push.** `agent-lock commit --push` and `agent-lock push`
  build and test nothing, and the commit is HEAD plus the agent's files, a tree
  nobody has built. Add an opt-in check command (a repo setting) that runs on
  that exact tree before publishing.
- **Replay conflicts.** A sync or push whose replay conflicts reports
  `conflict` and changes nothing; a human resolves it. Add
  `agent-lock sync --resolve` for conflicts confined to the caller's own files.
- **Bash-written files.** Files created or changed only through Bash
  (codegen, scaffolding, `sed -i` on an unclaimed file) get no claim until
  someone names them in `agent-lock commit -- <paths>`. Diff the dirty set
  around each Bash call and claim new dirty paths for the agent that ran it.

## Limits we keep for now

- Processes that git spawns itself (repo hooks, `rebase -x`, `bisect run`)
  bypass the shim. Pre-commit hooks are scanned statically before a commit and
  checked for damage after it.
- Writes the hooks cannot see (python scripts, editors outside Claude Code)
  are detected after each Bash command by an mtime check, not prevented.
- No cross-machine locks: machines coordinate through `push` and `sync`.
- Old NFS without flock and Windows are unsupported.
- Claims exist only inside git worktrees.

---

## Shipped in v0.3

- Shared files: config files and manifests (built-ins plus
  `.agent-lock-shared`) take co-holders instead of blocking. One write at a
  time, one commit for all holders, dead holders taken over.

- One native binary, Rust with std only (about 10,300 non-test, non-comment
  lines), plays the Claude Code hooks, the shims and the CLI. No Node. A hook
  as Claude Code runs it takes about 7.4 ms (median, macOS arm64, release); the
  `sh -c` floor is 4.8 ms of that. v2 took about 80 ms.
- One state file per checkout, `<gitdir>/agent-lock.json`, holds every claim,
  session, wait, lock, orphan note, upstream tip and the off switch. Every
  change is one flock transaction, so a killed process leaves no stuck lock.
  2,000 contended transactions from 8 threads take about 0.1 s (release build)
  with no lost update.
- The git shim judges the real argv, after the shell, xargs, `find -exec` or
  an alias produced it. It refuses whole-tree git only while other agents hold
  live claims, and allows the recovery forms. It adds about 2.4 ms per git
  call. On macOS it execs the developer directory's git directly, skipping
  the xcrun trampoline behind `/usr/bin/git` (about 5 ms), so shimmed git is
  faster than `/usr/bin/git`.
- Package-manager shims (npm, pnpm, yarn, bun, cargo, uv, poetry, go, bundle)
  serialize installs and claim the manifest and lockfile of a dependency
  change, workspaces included. SessionStart puts a package manager's shim on
  PATH only when that tool is installed.
- Short ids (`a1b2c3d4`, `a1b2c3d4:9f3e01aa`). Subagents get their own ids,
  and their files go back to the main agent when they finish. Background
  subagents and workflows are tracked through post-agent and Stop's
  `background_tasks`.
- Claim keys use git's spelling (NFC, case), so macOS Unicode and case
  variants of a name are one claim.
- Dead owners' files can be taken over: a TAKEOVER that refuses the edit once
  while others are active, or a note when the agent is alone.
- A first claim on a file that already has changes nobody claimed gets a note.
  This costs one `git status` (about 8 ms), and only while another session is
  active. After each Bash command, an mtime check warns when the command
  changed other agents' files.
- `agent-lock commit`: hooks run on a private index of the caller's files,
  with a static scan for hooks that rewrite the worktree, `hook-clobbered`
  detection, `case-rename` and `staged-untrack` refusals, and lockfile pairing.
- `sync` and `push`: tracking-ref fetch, merge-tree replay, an install that
  never touches uncommitted files, recovery from an interrupted sync, and a
  push lease with retries.
- CLI renames: `release --mine` (was `release-mine`), `doctor --fix` (was
  `purge-all`), `status` (was `list` and `watch`), `selftest` (was `test`).
  `hold` is gone, because the end-of-turn reminder asks only once per file
  (and again while someone waits for it).
- 268 tests: `cargo test --locked` runs 267, and one benchmark is
  `#[ignore]`d. In a stress test, 9 concurrent agents
  made about 3,700 hook calls and no file ever had two holders.
- `.github/workflows/native.yml` builds the universal macOS and Linux musl
  (x86_64, arm64) binaries. It has not run yet.
