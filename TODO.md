# agent-lock roadmap

Tracks features that haven't shipped yet. Each item is sized small enough to
land in one PR with full tests.

---

## Operational

### `agent-lock release-mine` improvement — read `CLAUDE_SESSION_ID` from env
Currently requires the session prefix as an argument. If Claude Code exposes
the active session_id via env, default to it.

```
agent-lock release-mine            # uses $CLAUDE_SESSION_ID
agent-lock release-mine <prefix>   # explicit
```

---

## Sub-agent ID propagation

Sub-agents spawned via the `Agent` tool inherit the parent's `session_id`,
so they all collapse to the same `agentId`. A parent that locked a file and
then spawns a sub-agent which also tries to write that file currently
self-passes the lock (idempotent same-agent acquire) — so the parent's lock
gets released early when the sub-agent's PostToolUse fires.

First step: add a debug-capture hook to record real sub-agent stdin payloads
and confirm whether Claude Code exposes any per-agent identifier. Then
suffix `agentId` accordingly (e.g. `claude-code-sess-XXXX#<suffix>`).

---

## Already shipped

- ✅ Whole-file central-lockdir redesign — no source-file mutation
- ✅ Future-timestamp stale bypass fixed (negative ages cannot pin a lock)
- ✅ Direct `node` hook invocation — no `sh -c`
- ✅ `agent-lock watch` — live TUI of active locks
- ✅ `agent-lock doctor` — audits lockdir for stale / future / orphan / invalid-date entries
- ✅ Self-heal on SessionEnd (own-session release + stale sweep)
- ✅ Globally wired in `~/.claude/settings.json` — every project is protected via absolute-path hooks
