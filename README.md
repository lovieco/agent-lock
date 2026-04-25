# livehub

**A Claude Code skill that lets multiple agents work on the same codebase — locking at the function/class level, not the file level.**

When an agent starts editing, livehub drops a comment line at the top of the file identifying which top-level declaration (function, class, const) it's touching. Another agent editing a *different* declaration in the same file continues uninterrupted; an agent going after the *same* declaration gets blocked with a clear error. No sidecar `.lock` files, no hidden state — the lock *is* the comment.

---

## Skip the merge. Just edit.

The usual way to run parallel agents:

```
  branch ──► change ──► PR ──► conflicts ──► merge ──► repeat
                                   ▲
                                   └── you live here
```

Every agent gets its own branch or worktree, does work in isolation, opens a PR, and then somebody — usually you — referees the merge when three agents all touched `schema.ts`. The coordination cost shows up at the end, as conflicts, and it compounds with every extra agent.

livehub flips that. Agents share **one tree** and take turns at the *node* level:

```
  lock(function X) ──► edit ──► unlock ──► next agent
   │
   └── the scope is the actual top-level declaration being edited,
       not the whole file — so different functions in the same file
       can be edited in parallel.
```

No branches. No PRs. No merge conflicts — because two agents can never hold the same semantic unit at the same moment. When a second agent tries, it gets this, immediately, before any bytes hit disk:

```
[livehub] BLOCKED: src/schema.ts (node fn:userSchema) is locked by agent
"claude-code-sess-a1b2c3d4" since 2026-04-23T00:41:12Z (4s ago, reason:
Claude Code Edit). Wait, work on a different node, or set
CLAUDE_FILE_LOCK=0 to override.
```

Side by side:

| | Branch/PR workflow | livehub live edit |
|---|---|---|
| **Isolation** | per-agent worktree | shared tree, per-node mutex |
| **Unit of coordination** | the file | the top-level declaration (function, class, const) |
| **Where coordination happens** | at merge time (after the work) | at edit time (before the work) |
| **Failure mode** | silent merge regressions | loud `BLOCKED` exit, agent retries |
| **State visible in** | git log, PR diffs | the file itself (one comment line per active lock) |
| **Cleanup** | rebase, squash, close PRs | markers auto-strip on release |
| **Scales to N agents?** | merge pain grows O(N²) | lock contention stays local to each node |

Because the lock is literally a comment in the file, you can see who's working on what just by opening the file — or on macOS, by glancing at Finder (locked files get a red dot).

**The short version:** stop merging. Start editing.

---

## Table of contents

