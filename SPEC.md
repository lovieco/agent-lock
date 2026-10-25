# Agent Lock — protocol spec (v3)

Normative description of agent-lock 0.3.0. README and the docs site explain
*why*; this file says *exactly what*. The code in `rust/src/` is the
reference: when it and this spec disagree, one of them is a bug. File names
without a directory are in `rust/src/`.

## 0. Goal and shape

N agents (Claude Code sessions, their foreground and background subagents,
and scripts) work in **one shared checkout, on one branch** (usually `main`):
no branches, no PRs, no merges.

1. **No lost or tangled edits.** At most one agent owns a file's uncommitted
   changes at a time.
2. **Clean commits.** Each agent commits exactly its own files and publishes
   without touching anyone's uncommitted work.
3. **Invisible when alone.** With one agent in a checkout nothing blocks,
   nags or needs configuring.

**One binary, three roles** (`main.rs`), picked by how it is called:

| Called as | Role | Entry |
|---|---|---|
| `agent-lock hook <event>` | Claude Code hook: one JSON payload on stdin, at most one JSON object on stdout, exit 0 | `hooks::run` |
| `git`, `npm`, `pnpm`, `yarn`, `bun`, `cargo`, `uv`, `poetry`, `go`, `bundle`, `bundler` (argv[0]) | shim: check, then run the real tool | `shim::run` |
| `agent-lock <command>` | CLI | `cli::run` |

Rust, std only, no crates (`rust/Cargo.toml`). Unix only.

| Plugin path | What |
|---|---|
| `bin/agent-lock` | POSIX sh launcher. macOS (`/System/Library` exists): execs `bin/native/agent-lock-darwin`, else `agent-lock-darwin-<arch>` (arch = `$HOSTTYPE`, which bash sets; other shells ask `/usr/bin/uname -m`; `aarch64` → `arm64`). Linux: `agent-lock-linux-<uname -m>` (`aarch64` → `arm64`). No binary: prints `agent-lock: no native binary for this machine in <dir> (agent-lock is inactive)` to stderr, exit 0 (fail open). |
| `bin/native/` | Today only `agent-lock-darwin-arm64`. `.github/workflows/native.yml` builds `agent-lock-darwin` (universal) and `agent-lock-linux-{x86_64,arm64}` (static musl) on pushes to `main` that touch `rust/**` or the workflow file, and on manual dispatch; it has not run yet. |
| `hooks/hooks.json` | hook wiring (§6) |
| `skills/agent-lock/SKILL.md` | the agent-facing skill |
| `rust/` | the crate: `src/{main,json,sys,ids,state,rules,hooks,msg,git,shim,cli,lockfiles}.rs`, `tests/`, `build.sh` |
| `install.sh` | shell CLI install (no Node) |

**Distribution**: a Claude Code plugin, installed with
`claude plugin install agent-lock --marketplace lovieco/agent-lock` (older Claude Code:
`/plugin marketplace add lovieco/agent-lock`, then
`/plugin install agent-lock@lovieco`). No per-project files. Teams commit `enabledPlugins` + `extraKnownMarketplaces` to
`.claude/settings.json`; `agent-lock install` writes exactly that.

**Non-goals**: cross-machine locks (machines coordinate through `push` and
`sync`), line-range locks, defense against malicious agents, files outside a
git worktree, Windows.

## 1. Vocabulary and ids

| Term | Meaning |
|---|---|
| **claim** | One agent's exclusive ownership of one file's uncommitted changes, from its first edit until it commits (or releases) the file. One record in `claims`. |
| **agent id** | Main agent: `<sid8>`. Subagent: `<sid8>:<sub8>`. Script: any string without `:`. |
| **family** | The id before `:`. A main agent and its subagents share it. |
| **owner** | The process a claim, session or lock belongs to: `{pid, start}` (§4). |
| **live claim** | A claim that is not stale (§5.2). |
| **others** | Live claims held by any id other than the caller (`rules::others`). |
| **multi-agent** | For a caller: `others` is non-empty, or another family's session is not `ended` and its owner is alive (ownerless: seen < `TTL`) (`rules::multi_agent`). TAKEOVER denials, the Stop reminder, the dirty-file note and the stray-commit check apply only then. |
| **marker** | A claim's `pending` field: a tool call or a process is writing the file now (§4.3). |
| **orphan** | A note that a path may hold uncommitted changes of an agent whose claim ended without a commit (§5.5). |
| **want / blocked** | A recorded wait: on the holder's session (`wants`) and the waiter's (`blocked`) (§5.6). |
| **state file** | `<gitdir>/agent-lock.json` (§2). |
| **data dir** | `$CLAUDE_PLUGIN_DATA`, else `$TMPDIR/agent-lock-<uid>` (`sys::data_dir`). |
| **session index** | `<data dir>/sessions.json`: the repos each session touched (§2.6). |

Ids (`ids.rs`):

- `sid8(session_id)`: the first 8 chars, lower-cased, when all are ASCII
  alphanumeric; else a hash (FNV-1a 64, top 32 bits, 8 hex chars).
- `sub8(agent_id)` (Claude Code's subagent `agent_id`): the first 8 chars of
  a trailing run of 16 hex chars, lower-cased; else the hash.
- `normalize` (what `--as` and `$AGENT_LOCK_AGENT_ID` accept): strips a v2
  `claude-code-sess-` prefix; `x:y` → both halves lower-cased; a 36-char UUID
  → its `sid8`; 8 hex chars → lower-cased; anything else is kept as typed (a
  script id).
- `caller`: `--as` › `$AGENT_LOCK_AGENT_ID` › `sid8($CLAUDE_CODE_SESSION_ID)`.
  Hooks do not use it: they read `session_id` and `agent_id` from the payload.

Example: session `a1b2c3d4-1111-…` is agent `a1b2c3d4`; its subagent
`a479a55566d243534` is `a1b2c3d4:479a5556`.

## 2. The state file

### 2.1 Location

One file per worktree: `<gitdir>/agent-lock.json`, where `<gitdir>` is that
worktree's git dir (`.git`, or the `gitdir:` target of a `.git` file; a
linked worktree has its own). Never committed. `sys::find_repo` walks up
from a directory to the first `.git` whose git dir holds a `HEAD`; a
directory with a `.git` path component is in no worktree. The first
transaction creates the file.

It holds every claim, session (with its subagents, waits and Bash
snapshots), the `git`, `pkg` and `sync` locks, orphans, the last integrated
upstream tip and the off switch. Other files: the session index and shim
links in the data dir (§2.6, §7.1), and short-lived temp files in the git
dir (§8.3, §9.3).

### 2.2 Schema

`State::to_json` (`state.rs`), format version `VERSION = 3`, 2-space
indentation. `v`, `claims`, `sessions` and `locks` are always present; a
null field and an empty optional collection are left out. Times are Unix
seconds.

```jsonc
{
  "v": 3,
  "off": true,                        // only when switched off (`agent-lock off`)
  "claims": {
    "<path>": {                       // repo-relative, '/'-separated, git's spelling, as first written
      "by": "a1b2c3d4",               // agent id
      "session": "<session_id>",
      "owner": {"pid": 80536, "start": 1792881131610395},
      "at": 1792881131,               // created
      "touched": 1792881131,          // last activity on the claim
      "edits": 1,                     // writes seen
      "pending": "toolu_…",           // marker (§4.3)
      "made_by": "toolu_…",           // what created it: the tool call, "pkg" (package shim) or the commit marker (`agent-lock commit`)
      "inherited": "a1b2c3d4",        // whose uncommitted edits came with a hand-over to a subagent
      "reason": "wait"                // a `wait --claim` reservation
    }
  },
  "sessions": {
    "<session_id>": {
      "agent": "a1b2c3d4",            // main agent id
      "owner": {"pid": 80536, "start": 1792881131610395},
      "started": 1792881131,
      "seen": 1792881131,             // last hook of this session
      "phase": "active",              // active | idle | ended
      "end_reason": "clear",          // SessionEnd's reason, or "clear" for a /clear hand-off
      "subs": {"a1b2c3d4:479a5556": {"seen": 0, "done": false, "busy": 1}},   // busy: tool calls in flight
      "wants": [{"waiter": "b5e6f7a8", "path": "a.txt", "at": 0, "told": false}],
      "blocked": {"b5e6f7a8": {"path": "a.txt", "holder": "a1b2c3d4", "at": 0}},
      "nagged": ["a.txt"],            // files the Stop reminder already named
      "takeover": {"path": "a.txt", "agent": "…", "from": "…", "at": 0},   // a denied takeover awaiting its retry
      "bash": {"<tool_use_id>": {"agent": "…", "at": 0, "files": [["a.txt", 1792881131000000000, 7]]}},  // [path, mtime ns, size]
      "bg": ["a1b2c3d4:479a5556"]     // subagents Stop last saw running in the background
    }
  },
  "locks": {                          // each entry only while held
    "git":  {"by": "…", "owner": {…}, "since": 0, "what": "commit"},        // commit | sync | push
    "pkg":  {"by": "…", "owner": {…}, "since": 0, "what": "npm install x"},
    "sync": {"by": "…", "owner": {…}, "since": 0, "head": "<oid>", "tip": "<oid>",
             "paths": ["…"], "index_tmp": "<gitdir>/agent-lock-sync-<tag>.index", "lock_ino": 0}
  },
  "orphans": {"<path>": {"from": "a1b2c3d4", "at": 0}},
  "upstream": {"remote": "origin", "branch": "main", "oid": "<oid>"}       // last integrated upstream tip
}
```

