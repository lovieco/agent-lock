# Agent Lock

[Lovie.co](https://www.lovie.co) · Eng · Agent Lock

**Live multi-agent coding in one shared checkout. No branches, no PRs, no merge.**

In Google Docs, Figma and Notion, two people editing the same file at the same time is normal. Code is the last holdout: every change still goes through a branch, a PR and a merge. That loop made sense when humans were the only writers. With many AI agents in one repo, it is the bottleneck.

Agent Lock lets every agent work in the same tree, on the same branch (usually `main`), at the same time. The first agent to edit a file **claims** it until it commits. Any other agent that tries to write that file is told "taken" before it writes a byte, and does other work. Each agent then commits exactly its own files with `agent-lock commit`, never someone else's half-finished change.

```bash
claude plugin install agent-lock --marketplace lovieco/agent-lock
```

That is the whole setup. Alone in a checkout, nothing blocks and nothing nags. Start a second agent and coordination starts.

v0.3 is one native binary (Rust, standard library only, about 1.6 MB) and one state file per checkout. A hook costs about 7 ms as Claude Code runs it; v0.2 (Node) cost about 80 ms.

---

## The problem in one picture

Today, every code change goes through the git dance:

```
  git clone  ─►  branch  ─►  edit  ─►  commit  ─►  push  ─►  PR  ─►  conflict  ─►  merge
                                                                          │
                                                                          ▼
                                                                you, manually
```

That made sense when one human was making one change. Now picture 100 agents on 100 small tasks at once:

```
              one repo on github
                     │
   ┌──────────┬──────┴──────┬──────────┐
   ▼          ▼             ▼          ▼
 clone #1   clone #2      clone #3   …  clone #100
 branch #1  branch #2     branch #3  …  branch #100
   │          │             │          │
   PR #1     PR #2         PR #3   …   PR #100
                     │
                     ▼
            100 PRs, hundreds of merge conflicts,
            100 copies of node_modules on disk,
            and one human refereeing all of it
```

100 agents means 100 full copies of the repo, 100 `node_modules` folders, 100 build caches: 100 snapshots that drift from the real repo the moment they are created. Every pair of agents that touched the same file is one more merge you referee by hand.

**With Agent Lock there is one tree. No clones, no branches, no snapshots.** Every agent edits the same live repo. A file being changed has exactly one owner until that owner commits it. There is nothing to merge because nothing forked.

```
        one shared tree (main)
  ┌──────────────────────────────┐
  │   src/schema.ts   [claimed A]│ ← A's, until A commits
  │   src/api.ts                 │ ← free
  │   src/utils.ts    [claimed C]│ ← C's, until C commits
  └──────────────────────────────┘
        ▲       ▲       ▲
     Agent A  Agent B  Agent C   …   Agent N
              (picks a free file)
```

### Why a shared tree needs coordination

Put several agents in one checkout with no rules and two things go wrong.

**Interleaved edits.** Claude Code refuses an edit to a file that changed since the agent read it, so the agent re-reads and retries, and now builds on another agent's half-finished change. Both changes end up in one dirty file, and neither agent knows which lines are its own. Writes from shell commands get no such check: the second write silently replaces the first.

**Tangled commits.** Agent A runs `git add -A && git commit`, or just `git commit`, which takes whatever anyone staged, and ships B's half-finished edits under A's message. Or `git stash` / `reset --hard` throws B's work away. The history stops saying who changed what, and `main` gets half-done code.

Agent Lock fixes both by deciding ownership up front: one agent per dirty file, from its first edit to its commit.

```
        same checkout
  ┌─────────────────────────────┐
  │   src/schema.ts [claimed A] │
  └────────▲───────▲────────────┘
        Edit       Edit
       ┌───┴───┐ ┌─┴─────┐
       │Agent A│ │Agent B│ ── BLOCKED: claimed by a1b2c3d4
       └───┬───┘ └───────┘     (B works on another file, or waits in the background)
           ▼
   agent-lock commit  ─►  commits schema.ts (and only that), releases it
```

---

## Why claims beat the alternatives

Four ways to coordinate N agents on one codebase, with agents A and B both wanting `src/schema.ts`:

| Approach | What happens | Cost for agents |
|---|---|---|
| **Branches + PRs** | A and B each edit a copy; you merge at the end | Paid at the end, by you. N copies of dependencies and caches. Merging two rewrites of one function is guesswork about intent. Overlapping pairs grow as O(N²). |
| **Separate worktrees** | Same as branches, on one disk | Hides the conflict until the worktrees come back together. |
| **Optimistic concurrency** (git merges, CRDTs) | Let everyone write, detect and merge later | Works for "add a line"; fails for "rewrote the function" or "renamed the symbol". *Write skew* (each write fine alone, wrong together) is what agents produce. A shared tree with no rules is OCC without even the detect step. |
| **Pessimistic claims** (Agent Lock) | A claims the file at its first edit; B is told "taken" | Paid up front, before a byte is written. No merge, no referee. |

Acquire-before-write lost to optimistic concurrency for human use. Three variables decide between them, and for agents all three flip:

```
                       claims win            optimistic wins
                       ──────────            ───────────────
  Conflict cost        high                  low
  Merge feasibility    hard / guesswork      easy / automatic
  Retry cost           low                   high
```

| | Humans | AI agents |
|---|---|---|
| How long they own a change | hours to days (a branch) | one task: minutes, until they commit |
| Cost of "taken, do something else" | high (breaks flow) | near zero (pick another file, come back) |
| Cost of a silent overwrite or tangled commit | caught in review | invisible until something breaks on `main` |
| Who wrote this line? | `git blame`, the PR | only knowable if each agent commits its own files |

A BLOCKED edit costs an agent one tool call. What no model recovers from is not knowing whose uncommitted change is whose. A claim answers that, which is what lets `agent-lock commit` commit exactly your work and keeps `git log` honest.

---

## How it works

### One binary, three roles

```
   Claude Code (one session per agent)           the agent's Bash shell
   ───────────────────────────────────           PATH = <plugin data>/shims/core:…
   hooks/hooks.json events                               │                │
        │ agent-lock hook pre-edit                       │ git add -A     │ agent-lock commit
        ▼ (JSON on stdin)                                ▼ (argv[0]=git)  ▼ (the CLI)
   ┌───────────────────────────────────────────────────────────────────────────┐
   │ agent-lock: one native binary. argv picks the role:                       │
   │   hook <event>  │  git, npm, pnpm, yarn, bun, cargo, uv, poetry, go,      │
   │                 │  bundle, bundler (shims)  │  any other command (CLI)    │
   └───────┬───────────────────────────────────────────────────┬───────────────┘
           │ one flock transaction per change                  │ allowed: exec the real tool
           ▼                                                   ▼
   <gitdir>/agent-lock.json                            /usr/bin/git, npm, …
```

- `bin/agent-lock` is a POSIX `sh` launcher. It execs this machine's binary from `bin/native/agent-lock-<os>-<arch>`.
- **SessionStart** links the binary as `$CLAUDE_PLUGIN_DATA/shims/core/{git,agent-lock}` and `shims/<tool>/<tool>` for each package manager. Through `CLAUDE_ENV_FILE` it puts the shims first on the agent's `PATH` (a package manager's shim only when that tool is installed) and sets `AGENT_LOCK_AGENT_ID` and `GIT_OPTIONAL_LOCKS=0`.
- Every other hook execs `shims/core/agent-lock` directly, skipping the launcher.
- **Fail open everywhere.** A hook that errors exits 0 with no output. A shim that errors runs the real tool unchanged. A machine with no binary prints one line on stderr and runs without agent-lock.

