# agent-lock

**Live multi-agent coding in one shared checkout — no branches, no PRs, no merge.**

In Google Docs, Figma, and Notion, two people editing the same file at the same time is normal. Code is the last holdout: every change still goes through a branch, a PR, and a merge conflict. That loop made sense when humans were the only writers. With multiple AI agents in one repo, it's the bottleneck.

agent-lock lets every agent edit the same tree at the same time. When one starts editing a file, the others see "taken, try another" — and pick a different file or wait a few seconds. The whole thing is one tiny lockfile per active edit. That's the whole protocol.

---

## The problem in one picture

Today, every code change goes through the git dance:

```
  git clone  ─►  branch  ─►  edit  ─►  commit  ─►  push  ─►  PR  ─►  conflict  ─►  merge
                                                                          │
                                                                          ▼
                                                                you, manually
```

That made sense when one human was making one change. But now imagine 100 agents working on 100 small tasks at the same time:

```
              one repo on github
                     │
   ┌──────────┬──────┴──────┬──────────┐
   ▼          ▼             ▼          ▼
 clone #1   clone #2      clone #3   …  clone #100
 branch #1  branch #2     branch #3  …  branch #100
   │          │             │          │
   edit       edit          edit       edit
   │          │             │          │
   PR #1     PR #2         PR #3   …   PR #100
                     │
                     ▼
            100 PRs, ~hundreds of merge conflicts,
            100 copies of node_modules on disk,
            and one human refereeing all of it
```

100 agents means 100 full copies of the repo. 100 `node_modules` folders. 100 build caches. 100 snapshots that all start drifting from the real repo the moment they're created. And every pair of agents that touched the same file is one more merge conflict you have to resolve by hand.

**With agent-lock there is one tree. No clones, no branches, no snapshots.** Every agent edits the same live repo. When one is editing a file, the others see "taken — pick another" and move on. When it's done, the file is immediately up to date for everyone. There is nothing to merge because nothing ever forked.

```
        one shared tree
  ┌─────────────────────────┐
  │   src/schema.ts   🔒 A  │ ← A is editing now
  │   src/api.ts            │ ← free
  │   src/utils.ts    🔒 C  │ ← C is editing now
  └─────────────────────────┘
        ▲       ▲       ▲
        │       │       │
     Agent A  Agent B  Agent C   …   Agent N
              (picks
               a free
               file)
```

### Why the old way breaks down

When two agents both edit `schema.ts` on their own branches, you end up with two diffs that each look fine on their own. You have to read both, figure out what each agent was trying to do, and stitch them together. A merge tool can't do this for you — it doesn't know the intent. And it happens every time two agents overlap.

You might think the filesystem already handles this — and it does, in a useless way. Both writes go through, just one after the other. But each agent decided what to write *before* the other one's changes existed. So the second write wipes out the first one's work. Nothing crashes. Nothing looks wrong. The edits are just gone.

This is the classic "lost update" problem. You can't catch it with tests or `git diff` — the file looks fine. The only sign is that something an agent claimed to do isn't there anymore.

agent-lock prevents this by changing *when* coordination happens. Instead of paying the cost at the end (by hand, on every conflict), you pay it at the start: one of the agents is told "this file is taken" before it writes a single byte.

## The fix in one picture

```
        same checkout
  ┌─────────────────────────┐
  │   src/schema.ts   🔒 A  │
  └────────▲───────▲────────┘
           │       │
       Write       Write
           │       │
       ┌───┴───┐ ┌─┴─────┐
       │Agent A│ │Agent B│ ── BLOCKED: locked by A
       └───────┘ └───────┘     (B retries, picks another file,
                                or waits — its choice)
```

A is editing → A holds the lock → B is told "no, not now" *before* it writes. No clobber, no merge, no surprise. The coordination cost is paid up-front, in microseconds, in exchange for never having to detect or repair a lost update later.

---

## Why locking beats the alternatives for AI agents

There are basically four ways to coordinate N agents on one codebase. Three of them are bad fits for agents. To see why, walk each one with a concrete example: agents A and B both want to edit `src/schema.ts`.

### Option 1 — branches + PRs (what humans do)

```
  main ─────●─────────────────────────●── merge
            │\                       /│
            │ ●── A edits schema.ts ─●
            │                        │
            │ ●── B edits schema.ts ─●
            │/                       \│
                  ↑                    ↑
            both touch the           you become
            same file                a merge referee
```

