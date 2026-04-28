// All user-visible copy lives here. The renderer in main.js reads from this
// object and never embeds strings of its own. Add new copy here, not in JSX.

export const data = {
  meta: {
    title: 'agent-lock — live multi-agent coding without the merge',
    description:
      'A Claude Code skill that lets multiple agents share one tree, ' +
      'locking at the function level instead of the file level. No branches, ' +
      'no PRs, no merge conflicts.',
  },

  nav: {
    brand: 'agent-lock',
    brandTag: 'v0.1',
    links: [
      { href: '#why',          label: 'Why' },
      { href: '#how',          label: 'How' },
      { href: '#architecture', label: 'Architecture' },
      { href: '#design',       label: 'Design' },
      { href: '#schema',       label: 'Schema' },
      { href: '#install',      label: 'Install' },
      { href: 'https://github.com/lovieco/agent-lock', label: 'GitHub', external: true },
    ],
  },

  hero: {
    eyebrow: 'live agentic development',
    title: ['Skip the merge.', 'Just edit.'],
    body:
      'Google Docs, Figma, Notion — everywhere else we edit together, live. ' +
      'For code we still use branches, PRs, and merge conflicts. ' +
      'With multiple agents in one repo, we can finally do better. ' +
      'agent-lock lets every agent edit the same tree at the same time — ' +
      'no branches, no PRs, no merge.',
    install: {
      label: 'one-liner install',
      command:
        'curl -fsSL https://raw.githubusercontent.com/lovieco/agent-lock/main/install.sh | bash',
      copyLabel: 'Copy',
      copiedLabel: 'Copied',
    },
    stats: [
      { value: '1',    label: 'syscall per acquire' },
      { value: '0',    label: 'runtime deps' },
      { value: '~700', label: 'lines of code' },
      { value: '45',   label: 'tests' },
    ],
  },

  pitch: {
    id: 'why',
    eyebrow: 'the idea',
    title: 'Code is the last thing we still merge',
    body:
      'In Docs, Figma, Notion, two people typing in the same file is normal. ' +
      'Code is the holdout — every change still goes through a branch, a PR, ' +
      'and a merge. With multi-agent coding, that loop is the bottleneck. ' +
      'agent-lock removes it: agents share one tree and take turns on a file, ' +
      'one at a time, for a few seconds each. No branches. No PRs. No merge.',
    flows: [
      {
        kind: 'branch',
        label: 'branch / PR workflow',
        steps: ['branch', 'edit', 'PR', 'conflict', 'human merge', 'retry'],
        verdict: 'merge cost is paid by you, after the work is done',
      },
      {
        kind: 'agent-lock',
        label: 'agent-lock live edit',
        steps: ['acquire', 'edit', 'release'],
        verdict: 'collision is detected before the first byte is written',
      },
    ],
    blocked: {
      label: 'what a colliding agent sees',
      lines: [
        '[agent-lock] BLOCKED: src/schema.ts is locked by agent',
        '"claude-code-sess-a1b2c3d4" since 2026-04-27T00:41:12Z (4s ago,',
        'reason: Claude Code Edit). Wait, edit a different file, or set',
        'CLAUDE_FILE_LOCK=0 to override.',
      ],
    },
    table: {
      head: ['', 'branch / PR', 'agent-lock'],
      rows: [
        ['when coordination happens', 'at merge (after work)', 'at acquire (before work)'],
        ['who pays the cost',         'you, manually',         'one O_EXCL syscall'],
        ['failure mode',              'silent lost-update',    'loud BLOCKED, agent retries'],
        ['state lives in',            'PRs, git log, your head', '.agent-lock/locks/*.json'],
        ['scales with N agents',      'merge pain ~ O(N²)',    'one lockdir, no extra cost'],
      ],
    },
  },

  marker: {
    id: 'how',
    eyebrow: 'how it works',
    title: 'One JSON file per active edit',
    body:
      'When an agent picks up a file, agent-lock atomically creates a tiny ' +
      'JSON lockfile under `.agent-lock/locks/`. Another agent that wants the ' +
      'same file sees the lockfile already exists and is told “taken” before ' +
      'it writes anything. Other files in the same tree stay free.',
    file: {
      name: '.agent-lock/locks/',
      lines: [
        { kind: 'code', text: '$ ls .agent-lock/locks/' },
        { kind: 'code', text: '3a7f9b2c1d4e5f60.json    b04c8e2a91f3d567.json', tag: 'A' },
        { kind: 'code', text: '' },
        { kind: 'code', text: '$ cat .agent-lock/locks/3a7f9b2c1d4e5f60.json' },
        { kind: 'code', text: '{', tag: 'A' },
        { kind: 'code', text: '  "path":      "/repo/src/schema.ts",', tag: 'A' },
        { kind: 'code', text: '  "agentId":   "claude-code-sess-a1b2c3d4",', tag: 'A' },
        { kind: 'code', text: '  "startedAt": "2026-04-28T08:41:12Z",', tag: 'A' },
        { kind: 'code', text: '  "ttlMs":     600000,', tag: 'A' },
        { kind: 'code', text: '  "reason":    "Claude Code Edit"', tag: 'A' },
        { kind: 'code', text: '}', tag: 'A' },
        { kind: 'code', text: '' },
        { kind: 'code', text: '$ cat .agent-lock/locks/b04c8e2a91f3d567.json' },
        { kind: 'code', text: '{ "path": "/repo/src/api/routes.ts", "agentId": "claude-code-sess-e5f6g7h8", ... }', tag: 'B' },
        { kind: 'code', text: '' },
        { kind: 'code', text: '# everything else in /repo is free for any other agent', tag: 'free' },
      ],
    },
    legend: [
      { swatch: 'A',    label: 'session A holds src/schema.ts' },
      { swatch: 'B',    label: 'session B holds src/api/routes.ts' },
      { swatch: 'free', label: 'every other file is free — any third agent can take it' },
    ],
  },

  architecture: {
    id: 'architecture',
    eyebrow: 'architecture',
    title: 'Four layers, one tree',
    body:
      'Agents make tool calls. Hooks intercept them. Rules decide. State ' +
      'lives inside the source files themselves — `grep "agent-lock lock:"` ' +
      'is the source of truth.',
    layers: [
      {
        name: 'Agents',
        path: 'Claude Code sessions',
        role: 'Issue Write / Edit / MultiEdit tool calls. Identified by session id.',
      },
      {
        name: 'Hooks',
        path: '.agent-lock/hooks/*.mjs',
        role: 'PreToolUse acquires or blocks. PostToolUse releases. SessionEnd purges stale.',
      },
      {
        name: 'Rules',
        path: '.agent-lock/lock/file-lock.cjs',
        role: 'acquire / release / withLock. Decides collisions and staleness. Pure Node, zero deps.',
      },
      {
        name: 'State',
        path: '.agent-lock/locks/*.json',
        role: 'One JSON file per active lock. `ls` is the source of truth. No database.',
      },
    ],
    sidecar: {
      name: 'Skill (sidecar)',
      path: '.claude/skills/file-lock/SKILL.md',
      role: 'Protocol doc Claude reads on session start. Teaches the agent to recover gracefully from BLOCKED.',
    },
  },

  design: {
    id: 'design',
    eyebrow: 'design',
    title: 'One syscall is the whole protocol',
    body:
      'agent-lock’s correctness rests on `open(O_CREAT | O_EXCL)`. POSIX ' +
      'guarantees that, under concurrent callers, at most one creates the ' +
      'lockfile and the rest fail with EEXIST. No daemon, no database, no ' +
      'kernel mutex — just a directory we can creat into.',
    principles: [
      {
        name: 'Central lockdir, not per-file markers',
        body:
          'v1 wrote `// agent-lock lock: …` into the top of each protected file. ' +
          'That changed the file’s hash mid-edit, broke Edit-tool hash checks, ' +
          'forced per-language comment styles, and required shebang gymnastics. ' +
          'A central `.agent-lock/locks/` directory eliminates all four.',
      },
      {
        name: 'Whole-file scope',
        body:
          'Per-declaration (“semantic”) locks looked elegant but the parser ' +
          'was fragile and agents mostly do whole-file rewrites anyway. ' +
          'Whole-file is simpler, has no false positives, and matches how ' +
          'agents actually behave.',
      },
      {
        name: 'No file mutation',
        body:
          'Lock state never lives inside your source. `git diff` shows your ' +
          'work, not lock acquisition. `grep` from inside the codebase is no ' +
          'longer self-referential.',
      },
      {
        name: 'Two states, two transitions',
        body:
          'The lockfile is either present (held) or absent (free). Acquire ' +
          'creates it; release unlinks it. Stale-sweep is just release fired ' +
          'from `SessionEnd`. There is no “expiring” state, no lease, no renew.',
      },
      {
        name: 'Stateless tooling',
        body:
          'Every CLI command is a pure read or write of `.agent-lock/locks/`. ' +
          'No caching, no warm-up, no migrations. `ls .agent-lock/locks/` is ' +
          'the source of truth for what is held right now.',
      },
      {
        name: 'Cheap retry, expensive overwrite',
        body:
          'Agents pay near-zero cost when told “no, retry later.” They pay an ' +
          'invisible, debugging-hours cost when a lost update survives into ' +
          'tests. Pessimistic locking trades the cheap cost for the expensive one.',
      },
    ],
    lifecycle: {
      label: 'lockfile lifecycle',
      lines: [
        '  ┌──────────┐   acquire (O_EXCL)    ┌──────────┐',
        '  │  absent  │  ───────────────────► │   held   │',
        '  └──────────┘                       └──────────┘',
        '       ▲                                   │',
        '       │  release (unlink)                 │',
        '       │  or stale-sweep on SessionEnd     │',
        '       └───────────────────────────────────┘',
      ],
    },
  },

  schema: {
    id: 'schema',
    eyebrow: 'schema',
    title: 'The lockfile, in 200 bytes',
    body:
      'Every active lock is one JSON file under `.agent-lock/locks/`, named ' +
      '`sha1(absPath).slice(0,16) + ".json"`. Five fields, no envelope.',
    file: {
      name: '.agent-lock/locks/3a7f9b2c1d4e5f60.json',
      lines: [
        '{',
        '  "path":      "/abs/path/to/src/schema.ts",',
        '  "agentId":   "claude-code-sess-a1b2c3d4",',
        '  "startedAt": "2026-04-27T15:00:00.000Z",',
        '  "ttlMs":     600000,',
        '  "reason":    "Claude Code Edit"',
        '}',
      ],
    },
    fields: [
      {
        name: 'path',
        type: 'string (absolute path)',
        role:
          'The file this lock protects. Stored explicitly so `agent-lock list` ' +
          'doesn’t have to reverse the SHA-1 hash in the filename.',
      },
      {
        name: 'agentId',
        type: 'string',
        role:
          'Identifier of the holder. For Claude Code: ' +
          '`claude-code-sess-<first-8-chars-of-session-id>`. For your own ' +
          'scripts: anything you pass to `withLock`.',
      },
      {
        name: 'startedAt',
        type: 'string (ISO 8601)',
        role:
          'When the lock was acquired. Basis for stale detection. Treated as ' +
          'stale if missing, unparseable, ≤ epoch, in the future, or older ' +
          'than `ttlMs`.',
      },
      {
        name: 'ttlMs',
        type: 'number (ms)',
        role:
          'How long this lock may be held before stale-eligible. Default ' +
          '600000 (10 minutes). Caps the worst-case stuck-lock duration ' +
          'after a crash.',
      },
      {
        name: 'reason',
        type: 'string',
        role:
          'Free-form human-readable label. Surfaced in the BLOCKED message ' +
          'so the second agent knows what the first one was doing.',
      },
    ],
    stale: {
      label: 'a lock is stale if any of these hold',
      rules: [
        '`startedAt` is missing',
        '`startedAt` is not a parseable ISO date',
        '`startedAt` parses to ≤ 0 (epoch or before)',
        '`startedAt` is in the future (`> Date.now()`)',
        '`Date.now() - startedAt >= ttlMs`',
      ],
    },
  },

  install: {
    id: 'install',
    eyebrow: 'install',
    title: 'Two commands. You’re done.',
    steps: [
      {
        n: '01',
        label: 'Install the CLI',
        command:
          'curl -fsSL https://raw.githubusercontent.com/lovieco/agent-lock/main/install.sh | bash',
        note: 'clones to ~/.agent-lock-core, symlinks `agent-lock` onto your PATH',
      },
      {
        n: '02',
        label: 'Wire a project',
        command: 'agent-lock install /path/to/your/project',
        note: 'copies hooks + skill, patches .claude/settings.json',
      },
      {
        n: '03',
        label: 'Verify',
        command: 'agent-lock test',
        note: 'simulates acquire / collision / release end-to-end',
      },
    ],
    requirements: ['Node ≥ 16', 'git', 'macOS or Linux'],
  },

  footer: {
    text: 'agent-lock · MIT-licensed · built for Claude Code',
    link: { label: 'lovieco/agent-lock on GitHub', href: 'https://github.com/lovieco/agent-lock' },
  },
};