### One state file, one transaction

All coordination state for a worktree is one file, `<gitdir>/agent-lock.json`: claims, sessions, waits, locks (git, pkg, sync), orphan notes, the upstream tip and the off switch. Every change is one transaction:

```
open → flock(LOCK_EX) → read → parse → change in memory → one write at offset 0 + truncate → close
```

- The kernel drops the lock when the process exits or dies, so there are no stuck locks. The inode never changes, so every waiter locks the same file.
- A check and the change it justifies happen in one transaction: "is it free?" and "claim it" cannot be split.
- A write cut short leaves trailing bytes; readers parse the first JSON value. An unparseable file is kept as `agent-lock.json.bad` for `doctor`, and the state starts over.
- A file written by a newer agent-lock is read but never rewritten.
- Readers take a shared lock. No transaction is held while git or a package manager runs.
- A lock wait gives up after 5 s. Then a hook fails open, except that an edit is refused if the file as last written shows another agent's claim on that path.
- Nothing lives in your working tree. Linked worktrees each have their own file. The hooks also keep `$CLAUDE_PLUGIN_DATA/sessions.json`, the list of repos each session touched, so end-of-turn and end-of-session cleanup visit all of them.

### A claim's life

```
  Edit / Write / MultiEdit / NotebookEdit on a file in a git worktree
        │
        ▼  PreToolUse: agent-lock hook pre-edit (one transaction)
        ├─ no claim, or already yours ────────► edit runs   (PostToolUse: edits + 1)
        ├─ your finished subagent's ──────────► moves to you, edit runs
        ├─ another live agent's ──────────────► denied: BLOCKED
        └─ owner's process gone ──────────────► taken over; if it holds uncommitted
                                                edits and others are active, the first
                                                try is denied once with TAKEOVER
        …more edits; the claim stays…
        ▼
  agent-lock commit -m "…"   ─►  commits exactly your files, releases them
```