The standard human workflow: each agent gets a branch, does work in isolation, opens a PR, and somebody — usually you — referees the merge when three agents all touched the same file.

This works for humans because:
- Humans have judgment when reconciling two edits that both make sense in isolation.
- A merge conflict is *visible* — you see `<<<<<<< HEAD` markers and you know to act.
- The cadence is slow enough that one human can be the referee for many branches.

It fails for agents because:
- Agents can't do meaningful merges. They produce code that looks correct in isolation; reconciling two such pieces requires understanding what the other agent was *trying* to do, which it can't see.
- The coordination cost shows up at the *end*, after work is done. With agents the cost is paid by you, manually, every time.
- Conflict resolution often regenerates code, which costs tokens, time, and introduces drift from what was tested.
- It scales O(N²): every pair of agents that touched the same file is one more merge to referee.

### Option 2 — separate worktrees, no shared state

```
  agent-A worktree:  src/schema.ts (A's version)
  agent-B worktree:  src/schema.ts (B's version)
                       │
                       ▼
                ??? whose wins ???
```

A common reflex: give every agent its own worktree (`git worktree add`) so they can't see each other. This solves nothing; it just defers the conflict. When you eventually merge the worktrees back together, you have all the same problems as Option 1, plus you've duplicated `node_modules` and your build cache N times. For agent edits that overlap on the same file, separate worktrees make the problem invisible until the very end, which is the worst time to discover it.

### Option 3 — optimistic concurrency (what Git internally, and CRDTs, do)

```
  let everyone write    →    detect conflicts after    →    auto-merge
                                                              │
                                                              ▼
                                              works for {add line, edit unrelated line, delete}
                                              fails for "rewrote the function"
                                              fails for "renamed the symbol"
                                              fails for "moved between files"
```

Optimistic concurrency control (OCC) — assume conflicts are rare, let everyone write, detect-and-resolve at commit time — is the model behind Git's three-way merge, document collaboration tools (Google Docs, Figma's CRDTs), most distributed databases. It is the right answer when:

- Collisions are rare (writers usually touch different parts).
- Conflicts are small and structurally clean (line-level, character-level).
- The merge is computable from data alone.

Agent edits violate all three. Two agents asked to "improve error handling in `schema.ts`" will produce two complete rewrites that touch nearly every line, and the diff between them isn't a series of small clean conflicts — it's two parallel universes that need a human to choose one.

OCC's classic failure mode is the *write-skew anomaly*: each individual write looks fine, but their composition violates an invariant. Agents produce write-skews constantly. They are the worst possible client for OCC.

### Option 4 — pessimistic locking (agent-lock) ✅

```
                ┌────────────────────────┐
                │  .agent-lock/locks/       │
                │  ├── <hashA>.json  ←── A holds schema.ts
                │  └── (no entry)    ←── schema.ts free for everyone else
                └────────────────────────┘
                          ▲
                          │ check before write
                          │
                ┌─────────┴──────────┐
                │   Agent A   Agent B │
                └─────────────────────┘

  Coordination cost paid up-front, in microseconds, before any bytes are written.
  No merge, no conflict, no referee. The file is either yours for now, or it isn't.
```

Pessimistic locking — acquire-before-write — is the textbook opposite of OCC. It was the *first* concurrency-control strategy databases tried and was eventually displaced for human use cases by OCC. So why bring it back?

Because **the tradeoff between PCC and OCC is governed by exactly two variables**, and for AI agents both flip:

```
                       PCC wins              OCC wins
                       ────────              ────────
  Lock duration        short                 long
  Conflict cost        high                  low
  Merge feasibility    hard / manual         easy / automatic
  Retry cost           low                   high
```

**The asymmetry that makes locking work for agents:**

| | Humans | AI agents |
|---|---|---|
| How long they hold a file | minutes to hours | seconds |
| Can they resolve a merge conflict? | yes, painfully | not really |
| Cost of "you're blocked, try later" | high (interrupts flow) | nearly zero (just retries) |
| Cost of a silent overwrite | recoverable from git | invisible until tests fail |
| Edit shape | small, surgical | full rewrites of the touched region |
| Concurrency degree | small (1-3 devs / file / week) | high (10s of agents / file / minute) |

For humans, the column on the left makes PCC awful: long holds + cheap merges + expensive blocking + recoverable mistakes. For agents, the column on the right makes PCC good: short holds + impossible merges + cheap blocking + expensive mistakes.