Reading (`from_json`): a record without its required fields (a claim's or
session's `by`/`agent`, a lock's `by`, a sync lock's `head`/`tip`,
upstream's three) is dropped; other missing fields default to 0, false or
empty; unknown fields are ignored; a `pending` that is not a well-formed
process marker (e.g. a bare `"pkg"`) reads as a tool marker. mtime ns is stored as a JSON number
(f64), so Bash stamps are compared at f64 precision (`hooks::same_stamp`).

### 2.3 Key spelling

A claim's path is git's spelling of the repo-relative path, so claim keys
match what `git status` prints (`sys.rs`):

1. `canonical(file, cwd)`: `.` and `..` resolved lexically; an existing
   non-symlink leaf → `realpath` (its one on-disk spelling, whatever case or
   Unicode form was typed on macOS); a symlink leaf keeps its own name, as
   its directory entry spells it; a missing path → realpath of the deepest
   existing ancestor + the rest as typed.
2. `locate(file, cwd)`: a symlink to a file inside a worktree means that file
   (its repo, its path). A path inside a git dir or outside every worktree
   gets no claim.
3. `git_spelling(repo, rel)`, macOS and non-ASCII paths only (ASCII paths and
   Linux: unchanged, no process): git precomposes (NFC) while the disk may
   hold NFD, so git is asked once: `git ls-files -z --cached --others --
   :(literal[,icase])<rel>`, and exactly one hit is the spelling; else
   `git rev-parse --show-prefix` of the deepest existing directory + the rest
   as typed. post-edit re-keys a new file's claim once the file exists (§6.3).
4. `State::key`: under `core.ignorecase` (read from the common dir's
   `config`, `Repo::ignore_case`) keys are lower-cased; else exact. The JSON
   keeps the spelling as first written; every lookup goes through `key`.

So `src/Logger.ts` and `src/logger.ts` are one claim on a case-folding
volume, and the NFC and NFD spellings of a name are one claim on macOS.

### 2.4 Pruning

`state::prune` runs inside every transaction that changes the state, just
before the write. `DAY` = 86,400 s.

| Entry | Dropped when |
|---|---|
| a session some claim's `session` names | never |
| an ended session | `now − seen ≥ DAY`, or its owner is dead or unknown |
| any other session | owner dead or unknown and `now − seen > 3600`; or `now − seen ≥ 30 × DAY` |
| a `wants` entry | older than 1,800 s |
| a `bash` snapshot, a `blocked` entry | older than 3,600 s |
| `takeover` | older than 600 s |
| an orphan | older than 7 × DAY |
| `locks.git`, `locks.pkg` | owner dead (an ownerless entry is kept) |
| `locks.sync` | never: the next `agent-lock commit`, `sync`, `push` or `doctor --fix` recovers it (§9.4) |

### 2.5 Version guard and corruption

- `v` > 3 (written by a newer agent-lock): `read` loads the known fields;
  `transact` returns an error without running its closure, so the file is
  never rewritten and no field is lost. Callers fail open (§3.2).
- Unparseable, or no `v`: the text is copied to
  `<gitdir>/agent-lock.json.bad` (`doctor` reports it) and the state restarts
  empty.
- Not UTF-8: treated the same way (raw bytes copied to `.bad`, state restarts
  empty).
- An empty or missing file is an empty state.

### 2.6 Session index

`<data dir>/sessions.json`, one entry per session id:
`{"repos": [<git dir>…], "shown": [<agent ids that got the rules>], "seen": t,
"ended": bool, "end_reason": str|null, "owner": {pid, start}, "bg":
[<subagent ids>], "workflows": [<task ids>]}`. Session-level hooks act in the
cwd repo plus every listed repo that still exists, so work in a second repo
is handed back and cleaned up too. Hooks read their own entry without a lock
(a hint) and change the file only through `state::transact_json` (the
transaction of §3, for any small JSON file). SessionStart, SessionEnd,
SubagentStop, post-agent and Stop write what they own; the other hooks write
only when a repo or a `shown` agent is new, or the entry is a day old.
SessionStart drops entries not seen for 30 days (`INDEX_DAYS`).

## 3. Transactions

`state::transact(repo, f)`:

1. `open(<gitdir>/agent-lock.json, read + write + create)`. The file is never
   truncated on open and never replaced, so its inode stays the same and
   every waiter locks the same file.
2. `flock` exclusive: `File::try_lock` polled at 200 µs doubling to 10 ms, for
   up to `LOCK_WAIT` = 5 s; then a `TimedOut` error.
3. Read; parse the first JSON value (`json::parse_prefix`: trailing bytes are
   ignored). Version guard (§2.5).
4. Run `f` on the state in memory.
5. Only if the state changed (or the file was empty): prune (§2.4),
   serialize, and if the text differs, one `write` at offset 0, then
   `set_len` (truncate).
6. Close. The kernel drops the lock, also when the process dies, so no lock
   is ever stuck.

`state::read` takes a shared flock with the same wait; a missing file is an
empty state. `state::peek` reads without a lock and returns None for a torn
or unreadable file; it is used only when the lock cannot be had.

### 3.1 Rules

- One logical change is one transaction: a check and the change it justifies
  are never split. `rules.rs` holds the checks as pure functions over `State`.
- No transaction is held while git, a package manager or the session-index
  lock runs. Operations that need both bracket the external call with two
  transactions (Stop cleanup, commit mark/settle, the package shim, sync
  install).
- Crash safety: a process killed between the write and the truncate leaves
  the tail of an older, longer text, which the parser ignores. A file torn
  any other way (power loss) fails to parse and is reset (§2.5).
- Measured: 2,000 contended transactions from 8 threads (each a fresh open +
  flock + read-increment-write) take about 0.1 s in a release build (0.15 s
  in a debug test build) with no lost update
  (`state::tests::concurrent_processes_lose_no_update`).

### 3.2 Fail open

Every hook error or panic → exit 0, no output (a panic prints `[agent-lock]
internal error (ignored): …` on stderr). Every shim error → the real tool
runs unchanged. When the state cannot be read or written (lock held > 5 s,
I/O error, newer version):

| Caller | Behavior |
|---|---|
| hooks | nothing; the tool call proceeds. pre-edit first `peek`s the last-written state: another agent's claim on this path → deny `msg::state_busy` ("retry it in a moment"); else allow and print `agent-lock: <state file>: <error>; this edit of <rel> was not coordinated` on stderr. |
| git shim | judges from `peek`; nothing readable → runs git. |
| package shims | run the tool as asked. |
| CLI | exit 1: `agent-lock: cannot update <state file>: <error>`. |

## 4. Liveness

### 4.1 Owners

`sys::Owner {pid, start}`: a pid plus its start time, so a reused pid does
not look alive.

| Whose | Owner |
|---|---|
| hook calls, the claims and sessions hooks write | the Claude Code process, `$CLAUDE_PID` (`sys::claude_owner`) |
| claims the CLI makes (`commit`, `wait --claim`) | `$CLAUDE_PID`, else the caller's session's owner |
| `locks.git`, `locks.sync`, commit markers | the agent-lock process itself (`sys::self_owner`) |
| `locks.pkg`, pkg markers | the shim, then the tool it spawned (§7.8) |
| scripts without `$CLAUDE_PID` | none: such claims expire by `TTL` |

`start`: macOS `proc_pidinfo(PROC_PIDTBSDINFO)`, µs since the epoch, with
`sysctl(KERN_PROC_PID)` as the fallback (it also reads zombies and other
users' processes); Linux `/proc/<pid>/stat` field 22 (clock ticks since
boot). No child process.

### 4.2 `sys::liveness(owner)`

1. `kill(pid, 0)` fails with `ESRCH` → Dead (`EPERM` counts as existing).
2. A zombie (exited, not reaped: macOS `p_stat == SZOMB`, Linux state `Z` or
   `X`) → Dead.
3. `start != 0` and the process's start time differs → Dead (pid reused).
4. Otherwise Alive, also when the start time cannot be read. `start == 0`
   means unknown: only the pid is checked.

### 4.3 Markers

A claim's `pending` says a write is in flight (`rules::marker`):

| Value | Meaning | Cleared by |
|---|---|---|
| a `tool_use_id` (or any other text) | a Write/Edit call between PreToolUse and PostToolUse | post-edit, edit-failed; also on main's claims at UserPromptSubmit and Stop (no main tool call is in flight then), and on a finished subagent's claims when they are handed back |
| `commit:<pid>:<start>` | an `agent-lock commit` is committing the file | its settle step; once its process is dead, the next `agent-lock commit`, `release` or `doctor --fix` (`unstick`), or hook cleanup |
| `pkg:<pid>:<start>` | a package-manager run will rewrite the file | that run; once dead, as above |

A process marker is three `:`-separated parts: a lower-case ASCII kind, a
numeric pid and start (`rules::proc_marker`). While its process is alive the
claim is never stale (§5.2), however long the commit or install takes. Any
marker blocks a hand-over (§5.1). `rules::marker_dead`: a process marker
whose process is dead; anyone may clear it. A tool marker is never "dead":
hooks decide when its call ended.

## 5. Claim rules (`rules.rs`)

Every function here is pure over `State` and runs inside one transaction;
`Ctx {now, live}` carries the clock and the liveness check.

### 5.1 Acquire

`acquire(st, path, ask, ctx) -> Result<Got, Held>`, `ask` = {agent,
session, owner, pending, made_by, reason}. No claim → **Created** (edits 0;
pending, made_by and reason from `ask`). Else `conflict(st, claim,
requester)`:

| Holder vs requester | Result |
|---|---|
| same id | **Own** → Refreshed: `touched = now`; pending, owner and session updated when given; an edit without `reason` clears a reservation's `reason` |
| the claim has any `pending` | **Block** (a write in flight never changes hands) |
| same family, holder is main, requester is its subagent | **Transfer**; `inherited` = holder when the claim has edits |
| same family, holder is a subagent that is done and not in `session.bg` | **Transfer** (to main or a sibling; `inherited` is set only when the receiver is a subagent and the claim has edits) |
| anything else | **Block** |

Transfer (`hand_over`): `by` = requester; session and owner updated;
`pending`, `made_by` and `reason` cleared, then `pending` from `ask`;
`touched = now`. Result: `Transferred {from, inherited}`.

Block: when `stale(...)` gives a reason, a fresh claim replaces the old one:
**TookOver {from, why}**. Else **Held {claim, own_sub, idle, alive}**
(`own_sub`: the holder is a subagent of the requester's family; `idle` = now
− last activity; `alive`: the holder's owner liveness).

### 5.2 Staleness

`stale(st, claim, ctx, requester_owner)`: the first row that applies.

| # | Condition | Result |
|---|---|---|
| 1 | `pending` is a process marker whose process is alive | live |
| 2 | the owner (the claim's, else its session's) is dead | `OwnerGone`: "its process has exited" |
| 3 | its session is `ended` and its owner equals the requester's (the same Claude process, e.g. after `/clear`) | `SessionEnded` |
| 4 | owner known, `edits > 0` | live, however long it idles |
| 5 | owner known, `edits == 0`, idle ≥ `UNWRITTEN_GRACE` | `Unwritten`: "its edit never ran" |
| 6 | no owner, idle ≥ `TTL` | `Idle` |
| 7 | otherwise | live |

idle = now − max(claim.touched, session.seen) (`last_active`).

| Constant | Value | Use |
|---|---|---|
| `rules::TTL` | 600 s | ownerless claims; an ownerless session counts for multi-agent this long |
| `rules::UNWRITTEN_GRACE` | 45 min (2,700 s) | a live owner's never-written claim (an edit rejected at a prompt or cancelled) |
| `rules::SUB_IDLE` | 30 min (1,800 s) | Stop's fallback when Claude Code sends no `background_tasks`: an unfinished subagent with no call in flight and no hook for this long counts as finished |
| `hooks::TAKEOVER_RETRY` | 300 s | a denied takeover's retry within this window proceeds |
| `hooks::NEVER_RAN` | 60 s | a claim with 0 edits and a marker this old is reported as "its edit never ran" |

So an alive owner keeps its dirty files indefinitely; only a user-approved
`release --force` moves them.

### 5.3 Subagents and hand-overs

- `sub_done(id)`: the session has no record of the subagent, or it is
  `done`. `sub_idle(id, now, limit)`: known, not done, `busy == 0`, no hook
  for `limit`. `live_subs(main)`: some subagent of the family is not done.
- `transfer_all(st, pred, to, …)`: hands every matching claim to `to`,
  skipping claims with any `pending`.
- A finished subagent's files go back to main at SubagentStop,
  UserPromptSubmit, Stop and SessionStart (§6.3).
- `inherited`: set when a claim with edits moves to a subagent. The
  subagent's default commit leaves such a claim out while it is dirty (§8.1);
  a BLOCKED parent is told its changes moved with the file.

### 5.4 Writes and failures

- `wrote(path, ask, tool_use_id)`: own claim → `edits += 1`, `touched =
  now`, `reason` cleared, `pending` cleared when it is this call's
  (**Counted**). Another agent's claim → **NotOwner(by)** (a warning, §6.3).
  No claim → a claim with edits 1 is created (**PostHoc**: a write no
  PreToolUse saw).
- `failed(path, agent, tool_use_id)`: the call failed or was denied. A claim
  this very call created (`made_by` = the call, edits 0) is removed; else only
  its marker is cleared.

### 5.5 Release, commit, orphans

- `release(path, agent?, force)`: only the holder may, unless `force`. A
  claim with `edits > 0` dropped without a commit becomes an orphan `{path,
  from, at}`. Waits on the path are dropped.
- `committed(path, agent)`: removes the claim, its orphan and the waits on it
  (the changes are in a commit).
- A takeover of a claim with edits also records an orphan. The next agent to
  create a claim on that path is told (`msg::orphan_note`) and the orphan is
  removed; a package run that changes the file removes it too. Orphans are
  pruned after 7 days.

### 5.6 Shared files (`shared.rs`)

- `State.shared` (not stored) = the built-in patterns (manifests, lockfiles,
  `tsconfig*.json`, `*.config.{js,cjs,mjs,ts}`, `.gitignore`, `CLAUDE.md`, …)
  then `<top>/.agent-lock-shared`: one glob per line, `#` comments, `!pat`
  excludes, last match wins; no `/` → matches the file name, else the path
  from the top; `*`/`?` stay in a segment, `**` crosses. Loaded by `transact`,
  `read` and `peek`.
- `Claim.with`: co-holders besides `by` (JSON `with`, omitted when empty).
  `holds(a)` = `by` or in `with`.
- `conflict`: holder `by` with no co-holders, or with no `pending` → Own. Any
  `pending` → Block (one write at a time, for every holder). In `with` → Own.
  Family rules as before. Then a shared path → **Join**; else Block.
- `acquire`: a Tool marker on a shared path idle ≥ `SHARED_IN_FLIGHT` (120 s)
  is cleared first (a lost PostToolUse). Join on a claim with no co-holders
  whose holder is `stale` → handled as Block (TookOver). Join appends the
  requester to `with`, sets `pending`, `touched`: **Joined {with}** (the other
  holders; `msg::shared_note`).
- `wrote`: a holder's write counts; a write to an unheld shared claim with no
  marker joins it. `failed`: holders only; only a sole holder's created claim
  is removed.
- `release(path, agent, false)` with co-holders: the agent leaves (`by` →
  `with[0]` when it was `by`), no orphan, waits kept. `committed(path, agent)`:
  any holder → the claim is removed for all.
- Commit (§8.1): a held or joinable shared path is `Act::Join(others)`,
  annotated `(shared; with changes by <ids>)`; `mark` joins it.

### 5.7 Waits and deadlocks

- `record_wait(path, requester, holder)`: `blocked[requester] = {path,
  holder, at}` on the requester's session, `wants += {waiter, path, at,
  told: false}` on the holder's.
- `wait_chain(requester, holder)`: from `holder`, follow `blocked` to the
  path it waits for, then to that path's current holder, up to 8 hops;
  reaching `requester` is a deadlock and returns the chain. Each edge follows
  the file's holder now, not the one recorded.
- `take_notices(agent)`: wants on files `agent` still holds that it was not
  told about; marks them told (one notice per waiter and file).

## 6. Hooks (`hooks.rs`, `hooks/hooks.json`)

### 6.1 Wiring

SessionStart runs `"${CLAUDE_PLUGIN_ROOT}/bin/agent-lock" hook
session-start` (the launcher). Every other hook runs the link SessionStart
keeps (§7.1), skipping the launcher, and falls back to it:

```sh
a="$CLAUDE_PLUGIN_DATA/shims/core/agent-lock"; [ -x "$a" ] || a="${CLAUDE_PLUGIN_ROOT}/bin/agent-lock"; exec "$a" hook <name>
```

Timeout 10 s each.

| Event | Matcher | Hook | Does |
|---|---|---|---|
| SessionStart | `startup\|resume\|clear\|compact` | `session-start` | shim links, env file, session record, /clear hand-off, subagents' claims back to main, rules context |
| SubagentStart | — | `subagent-start` | subagent record, subagent rules context |
| UserPromptSubmit | — | `prompt` | settle subagents, clear main's markers, hand finished subagents' claims back, waiter notices |
| PreToolUse | `Write\|Edit\|MultiEdit\|NotebookEdit` | `pre-edit` | claim the file or deny (BLOCKED, DEADLOCK, TAKEOVER, sync busy) |
| PreToolUse | `Bash` | `pre-bash` | deny a wrong agent-lock identity; ask before kill switches and lock-state writes; snapshot claimed files |
| PostToolUse | `Write\|Edit\|MultiEdit\|NotebookEdit` | `post-edit` | count the write |
| PostToolUse | `Bash` | `post-bash` | report writes to others' claimed files |
| PostToolUse | `Agent\|Task\|Workflow` | `post-agent` | record background launches in the index |
| PostToolUseFailure | edit tools | `edit-failed` | undo a claim the failed call created |
| PostToolUseFailure | `Bash` | `post-bash` | as PostToolUse |
| PermissionDenied | edit tools | `edit-failed` | as PostToolUseFailure |
| PermissionDenied | `Bash` | `post-bash` | bookkeeping only, no output |
| SubagentStop | — | `subagent-stop` | subagent done, its claims to main |
| Stop | — | `stop` | idle, settle subagents, release clean and unwritten claims, commit reminder |
| SessionEnd | — | `session-end` | release unwritten claims, session ended |

### 6.2 Every hook

- `CLAUDE_FILE_LOCK=0` → exit 0 without reading stdin.
- stdin ≤ 64 MiB (`MAX_INPUT`): one JSON object with `session_id`; anything
  else → nothing.
- Identity: `agent = agent_id(session_id, agent_id)`, `main =
  sid8(session_id)`, `owner = $CLAUDE_PID`. cwd = payload `cwd` ›
  `$CLAUDE_PROJECT_DIR` › the process cwd, canonicalized.
- Repos: edit hooks act in the edited file's repo (`sys::locate`; outside a
  worktree → nothing). Session-level hooks act in the cwd repo plus the
  session index's repos. A repo whose state is `off` is skipped.
- Every hook that changes a repo's state refreshes its session record there
  (`see_session`: created on first sight, `seen = now`, owner set);
  post-agent changes only the session index, and a pre-bash identity denial
  changes nothing.
- Output, at most one object, exit 0 always:
  - deny/ask: `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny"|"ask","permissionDecisionReason":…}}`;
  - context: `{"hookSpecificOutput":{"hookEventName":<event>,"additionalContext":…}}`
    (parts joined by a blank line); `"allow"` is never sent;
  - Stop: `{"decision":"block","reason":…}`.

### 6.3 Per hook

**session-start** (payload `source`: startup | resume | clear | compact)

1. Shim links and env file lines (§7.1).
2. Index transaction: drop entries older than 30 days. On `clear`: adopt the
   repos of other entries with the same owner process (unless they ended
   for a reason other than "clear") and mark them ended "clear". This entry:
   repos ∪ the cwd repo, `seen`, `ended: false`, owner; except on compact,
   `bg` and `workflows` are dropped.
3. Per repo transaction: phase active. compact: drop `blocked[main]`. Else
   every subagent is done (busy 0) and `bg`, `blocked` and `bash` are cleared:
   subagents never survive a restart, `/clear` or `/resume`. On `clear`,
   other sessions of the same owner process (not ended for another reason)
   are marked ended "clear" and their families' claims come to this main
   agent. This session's subagent claims go to main (compact: only finished
   subagents'; else all, markers cleared). Main claims with another owner
   get the current owner and session (a resume after an exit).
4. Any repo active → context `msg::session_context`; the agent is marked
   `shown` in the index.

**subagent-start**: subagents only. `subs[agent] = {seen, done: false, busy:
0}` in every session repo; context `msg::subagent_context`.

**prompt** (UserPromptSubmit): phase active. Unless the index lists
running workflows, every unfinished subagent not in `bg` (session or index)
is marked done: a foreground subagent cannot outlive its turn. Markers on
main claims are cleared (tool markers and dead process markers). Finished
subagents' claims (done, not in `bg`) go to main. Waiter notices for main.

**pre-edit** (file = `tool_input.file_path` › `notebook_path`; a NUL →
nothing):

1. Outside a worktree → nothing.
2. `locks.sync` lists the path and its owner is alive → deny `msg::sync_busy`.
3. `acquire` with pending = made_by = `tool_use_id`:
   - Created: an orphan from another agent → note `msg::orphan_note` (the
     orphan is removed). No orphan and multi-agent → after the transaction,
     if the file exists and one `git status -- <path>` reports it changed →
     note `msg::dirty_note`.
   - Transferred with `inherited` → note `msg::inherited`.
   - TookOver from a claim with edits: orphan recorded. Multi-agent and no
     matching `session.takeover` (same path, from and agent, < 300 s) →
     store it, clear the new claim's marker and deny `msg::takeover`: the
     claim is now the requester's, and a retry proceeds. Else note
     `msg::takeover_note`. A takeover of a claim without edits is silent.
   - Held: `record_wait`; a deadlock → deny `msg::deadlock`; else deny
     `msg::blocked`.
4. Allowed: `blocked[agent]` removed; a subagent's `busy` +1 (and `done =
   false`: a call proves it runs).
5. An agent not yet `shown` in the index gets its session or subagent context
   first (once per agent per session).
6. State lock not obtainable → §3.2.

**post-edit**: a subagent's `busy` −1. A claim this call created for a new
file is moved to the path's git spelling now that the file exists. `wrote`:
NotOwner → context `msg::not_owner`. Waiter notices for the agent.

**edit-failed**: `busy` −1; `failed`.

**pre-bash**: the scan of §6.4. When the call is allowed, in each session
repo: a subagent's `busy` +1, and with a `tool_use_id`, `bash[tool_use_id] =
{agent, at, files}` with the (mtime ns, size) stamps of the caller's own
claimed files and of others' live claims (stored only when there are any).
When the shell's own repo is off, nothing is printed.

**post-bash**: `busy` −1; the snapshot is removed. Unless
`tool_response.backgroundTaskId` is set, each stamp that changed: the
caller's own file → `touched = now`, `edits ≥ 1`; another agent's file whose
claim was not touched since the snapshot and whose holder has no Bash call
in flight → listed in `msg::bash_wrote`. Waiter notices. On PermissionDenied:
bookkeeping only, no output.

**post-agent**: a `tool_response` (object or JSON string) with `status:
"async_launched"` keeps running after the turn. With `agentId` → index `bg`
+= its agent id; without → index `workflows` += `taskId` › `task_id` ›
`tool_use_id`. Only the index changes; claims follow at prompt and Stop.

**subagent-stop**: `subs[agent]` done, busy 0; its `blocked` entry, `bash`
snapshots and `bg` entry dropped; its claims go to main (markers cleared).
Index `bg` minus it.

**stop**:

1. `background_tasks`, when present: tasks with status running or pending;
   types subagent | agent | local_agent → their agent ids; workflow |
   local_workflow | teammate | in_process_teammate → unmapped (any
   unfinished subagent may be one of their workers). The index `bg` and
   `workflows` are updated.
2. Per repo transaction: phase idle. With the list: subagents not listed are
   done (unless unmapped tasks run and the subagent is not done); `bg` = the
   listed ids. Running = listed ids, plus unfinished subagents when unmapped
   tasks run; without the list: not done and not idle for `SUB_IDLE`.
   Finished holders' claims go to main; markers on main claims are cleared;
   `blocked` and `bash` are kept only for running agents. Main claims with 0
   edits, no marker and `reason` ≠ "wait" are released. Candidates: main
   claims with edits and no marker.
3. No transaction held: `git::clean(candidates)` (positively clean vs HEAD;
   anything git does not confirm counts as dirty), and `git::stash_time` when
   any is clean.
4. Per repo transaction: a clean candidate unchanged since step 2 is released
   as committed, unless refs/stash was written at or after its `touched` (a
   hook may have stashed the work). The remaining dirty main claims are the set. The
   reminder is due when `stop_hook_active` is not true, the agent is in
   multi-agent mode, the set is non-empty, no background agent-like task
   runs, and some file is not in `nagged` or another agent wants one. Then
   `nagged ∪= set` and the hook blocks with `msg::commit_reminder`.

**session-end** (payload `reason`, default "other"): the family's claims with
0 edits are released unless a live process marker holds them; dirty ones
stay (SessionEnd is not death: `/clear` and `/resume` continue in the same
process). Phase ended, `end_reason`; subagents done; `blocked` and `bash`
cleared. Index: `ended`, `end_reason`. No git.

### 6.4 The Bash command scan (pre-bash)

`hooks::sh` reads the command like a POSIX shell: every simple command, also
inside `$(…)`, backticks and process substitution; words with source spans,
quotes removed, expansions marked dynamic; output redirections; heredoc
bodies and here-strings are data. `bash|sh|zsh|dash|ksh -c <string>` and
`eval` are read recursively (depth < 4). Wrappers are skipped to the real
command (`env`, `sudo`, `doas`, `nohup`, `builtin`, `exec`, `command`,
`nice`, `stdbuf`, `timeout`, `xargs`, `caffeinate`, `chronic`, `ionice`,
with their value options), as are shell keywords and assignments.

1. **Identity (deny).** An `agent-lock` call (also through `npx`, `bunx`,
   `pnpx` or `node`) of `commit`, `push`, `sync`, `release`, `wait` or `mine`
   runs as: its `--as`/`--as=` value › an `AGENT_LOCK_AGENT_ID=` assignment on
   it or earlier in the line (`export`, `declare`, …; `unset` restores the
   default) › the caller's family id (what the session environment says).
   When that does not normalize to the caller's id, or is dynamic → deny
   `msg::identity` with the corrected command: `--as <id>` inserted after the
   subcommand, or the wrong value replaced. A command over 4,000 chars, or a
   fix that cannot be placed, gets "(add --as <id> to every agent-lock
   command)".
2. **Ask** (reasons joined by newlines):
   - an assignment `CLAUDE_FILE_LOCK=0`, or a dynamic value → `msg::ask_killswitch`;
   - `agent-lock off` → `msg::ask_off`; `agent-lock release … --force|-f` →
     `msg::ask_force_release`;
   - a write or removal that reaches `agent-lock.json` (in any directory),
     `index.lock` inside a `.git` directory, or removes `.git` itself →
     `msg::ask_internals`. Checked: output redirections; `rm`, `unlink`,
     `rmdir`, `shred`, `srm`; `mv` (sources and target); `cp`, `install`,
     `ln`, `rsync` (target; a `.git` target directory means
     `<target>/<basename>`); `tee`, `truncate`, `touch`; `sed`, `gsed`, `perl
     -i`; `dd of=`; `find` with `-delete`, `-exec`, `-execdir`, `-ok` or
     `-okdir` whose `-name`/`-path` pattern matches a lock name (or rooted at
     `.git` without a name test). Globs are matched.
3. A deny wins over an ask.

## 7. Shims (`shim.rs`, `lockfiles.rs`)

### 7.1 On the agents' PATH

SessionStart (`hooks::shim_links`) keeps symlinks to
`std::env::current_exe()`, each made as `.<name>.<pid>.tmp` + rename:

- `<data dir>/shims/core/git` and `<data dir>/shims/core/agent-lock`;
- `<data dir>/shims/<tool>/<tool>` for npm, pnpm, yarn, bun, cargo, uv,
  poetry, go, bundle, bundler.

The dir is machine-wide: only a `startup` session repoints an existing
working link, so an older session's resume or compact never downgrades the
others. It appends to `$CLAUDE_ENV_FILE` (sourced before every Bash call),
each line once:

```sh
export AGENT_LOCK_AGENT_ID=a1b2c3d4
_alp=; for _alt in npm pnpm yarn bun cargo uv poetry go bundle bundler; do command -v "$_alt" >/dev/null 2>&1 && _alp="<data>/shims/$_alt:$_alp"; done; export PATH="<data>/shims/core:$_alp$PATH"; unset _alp _alt
export GIT_OPTIONAL_LOCKS=0
```

A package-manager shim is on PATH only where the shell finds the real tool,
so `command -v pnpm` stays false where pnpm is not installed.
`GIT_OPTIONAL_LOCKS=0` keeps the agents' read-only git off index.lock.

### 7.2 Dispatch

- argv[0]'s file name picks the shim. Arguments that are not all UTF-8 are
  judged on a lossy copy and, for git, also on a copy where each such
  argument is `.` (as if it named the whole tree); refused when either is.
  The tool always gets the raw bytes.
- The real tool: the first `<name>` on PATH that is not an agent-lock binary
  (canonical path is this executable, or the file name is `agent-lock` or
  `agent-lock-*`) (`sys::real_tool`). For git on macOS, `/usr/bin/git` is
  replaced by the developer directory's git (`$DEVELOPER_DIR`, the
  `xcode-select` link, CommandLineTools, Xcode.app) to skip the xcrun
  trampoline (`sys::untrampoline`). None → `agent-lock: <name>: command not
  found (no real <name> on PATH after the agent-lock shims)`, exit 127.
- `AGENT_LOCK_SHIM` is set on what a shim runs: `<pid>:<hops>` on what it
  execs (the same process), `^<pid>:1` on what it spawns. A shim that finds
  its own mark (the "real" tool was another agent-lock copy) execs the next
  `<name>` on PATH after its own entry without judging again.
- Pass-through (exec unchanged) when `CLAUDE_FILE_LOCK=0`, there is no caller
  id (`$AGENT_LOCK_AGENT_ID` › `$CLAUDE_CODE_SESSION_ID`), no worktree, the
  state is `off`, or anything fails or panics.

### 7.3 git: inputs

Global options (`parse_call`): `-C <dir>` (chained), `-c k=v`,
`--config-env=k=VAR`, `--git-dir`, `--work-tree` (and `GIT_DIR`,
`GIT_WORK_TREE`), `--namespace`, `--super-prefix`, `--attr-source`,
`--literal-pathspecs`, `--glob-pathspecs`, `--noglob-pathspecs`,
`--icase-pathspecs` (and their `GIT_*_PATHSPECS` variables), `-p`, `-P`,
`--paginate`, `--no-pager`, `--bare`, `--no-replace-objects`,
`--no-lazy-fetch`, `--no-optional-locks`, `--no-advice`. `--version`,
`--help`, a bare `--exec-path`, `--html-path` and unknown options run git as
asked.

**Fast path**: a builtin outside `CHECKED` runs at once, without finding the
repo or reading the state. `CHECKED` = add, stage, commit, stash, reset, rm,
mv, checkout, restore, clean, switch, pull, rebase, merge, cherry-pick,
revert, am, filter-branch, checkout-index, bisect, sparse-checkout,
read-tree.

Otherwise the state is read (shared lock, else `peek`):

- `others`: live claims of every other id;
- `shared`: the caller is a main id and one of its subagents may be running
  (`live_subs`). A subagent's Bash inherits the main id, so the caller may be
  that subagent: the main agent's own live claims (`mine`) then count too,
  for sweeping pathspecs (`.`, a directory, a glob) and whole-tree commands,
  not for a pathspec that names the file itself;
- `multi`: `others` is non-empty, or multi-agent.

Neither `multi` nor `mine` → git runs. Else `classify` (§7.4) and the
verdict (§7.7).

### 7.4 git: classification

Kinds: **Safe** runs. **Sweep** is refused while `others` ∪ `mine` is
non-empty. **Scoped** (pathspecs, or the whole repo) is refused when it
reaches a claim in `others`, or sweeps over one in `mine`. **Clean** is
Scoped limited to claimed files that exist and are untracked.

| Subcommand | Safe | Refused |
|---|---|---|
| `add`, `stage` | `-n`/`--dry-run`, `--refresh`, `-h`; no arguments | pathspecs → Scoped; `--pathspec-from-file` → repo; `-A`/`--all`, `-u`/`--update`, `-p`, `-i`, `-e`, `--renormalize` without paths → repo |
| `commit` | `--dry-run`, `-h` | whole index (no paths and no `--pathspec-from-file`, or `-a`, `-i`/`--include`, `-p`/`--interactive`) or `--amend`: refused while `others` ∪ `mine` is non-empty; when both are empty but `multi`, refused when `git diff --cached` (`-a`: `git diff HEAD`) lists files the caller has not claimed ("stray"). Paths → Scoped; `--pathspec-from-file` → repo |
| `stash` | `list`, `show`, `create`, `store`, `-h` | `pop`, `apply`, `drop`, `clear` → Sweep (the hint names `stash@{n}`); `push` with pathspecs (not `--staged`) → Scoped; anything else (bare, `push`, `save`, `-u`, `-k`, …) → Sweep |
| `reset` | `-h`; no arguments; `-- <paths>`, several paths, `--pathspec-from-file`, `-p` (index only); `HEAD`, `@`, a path | `--hard`, `--merge`, `--keep` → Sweep; one revision → Sweep (it moves the shared branch): `~`/`^`, 7–40 hex chars, `ORIG_HEAD`/`FETCH_HEAD`/`MERGE_HEAD`, `origin/…`, `upstream/…`, `refs/…`, `@{…}`, or any other word that is not a path (a branch) |
| `rm` | `-n`/`--dry-run`, `-h` | pathspecs (also with `--cached`) → Scoped; `--pathspec-from-file` → repo |
| `mv` | `-n`, `-h`, fewer than 2 operands | sources + destination (`<dir>/<basename>` into a directory) → Scoped |
| `checkout` | `-h`; no operands (without `-f`, `-m`, `-p`); `--` with nothing on either side | `-b`, `-B`, `--orphan`, `--detach`, `-`, a single non-path operand, `<branch> --` → Sweep; `-f`/`-m` without operands → Sweep; paths after `--`, a path-like first operand (`.`, `..`, `./…`, `../…`, `:…`, a glob, an existing path), `<rev> <paths>` → Scoped; `-p` without operands, `--pathspec-from-file` → repo |
| `restore` | `-h`; `--staged` without `--worktree`; no operands | paths → Scoped; `-p`, `--pathspec-from-file` → repo |
| `clean` | `-n`, `-h`, no `-f`/`-i` (`clean.requireForce`) | paths (default `.`) → Clean |
| `rebase`, `merge`, `cherry-pick`, `revert`, `am` | `-h`; a recovery flag as the first argument | anything else → Sweep |
| `pull`, `switch`, `filter-branch`, `checkout-index` | `-h` | anything else → Sweep |
| `bisect` | no arguments, `log`, `visualize`, `view`, `help` | anything else → Sweep |
| `sparse-checkout` | no arguments, `list`, `-h` | anything else → Sweep |
| `read-tree` | without `--reset` or `-u` | `--reset`, or `u` in a short-flag cluster → Sweep |
| every other builtin | always | — |

Recovery forms, as the first argument only (`git merge -m --abort` merges):
rebase `--quit`, `--continue`, `--edit-todo`, `--show-current-patch`; merge
`--abort`, `--quit`; cherry-pick and revert `--abort`, `--quit`,
`--continue`; am `--abort`, `--quit`, `--show-current-patch`. rebase
`--abort`/`--skip`, am `--skip` and concluding a merge stay refused while
others work: they discard or commit others' work.

Options are read with git's rules: short options taking a value (attached or
the next argument), combined short flags (`-am`), unique long-option
prefixes.

### 7.5 git: pathspecs

Each pathspec is made repo-relative (`spec`): the cwd's prefix applies;
`:/` and `:(top)` start at the top; an absolute path is canonicalized; `..`
above the top matches nothing. Magic: `:(top,exclude,literal,glob,icase)`
and the short forms `:/`, `:!`, `:^`; unknown magic (e.g. `attr:`) means the
whole repo. A pathspec reaches a path (`reaches`) when it equals it, names a
directory above it, or, as a glob, wildmatches it or a directory above it.
`*` crosses `/` unless `:(glob)` or `--glob-pathspecs` (then only `**`
does); `:(literal)`, `--literal-pathspecs` and `--noglob-pathspecs` turn
globbing off. Exclusions subtract; exclusions alone mean everything else.
Case folds under `core.ignorecase` or `icase`. A literal non-ASCII pathspec
is respelled with `git_spelling`. A pattern that runs out of its budget
(100,000 steps) counts as a match.

### 7.6 git: aliases

A subcommand that is not a builtin is looked up as `alias.<sub>`: `-c
alias.<sub>=…` first (the last one wins), then one `git config --get
alias.<sub>`. Up to 8 expansions; `-c`, pathspec and pager options at the
start of an alias apply. Not an alias, or unresolvable → git runs (and
reports it). A plain alias is judged as its expansion; the refusal says
`(alias for git …)`.

A `!` alias runs in a shell whose PATH starts with git's exec-path, so its
git calls never reach the shim. The shim judges them from the alias text
before git runs: the text is split into simple commands (`;`, `&`, `|`,
newline, braces, parentheses; quotes respected); `$@`, `$*` and `$N` become
the alias's arguments (git also appends them); any other `$…` word becomes
`:/`, the whole tree. Each `git …` command (after assignments,
`command`/`exec`/`env`/`nohup`/`time`/`builtin` and shell keywords) is
judged at the repo top, up to 2 more `!` levels. A command that may run git
in a way not followed (`sh -c '…'`, `xargs git …`, `eval`, `git $cmd`) and
mentions a checked or unknown subcommand makes the whole alias a Sweep.

### 7.7 git: refusal

One line on stderr, exit 1, git is not run:

```
agent-lock: `git add -A` covers files other agents are editing (a.txt by a1b2c3d4). Name only your own files, or commit them with agent-lock commit -m "<msg>" -- <paths>.
```

The list names up to 3 `<path> by <id>`, then "and N more". The tail is a
safe alternative per hint: Commit (`agent-lock commit -m "<msg>" [--
<paths>]`; undo your own change with `git checkout -- <file>`), Reset (make a
new commit; unstage with `git reset -- <paths>`), Branch (stay on the shared
branch), Stash (get stashed work back with `git checkout stash@{n} --
<paths>`), Sync (`agent-lock sync`; or, while a rebase, am, merge,
cherry-pick or revert is in progress or HEAD is detached, which forms are
allowed and when to ask the user). A refused `git commit` suggests the
`agent-lock commit` with its own `-m`/`-F` values. When `shared` made the
main agent's own files count, the line says so and how a subagent names
itself (`AGENT_LOCK_AGENT_ID=<its id> git …`).

### 7.8 Package managers

`classify_pkg` sorts a command into pass-through, **lock** (takes
`locks.pkg` only) or **deps** (also claims manifests and lockfiles):

| Manager | deps | lock | pass-through |
|---|---|---|---|
| npm | `install` (`i`, `add`, …) with packages and without `--no-save`, or with `--package-lock-only`; `uninstall` (`rm`, `remove`, …); `update` (`up`, …); `dedupe`; `audit fix` | `install` without packages, `ci`, `install-test`, `install-ci-test`, `prune` | `-g`/`--global`/`--location=global`, `audit`, other verbs |
| pnpm | `install` with packages or `--fix-lockfile`/`--lockfile-only`; `add`, `remove`, `update`, `dedupe`, `import` | bare `pnpm`, `install` | `-g`, `-v`, `-h`, other verbs |
| yarn | `add`, `remove`, `upgrade`, `upgrade-interactive`, `dedupe`, `import`, `install <args>`; `workspace <name> <cmd>` (that command, scoped to the workspace) | bare `yarn`, `install` | `-v`, `-h`, other verbs |
| bun | `add`, `remove`, `update`, `install <packages>` | `install` | `-g`, other verbs |
| uv | `add`, `remove`, `lock` | `sync`, `pip install`/`uninstall`/`sync` | other |
| poetry | `add`, `remove`, `lock`, `update` | `install`, `sync` | other |
| cargo | `add`, `remove`, `update`, `generate-lockfile` (after an optional `+toolchain`) | — | other (`build`, `test`, …) |
| go | `get`, `mod tidy`, `mod edit` | `mod vendor` | other |
| bundle, bundler | `add`, `update`, `remove`, `lock` | bare, `install`, `i` | `--version`, `--help`, other |

Directory options: npm `--prefix`/`-C`, pnpm `-C`/`--dir`, yarn and bun
`--cwd`, uv `--directory`/`--project`, poetry
`-C`/`--directory`/`-P`/`--project`, cargo `--manifest-path` (its
directory), go `-C`, bundler `--gemfile` (its directory).

Files a deps command claims: the nearest manifest at or above that directory
(not above the worktree top; none → `<dir>/<manifest>`) and its lockfiles
(`lockfiles_for` with the manager). Workspace selectors (npm `-w`,
`--workspace`, `--workspaces`, `--include-workspace-root`; pnpm `--filter`,
`-F`, `--filter-prod`, `-r`, `-w`; `yarn workspace`; bun `--filter`) pick
packages from `pnpm-workspace.yaml` or the `package.json` `workspaces`
(globs: `*` one level, `**` any depth, `!` excludes; dot directories and
`node_modules` skipped) by directory or package name. `-r`, a glob, a graph
selector or one that matches nothing → every package. The root's own
manifest only when asked for.

`lockfiles.rs`:

| Manifest | The manager's lockfiles (`manager`) | Committed with it (`PAIRS`) |
|---|---|---|
| `package.json` | npm: `package-lock.json`, `npm-shrinkwrap.json`; pnpm: `pnpm-lock.yaml`; yarn: `yarn.lock`; bun: `bun.lock`, `bun.lockb` | all six |
| `pyproject.toml` | uv: `uv.lock`; poetry: `poetry.lock` | `uv.lock`, `poetry.lock` |
| `Cargo.toml` | cargo: `Cargo.lock` | `Cargo.lock` |
| `go.mod` | go: `go.sum` | `go.sum` |
| `Gemfile` | bundle, bundler: `Gemfile.lock` | `Gemfile.lock` |

`lockfiles_for`: the existing lockfiles next to the manifest; for `UPWARD`
manifests (`package.json`, `Cargo.toml`, `pyproject.toml`) upward to the top,
where the nearest directory with one wins. With a manager and none existing
yet, the one it will create is predicted at the outermost manifest's
directory (the workspace root), under the first name in its list.

The run (`Pkg::run`):

1. **Take** (one transaction). Off → run as asked. Inside a lifecycle script
   of the install that holds the lock (`$AGENT_LOCK_PKG_HELD` =
   `<since>:<by>` of the current lock, owner alive) → run as asked. The lock
   held by another live owner (ownerless: younger than `PKG_LOCK_MAX` =
   1,200 s) → wait: print ``agent-lock: waiting for `<what>` (<by>, pid N) to
   finish before `<what>`…`` once, poll every 250 ms, give up after
   `$AGENT_LOCK_PKG_WAIT` seconds (default 1,200) with exit 1. Else set
   `locks.pkg = {by, owner: the shim, since, what: the command line, ≤ 100
   chars}` and claim each file with marker `pkg:<pid>:<start>` (`made_by:
   "pkg"`, owner `$CLAUDE_PID`). A file the caller's own family holds (main
   caller) stays with its holder and gets the marker unless one is set.
   Created over an orphan
   → note; TookOver with edits → orphan + takeover note (both on stderr).
   Held by another live agent → the transaction is undone (no lock, no
   claim) and the run is refused, exit 1: ``agent-lock: `<what>` rewrites
   <f>, which <id> is editing (uncommitted). Prefer other work; to wait:
   agent-lock wait --claim <abs> --timeout 540 with run_in_background (a
   subagent adds --as <its id>).`` (a subagent caller gets `wait --as <id>`
   and no parenthesis).
2. Stamp the files; spawn the real tool (stdio inherited) with
   `AGENT_LOCK_PKG_HELD` and `AGENT_LOCK_SHIM=^<pid>:1`. The shim ignores
   SIGINT and SIGQUIT (the terminal delivers them to the tool's process
   group) and forwards SIGHUP, SIGALRM and SIGTERM to the tool.
3. One transaction moves the lock's owner and the markers to the tool's own
   pid and start, so killing the shim alone frees nothing while the tool
   runs.
4. Wait. Exit with the tool's code (signal n → 128 + n).
5. One transaction: release the lock if still ours; clear our markers; a file
   whose stamp changed gets `edits += 1` and `touched` (its orphan is
   dropped: the change is the claim's now); an unchanged file this run
   claimed with 0 edits is unclaimed again.

## 8. Commit

### 8.1 The file set (`cli::commit`)

The caller is required (§10). Message: `-m` (repeatable, joined by a blank
line) or `-F <file|->` (once, not with `-m`); empty → usage error, except
with `--dry-run`.

1. Without paths: the caller's exact-id claims. A subagent's claims carrying
   `inherited` are held back while dirty ("left for your parent to commit").
   With `-- <paths>`: each path is canonicalized; one outside the worktree or
   inside the git dir is a usage error; a file or symlink is taken under its
   git spelling; a directory means the caller's claims under it (none → an
   error).
2. Each path (`Plan::consider`): unclaimed → claimed at commit time; own;
   a finished same-family subagent's (Transfer) → taken over; anyone else's
   → an error, with "it looks abandoned (…): `agent-lock release <abs>` frees
   it" when stale.
3. A symlink in the set that is unchanged while its target in the worktree is
   dirty adds the target ("via symlink"). A manifest's lockfiles (`PAIRS`)
   that are dirty along with it are added ("lockfile of …"); one held by
   another agent is an error.
4. One `git status` decides what is dirty. Any path that step 2 or 3 found
   held by another agent → `nothing was committed:` with the reasons, exit 1.
5. Clean own claims are released when `git::clean` confirms them and
   refs/stash (if any) is older than their `touched`.
6. Nothing dirty → `nothing to commit: …`, exit 0 (`--push` still pushes).
7. `--dry-run` → `commit_paths(dry_run)` and the list; no state changes.

### 8.2 Marking and settling

- SIGHUP, SIGINT and SIGTERM are deferred (caught; one already ignored stays
  ignored) while claims carry the marker; the command then exits 128 + n.
- `mark`, one transaction: clear dead markers (`unstick`); claim unclaimed
  items (`created`); take over finished subagents' items; set `pending =
  commit:<pid>:<start>` and `touched = now`. An item someone else claimed
  since the plan → nothing changes, exit 1 ("re-run").
- `git::commit_paths` (§8.3), then one `git status`. Clean = committed paths
  absent from it, plus what `git::clean` confirms of the rest.
- `settle`, one transaction: a claim whose marker an own edit replaced
  meanwhile is kept (it belongs to that edit); clean and (committed or not
  created) → released as committed; created and nothing landed → removed;
  otherwise the marker is cleared, and a committed path that is dirty again
  keeps the claim with `edits ≥ 1` ("still claimed by you (changed again
  during the commit)"). Dropped unsettled (an error, a panic), it clears the
  markers and removes the claims it created.
- `refuse_dangerous` = some other agent's live claim has edits or a marker.
- Report: `committed <sha12>: N files`, the files, what was not committed and
  why, hook output (≤ 40 lines), inherited notes, then context: files the
  caller's subagents hold, a count of other agents' dirty files, unattributed
  changes (≤ 10 listed). It never says "re-run to include them".

### 8.3 `git::commit_paths(repo, agent, paths, message, no_verify, dry_run, refuse_dangerous)`

1. git ≥ 2.40 (`MIN_VERSION`). Every path relative and non-empty, without
   NUL, `..` or `.git` components. A non-empty message (unless dry run). Not a
   dry run: recover an interrupted sync first (§9.4; one at work → `busy`).
2. `git status --porcelain=v2 -z --untracked-files=all --no-renames
   --ignored=matching -- :(literal[,icase])…`:
   - a case-only rename (a staged delete and add under one key) →
     `case-rename`;
   - a staged removal still on disk (`git rm --cached`) → `staged-untrack`
     (a path-limited commit would re-add it);
   - no entry, or ignored and not deleted → skipped; nothing left → `nothing`
     (ok).
3. Hooks: `core.hooksPath`, else `<common dir>/hooks`; present when
   `pre-commit` or `commit-msg` is executable or any `hook.*` config exists.
   With hooks and `refuse_dangerous`: `dangerous_hooks` → `dangerous-hooks`.
   A hook is dangerous when its text runs `git stash`, `checkout [-f] [--]
   .` or `reset --hard`, is a pre-commit.com hook ("File generated by
   pre-commit"), or runs lint-staged without `--no-stash`. Scanned along
   with it: package.json scripts it runs (`npm|pnpm|yarn|bun [run] <name>`),
   husky files, husky v4 `package.json` hooks, and one level of scripts or
   make recipes it hands off to, each ≤ 256 KiB. Comments count.
4. Preflight, when there are hooks or a path has both staged and unstaged
   changes: a private index `<gitdir>/agent-lock-commit-<tag>.idx` =
   `read-tree HEAD` (or `--empty`) + `add -A` of the paths. With hooks (not
   a dry run):
   - snapshot refs/stash and every other dirty path (fingerprint: mode, size,
     mtime, inode, content hash for files ≤ 16 MiB, link target; its claim
     record; whether its owner has a Bash call in flight);
   - `git hook run --ignore-missing pre-commit`, then `commit-msg --
     <msgfile>`, with `GIT_INDEX_FILE` = the private index;
   - whether they passed or failed: refs/stash moved, or a file changed with
     no change recorded by its owner (claim record unchanged, not in another
     agent's commit, owner running no Bash then or now; an unclaimed
     untracked file only if it vanished) → `hook-clobbered`, claims kept;
   - a hook failed → `hook-failed`.
   Then `write-tree`; any changed path outside the requested set →
   `hook-staged`. The commit's paths = the tree's changes vs HEAD.
5. Dry run → ok with the list.
6. `locks.git` (§9.1; timeout → `busy`). Recover again. When the preflight ran
   and HEAD moved meanwhile in one of our paths → `commit-failed` ("re-run").
7. Untracked paths: `git add -N`. Then `git commit --only --no-verify -q -F
   <msgfile> --pathspec-from-file=- --pathspec-file-nul` with `:(literal)`
   pathspecs. An index.lock error is retried at 25 ms doubling to 500 ms, for
   up to 10 s (`INDEX_LOCK_WAIT`). On failure the `-N` entries are reset:
   nothing stays staged. "nothing to commit" (someone committed exactly this
   meanwhile) → `nothing`; else `commit-failed`.

Codes: `nothing`, `hook-staged`, `hook-clobbered`, `hook-failed`,
`dangerous-hooks`, `case-rename`, `staged-untrack`, `commit-failed`, `busy`.
Hooks run outside `locks.git`; only the commit itself is serialized.

## 9. Sync, push, recover (`git.rs`)

The remote is always `origin`; the branch is HEAD's (detached → `error`).
git ≥ 2.40.

### 9.1 `locks.git`

Serializes agent-lock's own commits, syncs and pushes in a checkout
(`lock_git`). Free when absent, held by this process, its owner is dead, or
older than `GIT_ABANDON` = 600 s. Otherwise poll at 20 ms doubling to 250 ms,
for up to `GIT_WAIT` = 60 s, then `busy`: `another agent-lock <what> (by
<id>, pid N) has been running for Ns; retry shortly`. Released on drop.

### 9.2 sync

`sync(repo, agent, accept_rewrite)`, holding `locks.git` throughout:

1. Recover an interrupted install (§9.4).
2. B = `refs/remotes/origin/<branch>` before the fetch. `git fetch --quiet
   --no-write-fetch-head --no-auto-maintenance origin
   +refs/heads/<branch>:refs/remotes/origin/<branch>`: into the tracking ref,
   never FETCH_HEAD. "couldn't find remote ref" → `no-upstream`; any other
   failure → `error`.
3. U = the tracking ref. Rewrite check: of the recorded `upstream.oid` (same
   remote and branch) and B, those that are not ancestors of U are lost. Lost
   and no `--accept-rewrite` → `rewritten`: unless the recorded tip is itself
   lost, the first lost oid becomes `upstream` (so the evidence survives the
   moved tracking ref); the dropped commits are counted (`rev-list <lost>
   ^U`); nothing else changes.
4. Up to 5 rounds:
   1. H = HEAD. H == U and nothing lost → `up-to-date`.
   2. commits = `rev-list --reverse --topo-order --parents H ^U [^<lost>…]`.
      U is an ancestor of H (and accepting drops none of H) → `up-to-date`
      ("N local commit(s) not pushed").
   3. Replay onto U, oldest first. A merge or root commit → `error` (linear
      history only). `git merge-tree --write-tree -z --name-only
      --no-messages --merge-base=<parent> <base> <commit>`: exit 1 →
      `conflict` (files listed, nothing changed); a tree equal to the base's →
      skipped (already upstream); else `commit-tree <tree> -p <base>` with the
      original author name, email and date, encoding and exact message bytes
      (`-S` under `commit.gpgSign`).
   4. ents = `diff-tree -r H tip`. Untracked or ignored local files at those
      paths (`ls-files -o`) → `blocked`. Tracked files deleted locally
      (stage 0, not skip-worktree) that tip has → `blocked` ("commit the
      deletion first").
   5. install (§9.3): Retry → next round; Busy → `busy`; Blocked →
      `blocked`; Done → `upstream = U`, `rebased` (commits replayed) or
      `fast-forward`.
5. Five rounds without settling → `error` ("HEAD kept moving during sync").

Statuses: `up-to-date`, `no-upstream`, `fast-forward`, `rebased` (ok);
`conflict`, `blocked`, `rewritten`, `busy`, `error`.

### 9.3 install: HEAD and the index as one compare-and-swap

1. Write a marker body `<P>.body`, where P =
   `<gitdir>/agent-lock-sync-<tag>.index`: `{"agent-lock": "sync", by, pid,
   start, head: H, tip, index_tmp: P}`.
2. Ignore SIGHUP, SIGINT and SIGTERM (children inherit it) until the install
   settles: only SIGKILL can cut it, and §9.4 handles that.
3. One transaction: `locks.sync` exists → `busy`. A claim on a path in ents
   with edits, a marker, or not stale (so a dead owner's dirty claim too) →
   `blocked` with (path, holder). Else `locks.sync = {by, owner: self, since,
   head: H, tip, paths, index_tmp: P, lock_ino: the body's inode}`. From here
   pre-edit refuses those paths (§6.3).
4. `link(body, <gitdir>/index.lock)`: an exclusive create of a fully written
   file, so the lock is never empty. Retried at 10 ms doubling to 200 ms for
   up to 10 s; then `locks.sync` is cleared and → `busy` ("never delete it").
5. Re-read HEAD under the lock: oid ≠ H, or not on `refs/heads/<branch>` →
   undo, Retry.
6. `link(<gitdir>/index, P)`: the hard-linked copy keeps the mtime, so
   racy-git detection still sees same-second edits; git replaces P through
   `P.lock` + rename, so the live index inode is never written.
7. `GIT_INDEX_FILE=P git read-tree -m -u H tip`: a two-way merge with
   checkout semantics. It refuses any path that differs between H and tip and
   has local index or worktree changes or would clobber an untracked file,
   and it keeps other agents' staged entries. Refused ("not uptodate",
   "would be overwritten") → `update-index -q --refresh` on P and one retry;
   still refused → `blocked` (nothing was written). Any other failure (a
   filter, a signal) → rollback (§9.4), `busy`.
8. `git update-ref -m "agent-lock sync: <tip12>" HEAD tip H` (all zeros when
   unborn). Failure (a ref writer that bypasses index.lock moved HEAD) →
   `read-tree -m -u tip H` back (else rollback), `busy`.
9. `rename(P, <gitdir>/index)`. Failure → `locks.sync` stays for recovery
   ("the next agent-lock git command finishes it").
10. Remove `P.lock`; remove `index.lock` only while its inode is ours; clear
    `locks.sync`.

### 9.4 Rollback and recovery

`rollback(H, tip, ents)`: for file and symlink entries whose index entry is
still H's, a path goes back to H when its worktree is exactly tip's (tip's
blob hash, link target, or absent where tip removes it), or it is missing
where H has it (read-tree unlinks before it writes). Restored from H through
a temp index (`read-tree H` + `checkout-index -f`); files tip added are
deleted, with directories that become empty. Anything else, someone's edit,
is left alone. The index is refreshed afterwards.

`recover(repo)` runs before every commit (before the hooks and again under
`locks.git`), sync and push, and in `doctor --fix`:

1. The record: `locks.sync`, else the marker in `index.lock` (a file starting
   with `{` whose `"agent-lock"` is `"sync"`; a real index starts with
   `DIRC`). None → nothing to do (one shared read, no process).
2. Owner alive (ownerless: younger than 600 s) → `busy`.
3. An orphaned git child still at work → `busy`: `ps -A -ww -o command=`
   shows `read-tree` or `update-ref` with the tip; without `ps`, a `P.lock`,
   `HEAD.lock` or branch ref lock younger than 600 s.
4. Take the record over in one transaction (`by: recover:<by>`, owner:
   self), so two recoverers never both act.
5. HEAD == tip (≠ H): the install had moved HEAD → finish: rename P into
   place (through our index.lock, or after linking P to a free index.lock).
   Else, when read-tree may have written (P or `P.lock` exists) and the lock
   is ours or HEAD is still H → rollback; if HEAD reached tip meanwhile (an
   `update-ref` past `ps`'s look) → bring the rolled-back files forward again
   (`redo`) and finish.
6. Remove P, `P.lock` and the body; `index.lock` only when its inode and
   marker are ours; clear `locks.sync`; refresh the index if files were
   rolled back. Report: `recovered from an interrupted agent-lock sync (by
   <id>, pid N): <what was done>[; removed the index.lock it left behind]`.

### 9.5 push

`push(repo, agent, accept_rewrite)`, holding `locks.git`: recover, then up to
`PUSH_TRIES` = 5 attempts:

1. oid = HEAD (none → `error`).
2. A lease `--force-with-lease=refs/heads/<branch>:<upstream.oid>` only when
   the recorded upstream tip is an ancestor of HEAD, so it can never force a
   non-fast-forward; none after a `no-upstream` sync.
3. `git push --porcelain [lease] origin <oid>:refs/heads/<branch>`. The
   porcelain line for that ref: ok and flag ≠ `!` → `upstream = oid`; flag
   `=` → `up-to-date`, else `pushed`. No line → `error`. Rejected for another
   reason → `rejected`.
4. Rejected with "fetch first", "non-fast-forward", "stale info", "cannot
   lock ref", "failed to update ref" or "incorrect old value provided" →
   sync under the same lock (stop on `conflict`, `blocked`, `rewritten`,
   `error`, `busy`), sleep base/2 + random(base) ms with base = min(2000, 50
   × 2^attempt), retry.
5. After 5 attempts → `rejected` ("gave up after 5 attempts").

Statuses: `pushed`, `up-to-date` (ok); `rejected`, `error`, `busy`, or the
sync status that stopped it. agent-lock does not build or test before it
pushes.

## 10. CLI (`cli.rs`)

`agent-lock help`:

```
agent-lock: many agents in one shared git checkout, on one branch.

Usage: agent-lock <command> [options]

  commit -m <msg> [-- <paths>]            commit exactly your claimed files, or only <paths> (also -F <file|->, --push, --no-verify, --dry-run)
  push [--accept-rewrite]                 publish HEAD to origin; syncs and retries when the remote moved
  sync [--accept-rewrite]                 bring in upstream commits without touching anyone's uncommitted work
  wait <file>… [--claim] [--timeout <s>]  wait until you could edit the files (--claim takes them); exit 0 free, 1 timeout, 3 deadlock
  release <file>… [--force] | --mine      release your own or stale claims (--force: another agent's, with the user's approval)
  mine [-z]                               print the files you hold (-z: NUL-separated: `agent-lock mine -z | xargs -0 -r <tool>`)
  status [dir] [--json]                   claims, sessions, locks and orphans of this checkout (--json: the state)
  off [dir]                               turn agent-lock off in this checkout
  on [dir]                                turn it back on
  doctor [dir] [--fix]                    report stale claims, interrupted syncs, a corrupt state file, setup problems (--fix repairs)
  install [dir] [--force]                 enable the plugin for a project in <dir>/.claude/settings.json
  uninstall [dir] [--force]               remove it from <dir>/.claude/settings.json
  selftest [--keep]                       run the real hooks, shims and CLI in a throwaway repo; PASS/FAIL per step
  version                                 print the version
  help                                    this text

Who you are: --as <id> › $AGENT_LOCK_AGENT_ID › $CLAUDE_CODE_SESSION_ID (the session's main agent).
Subagents share the session's environment, so they pass --as <their id>.
Options anywhere: --as <id>, -C <dir> (run as if started in <dir>).
Exit codes: 0 ok, 1 refused or failed, 2 usage error, 3 deadlock (wait).
```

- Caller: §1. `commit`, `wait`, `mine` and `release --mine` require one
  (usage error otherwise); `push`, `sync`, `release` and `status` use it when
  known.
- `--as <id>`, `--as=<id>`, `-C <dir>` and `-C<dir>` (chained like git's) are
  accepted anywhere before `--`; the Bash hook inserts `--as` right after the
  subcommand. `--message` = `-m`, `--file` = `-F`, `--null` = `-z`; `-m<msg>`
  attached works. `-h`/`--help` anywhere → help.
- v2 commands are usage errors with a hint: `release-mine` → `release
  --mine`; `purge-all` → `doctor --fix`; `list`, `watch` → `status`; `test` →
  `selftest`; `hold` → none (the reminder asks once per file, and again
  while someone waits for it).
- Exit codes: 0 ok; 1 refused or failed; 2 usage error (followed by ``(see
  `agent-lock help`)``); 3 deadlock (`wait`); 128 + n after a deferred signal
  in `commit`.

| Command | Behavior |
|---|---|
| `commit` | §8. `--push` runs `push` afterwards. `--no-verify` skips the repo hooks. |
| `push`, `sync` | §9. Prints `<verb>: <message>`; exit 0 on an ok status, else 1 (on stderr). Blocked by claims: `<verb>: blocked: upstream changed N file(s) that are claimed in this checkout (sync never overwrites uncommitted work); nothing was changed`, then advice per file from the caller's side: own dirty (commit it first), own clean claim (commit or release it), own running subagent (wait for it), own finished subagent (commit takes it over), the parent's (leave it to the parent), another agent's (retry after it commits; `agent-lock wait <abs>`), an abandoned dirty claim (ask the user, or `wait --claim` and commit). |
| `wait <file>… [--claim] [--timeout <s>]` | Default 110 s; polls every 250 ms. A path outside a worktree → exit 2. A file waits while `conflict` is Block and the claim is not stale for the caller. On the first block the waits are recorded (holders get waiter notices; deadlocks are found) → DEADLOCK, exit 3. Free → `free: <files>`, exit 0. `--claim` takes every file (one transaction per repo, undone across repos on a miss; reason "wait": Stop keeps it) or none, if another agent got one first (then it keeps waiting); prints `claimed: <files>` and the notes a first edit would give (takeover, orphan, inherited, dirty). Timeout → the holders on stderr, exit 1. |
| `release <file>… [--force]` | Per file, after clearing dead markers: own, or the caller's finished subagent's → `released`; stale → `released stale claim of <id> (<why>)`; `--force` → `force-released`; else refused with advice, exit 1. A dirty claim becomes an orphan. Outside a worktree → `not claimed`. |
| `release --mine` | A main id: its own and its subagents' claims; a subagent or script: its exact id. |
| `mine [-z]` | Absolute paths of the caller's exact-id claims that exist (missing ones to stderr). `-z`: NUL-separated. |
| `status [dir] [--json]` | Coordination on/off, you, claims (path, agent, edits + writing/committing/installing, claimed, idle, owner, stale), sessions (phase, owner, seen, claims, subagents), locks, orphans, upstream. `--json` prints the state (§2.2). |
| `off [dir]`, `on [dir]` | Set or clear `off`: hooks and shims do nothing in this checkout. `on` notes a `CLAUDE_FILE_LOCK=0` in the environment. |
| `doctor [dir] [--fix]` | `[kind] detail` lines; exit 1 while problems remain. Kinds: `git` (< 2.40), `worktree`, `plugin` (not enabled), `kill-switch` (`env.CLAUDE_FILE_LOCK=0` in `.claude/settings.json`, `.claude/settings.local.json` or `~/.claude/settings.json`), `legacy` (v1 leftovers), `shims` (inside Claude Code: `AGENT_LOCK_AGENT_ID` unset, or the first git on PATH is not the shim), `corrupt`, `dead-owner`/`ended-session`/`unwritten`/`stale` (stale claims, with advice by whether the file is dirty), `stuck` (dead process markers), `interrupted-sync`, `index-lock` (a foreign index.lock older than 600 s: ask the user to delete it), `lock` (git lock > 600 s, pkg lock > 1,200 s, sync > 600 s, each with a live owner), `leftover` (temp files of killed commands, older than 3,600 s). `--fix` repairs the fixable ones: removes the `.bad` file and leftovers, releases stale claims (dirty ones become orphans), clears stuck markers, recovers interrupted syncs. |
| `install [dir] [--force]`, `uninstall [dir] [--force]` | Add or remove `extraKnownMarketplaces.lovieco = {"source": {"source": "github", "repo": "lovieco/agent-lock"}}` and `enabledPlugins["agent-lock@lovieco"]` in `<dir>/.claude/settings.json` (default: the repo top), keeping every other setting and the file's indentation (tmp + rename). A `lovieco` entry that points elsewhere is kept with a warning unless `--force`; uninstall keeps the marketplace while other `@lovieco` plugins are enabled. Both remove v1 leftovers: vendored hook entries in `settings.json` and `settings.local.json`, `.agent-lock/{src,hooks}`, and the v1 project skill (only when recognizably v1's). |
| `selftest [--keep]` | The real hooks, shims and CLI in a throwaway repo with a bare remote and two sessions: 12 steps, `PASS`/`FAIL` each (under 0.5 s); exit 1 on any failure. |
| `version` | `agent-lock 0.3.0` |

Two sessions, one claim (`a1b2c3d4` edited `a.txt`; `b5e6f7a8` was blocked):

```
$ agent-lock status
agent-lock status: /work
coordination: on
you: b5e6f7a8

claims (1):
  PATH   AGENT     EDITS  CLAIMED  IDLE  OWNER  STALE
  a.txt  a1b2c3d4  1      24s      24s   alive  -

sessions (2):
  AGENT     PHASE   OWNER  SEEN     CLAIMS  SUBAGENTS
  a1b2c3d4  active  alive  24s ago  1       -
  b5e6f7a8  active  alive  24s ago  0       -

locks: none
```

## 11. Messages

| File | Text |
|---|---|
| `msg.rs` | every sentence hooks show agents: contexts, BLOCKED, DEADLOCK, TAKEOVER, notes, waiter notices, the Stop reminder, Bash warnings, DENIED, asks |
| `shim.rs` | git refusals and hints (`Judge::verdict`, `Judge::hint`); package-manager wait and refusal lines |
| `cli.rs` | CLI output, release refusals, sync/push advice, doctor findings |
| `git.rs` | operation results: commit codes, sync and push statuses, recovery |

Conventions: hook text starts with `[agent-lock] `; shim and CLI errors with
`agent-lock: `. Prose names a file by its repo-relative path. A command meant
to be copied uses the absolute, shell-quoted path (`sys::quote`: quoted only
when needed), carries `--as <id>` for a subagent (`msg::as_flag`) and `-C
<top>` when the agent's shell is in another repo (diffs: `git -C <top> diff
-- <rel>`). Durations read `5s`, `3m`, `2h 5m` (`msg::ago`).

| `msg::` | Shown | Says |
|---|---|---|
| `session_context` | SessionStart, or the agent's first edit | the checkout may be shared on this branch; your id; the rules (below) |
| `subagent_context` | SubagentStart, or the first edit | the same, plus: pass `--as <id>` to every agent-lock command and prefix package-manager commands with `AGENT_LOCK_AGENT_ID=<id>` |
| `blocked` | pre-edit deny | holder, claimed ago, last active ago, holder process alive/gone, uncommitted edits (or "its edit never ran"); do other work; `agent-lock wait --claim <abs> --timeout 540` with run_in_background; never write it another way; `release --force` needs the user's approval (is safe with it when the edit never ran). Own running subagent: wait for its completion notice. Own finished subagent whose last call never completed: it passes to you when your turn ends. |
| `deadlock` | pre-edit deny, `wait` exit 3 | the chain of waits; commit the files you hold now |
| `takeover` | pre-edit deny, multi-agent | who held it, why it is stale, N uncommitted edits; the claim is now yours, so your commit includes them: look at the diff first; retry, or `agent-lock release <abs>` |
| `takeover_note` | pre-edit when alone, `wait --claim`, package shim | the same; the edit goes ahead |
| `orphan_note` | first claim of an orphaned path | it may hold another agent's uncommitted changes; check the diff |
| `inherited` | a subagent gets a claim with edits | leave committing it to your parent; your commit leaves it out unless you name it |
| `dirty_note` | first claim of an already-changed file, multi-agent | changes not made through your edits; check the diff |
| `not_owner` | post-edit | your write landed on another agent's claim; do not edit it again; tell the user |
| `sync_busy`, `state_busy` | pre-edit deny | a sync is updating the file, or the state stayed locked; retry in a moment |
| `waiter_notice` | post-edit, post-bash, prompt, to the holder, once per waiter and file | `<waiter> is blocked on <file> (yours)`; when consistent, `agent-lock commit -m "<msg>" -- <abs>` |
| `commit_reminder` | Stop block | your uncommitted claimed files, per repo; who waits; commit if complete and consistent (`--push` only if the user wants it published), else say so in one line and stop |
| `bash_wrote` | post-bash | your Bash command changed files others have claimed (≤ 5 listed) |
| `identity` | pre-bash deny | `DENIED: this would run agent-lock as <id>, but you are <id>` and the corrected command |
| `ask_killswitch`, `ask_off`, `ask_force_release`, `ask_internals` | pre-bash ask | what the command turns off or can destroy; it needs the user's approval |

The session context (main agent `a1b2c3d4`, shell in the repo), as sent:

```
[agent-lock] This checkout may be shared with other agents working on this branch. Your agent id is a1b2c3d4. Your edits claim files until you commit them. If an edit is BLOCKED, do other work: the holder keeps the file until it commits. Never write a claimed file another way (redirection, sed -i, cp, scripts). Commit with `agent-lock commit -m "<msg>"`: it replaces git add + git commit (`-m "$(cat <<'EOF' … EOF)"` works). Publish with `agent-lock push`; get upstream changes with `agent-lock sync`. While others work, the git and npm shims refuse whole-tree git (add -A, commit -a, stash, reset --hard, checkout ., pull, rebase). Run formatters only on your files: `agent-lock mine -z | xargs -0 -r <tool>`. An index.lock error means wait and retry; never delete it.
```

A BLOCKED denial, as sent:

```
[agent-lock] BLOCKED: a.txt is claimed by a1b2c3d4 (claimed 0s ago, last active 0s ago, holder process alive, 1 uncommitted edit).
The holder keeps this file until it commits: do other work now.
If you must wait: `agent-lock wait --claim /work/a.txt --timeout 540` with run_in_background.
Never write it another way (redirection, sed -i, cp, scripts).
If the holder looks abandoned, ask the user: `agent-lock release --force /work/a.txt` needs their approval.
```

## 12. Guarantees and limits

For cooperating agents on one machine:

- **One holder per file.** Claims change only inside flock transactions
  (§3), keyed by git's spelling (case-folded under `core.ignorecase`, NFC on
  macOS), so two agents never hold one file. Stress test: 9 concurrent
  agents, about 3,700 hook calls, never two holders.
- **Alive owners keep dirty files.** Only a user-approved `release --force`
  moves them, and the Bash hook asks before it runs. A dead owner's files can
  be taken over: a one-time TAKEOVER denial while others are active, a note
  when alone; the changes are remembered as an orphan until committed.
- **No stuck locks.** The kernel drops a dead process's flock; dead owners'
  `locks.git` and `locks.pkg` are pruned; anyone clears a dead process's
  marker; an interrupted sync is rolled back or finished by the next
  `agent-lock commit`, `sync`, `push` or `doctor --fix`.
- **Commit isolation.** `agent-lock commit` commits only its set (`git commit
  --only` with a pathspec file; the preflight write-tree gate refuses what a
  hook staged), leaves nothing staged on failure, and reports a hook that
  stashed or rewrote other agents' files (never auto-restored).
- **Sync and push never modify uncommitted work.** A live or dirty claim on a
  path upstream changed blocks the install; read-tree refuses locally changed
  paths; local untracked, ignored or deleted files in the way block; HEAD and
  the index change as one compare-and-swap under index.lock; a kill
  mid-install is undone or finished.
- **Whole-tree git is refused only while it matters**: while another agent
  holds a live claim, or while a subagent may run and the main agent holds
  one (or the stray check finds unclaimed changes in a whole-index commit).
  Recovery forms stay allowed.
- **Fail open.** An error in a hook allows the tool call; an error in a shim
  runs the real tool; a missing native binary leaves agent-lock inactive.

Measured (macOS arm64, release build, medians):

| What | Time |
|---|---|
| a hook as Claude Code runs it (`sh -c`, fast-path command) | ≈ 7.4 ms, of which the `sh -c` floor is ≈ 4.8 ms: agent-lock's own cost ≈ 1.2–2.6 ms (v2, Node: ≈ 80 ms) |
| git through the shim | ≈ +2.4 ms, less than `/usr/bin/git`'s xcrun trampoline adds |
| the first claim of an existing file while another session is active | + one `git status`, ≈ 8 ms |
| 2,000 contended state transactions from 8 threads | ≈ 0.1 s (0.15 s in a debug test build), no lost update |
| `agent-lock selftest` (12 steps) | under 0.5 s |

Size: 267 tests (`cargo test`; one more is `#[ignore]`d). Production code: ≈
10,300 non-blank, non-comment lines in `rust/src`, test modules excluded.

Limits (verified):

- **Subagent identity in Bash.** Subagents share the session's Bash
  environment, so the shims see the main id. While a subagent may run, the
  main agent's own files are protected from sweeping git too. Subagents pass
  `--as <id>` to agent-lock commands (the Bash hook enforces it) and prefix
  `AGENT_LOCK_AGENT_ID=<id>` for raw git and package managers.
- **Processes git spawns bypass the shim**: repo hooks, `rebase -x`, `bisect
  run`, the shell of a `!` alias. `!` aliases are judged from their text;
  pre-commit and commit-msg hooks are scanned statically and checked for
  damage after they run (§8.3); other hooks are not checked.
- **Writes the hooks cannot see** (Python or Node scripts, editors outside
  Claude Code) are detected after a Bash command by an mtime/size check of
  claimed files, not prevented. Files a Bash command creates are unclaimed
  until someone claims them (`agent-lock commit -- <paths>` claims unclaimed
  paths).
- Package-manager commands outside §7.8's verbs run unguarded (e.g. `cargo
  build` updating `Cargo.lock`).
- No cross-machine locks: machines coordinate through push and sync. Old NFS
  without flock is unsupported. Windows is unsupported. Claims exist only
  inside git worktrees; each worktree has its own state file.
- A replay conflict, a rewritten upstream (`--accept-rewrite` needs the
  user), merge commits in local history and case-only renames need a human.
- agent-lock does not build or test before `--push`.
- When the state file stays locked for more than 5 s, hooks let edits
  through unless the last-written state shows another agent's claim on that
  path (§3.2).