1. [Why](#1-why)
2. [How it works in 30 seconds](#2-how-it-works-in-30-seconds)
3. [Architecture](#3-architecture)
4. [Install](#4-install)
5. [Verify it works](#5-verify-it-works)
6. [Daily use](#6-daily-use)
7. [All commands](#7-all-commands)
8. [What files are protected](#8-what-files-are-protected)
9. [Kill switches](#9-kill-switches)
10. [Using it from your own scripts](#10-using-it-from-your-own-scripts)
11. [Uninstall](#11-uninstall)
12. [Layout](#12-layout)

---

## 1. Why

Two agents editing the same file at the same time = one agent's work silently overwritten. livehub gives you a file-level mutex so that never happens — without branches, worktrees, or PRs.

## 2. How it works in 30 seconds

- **PreToolUse hook** — before Claude Code runs `Write`/`Edit`/`MultiEdit`, livehub parses the target file with a tiny JS/TS heuristic, finds the top-level declaration the edit lands in (e.g. `fn:handleRequest`, `cls:UserController`, `var:config`), and drops a comment line at the top: `// livehub lock: agent=… node=fn:handleRequest started=… reason=…`.
- **Another agent shows up** — its PreToolUse hook reads the same marker list. If they want a *different* node, they add their own marker and proceed. If they want the *same* node (or the whole file, for a `Write`), they exit with code 2 and see the block message.
- **PostToolUse hook** — as soon as the edit completes, livehub strips that agent's markers from the file.
- **Whole-file fallback** — when the heuristic can't place the edit (non-JS languages, `Write` tool, file doesn't parse), the lock escalates to `node=*` which dominates any per-node lock.
- **Stale locks** — anything older than 10 minutes is treated as abandoned and auto-cleared on the next attempt (and on `SessionEnd`).

On macOS, locked files also get a red **Locked** Finder tag so you can spot them at a glance.

### What a file looks like while locked

```ts
// livehub lock: agent=claude-code-sess-a1b2c3d4 node=fn:handleRequest started=2026-04-23T00:41:12Z reason=Claude Code Edit
// livehub lock: agent=claude-code-sess-e5f6g7h8 node=fn:formatResponse started=2026-04-23T00:41:15Z reason=Claude Code Edit
import express from 'express';

function handleRequest(req, res) { … }   // ← sess-a1b2c3d4 is editing this
function formatResponse(data) { … }       // ← sess-e5f6g7h8 is editing this
function parseInput(raw) { … }            // ← anyone else can edit this
```

## 3. Architecture

Four concepts, stacked:

```
  ┌─────────────────────────────────────────────────────────────┐
  │  AGENTS                                                     │
  │  Claude Code sessions (not shipped by livehub).             │
  │  Each has an id like "claude-code-sess-a1b2c3d4" and issues │
  │  Write / Edit / MultiEdit tool calls against the codebase.  │
  └───────────────────────────────┬─────────────────────────────┘
                                  │ tool call intercepted by
                                  ▼
  ┌─────────────────────────────────────────────────────────────┐
  │  HOOKS                                                      │
  │  .livehub/hooks/*.mjs  —  wired in .claude/settings.json    │
  │                                                             │
  │    PreToolUse   ─►  file-lock-pre.mjs    (acquire or block) │
  │    PostToolUse  ─►  file-lock-post.mjs   (release)          │
  │    SessionEnd   ─►  file-lock-purge.mjs  (sweep stale)      │
  └───────────────────────────────┬─────────────────────────────┘
                                  │ delegates to
                                  ▼
  ┌─────────────────────────────────────────────────────────────┐
  │  RULES                                                      │
  │  .livehub/lock/*.cjs  —  pure Node, no dependencies         │
  │                                                             │
  │    semantic.cjs   → locate the enclosing top-level node     │
  │                     (fn:foo, cls:Bar, var:x, or '*')        │
  │                                                             │
  │    file-lock.cjs  → • node='*' dominates any other lock     │
  │                     • node=X blocks another agent on X      │
  │                     • same agent + same node = self-release │
  │                     • age > 10 min = stale, auto-override   │
  └───────────────────────────────┬─────────────────────────────┘
                                  │ reads + writes markers in
                                  ▼
  ┌─────────────────────────────────────────────────────────────┐
  │  STATE — the codebase itself                                │
  │                                                             │
  │    src/foo.ts                                               │
  │    // livehub lock: agent=sess-A node=fn:handleRequest …    │
  │    // livehub lock: agent=sess-B node=fn:formatResponse …   │
  │    function handleRequest() { … }   ← sess-A                │
  │    function formatResponse() { … }  ← sess-B                │
  │    function parseInput() { … }      ← free, anyone can edit │
  └─────────────────────────────────────────────────────────────┘

  ┌─────────────────────────────────────────────────────────────┐
  │  SKILL (sidecar, not in the enforcement path)               │
  │  .claude/skills/file-lock/SKILL.md                          │
  │                                                             │
  │  Protocol doc Claude reads on session start. Teaches the    │
  │  agent the vocabulary — "nodeId", "ELOCKED", "whole-file    │
  │  fallback" — so it can reason gracefully about a BLOCKED    │
  │  result (wait / work on a different node / escalate).       │
  └─────────────────────────────────────────────────────────────┘
```

The enforcement is in **Hooks + Rules**. The **Agent** is the thing being enforced against. The **Skill** is how the agent learns the protocol. The **State** — crucially — lives inside the files themselves, not in a sidecar database, so `grep "livehub lock:" src/` is the source of truth.

| Layer | Lives in | Role |
|---|---|---|
| Agents | Claude Code sessions | Make the edits. Identified by session id. |
| Hooks  | `.livehub/hooks/*.mjs` + `.claude/settings.json` | Intercept tool calls. Translate Claude's `{tool_name, tool_input}` payload into `acquireLock` / `releaseLock` / `purge`. |
| Rules  | `.livehub/lock/*.cjs` | Decide what counts as a node (`semantic.cjs`) and what counts as a collision (`file-lock.cjs`). No I/O beyond the marker lines. |
| Skill  | `.claude/skills/file-lock/SKILL.md` | Docs. Teach the agent how to read a BLOCKED message and what to do next. |

## 4. Install

### One-liner (gets the CLI on your PATH)

```bash
curl -fsSL https://raw.githubusercontent.com/lovieco/livehub/main/install.sh | bash
```

This clones livehub into `~/.livehub-core`, installs its dev deps (for `npm test`), and symlinks the `livehub` CLI into a writable bin dir on your `PATH` (`~/.local/bin`, `~/bin`, or `/usr/local/bin`). Node ≥ 16 and git required.

Flags (pass after `bash -s --`):

| Flag | Meaning | Default |
|---|---|---|
| `--home <dir>`   | clone target | `~/.livehub-core` |
| `--ref <ref>`    | branch / tag / commit | `main` |
| `--bin <dir>`    | where to symlink | auto-detected |
| `--no-symlink`   | skip the PATH symlink | — |
| `--no-deps`      | skip `npm install` | — |

Example — pin a specific release and symlink into `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/lovieco/livehub/main/install.sh \
  | bash -s -- --ref v0.1.0 --bin ~/.local/bin
```

Re-running the one-liner updates to the latest on the chosen ref.

### Then, wire it into a project

```bash
livehub install                     # wire up the current project
livehub install /path/to/project    # or a specific project
```

### Manual install (no curl, no symlink)

```bash
git clone https://github.com/lovieco/livehub.git ~/.livehub-core
ln -s ~/.livehub-core/bin/livehub /usr/local/bin/livehub
```

### What install actually does

1. Copies `lock/` and `hooks/` into `<project>/.livehub/`
2. Copies `skill/SKILL.md` into `<project>/.claude/skills/file-lock/SKILL.md`
3. Patches `<project>/.claude/settings.json` and wires three hooks:

   | Event | Matcher | Script |
   |---|---|---|
   | `PreToolUse`  | `Write\|Edit\|MultiEdit` | `.livehub/hooks/file-lock-pre.mjs` |
   | `PostToolUse` | `Write\|Edit\|MultiEdit` | `.livehub/hooks/file-lock-post.mjs` |
   | `SessionEnd`  | —                        | `.livehub/hooks/file-lock-purge.mjs` |

Hooks are **prepended** to any existing groups, so anything you already have wired keeps running.

## 5. Verify it works

```bash
livehub status   # install state, kill-switch state, active locks
livehub test     # simulates acquire → collision → release end-to-end
```

A healthy `test` prints four `OK` lines.

## 6. Daily use

You don't. Once installed, Claude Code handles it automatically — edits just work, and collisions just block.

The commands you'll actually reach for:

- `livehub list` — what's locked right now?
- `livehub release <file>` — something's stuck, unblock me.
- `livehub off` / `livehub on` — temporarily disable / re-enable the whole system.

## 7. All commands

| Command | What it does |
|---|---|
| `install [<target>]`    | copy files + wire `settings.json` |
| `uninstall [<target>]`  | strip `settings.json` + remove `.livehub/` and skill copy |
| `status [<target>]`     | show install state, kill-switch state, active locks |
| `list [<target>]`       | list every locked file in the project |
| `release <file>`        | force-release one file's lock (any owner) |
| `purge-all [<target>]`  | strip every marker from every file |
| `on` / `off` `[<target>]` | toggle `CLAUDE_FILE_LOCK` kill-switch in `settings.json` |
| `test [<target>]`       | simulate acquire / collision / release end-to-end |
| `help`                  | print usage |

## 8. What files are protected

Files whose extension has a known comment syntax:

```
.ts .tsx .js .jsx .cjs .mjs .md .html .py .sh .sql .css
```

**Semantic (node-level) scope** is available on the JS/TS family — `.ts .tsx .js .jsx .cjs .mjs`. For those files, livehub's heuristic parser locates the enclosing top-level `function`, `class`, or `const/let/var` binding and locks just that node. Other agents can freely edit other top-level declarations in the same file.

For all other supported extensions (`.md`, `.py`, `.sh`, `.sql`, `.css`, `.html`), and whenever the JS/TS heuristic can't place an edit (`Write` tool, file doesn't parse, edit lands between declarations), the lock falls back to **whole-file** (`node=*`) — the safe default.

JSON, binary, and other unsupported extensions have no comment syntax to host the marker, so they cannot be locked — edits proceed unprotected. (If you need to coordinate writes to a JSON file, use `withLock()` from your own script; see §9.)

## 9. Kill switches

| Switch | Effect |
|---|---|
| `CLAUDE_FILE_LOCK=0`     | disables livehub for the current process |
| `CLAUDE_FILE_LOCK_TAG=0` | disables the macOS Finder "Locked" tag only |
| `livehub off`            | sets `CLAUDE_FILE_LOCK=0` persistently in `<project>/.claude/settings.json` |
| `livehub on`             | reverses `livehub off` |

## 10. Using it from your own scripts

Non-Claude scripts can opt into the same lock:

```js
const { withLock } = require('/path/to/.livehub/lock/file-lock.cjs');

// whole-file lock (default — nodeId = '*')
await withLock(
  'src/foo.ts',
  { agentId: 'my-pipeline-v1', reason: 'applying batch updates' },
  async () => { /* do edits here */ }
);

// scoped to one top-level declaration
await withLock(
  'src/foo.ts',
  { agentId: 'my-pipeline-v1', nodeId: 'fn:processBatch', reason: '…' },
  async () => { /* edits to processBatch() only */ }
);
```

`withLock` acquires, runs your callback, and releases on success **or** throw. If it can't acquire, it throws with `err.code === 'ELOCKED'`.

Lower-level API: `acquireLock(filePath, opts)`, `releaseLock(filePath, opts)`, `listLocks(dir)`. Each accepts `opts.nodeId` — use `'*'` (the default) for whole-file scope, or a value like `'fn:foo'`, `'cls:Bar'`, `'var:config'` for per-node scope. To discover available node ids programmatically:

```js
const { listTopLevelNodes } = require('/path/to/.livehub/lock/semantic.cjs');
const nodes = listTopLevelNodes(fs.readFileSync('src/foo.ts', 'utf-8'), '.ts');
// → [{ id: 'fn:processBatch', kind: 'fn', name: 'processBatch', … }, …]
```

## 11. Uninstall

```bash
livehub uninstall            # current project
livehub uninstall /path/...  # specific project
```

This strips livehub entries from `settings.json` and deletes `.livehub/` plus the skill copy. Your own hooks and settings are preserved.

## 12. Layout

```
livehub/
├── README.md
├── install.sh                # one-liner host installer (curl | bash)
├── bin/livehub               # the install/uninstall CLI
├── lock/
│   ├── file-lock.cjs         # core: acquireLock, releaseLock, withLock, listLocks
│   ├── semantic.cjs          # JS/TS top-level declaration finder (node scoping)
│   └── mac-tags.cjs          # red "Locked" Finder tag via xattr
├── hooks/
│   ├── file-lock-pre.mjs     # PreToolUse: acquire-or-block
│   ├── file-lock-post.mjs    # PostToolUse: release immediately
│   └── file-lock-purge.mjs   # SessionEnd: sweep stale markers
├── skill/SKILL.md            # protocol doc copied into .claude/skills/
└── test/                     # test suite (run `npm test`)
```

---

## Running the test suite

```bash
npm install          # installs c8 for coverage (only dev dep)
npm test             # runs the full suite
npm run coverage     # runs the suite under c8 and prints a coverage report
```

The suite covers the lock module, mac-tags helper, all three hooks, and every CLI command. Target: 100% line/branch/function coverage.
