# agent-lock — operating rules

When the `agent-lock` plugin is enabled, follow these rules. They cover the
discretionary gaps the hooks themselves cannot enforce.

## 1. Wait on BLOCKED — don't disable the lock

When a tool call fails with `[agent-lock] BLOCKED: <path> is locked by agent X`,
the default response is to **wait** or **work on a different file**. Do NOT
`export CLAUDE_FILE_LOCK=0` to power through — that disables the entire
coordination mechanism and is the exact failure mode the plugin exists to
prevent.

The kill switch (`CLAUDE_FILE_LOCK=0`) is a deliberate escape hatch for
operator use during incident recovery, not a workaround for normal contention.

## 2. Never `rm` a `.lock` sidecar manually

Use `agent-lock release <path>` (the CLI) or `agent-lock purge-all` for bulk
cleanup. Both run PID-liveness and TTL checks before releasing. A raw `rm` of
`<file>.lock` skips both checks and can clobber an in-flight write from a peer
agent that's still alive.

## 3. Force-release of a live (non-stale) lock needs user confirmation

Stale locks (older than the TTL, default 10 min) auto-clear and don't need a
prompt. A non-stale lock belongs to a live peer agent — releasing it may
destroy work-in-progress. **Always ask the user before** running
`agent-lock release --force` (or any flag that overrides ownership) against a
fresh lock. Tooling-level enforcement is planned; until then this rule applies.

## 4. Don't pre-acquire locks for read-only access

The PreToolUse hook only fires on `Write`, `Edit`, and `MultiEdit` by design.
Do not wrap pure reads (`Read`, `Grep`, `Glob`, `cat`, log inspection, etc.) in
`withLock()` "just in case" — it starves other agents and lengthens contention
chains for no protection benefit. Reads do not conflict with concurrent reads
or writes at the application level; conflicts only matter at write time.
