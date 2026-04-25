---
name: file-lock
description: Prevent two agents from editing the same file at once. A pre-edit Claude Code hook auto-acquires a sidecar `.lock` file (plus a visible `// AGENT-LOCK` comment at the top of commentable source files) for every Write/Edit/MultiEdit tool call, and blocks collisions with exit 2. Scripts can opt in by wrapping their own writes with `withLock()` from the lock module.
---

# File-Lock Protocol

Two agents editing the same file = lost writes. This skill enforces a file-level mutex with one unified lock module — used both by Claude Code's Edit/Write hooks and by Node scripts that mutate shared state.

## How it works

**One module, two entry points** — everything goes through a single lock library (conventionally `src/lock/file-lock.cjs`, but any path is fine as long as both the hooks and scripts import the same copy).

- **Sidecar `.lock` file**: `<file>.lock` is the authoritative lock, written atomically via `O_EXCL`. Contains `{ agentId, startedAt, reason, pid }`.
- **Visible banner** at line 1 of commentable files (`.ts .tsx .js .jsx .cjs .mjs .md .html .py .sh .sql .css`):
  ```ts
  // AGENT-LOCK: id=<agent> started=<iso> reason=<what>
  ```
  Shebang-aware: on files that start with `#!`, the banner goes on line 2 so `node` / `sh` still recognize the interpreter line.
- **Stale TTL**: 10 min default. A lock older than TTL is treated as abandoned and auto-overridden — protects against crashed sessions.
- **Env flag**: `export CLAUDE_FILE_LOCK=0` to disable everywhere.

## Claude Code integration (automatic)

Wired via `.claude/settings.json`:

| Event | Hook | What it does |
|---|---|---|
| PreToolUse (Write / Edit / MultiEdit) | `.claude/hooks/file-lock-pre.mjs` | Acquires lock or blocks (exit 2) |
| PostToolUse (Write / Edit / MultiEdit) | `.claude/hooks/file-lock-post.mjs` | Releases lock |
| SessionEnd | `.claude/hooks/file-lock-purge.mjs` | Removes stale sidecars |

Every hook just shells into the lock module via `createRequire`, so behavior stays consistent across entry points.

**What Claude sees on a collision:**
```
[file-lock] BLOCKED: path/to/file is locked by agent "claude-code-sess-abc"
since 2026-04-19T08:30:00Z (42s ago, reason: Claude Code Edit). Wait for it to finish,
or set CLAUDE_FILE_LOCK=0 to override, or force-release with:
  node <lock-module>.cjs release "/abs/path/to/file"
```
The tool call never runs. Claude should wait, work on a different file, or ask the user to intervene.

## Script API (opt-in, for Node scripts)

```js
const { withLock, acquireLock, releaseLock, listLocks } = require('<path-to-lock-module>');

// Preferred — auto-release on success OR throw
await withLock('<path/to/shared-state.json>',
  { agentId: '<feature-name>-v1', reason: '<short action phrase>', staleMs: 5 * 60 * 1000 },
  async () => {
    // read/modify/write the file safely here
  }
);

// Manual
const res = acquireLock(filePath, { agentId, reason });
if (!res.ok) throw new Error(`Locked by ${res.heldBy}`);
try { /* edit */ } finally { releaseLock(filePath, { agentId }); }
```

**Error behavior:**
- `withLock` throws with `err.code === 'ELOCKED'` and `err.details = { heldBy, since, reason, ageMs }` when unable to acquire.
- `releaseLock` returns `{ ok: false, error: 'not owner (…)' }` if `agentId` mismatch — call without `agentId` to force-release.

## When to use

Use `withLock()` in scripts that mutate shared files **outside** a Claude Code session — cron jobs, batch pipelines, server-side API routes. Inside a Claude session, the hook handles it for you.

| Situation                                              | Action |
|--------------------------------------------------------|--------|
| Claude Code agent about to run Edit/Write/MultiEdit    | **Nothing — hook handles it automatically** |
| Node script that mutates a shared JSON/state file      | Wrap write with `withLock()` |
| Server-side API route mutating shared JSON             | Wrap write with `withLock()` |
| Parallel long-running agents sharing a state file      | Both — hook on edits, `withLock()` around multi-step batches |

## Shared state files that must be locked

Any file that more than one agent/process can write concurrently. Typical candidates:

- Shared JSON state databases (entity tables, catalogs, registries).
- Queue / approval / job-tracker JSON files.
- Generated graph, index, or cache files that multiple pipelines update.
- Configuration files edited by both humans and automation.

Add each project's list to its own `CLAUDE.md` or a `LOCKED_FILES.md` so everyone working in the repo knows which paths require `withLock()`.

## CLI

```bash
# List all active locks in the repo
node <lock-module>.cjs list

# Inspect one
node <lock-module>.cjs show <path/to/file>

# Force release (e.g. after a crash)
node <lock-module>.cjs release <path/to/file>

# Manually purge every stale sidecar
node .claude/hooks/file-lock-purge.mjs
```

## Conventions

- **agentId** format: `<feature-name>-v<n>` — e.g. `<pipeline-name>-v1`, `<sync-job>-v2`. Hooks use `claude-code-<session-id-prefix>` automatically.
- **reason** should be a short action-oriented phrase: `"<applying N updates>"` — not `"doing stuff"`.
- **staleMs**: 5 min for fast tasks, default (10 min) for batches, max 30 min.
- Always release in `finally`; prefer `withLock()` which does this for you.
- **Never** commit `.lock` files or `.claude/locks.json` — they must be gitignored.

## Lock discipline: acquire late, release immediately

**Hold locks for the minimum possible time.** The lock is a mutex — every second it's held is a second another agent is blocked. Treat it like a database transaction: open it as late as possible, close it as early as possible.

- **Acquire only when you're ready to write.** Don't lock a file, then go read other files, call an API, or think. Do all prep work first — read inputs, compute the diff, stage the new content in memory — *then* acquire the lock, write, and release.
- **Release the moment the write is done.** The instant the file is saved, the lock must drop. Never hold a lock across a user prompt, a network call, a subagent spawn, or a long compute.
- **One file finished = one immediate unlock.** When editing multiple files, release each as soon as its write completes. Do not batch-hold N locks while editing file N+1.
- **Prefer `withLock()` over manual acquire/release** — it guarantees release on both success and throw, with no window for leaks.
- **Target hold time: under 1 second for single writes, under a few seconds for multi-step batches.** If a lock is held longer than that, the code is doing something inside the critical section that doesn't belong there.
- **Claude Code hooks already do this** — the PreToolUse hook acquires, the PostToolUse hook releases immediately after the tool call. Don't build workflows that defeat this by chaining reads/computes between Edit calls on the same file under one manual lock.

**Why:** when two agents hold long-running locks on the same hot file, collisions compound and one agent's work is silently discarded. Keep locks short and collisions drop to near-zero.

## What the user sees

**In a source file** (commentable types):
```ts
// AGENT-LOCK: id=<feature>-v1 started=2026-04-19T08:30:00Z reason=<short action>
"use client";
import { ... }
```

**In a shebang script** (shebang preserved on line 1):
```sh
#!/usr/bin/env node
// AGENT-LOCK: id=claude-code-abc123 started=2026-04-19T08:30:00Z reason=Claude Code Edit
import fs from "node:fs";
```

**In JSON / binary files** (no comment) — only the sidecar exists:
```
path/to/state.json
path/to/state.json.lock      ← presence = locked
```

Inspect with `ls **/*.lock 2>/dev/null` (or `find . -name "*.lock" -not -path "./node_modules/*"`).

**In Finder (macOS)** — locked files get a **red `Locked` dot** via the `com.apple.metadata:_kMDItemUserTags` extended attribute. They show up with a coloured dot in every Finder view and respond to `mdfind "kMDItemUserTags == 'Locked'"` Spotlight searches. Acquire tags the file; release strips the tag; the stale-purge path (auto-override of a >TTL lock) also strips it. Pre-existing user tags on the file are preserved — Locked is added to / removed from the existing tag set, not an overwrite. No-op on Linux / Windows. Disable with `CLAUDE_FILE_LOCK_TAG=0`. Implemented in [`src/lock/mac-tags.cjs`](../../../src/lock/mac-tags.cjs) (shells out to `/usr/bin/xattr` + `/usr/bin/plutil` — no brew deps). Tagging failures are non-fatal: a Spotlight hiccup or an xattr-hostile filesystem (iCloud, Dropbox) will log and continue; the lock itself still acquires.

Inspect manually:
```bash
xattr -l path/to/locked-file                          # see the raw xattr
mdfind 'kMDItemUserTags == "Locked"' -onlyin .        # find every Locked file
```
