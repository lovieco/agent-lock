---
name: agent-lock
description: How to work in a git checkout that other agents share on the same branch, with the agent-lock plugin active. Use whenever a hook message starts with "[agent-lock]" (BLOCKED, DEADLOCK, TAKEOVER, DENIED, a Note or Warning, "<id> is blocked on <file> (yours)", the end-of-turn "You have uncommitted changes" reminder) or a git or package-manager command fails with a line starting "agent-lock:", and whenever you commit, push, pull, stash, reset, switch branches or run a formatter in such a checkout. There `agent-lock commit` replaces git add + git commit, `agent-lock push` publishes and `agent-lock sync` replaces git pull.
---

# agent-lock: working in a shared checkout

Other agents (Claude Code sessions and their subagents) may be editing this
checkout, on this branch, right now. agent-lock keeps their work and yours
apart:

- Hooks record who edits which file.
- Shims for `git` (and `npm`, `pnpm`, `yarn`, `bun`, `cargo`, `uv`, `poetry`,
  `go`, `bundle` when installed) come first on your PATH. They refuse commands
  that would touch other agents' uncommitted work.
- All state is in one file, `.git/agent-lock.json`.

When you are the only agent, nothing is blocked.

## Claims

- Your first Write, Edit, MultiEdit or NotebookEdit of a file claims it. You
  hold the file until you commit it. Reading never claims a file.
- Your session context gives your id ("Your agent id is …"): `a1b2c3d4` for a
  session's main agent, `a1b2c3d4:9f3e01aa` for a subagent.
- Another agent's claim blocks your edits for as long as its process runs.
  Its uncommitted changes stay its own.
- **Shared files** are the exception: config files and manifests
  (`package.json` and its lockfiles, `Cargo.toml`, `tsconfig*.json`,
  `*.config.*`, `.gitignore`, `CLAUDE.md`, … plus the repo's
  `.agent-lock-shared`) take several holders. Your edit joins the claim and
  you get `Note: <file> is a shared file, also being edited by <ids>`. One
  edit runs at a time: while another agent's edit of it is in flight you are
  BLOCKED for a moment, so retry; if Edit says the file changed, re-read it.
  Your commit of a shared file includes the other holders' changes in it
  (`(shared; with changes by <ids>)`): check `git diff -- <file>` first.
- A claim whose owner process has exited can be taken over (see TAKEOVER). So
  can a claim that was never written, such as an edit rejected at a prompt,
  once it has been idle for 45 min.
- When a subagent finishes, its claims pass to its parent.
- A file has one claim under git's spelling of its path, so case and Unicode
  (NFC/NFD) variants of a name on macOS count as the same file.
- Claims exist only inside git worktrees. A file you change only through Bash
  (codegen, scaffolding) has no claim. Name it when you commit (`-- <path>`).
- `agent-lock status` shows every claim, session and lock. `agent-lock mine`
  lists your own files.

## Messages and what to do

Hook messages start with `[agent-lock]`. Their commands are ready to copy:
they use absolute paths, `--as <id>` for a subagent, and `-C <repo>` when your
shell is in another directory.

**BLOCKED**: `[agent-lock] BLOCKED: src/a.ts is claimed by b1c2d3e4 (claimed 5m ago, last active 2m ago, holder process alive, 3 uncommitted edits).`

- The file belongs to the holder until it commits. Do other work.
- To wait, run the message's `agent-lock wait --claim <path> --timeout 540`
  with run_in_background. Exit 0 means the file is now yours, 1 means it timed
  out, 3 means a deadlock.
- Never write the file another way: no `>`, `tee`, `sed -i`, `cp`, `mv`,
  scripts, or formatters over its directory.
- If the holder looks abandoned ("holder process gone", or idle for hours),
  tell the user. `agent-lock release --force <path>` takes the holder's work
  and needs the user's approval. "its edit never ran" means the holder wrote
  nothing, so a forced release is safe, but you still need the user's approval.
- `BLOCKED: … is being edited by your subagent <id>, which is still running`:
  wait for its completion notice. Its files then pass to you.
- ``BLOCKED: `agent-lock sync` (run by <id>) is updating <file> from upstream right now`` and
  `BLOCKED: <file> is claimed by <id>, and agent-lock's state file stayed locked for seconds …`:
  retry in a moment.

**DEADLOCK**: `[agent-lock] DEADLOCK: a.ts is claimed by b1c2d3e4, and b1c2d3e4 is waiting for b.ts (yours).`
Commit the files you hold now, using the command in the message, so that both
of you can continue.

