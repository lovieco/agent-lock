# livehub roadmap

Tracks features that haven't shipped yet. Each item is sized small enough to
land in one PR with full tests and 100% coverage. Sequence them however —
none are dependent on each other except where noted.

---

## Operational

### `livehub doctor` — one-shot health audit
Scan the project tree once and print a report:
- Markers held by sessions that no longer exist (heuristic: agentId matches
  a `claude-code-sess-XXXXXXXX` pattern but the session isn't in the live
  Claude session list — probably need a stat file or a heuristic based on
  age).
- Markers older than `staleMs` (10 min default).
- Marker lines that don't parse with the current `MARKER_LINE_RE`.
- Duplicate markers on the same `(agentId, nodeId)` for one file.

Exits 0 if clean, 1 if any anomaly found. Useful for a `pre-commit` or CI hook.

Files: `bin/livehub` (add `cmdDoctor`), `test/cli.test.cjs` (4 cases).

### `livehub release-mine` — release every marker for the current session
The user's terminal session has its own `session_id`. After a crash, this
command releases everything that session held without waiting for `staleMs`.
Symmetric to the SessionEnd self-heal hook, but explicit and synchronous.

```
livehub release-mine [<dir>]                # uses CLAUDE_SESSION_ID env
livehub release-mine [<dir>] --session=XXXX # explicit session prefix
```

Files: `bin/livehub` (add `cmdReleaseMine`), `test/cli.test.cjs` (3 cases).

---

## Multi-language semantic coverage

The framework in `lock/semantic.cjs` is ready — each language is a
~10-line PR. Pick whichever languages you actually use.

### Python (.py)
```
def name(args):       → fn:name
class Name(Base):     → cls:Name
NAME = ...            → var:NAME    (top-level, all-caps convention)
```
Indent-aware block end: a top-level `def` ends at the first non-empty line
whose indent is ≤ the def's indent, OR at EOF.

### Go (.go)
```
func Name(...) {...}                 → fn:Name
func (r *R) Method(...) {...}        → cls:R.fn:Method
type Name struct { ... }             → type:Name
type Name interface { ... }          → iface:Name
```
Brace counting works as-is (Go uses `{}`). Methods on a receiver get
`cls:R.fn:Method` for free if we extend the method-descent code.

### Rust (.rs)
```
fn name(...)           → fn:name
struct Name { ... }    → cls:Name      (struct is the closest analog)
impl Name { fn m() }   → cls:Name.fn:m
trait Name { ... }     → iface:Name
mod name { ... }       → ns:name       (new kind, optional)
```

### CSS / SCSS (.css, .scss)
```
.selector { ... }      → sel:selector
@media ...{ ... }      → at:media-<hash>
```
Top-level only, brace-counted. Nested selectors stay under their parent.

### Shell (.sh, .bash)
```
name() { ... }         → fn:name
NAME=value             → var:NAME      (top-level, all-caps)
```
Curly-brace counted, `()` after name signals function.

---

## Marker format / storage

### Batch B #7: grouped marker comment block
Currently N concurrent locks produce N comment lines at the top of the file.
Replace with one block:

```
/* livehub-locks:
 *   A=sess-aaa N=fn:foo @2026-... R=Edit
 *   A=sess-bbb N=fn:bar @2026-... R=Edit
 */
```

**Risk**: format change. Migration plan needed. Keep `MARKER_LINE_RE`
parsing the line-per-marker form during the transition window, then drop
once all repos have rolled forward.

Files: `lock/file-lock.cjs` (split read/write paths), `test/file-lock.test.cjs`
(format-migration cases), bump version + CHANGELOG.

---

## Swarm / sub-agent support

### Sub-agent ID propagation
Currently every Claude session derives `agentId` from `session_id`. Sub-agents
spawned via the `Agent` tool inherit the parent's `session_id`, so they all
collapse to the same `agentId` — sub-agents editing different nodes of the
same file are not distinguishable in markers and a parent waiting for child
results may collide with itself.

Possible approaches:
1. Hook reads the agent-tool-call ID (if Claude Code exposes it on stdin) and
   appends it to `agentId`: `claude-code-sess-XXXXXXXX#agent-Y`.
2. Hook reads the agent's `name` parameter and uses that suffix.
3. Use a per-process PID suffix as the cheapest signal.

Needs research into what Claude Code's hook stdin payload actually contains
for sub-agent calls. Add a debug-print hook first to capture a real payload.

---

## Already shipped (for reference)

- ✅ Race-fix on self-reacquire (10-min refresh threshold)
- ✅ Self-heal on SessionEnd (own-session marker release)
- ✅ Method-level scope (`cls:Foo.fn:bar`)
- ✅ JSX component detection (`comp:Name` in .jsx/.tsx)
- ✅ Interface and type aliases (`iface:`, `type:`)
- ✅ Markdown support (frontmatter + headings)
- ✅ Shorter marker format (`A=`, `N=`, `@`, `R=`)
- ✅ `livehub watch` — live TUI of active locks