**Ids.** A session's main agent is `a1b2c3d4` (the first 8 characters of its session id). A subagent is `a1b2c3d4:9f3e01aa`. Scripts may use any id without `:`.

**Keys.** A claim is keyed by the repo-relative path in git's spelling: the on-disk spelling (realpath), NFC on macOS, and folded to lower case under `core.ignorecase`. So `Logger.ts` and `logger.ts`, or an NFD and an NFC name, are one claim. An edit through a symlink claims the file it points to.

**Who gets the file** when agent X edits a file held by Y:

| Situation | Result |
|---|---|
| No claim, or Y is X | X claims it, or refreshes its claim |
| A write is in flight on it (`pending`) | Blocked: no hand-over mid-write (unless Y's process is gone) |
| Y is X's main agent and X is its subagent | Moves to X. If it carries Y's uncommitted edits, X is told, and X's commit leaves the file to Y unless X names it |
| Y is a finished subagent of X's session | Moves to X |
| An `agent-lock commit` or package install working on it is alive | Blocked, however long it takes |
| Y's process is gone (pid exited, zombie, or reused with another start time) | Taken over |
| Y's session ended in the same Claude Code process (`/clear`) | Taken over |
| Y is alive and the file has uncommitted edits | **Blocked. No timer ever frees it** |
| Y is alive and never wrote it (an edit rejected at a prompt) | Blocked; free after 45 min idle |
| Y has no known process (a script without `$CLAUDE_PID`) | Blocked; free after 10 min idle |

A takeover of a file with uncommitted edits is denied once with `TAKEOVER` when other agents are active (look at `git diff` first; a retry within 5 min proceeds), and is a note when you are alone. A dirty claim that ends without a commit leaves an orphan note for 7 days, so the next agent to claim the file is told whose changes are in it.

**A claim ends when**

- **you commit:** `agent-lock commit` releases each file it committed once git reports it clean;
- **your turn ends** (Stop): claims you never wrote are released (except `agent-lock wait --claim` reservations), and so are claims whose file is back to clean vs HEAD, unless a stash is newer than your last edit;
- **the edit fails or is rejected:** a claim that very call created is released;
- **your session ends:** unwritten claims are released; dirty claims stay, so a resumed or `/clear`ed session picks them up;
- **your process dies:** the next agent may take the file over;
- **the user says so:** `agent-lock release --force <file>` (the Bash hook asks for approval).

### Shared files: config several agents edit at once

One owner per file is right for source, and wrong for the handful of files
every task touches: `package.json` and its lockfile, `Cargo.toml`,
`tsconfig.json`, `*.config.ts`, `.gitignore`, `CLAUDE.md`. Blocking there
serializes whole tasks on one line of config. So those files are **shared**:

- A second agent's edit **joins** the claim (`with: [...]` in the state
  file) and is told who else edits it.
- **One write at a time.** A write in flight still blocks the others, for the
  few milliseconds it runs; Claude Code's Edit refuses a file that changed
  since it was read, so no edit is lost. A marker whose PostToolUse never came
  stops blocking after 2 min.
- **One commit for all.** Any holder's `agent-lock commit` commits the file
  with everyone's changes in it (the report says whose) and clears the claim
  for every holder. Leaving (`release`) keeps it for the others.
- A dead holder is taken over as usual, not joined.

The built-in list covers manifests, lockfiles and common tool config; add or
exclude paths in `.agent-lock-shared`.

### Subagents