This is also why pessimistic locking *succeeded* in the contexts where it stayed alive — Perforce for game development, `svn lock` for Photoshop files, the EDA / CAD world — those are exactly the contexts where collisions are merge-impossible (binaries, large rewrites). Agents are merge-impossible too. They are, in concurrency terms, the same shape of client as a binary asset.

---

## How agent-lock implements it

```
  PreToolUse hook  (Write | Edit | MultiEdit)
        │
        ▼
  ┌──────────────────────────────────────┐
  │  fs.openSync(                        │
  │    .agent-lock/locks/<sha1(path)>.json, │
  │    'wx'                              │  ← atomic O_EXCL create
  │  )                                   │
  └──────────────────────────────────────┘
        │
        ├─── success ──►  exit 0, edit proceeds
        │
        └─── EEXIST  ──►  read existing JSON
                          │
                          ├─ stale?     → unlink, retry
                          ├─ same agent? → exit 0 (idempotent)
                          └─ other      → exit 2, BLOCKED

  PostToolUse hook   →   unlink lockfile
  SessionEnd  hook   →   release this session's locks + sweep stale
```

The entire correctness of agent-lock rests on one syscall: `open(O_CREAT | O_EXCL)`. POSIX guarantees that, in the presence of concurrent callers, **at most one** will create the file and the rest will fail with `EEXIST`. This is the cheapest cross-process mutex available on a Unix filesystem and it is implemented atomically all the way down to the inode allocator. We do not need a kernel mutex, a lock daemon, a database, or a filesystem with advisory `flock`. We just need a directory we can `creat` into.

### Why a central lockdir, not per-file markers

The previous design (v1) wrote `// agent-lock lock: ...` comment lines into the top of each protected file. That worked but had four hidden costs:

1. **Read-then-edit races.** When the lock writes a marker to the file, the file's hash changes. If an agent had just read that file, its `Edit` tool's hash check against the old content fails, and the edit is rejected for the wrong reason.
2. **Per-extension comment styles.** Every supported language needed its own comment delimiter (`//`, `#`, `<!-- -->`, `/* */`, `--`). Languages without a comment syntax (JSON, binary) needed a sidecar fallback. The dual code path doubled the surface area.
3. **Shebang gymnastics.** A `#!/usr/bin/env node` line had to stay on line 1, which meant special-casing the marker insertion point. Files with shebangs but no trailing newline were a corner case.
4. **The lock altered the artifact.** `git diff` showed lock acquisition / release as code changes. `grep` for the lock string from inside the codebase was self-referential.

A central lockdir eliminates all four. The trade is that you can't `grep` an arbitrary file to see if it's locked — you have to look in `.agent-lock/locks/`. That's a fair price for never touching your source files again.

### The lockfile

```json
{
  "path":      "/abs/path/to/file.ts",
  "agentId":   "claude-code-sess-a1b2c3d4",
  "startedAt": "2026-04-27T15:00:00.000Z",
  "ttlMs":     600000,
  "reason":    "Claude Code Edit"
}
```

- `path` — absolute path of the locked file. Stored explicitly so `agent-lock list` doesn't have to reverse the hash.
- `agentId` — identifier of the holder. For Claude Code, `claude-code-sess-<first-8-chars-of-session-id>`. For your own scripts, anything you pass to `withLock`.
- `startedAt` — ISO 8601 timestamp; the basis for stale detection.
- `ttlMs` — how long this lock is allowed to be held before being considered stale (default 10 min).
- `reason` — free-form human-readable label. Surfaced in the BLOCKED message so the second agent knows what's happening.