**TAKEOVER**: `[agent-lock] TAKEOVER: a.ts was claimed by b1c2d3e4 (its process has exited), which left 2 uncommitted edits in it.`
The claim is now yours, and agent-lock refused this one edit. Your commit of
the file will include those edits, so read
`git -C <repo> diff -- <file>` first. Then either retry the edit to go ahead,
or run `agent-lock release <path>` to leave the file alone. When no other
agent is active, you get a `Note:` instead and the edit goes ahead.

**Notes about changes that are not yours.** The edit goes ahead. Before you
commit the file as your own, read `git -C <repo> diff -- <file>`. If those
changes are not yours to keep, tell the user.

- `Note: <file> already has uncommitted changes that were not made through your edits`:
  made by another agent's Bash command, by the user, or in an earlier session.
- `Note: <file> may still hold uncommitted changes by <id>, whose claim on it ended without a commit.`
- `Note: <file> carries uncommitted changes by <id>, your parent agent; the claim moved to you with them.`
  This one is for subagents. Leave committing that file to your parent: your
  `agent-lock commit` leaves it out unless you name it.

**Waiter notice**: `[agent-lock] b1c2d3e4 is blocked on a.ts (yours).`
When your change to that file is consistent, commit it right away with the
command in the message (`agent-lock commit -m "<msg>" -- <path>`).

**End-of-turn reminder**: `[agent-lock] You have uncommitted changes in files you claimed: a.ts, b.ts.`
It may add `<id> is waiting for <file>.` Unless the user asked you not to
commit yet:

- If the change is complete and consistent, run `agent-lock commit -m "<msg>"`.
  When it lists several repos, run one commit per repo with `-C <repo>`. Add
  `--push` only if the user wants the work published.
- If the user wants to review first, or the work is mid-change, say so in one
  line and stop.

**Warnings**: `Warning: your write to <file> landed while it is claimed by <id>` or
`Warning: your Bash command changed files other agents have claimed: …`.
Do not change those files again. Tell the user so they can sort out whose
change it is.

**DENIED**: `[agent-lock] DENIED: this would run agent-lock as a1b2c3d4, but you are a1b2c3d4:9f3e01aa.`
Run the corrected command printed under it.

**Approval prompts**: `CLAUDE_FILE_LOCK=0`, `agent-lock off`,
`agent-lock release --force`, and writes to `.git/agent-lock.json` or
`.git/index.lock` ask the user first. Never use them to get past contention,
and never rewrite a command to avoid the prompt.

**Shim refusals**: a git or package-manager command exits 1 with one stderr
line that starts with `agent-lock:`. For example:

```
agent-lock: `git stash` acts on the whole shared checkout while other agents have uncommitted work (a.txt by b1c2d3e4). Commit only your own files: agent-lock commit -m "<msg>" [-- <paths>]; to undo your own change, name the file: git checkout -- <file>.
```

Use the alternative that the line names. Never route around a refusal by
running the real git or npm by its full path. Package installs run one at a
time (``agent-lock: waiting for `npm install` (<id>, pid <n>) to finish before …``). A
dependency change such as `npm install zod` claims the manifest and lockfile
it rewrites, and is refused while another agent is editing one of them.

## Committing

`agent-lock commit` replaces `git add` + `git commit`. Do not `git add`
first. It commits exactly the claimed files that you changed, and leaves out
everyone else's files:

```bash
agent-lock commit -m "$(cat <<'EOF'
Add slug helper for post URLs

Replaces the ad-hoc regex in three call sites.
EOF
)"
```

```
committed 77567c6670df: 1 file
  src/slug.ts
other agents have uncommitted changes in 1 file (not included)
```

- `-- <paths>` commits only those paths. An unclaimed path is claimed for you
  first; this is how you commit files you wrote through Bash. A path held by
  one of your finished subagents is taken over. A path held by any other agent
  is an error, and nothing is committed.
- `-C <repo>` commits in another repo, one commit per repo. Options can go
  anywhere before `--`.
- `--dry-run` lists what would be committed. Each `-m` becomes a paragraph;
  `-F <file>` or `-F -` reads the message from a file or stdin. `--push`
  publishes right after the commit.
- A changed manifest brings its changed lockfile along. `package.json` brings
  `package-lock.json`, `pnpm-lock.yaml`, `yarn.lock` or `bun.lock`;
  `Cargo.toml` brings `Cargo.lock`; `pyproject.toml` brings `uv.lock` or
  `poetry.lock`; `go.mod` brings `go.sum`; `Gemfile` brings `Gemfile.lock`.