- Each subagent gets its own id and its own claims. SubagentStart records it.
- A main agent's file can move to its own subagent. A running subagent's files block the main agent and its siblings. When it finishes (SubagentStop, or Stop's `background_tasks`), its claims pass to the main agent. Background subagents and workflows keep their files while they run.
- Subagents share the session's Bash environment, so the shims see the main agent's id. Subagents pass `--as <id>` to `agent-lock` commands (the Bash hook denies a call without it and shows the fixed command) and prefix `AGENT_LOCK_AGENT_ID=<id>` for raw git or npm. While a subagent may be running, the main agent's own files are protected from whole-tree git too.

### The shims: what they refuse, and when

A shim sees the real arguments, after the shell, `xargs`, `find -exec`, a script or a git alias produced them, so nothing is guessed from command text. Git commands that cannot touch the worktree (`status`, `diff`, `log`, `show`, `fetch`, `push`, `branch`, `tag`, …) exec the real git at once without reading the state. The rest read the state and are refused **only while another agent holds a live claim** in that checkout (two narrower cases: a whole-index commit while another session is active, in the table below, and your own files while one of your subagents may run, under Subagents). A refusal is one stderr line and exit 1, like a failing git.

| git command | Refused when |
|---|---|
| `add -A`, `--all`, `-u`, `-p`, `-i`; `add <pathspec>` (`.`, `:/`, a directory, a glob; `:(exclude)` honored) | the scope covers another agent's claimed file |
| bare `commit`, `commit -a`, `-i`, `-p`, `--amend` | another agent holds a live claim, or another session is active and the commit would take changes you did not claim (the index, or with `-a` every tracked change) |
| `commit -- <paths>`, `checkout -- <paths>`, `restore`, `rm`, `mv` | a path is, or a pathspec covers, another agent's claimed file |
| `clean -f` | it would delete another agent's untracked claimed file |
| `stash` (push, pop, apply, drop, clear), `reset --hard` / `--merge` / `--keep`, `reset <rev>` | another agent holds a live claim |
| `pull`, `rebase`, `merge`, `cherry-pick`, `revert`, `am`, `switch`, `checkout <branch>` / `-b` / `-`, `bisect`, `filter-branch`, `sparse-checkout set`, `read-tree -u` | another agent holds a live claim |
| aliases (`git co .`, `-c alias.x=…`, `!` shell aliases) | the expansion would be refused |

Allowed: scoped forms that name only your files, and the recovery forms of an operation in progress (`merge --abort`, `rebase --continue` / `--quit`, `cherry-pick` and `revert --continue` / `--abort` / `--quit`, `am --abort`). Real output, from a third agent while two others hold files:

```
$ git add -A
agent-lock: `git add -A` covers files other agents are editing (src/api.ts by e5f6a7b8:9f3e01aa, src/schema.ts by a1b2c3d4). Name only your own files, or commit them with agent-lock commit -m "<msg>" -- <paths>.
$ git pull
agent-lock: `git pull` acts on the whole shared checkout while other agents have uncommitted work (src/api.ts by e5f6a7b8:9f3e01aa, src/schema.ts by a1b2c3d4). Get upstream changes with agent-lock sync (it never touches uncommitted files).
```

**Package managers** (npm, pnpm, yarn, bun, cargo, uv, poetry, go, bundle): installs and dependency changes run one at a time per checkout under `locks.pkg`; a second one waits (up to 20 min, `AGENT_LOCK_PKG_WAIT`). Dependency changes (`npm install x`, `pnpm add`, `cargo add`, `uv add`, `go mod tidy`, `bundle update`, …) also claim the nearest manifest and its lockfiles for the caller, workspace-aware. If another agent holds one of them, the command is refused before it runs. Plain installs (`npm ci`, `uv sync`, `bundle install`) take only the lock; everything else (`npm run`, `cargo build`, `go test`) and global installs run straight through.

```
$ npm install left-pad
agent-lock: `npm install left-pad` rewrites package.json, which a1b2c3d4 is editing (uncommitted). Prefer other work; to wait: agent-lock wait --claim /repo/package.json --timeout 540 with run_in_background (a subagent adds --as <its id>).
```

### The Bash hook

git and package-manager calls are judged by the shims, so the PreToolUse Bash hook only:

- denies `agent-lock commit|push|sync|release|wait|mine` run under another agent's id (a subagent without `--as`), with the corrected command;
- asks the user before `CLAUDE_FILE_LOCK=0`, `agent-lock off`, `agent-lock release --force`, and any write or removal that reaches `agent-lock.json` or `.git/index.lock`;
- records the mtime and size of the claimed files. After the command, PostToolUse compares them: a change to another agent's file is reported to the agent that ran the command (tell the user); a change to your own counts as an edit.

### What a blocked agent sees

```
[agent-lock] BLOCKED: src/schema.ts is claimed by a1b2c3d4 (claimed 6m ago, last active 40s ago, holder process alive, 3 uncommitted edits).
The holder keeps this file until it commits: do other work now.
If you must wait: `agent-lock wait --claim /repo/src/schema.ts --timeout 540` with run_in_background.
Never write it another way (redirection, sed -i, cp, scripts).
If the holder looks abandoned, ask the user: `agent-lock release --force /repo/src/schema.ts` needs their approval.
```

When the holder's only claim is an edit that never ran, the message says so, and that `release --force` is safe with the user's approval. The holder gets a one-time notice that someone is waiting ("commit it now"). If A waits on B's file while B waits on one of A's, the agent that closes the loop gets `DEADLOCK` and is told to commit what it holds (`agent-lock wait` exits 3).

### Commit, sync, push

```
agent-lock commit -m "Add slug helper"              # every file you hold
agent-lock commit -m "Fix schema" -- src/schema.ts  # only these paths
```

1. **File set:** the files you hold (a subagent's leaves out files that carry its parent's uncommitted edits), or exactly `-- <paths>`: an unclaimed path is claimed now, a finished subagent's moves to you, anyone else's is an error and nothing is committed.
2. **Pairs:** a dirty manifest brings its dirty lockfile (`package.json`, `pyproject.toml`, `Cargo.toml`, `go.mod`, `Gemfile`).
3. **Hooks see only your files:** `pre-commit` and `commit-msg` run against a private index of HEAD + your paths. Hooks that would rewrite the shared worktree (`git stash`, `reset --hard`, lint-staged without `--no-stash`, also one level down in scripts and make targets) are refused up front while others have uncommitted work: run your checks and pass `--no-verify`. Files outside your commit are checked after the hooks; if a hook changed or stashed them, the commit stops and names them.
4. **Commit:** under `locks.git`, `git commit --only` with `:(literal)` pathspecs. Other agents' staged and unstaged changes are untouched. A case-only rename on a case-insensitive filesystem and a staged `git rm --cached` of a file still on disk are refused with an explanation.
5. **Report:** the sha, the files, and a count of other agents' dirty files (not included). `--push` publishes afterwards.

**`agent-lock sync`** fetches the tracking ref and replays the local commits onto upstream in memory (`git merge-tree`, author and message kept, commits already upstream skipped). If upstream changed a file someone has claimed, it stops, names the files and holders, and changes nothing. Otherwise it marks the paths in `locks.sync` (an edit to one is refused with "retry in a moment"), takes `index.lock`, re-checks HEAD and installs the new tip (`read-tree -m -u`, `update-ref HEAD <new> <old>`) with signals ignored. A sync killed mid-install is rolled back or finished by the next agent-lock commit, sync or push. A conflict reports the files and changes nothing. A rewritten upstream (force-push) stops until `--accept-rewrite`.

**`agent-lock push`** pushes with a lease on the last integrated tip; when the remote moved, it syncs and retries, up to 5 attempts.

### The end-of-turn reminder

When other agents are active and the main agent ends a turn holding files with uncommitted edits, with no background subagent running, the Stop hook asks it to commit finished work or say in one line that it is mid-change. It asks once per new file, and again at every turn end while someone waits for one. It never pushes.

---

## The state file

A real `.git/agent-lock.json`: three sessions, A holding `src/schema.ts`, a subagent of B holding `src/api.ts`, B waiting for A's file (whitespace condensed):

```json
{
  "v": 3,
  "claims": {
    "src/api.ts": {
      "by": "e5f6a7b8:9f3e01aa", "session": "e5f6a7b8-2222-4222-8222-222222222222",
      "owner": { "pid": 82576, "start": 1792881276427106 },
      "at": 1792881276, "touched": 1792881276, "edits": 1, "made_by": "toolu_5"
    },
    "src/schema.ts": {
      "by": "a1b2c3d4", "session": "a1b2c3d4-1111-4111-8111-111111111111",
      "owner": { "pid": 82575, "start": 1792881276426716 },
      "at": 1792881276, "touched": 1792881276, "edits": 3, "made_by": "toolu_2"
    }
  },
  "sessions": {
    "a1b2c3d4-1111-4111-8111-111111111111": {
      "agent": "a1b2c3d4", "owner": { "pid": 82575, "start": 1792881276426716 },
      "started": 1792881276, "seen": 1792881276, "phase": "active",
      "wants": [ { "waiter": "e5f6a7b8", "path": "src/schema.ts", "at": 1792881276, "told": false } ]
    },
    "c3d4e5f6-3333-4333-8333-333333333333": {
      "agent": "c3d4e5f6", "owner": { "pid": 82580, "start": 1792881276497568 },
      "started": 1792881276, "seen": 1792881276, "phase": "active"
    },
    "e5f6a7b8-2222-4222-8222-222222222222": {
      "agent": "e5f6a7b8", "owner": { "pid": 82576, "start": 1792881276427106 },
      "started": 1792881276, "seen": 1792881276, "phase": "active",
      "subs": { "e5f6a7b8:9f3e01aa": { "seen": 1792881276, "done": false, "busy": 0 } },
      "blocked": { "e5f6a7b8": { "path": "src/schema.ts", "holder": "a1b2c3d4", "at": 1792881276 } }
    }
  },
  "locks": {},
  "upstream": { "remote": "origin", "branch": "main", "oid": "1c8c7404a5b6b3e32e8982a147807d78e08f813b" }
}
```

| Key | Meaning |
|---|---|
| `v` | Format version (3). |
| `off` | `true` after `agent-lock off`: coordination is off in this checkout. |
| `claims.<path>` | `by` holder id; `session`; `owner` (pid + process start time: decides alive vs dead, survives pid reuse); `at` / `touched` (claimed / last active: an edit, a hand-over, a commit attempt; epoch seconds); `edits` (0 = claimed, never written); `pending` (a tool call in flight, or `commit:<pid>:<start>` / `pkg:…`: blocks hand-overs); `made_by` (the tool call that created it: a failed edit releases only its own claim); `inherited` (whose uncommitted edits came with it); `reason` (`wait` for a `wait --claim` reservation). Empty fields are left out. |
| `sessions.<id>` | `agent`, `owner`, `started`, `seen`, `phase` (active, idle, ended), `end_reason`, `subs` (subagents: done, tool calls in flight), `wants` (who waits for its files), `blocked` (what its agents wait for), `nagged`, `takeover`, `bash` (stamps of claimed files around a Bash call), `bg`. |
| `locks.git` / `locks.pkg` / `locks.sync` | agent-lock's own commit, sync or push; the one package-manager command running; a sync installing an upstream tip (`head`, `tip`, `paths`). A dead holder's git or pkg lock is dropped. |
| `orphans.<path>` | A dirty claim that ended without a commit: whose changes may still be in the file. |
| `upstream` | The last integrated remote tip: rewrite detection and the push lease. |

The file stays a few KB: ended sessions go after a day, waits after 30 min, orphan notes after 7 days. `agent-lock status` shows it as tables; `status --json` prints it.

---

## Install

**Once per machine**, from a shell:

```bash
claude plugin install agent-lock --marketplace lovieco/agent-lock
```

Inside a Claude Code session the same command is `/plugin install agent-lock --marketplace lovieco/agent-lock` (it asks you to confirm the source). It needs Claude Code 2.1.292 or later from a shell, 2.1.275 or later in a session; on older versions run `/plugin marketplace add lovieco/agent-lock`, then `/plugin install agent-lock@lovieco`. The terminal CLI, the IDE extensions and the desktop app's Code tab (local and SSH sessions) share the install.

The hooks, the shims, the skill and the `agent-lock` CLI are then active in every git repo you open. Nothing is written into your working trees (the state lives in `.git`).

**Updates.** Claude Code auto-updates only Anthropic's own marketplaces. Update with `claude plugin update agent-lock@lovieco`, or turn on auto-update once: `/plugin` → Marketplaces → lovieco → Enable auto-update.

**For a team**, enable it in the project so everyone who opens the repo gets it:

```bash
agent-lock install   # adds extraKnownMarketplaces.lovieco and enabledPlugins["agent-lock@lovieco"]
                     # to .claude/settings.json; commit that file
```

A teammate gets the plugin when they open the repo in Claude Code and trust the folder; there is nothing to install by hand. Add `"autoUpdate": true` to the `lovieco` entry to keep everyone on the same version. Cloud sessions (claude.ai/code) do not load plugins declared by a repo, and need none: each runs in its own clone, so there is no shared checkout to coordinate.

The command keeps every other setting and is idempotent. A `lovieco` marketplace entry that points elsewhere (a fork, a local checkout) is left alone with a warning; `--force` re-points it. It also removes v0.1's vendored hooks. `agent-lock uninstall` removes the plugin again.

**The CLI in your own shell** (optional, for `status`, `doctor`, `selftest`):

```bash
curl -fsSL https://raw.githubusercontent.com/lovieco/agent-lock/main/install.sh | bash
```

It clones the repo to `~/.agent-lock-core` and links `bin/agent-lock` onto your `PATH`. No Node, no runtime.

**Verify:** `agent-lock selftest` runs the real hooks, shims and CLI in a throwaway repo with a bare remote (12 steps, PASS/FAIL each, under half a second). `agent-lock doctor` checks the checkout and your setup; `--fix` repairs what it can.

**Platforms.** macOS and Linux, git ≥ 2.40, Claude Code with plugins. The repo ships a universal macOS binary and static (musl) Linux x86_64 and arm64 binaries in `bin/native/`; `.github/workflows/native.yml` builds, tests and commits them on every change to `rust/`. Windows is not supported. It is a Claude Code plugin only: claude.ai and Cowork refuse plugins that ship executables, so there is no `.plugin` upload. **On a machine without a binary, agent-lock is inactive and never blocks anything** (fail open): the launcher prints one line on stderr and exits 0.

## Commands

From `agent-lock help`. Agent commands act as `--as <id>` › `$AGENT_LOCK_AGENT_ID` › `$CLAUDE_CODE_SESSION_ID`. Every command takes `-C <dir>`.

| Command | What it does |
|---|---|
| `commit -m <msg> [-- <paths>]` | Commit exactly your claimed files, or only `<paths>` (also `-F <file\|->`, `--push`, `--no-verify`, `--dry-run`). |
| `push [--accept-rewrite]` | Publish HEAD to origin; syncs and retries when the remote moved. |
| `sync [--accept-rewrite]` | Bring in upstream commits without touching anyone's uncommitted work. |
| `wait <file>… [--claim] [--timeout <s>]` | Wait until you could edit the files (`--claim` takes them). Exit 0 free, 1 timeout, 3 deadlock. |
| `release <file>… [--force] \| --mine` | Release your own or stale claims (`--force`: another agent's, with the user's approval). |
| `mine [-z]` | Print the files you hold: `agent-lock mine -z \| xargs -0 -r <tool>`. |
| `status [dir] [--json]` | Claims, sessions, locks and orphans of this checkout (`--json`: the state). |
| `off [dir]` / `on [dir]` | Turn agent-lock off / back on in this checkout. |
| `doctor [dir] [--fix]` | Report stale claims, interrupted syncs, a corrupt state file, setup problems (`--fix` repairs). |
| `install [dir] [--force]` / `uninstall [dir] [--force]` | Enable / remove the plugin in `<dir>/.claude/settings.json`. |
| `selftest [--keep]` | Run the real hooks, shims and CLI in a throwaway repo. |
| `version` / `help` | Print the version / the command list. |

Exit codes: 0 ok, 1 refused or failed, 2 usage error, 3 deadlock.

## Configuration

| Setting | Effect |
|---|---|
| `CLAUDE_FILE_LOCK=0` | Every hook and shim becomes a no-op. For incident recovery, not contention (an agent setting it needs your approval). |
| `agent-lock off` / `on` | The same for one checkout (`"off": true` in the state file). |
| `AGENT_LOCK_AGENT_ID` | Who the shims and CLI act as. SessionStart sets it; a subagent prefixes its own. |
| `AGENT_LOCK_PKG_WAIT` | Seconds a package-manager command waits for another to finish (default 1200). |
| `.agent-lock-shared` | Committed at the repo top: one glob per line (`#` comments, `!pat` excludes, last match wins; no `/` = any directory, `**` crosses directories). Adds to the built-in **shared files**, which several agents may edit at once. `!*` turns sharing off. |
| `AGENT_LOCK_GIT` | The git binary agent-lock's own git calls run (commit, sync, push and its queries; default: the first `git` on `PATH` that is not agent-lock, with macOS's `/usr/bin/git` swapped for the developer directory's git). The shim always execs the next real `git` on `PATH`. |

## Performance

macOS arm64, release build, medians.

| | v0.2 (Node) | v0.3 (Rust) |
|---|---|---|
| One hook, as Claude Code runs it (`sh -c`) | ≈ 80 ms | ≈ 7.4 ms, of which the `sh -c` floor is ≈ 4.8 ms: agent-lock's own cost is ≈ 1.2–2.6 ms |
| `git` through the shim | no shim (Bash text was parsed) | +2.4 ms over the real git binary; still faster than `/usr/bin/git`, whose xcrun trampoline the shim skips |
| First claim of an existing file | one single-path `git status` | one single-path `git status` (≈ 8 ms), only while another session is active |
| State | one JSON file per claim and per session, plus mutex files | one file, one flock transaction per change |
| Runtime dependencies | Node ≥ 18 | none (Rust standard library) |

Under load: 2,000 contended transactions from 8 threads take about 0.1 s with no lost update (0.15 s in a debug test build). A stress test of 9 concurrent agents making about 3,700 hook calls never let two agents hold one file. 268 tests: `cargo test` in `rust/` runs 267, and one benchmark is `#[ignore]`d. Production code is about 10,300 lines (non-blank, non-comment, non-test lines in `rust/src`).

## Limitations

- **Subagents share the session's Bash environment.** The shims see the main agent's id, so while a subagent runs, the main agent's own files are protected from whole-tree git too. Subagents pass `--as <id>` to agent-lock commands (the Bash hook enforces it) and prefix `AGENT_LOCK_AGENT_ID=<id>` for raw git or npm.
- **What git runs itself bypasses the shim:** repo hooks, `rebase -x`, `bisect run`. Pre-commit hooks are scanned before `agent-lock commit` runs them and checked for damage afterwards. git or a package manager run by absolute path (`/usr/bin/git`), or under a `PATH` without the shims, is not checked either.
- **Writes the hooks cannot see** (Python scripts, editors outside Claude Code) are detected after the Bash command by an mtime and size check, not prevented. Files created through Bash have no owner until someone claims them (`agent-lock commit -- <paths>`).
- **One machine.** No cross-machine locks; machines coordinate through `push` and `sync`.
- **Local filesystems.** Old NFS without `flock` is unsupported. Windows is unsupported.
- **Git worktrees only.** Files outside a worktree are never claimed.
- **Whole files.** No line ranges: two agents who want one file take turns.
- **A rebase or replay conflict needs a human.** `sync` reports it and changes nothing.
- **No build or test before `--push`.** A scoped commit is HEAD + your files, a tree nobody built. Run the tests first.
- **Claude Code only**, and cooperative: it is not a security boundary.

---

## Prior art

| System | Era | Granularity | Notes |
|---|---|---|---|
| RCS, SCCS | 1980s | per-file lock | the original `co -l` model |
| Visual SourceSafe | 1995 – 2005 | per-file lock | bundled with Visual Studio; displaced by Git |
| Perforce | 1995 – | per-file lock (default) | still common in game dev, especially for binary assets |
| `svn lock` | 2004 – | per-file lock (opt-in) | for files that can't merge (Photoshop, video) |
| `flock` (Unix) | 1983 – | per-fd advisory | here it guards one transaction, not a claim |
| `git index.lock` | 2005 – | per-repo write lock | Git's own pessimistic lock on the index |
| **Agent Lock** | 2026 – | per-file claim, until commit | the same idea, applied to AI agents |

Visual SourceSafe is the closest historical analog: widely used through the mid 2000s, then displaced by Git for the reason every per-file-lock VCS lost: it was a bad fit for *humans*. The model that failed for one kind of client works for another, because the deciding variables (merge feasibility, the cost of being told to wait, the cost of a tangled change) flip when the client is an AI.

---

## FAQ

**Q. Why hold a claim until commit, not just during each edit?**

A. v0.1 locked per edit. That stopped interleaved writes, but once A's edit landed, B could edit on top of it and either agent could commit both changes. Holding the claim until commit gives every dirty file one owner, so each commit holds one agent's work.

**Q. Won't long claims block everyone?**

A. Agents are told to do other work first, and most tasks touch different files. When someone waits, the holder is told to commit as soon as its change is consistent, the Stop reminder turns finished work into commits, crossed waits are reported as deadlocks, and a dead holder's files free up at once.

**Q. Earlier versions said `flock` was the wrong tool. Why use it now?**

A. Because it now guards something else. A claim has to outlive every hook process and last until the agent commits, so a claim cannot be a held lock, and in v0.3 it is not: it is an entry in the state file. `flock` guards only the read-modify-write of that file, a few milliseconds at a time. There, "the lock dies with the process" is the property we want: a hook killed mid-transaction cannot leave a stuck lock. One file under one lock also makes every check-then-change atomic, with no per-file guard files, nonces or retries.

**Q. Why shims instead of parsing shell commands?**

A. v0.2 parsed every Bash command (here-docs, `$(…)`, `bash -c`, pipes, `xargs`, `find -exec`, aliases) to guess what git would do, and still could not see into scripts or Makefiles. A shim is git as the shell finally runs it: it gets the real argument vector, whatever produced it. Most git calls never read the state, and a shimmed git costs about 2.4 ms more than the real binary.

**Q. Why Rust?**

A. A hook runs on every edit and every Bash call. Starting Node cost about 80 ms per hook; a native binary costs about 1–3 ms of its own. The standard library has what agent-lock needs (files, `flock`, processes, signals), so there are no dependencies to install, audit or keep in step, and one binary per platform covers hooks, shims and CLI.

**Q. Can I still use plain git?**

A. Alone, yes: nothing is refused. With other agents active, the shim refuses only the forms that sweep their work. Scoped commands keep working.

**Q. How do I override a claim?**

A. `agent-lock release --force <file>`. An agent can run it only after you approve the prompt; in your own shell, just run it. To turn coordination off: `agent-lock off` (this checkout) or `CLAUDE_FILE_LOCK=0`; both need your approval when an agent tries them.

**Q. Does it change my repo or `.gitignore`?**

A. No. State lives in `.git/agent-lock.json`, plus the session index and shim links in the plugin's data dir. agent-lock changes files in your working tree only when you run `agent-lock install` or `uninstall`: `.claude/settings.json`, plus removing v0.1 leftovers (`.agent-lock/src` and `.agent-lock/hooks`, their hook entries in `.claude/settings.local.json`, the v0.1 project skill).

---

Built by [Lovie.co](https://www.lovie.co)