The lockfile name is `sha1(absPath).slice(0, 16) + '.json'`. The hash collapses arbitrarily long paths into a fixed-length filename; the 16-hex-char prefix gives 64 bits of collision resistance, which is overkill for the working set of any one project (you'd need ~4 billion distinct paths before a 50% birthday-collision chance). Hashing also avoids the headache of escaping path separators inside a filename.

### Stale detection

A lock is **stale** (and may be overridden) if any of these hold:

- `startedAt` is missing.
- `startedAt` is not a parseable ISO date.
- `startedAt` parses to `<= 0` (epoch or before).
- `startedAt` is in the future (`> Date.now()`).
- `Date.now() - startedAt >= ttlMs`.

The future-timestamp check is not paranoia. The previous implementation used `age >= staleMs` directly; a future-dated `startedAt` produced a *negative* age, which is `< staleMs`, which counted as fresh. A malicious or buggy lockfile with `startedAt: "2099-01-01T00:00:00Z"` could pin a file forever. The fix is to treat any timestamp not in the half-open interval `(0, now]` as stale on principle.

### Lockfile lifecycle

```
        Lockfile lifecycle
        ──────────────────

  ┌──────────┐   acquire (O_EXCL)    ┌──────────┐
  │  absent  │  ───────────────────► │  held    │
  └──────────┘                       └──────────┘
       ▲                                   │
       │  release (unlink)                 │
       │  or stale-sweep                   │
       └───────────────────────────────────┘
```

There are exactly two states (held / absent) and two transitions (acquire / release). Stale-sweep is the same transition as release — it just runs from `SessionEnd` instead of `PostToolUse`. There is no "expiring" state, no "renewing," no "lease." The lockfile either exists or it doesn't, and we trust `mtime` and `startedAt` to tell us how recently it became real.

### Crash recovery

```
  agent crashes mid-edit
        │
        ▼
  lockfile remains on disk
        │
        ├── next edit attempt:  if (now - startedAt >= ttlMs) → unlink + retry
        │
        └── next SessionEnd:    sweep all stale lockfiles
```

A crashed agent leaves its lockfile behind. Without intervention, that file would block all writers forever. Two things prevent it:

1. **TTL-based override on the next acquire.** When agent B tries to take a lock that A is "holding," B reads the JSON, sees `startedAt` is older than `ttlMs`, unlinks the file, and retries the O_EXCL create. Net effect: a crashed lock holds for at most `ttlMs`.
2. **SessionEnd self-heal.** When any agent's session ends cleanly, its `SessionEnd` hook runs `purge.mjs`, which (a) releases every lock owned by that session regardless of age, and (b) sweeps any other stale locks it encounters.

Together these mean that the worst-case "stuck lock" duration is `ttlMs`, not `forever`, and in practice it's much less because most sessions exit cleanly.

### What happens on collision (the BLOCKED message)

When agent B tries to acquire a lock A holds:

```
$ # B's PreToolUse hook stderr (Claude shows this to the agent)

[agent-lock] BLOCKED: src/schema.ts is locked by agent
"claude-code-sess-a1b2c3d4" since 2026-04-27T00:41:12Z (4s ago,
reason: Claude Code Edit). Wait, edit a different file, or set
CLAUDE_FILE_LOCK=0 to override.
```

The hook exits with status 2, which Claude Code treats as "tool call rejected, here's the message." B sees the message and decides what to do — typically: pick a different file, or wait and retry. The retry is up to the agent and its system prompt, not agent-lock. We don't want to put a busy-wait loop in the hook because that would consume tool-call budget for nothing.

---

## Layout

```
  <project>/
  ├── .claude/
  │   ├── settings.json              ← three hooks wired in here
  │   └── skills/file-lock/SKILL.md  ← protocol doc Claude reads
  └── .agent-lock/
      ├── lock/file-lock.cjs         ← acquire / release / list / withLock
      ├── hooks/
      │   ├── file-lock-pre.mjs      ← PreToolUse  → acquire-or-block
      │   ├── file-lock-post.mjs     ← PostToolUse → release
      │   └── file-lock-purge.mjs    ← SessionEnd  → self-heal
      └── locks/                     ← THE STATE
          ├── 3a7f...json            ← one file per active lock
          └── b04c...json
```

`grep -l . .agent-lock/locks/` is the source of truth for "what is held right now." There is no other state. No database, no ephemeral memory, no rendezvous server. If the locks directory is empty, no locks are held, by anyone, anywhere.

---

## Install

```bash
npx agent-lock install          # in any project
agent-lock status               # verify
agent-lock test                 # smoke-test acquire / collide / release
```

`install` copies `lock/` and `hooks/` into `<project>/.agent-lock/`, drops the `file-lock` skill into `.claude/skills/`, and wires three hooks into `.claude/settings.json` (Pre/PostToolUse `Write|Edit|MultiEdit` + SessionEnd).

It is idempotent: running `install` twice produces the same result as running it once. It is also non-destructive: any pre-existing hook entries in your `settings.json` are preserved, agent-lock's entries are simply prepended to the relevant matcher groups.

---

## Commands

| Command | What it does |
|---|---|
| `install [<dir>]` | Copy files, create `.agent-lock/locks/`, wire `settings.json`. |
| `uninstall [<dir>]` | Strip agent-lock hooks, remove `.agent-lock/` and the skill. |
| `status [<dir>]` | Files installed? Hooks wired? Kill-switch? Active locks. |
| `list [<dir>]` | Every active lock with agent + age. |
| `release <file>` | Force-release one file's lock. |
| `release-mine <prefix> [<dir>]` | Release every lock held by `claude-code-sess-<prefix>`. |
| `purge-all [<dir>]` | Release every active lock. |
| `on` / `off [<dir>]` | Toggle the `CLAUDE_FILE_LOCK=0` kill switch. |
| `watch [<dir>] [--once]` | Live TUI of active locks, refreshed every 1 s. |
| `doctor [<dir>]` | Audit lockdir for stale, future-dated, invalid-date, or orphan entries. |
| `test [<dir>]` | Acquire / collide / release smoke test. |

### Watch — live view

```
  $ agent-lock watch
  agent-lock watch · /Users/me/proj · 2026-04-27 15:00:14 · 2 active lock(s)

    PATH                     AGENT                            AGE   REASON
    src/schema.ts            claude-code-sess-a1b2c3d4        4s    Claude Code Edit
    src/api/routes.ts        claude-code-sess-e5f6g7h8        12s   Claude Code Write

  Ctrl+C to exit.
```

### Doctor — health audit

`doctor` reports four classes of anomaly, each of which would have a separate root cause:

| Kind | Meaning | Likely cause |
|---|---|---|
| `[stale]` | TTL expired but lockfile still present | crashed session that hasn't been swept yet |
| `[future]` | `startedAt` is in the future | clock skew or hand-edited lockfile |
| `[invalid-date]` | `startedAt` is unparseable | corrupted write |
| `[orphan]` | locked file no longer exists | the file was deleted while held |

Doctor's only job is to surface these. None of them require manual repair — `purge-all` clears everything; the next legitimate acquire would have done it anyway. Doctor exists for diagnostic comfort, not necessity.

---

## Kill switch

```bash
agent-lock off    # set env.CLAUDE_FILE_LOCK=0 in settings.json
agent-lock on     # delete env.CLAUDE_FILE_LOCK
```

Setting `CLAUDE_FILE_LOCK=0` anywhere — settings, shell, single command — makes the hooks no-ops. Use it when:

- You're certain you're the only agent running and you don't want the overhead.
- You're debugging agent-lock itself.
- You want to do a "force write" that bypasses a lock you can't be bothered to release.

The kill switch is per-process, not per-project: if Claude Code inherits `CLAUDE_FILE_LOCK=0` from the shell, that overrides whatever's in `settings.json`.

---

## Limitations

Things agent-lock does **not** do, on purpose:

- **No cross-machine coordination.** The lockdir is local to one checkout. Two developers running agents on two laptops against the same git repo do not coordinate. (This is a design choice — distributed locking would require a server, which would defeat the simplicity.)
- **No line-range locks.** A lock is whole-file. Two agents that both want to edit the same file at the same time will serialize, even if they would have touched different functions.
- **No queueing.** A blocked agent gets a "no" and goes away. It is up to the agent and its prompt to decide whether to retry, work elsewhere, or report failure. agent-lock will not buffer write requests.
- **No durability beyond filesystem semantics.** If your filesystem doesn't honor `O_EXCL` correctly (e.g. some legacy NFS configurations), agent-lock's correctness degrades to whatever the filesystem provides. Local filesystems on macOS / Linux are fine.
- **No protection against malicious agents.** A motivated agent can simply `unlink` the lockfile. agent-lock is a coordination protocol for cooperating parties, not a security boundary.

---

## Design notes

- **No file mutation.** Lock state lives in `.agent-lock/locks/`, not in your source files. No comment markers, no shebang gymnastics, no `Read → Edit` hash-mismatch races.
- **One primitive.** `fs.openSync(lockfile, 'wx')` is the only synchronization mechanism. No nested critical section, no per-file mutex, no extra daemons.
- **Cross-platform.** No macOS Finder tags, no extension-specific comment styles. Identical behavior on macOS and Linux.
- **Whole-file scope.** Earlier versions had per-function ("semantic") locks; in practice the granularity wasn't worth the parser fragility. Whole-file is simpler, has no false positives, and matches the way agents actually behave (large rewrites that touch most of a file anyway).
- **Stateless tooling.** Every CLI command is a pure read or write of `.agent-lock/locks/`. No caching, no warm-up, no migrations between versions.

---

## Verify

```bash
npm test
```

45 tests cover: O_EXCL race (100 acquire rounds, exactly one winner each), future-timestamp bypass, invalid-date handling, stale TTL behavior, hook stdin contract, install / uninstall idempotency, doctor anomaly classes, watch rendering, kill-switch behavior, multi-extension uniformity (`.ts`, `.json`, no-extension), and `withLock` async semantics including throw-during-callback.

---

## Prior art

| System | Era | Granularity | Notes |
|---|---|---|---|
| RCS, SCCS | 1980s | per-file lock | the original `co -l` model; lockfiles in a sibling directory |
| Visual SourceSafe | 1995 – 2005 | per-file lock | Microsoft, bundled with Visual Studio; killed by Git |
| Perforce | 1995 – | per-file lock (default) | still alive in game-dev / monorepo shops; especially binary assets |
| `svn lock` | 2004 – | per-file lock (opt-in) | for binary files that can't merge (Photoshop, Illustrator, video) |
| `flock` (Unix) | 1983 – | per-fd advisory | kernel-level cousin; cooperating processes only |
| `git index.lock` | 2005 – | per-repo write lock | Git's own pessimistic lock for `.git/index` mutations |
| **agent-lock** | 2026 – | per-file lock | the same idea, applied to AI agents |

Visual SourceSafe was the closest historical analog to agent-lock: a Microsoft product, broadly used in the Windows world from the late 1990s through the mid 2000s, eventually displaced by Git for the same reason every per-file-lock VCS lost to Git — it was a bad fit for *humans*. agent-lock is, essentially, "what if VSS was right after all, but for agents?" The model that failed for one class of client succeeds for another precisely because the tradeoff variables — lock duration, merge feasibility, retry cost — flip when the client is an AI rather than a person.

---

## FAQ

**Q. Why not use `flock(2)` / `fcntl(2)` / OS-level advisory locks?**

A. Because `flock` is per-file-descriptor, not per-path: when our hook process exits, the lock disappears, even though the *agent's* edit may not be done. We need the lock to outlive the hook process and survive across multiple tool calls. A persistent file-as-marker is the natural fit.

**Q. Why one lockfile per protected file, not one big locks.json?**

A. Because we want acquire to be a single atomic syscall (`O_EXCL`), and that's only true for "create a new file." Editing one big `locks.json` would require its own coordination layer (read, modify, write under a critical section) — which we'd then have to implement with... a lockfile. So we just go straight to the lockfile.

**Q. What happens if my filesystem is on NFS / SMB / a network mount?**

A. Modern NFSv4 honors `O_EXCL` correctly. NFSv3 had famous edge cases here; if you're on it, agent-lock's correctness degrades to "best-effort." For the agent use case (one developer's laptop, one local SSD) this never comes up.

**Q. Can I have two agents edit different functions in the same file at once?**

A. Not in v2. v1 supported "semantic locks" (per top-level declaration) but the parser was fragile and the wins were small in practice — agents tend to do whole-file rewrites. If you genuinely need this, the lockfile JSON has room for `startLine` / `endLine` fields; it's a future feature gated on real demand.

**Q. What if I want to give an agent permission to override a lock?**

A. `CLAUDE_FILE_LOCK=0` in that agent's environment, or a `agent-lock release <file>` before the override. There is no "force" flag on acquire by design — if you want to write past a lock, the right move is to first release it.

**Q. What's the overhead per tool call?**

A. One filesystem syscall (the O_EXCL `open`) plus a JSON write of ~200 bytes. Sub-millisecond on any modern SSD. Negligible compared to the time Claude takes to actually run a Write/Edit.

**Q. Does agent-lock interact with `.gitignore` / git in any way?**

A. Yes — add `.agent-lock/locks/` to `.gitignore`. The lockdir is per-checkout, ephemeral, and should never be committed. The rest of `.agent-lock/` (the lock module, the hooks) is committable; checking it in lets a fresh clone start with agent-lock already wired without an extra `install` step.

**Q. Why JSON for the lockfile and not a binary format?**

A. Because the lockfile is read by humans (`cat`), by `agent-lock doctor`, and by emergency `release` operations. JSON is the cheapest format that survives all three. The size cost is irrelevant — we're talking about a few hundred bytes per active lock, and the entire lockdir is gone within seconds of edit completion.