- The repo's pre-commit and commit-msg hooks run on a private index that holds
  only your files. A refusal prints `commit failed (<code>): …`. Nothing is
  committed and you keep your claims.
  - `hook-failed`: fix what the hook reports, then commit again.
  - `dangerous-hooks`: while other agents have uncommitted changes, the hooks
    would rewrite the shared worktree (`git stash`, lint-staged without
    `--no-stash`, the pre-commit.com framework, `checkout -- .`,
    `reset --hard`). Run the checks yourself on your own files, then commit
    with `--no-verify`.
  - `hook-staged`: a hook staged files you do not own. Fix the hook, or commit
    with `--no-verify`.
  - `hook-clobbered`: a hook stashed or changed other agents' files. Tell the
    user which files (and which stash) the message names. When a hook
    stashed, ask the user to run `git stash pop`; never run it yourself. Do
    not retry the commit: run the checks yourself and commit with
    `--no-verify`, or ask the user.
  - `case-rename` (a case-only `git mv` on a case-insensitive disk) or
    `staged-untrack` (`git rm --cached` of a file still on disk): ask the user.
  - `busy`: another commit or sync is running. Retry shortly.

## Push and sync

- `agent-lock push` publishes HEAD. If the remote has moved, it syncs and
  retries. Push only when the user wants the work published. agent-lock does
  not build or test anything, so run the tests first: your commit is HEAD plus
  your files, a tree that nobody has built yet.
- `agent-lock sync` brings in upstream commits. It never touches anyone's
  uncommitted files. Use it instead of `git pull`.
  - `blocked`: upstream changed files that someone here holds. Follow the
    advice given per file: commit your own file and sync again, wait for a
    subagent or another agent, or release a claim you no longer need. A
    local untracked file, a local deletion or a local change in the way
    blocks too; the message names the files.
  - `conflict`: nothing was changed. Report the files to the user.
  - `rewritten`: upstream history was rewritten. Ask the user before you run
    `agent-lock sync --accept-rewrite`.
  - `note: recovered from an interrupted agent-lock sync`: an earlier sync was
    killed and has been cleaned up. Mention it to the user.

## Subagents

A subagent shares its parent's shell environment, so by default agent-lock
treats it as the parent. Your session context gives your own id, such as
`a1b2c3d4:9f3e01aa`.

- Pass `--as <id>` to every agent-lock command, for example
  `agent-lock commit --as a1b2c3d4:9f3e01aa -m "<msg>"`. The Bash hook denies
  `commit`, `push`, `sync`, `release`, `wait` and `mine` without it.
- Put your id in front of raw git and package-manager commands:
  `AGENT_LOCK_AGENT_ID=a1b2c3d4:9f3e01aa npm install zod`.
- While any subagent of a session may be running, the git shim counts the main
  agent's files as another agent's. Whole-tree git is then refused for the main
  agent too.

## Formatters and fixers

Run them only on your own files:

```bash
agent-lock mine -z | xargs -0 -r prettier --write
```

`mine` prints the absolute paths of the files you hold. `-r` stops xargs from
running the tool on the whole tree when you hold no files. Nothing blocks a
repo-wide run (`prettier --write .`, `eslint --fix .`, `black .`,
`cargo fmt`); agent-lock only notices after the fact that it changed other
agents' files.

## Never

- Write a file that another agent holds, by any means.
- Run `CLAUDE_FILE_LOCK=0`, `agent-lock off` or `agent-lock release --force`
  unless the user asked for it.
- Edit, create or delete `.git/agent-lock.json`. Use `agent-lock release <path>`
  to drop your own claim.
- Delete `.git/index.lock`. An index.lock error means another git process is
  running: wait a few seconds, then retry. The next agent-lock commit, sync or
  push cleans up a lock left by an interrupted sync.
- Run whole-tree git while other agents work: `git add -A`, `git add .`,
  `git add -u`, bare `git commit`, `git commit -a`, `git commit --amend`,
  `git stash` and its pop, apply and drop, `git reset --hard`, `git reset <rev>`,
  `git checkout .`, `git restore .`, `git checkout <branch>`, `git switch`,
  `git clean -f`, `git pull`, `git rebase`, `git merge`, `git cherry-pick`,
  `git revert`, `git am`. The shim refuses them. It allows the recovery forms:
  `merge --abort`, `rebase --continue` and `--quit`, `cherry-pick` and
  `revert` with `--continue`, `--abort` or `--quit`, and `am --abort`.
  `rebase --abort` and `--skip` would discard others' work, so ask the user.

Scoped git is fine: `git status`, `git diff`, `git log`, `git show`,
`git add <your files>`, `git checkout -- <your file>`,
`git restore <your file>`, `git reset -- <your paths>`.
