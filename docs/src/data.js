// All user-visible copy lives here. The renderer in main.js reads from this
// object and never embeds strings of its own. Add new copy here, not in JSX.

export const data = {
  meta: {
    title: 'livehub — live multi-agent coding without the merge',
    description:
      'A Claude Code skill that lets multiple agents share one tree, ' +
      'locking at the function level instead of the file level. No branches, ' +
      'no PRs, no merge conflicts.',
  },

  nav: {
    brand: 'livehub',
    brandTag: 'v0.1',
    links: [
      { href: '#why',          label: 'Why' },
      { href: '#how',          label: 'How' },
      { href: '#architecture', label: 'Architecture' },
      { href: '#install',      label: 'Install' },
      { href: 'https://github.com/lovieco/livehub', label: 'GitHub', external: true },
    ],
  },

  hero: {
    eyebrow: 'live agentic development',
    title: ['Skip the merge.', 'Just edit.'],
    body:
      'livehub lets multiple Claude Code sessions work in the same tree — ' +
      'and locks at the function level, not the file level. Two agents can ' +
      'edit different functions in the same file at the same time.',
    install: {
      label: 'one-liner install',
      command:
        'curl -fsSL https://raw.githubusercontent.com/lovieco/livehub/main/install.sh | bash',
      copyLabel: 'Copy',
      copiedLabel: 'Copied',
    },
    stats: [
      { value: '100%', label: 'test coverage' },
      { value: '0',    label: 'runtime deps' },
      { value: '~600', label: 'lines of code' },
      { value: '307',  label: 'tests' },
    ],
  },

  pitch: {
    id: 'why',
    eyebrow: 'the idea',
    title: 'The branch/PR loop is the bottleneck',
    body:
      'Most multi-agent setups isolate each agent on its own branch or ' +
      'worktree, then pay the coordination cost at merge time. livehub ' +
      'flips that — coordinate at edit time, not merge time.',
    flows: [
      {
        kind: 'branch',
        label: 'branch / PR workflow',
        steps: ['branch', 'change', 'PR', 'conflicts', 'merge', 'repeat'],
        verdict: 'pain compounds with every extra agent',
      },
      {
        kind: 'livehub',
        label: 'livehub live edit',
        steps: ['lock(node)', 'edit', 'unlock', 'next agent'],
        verdict: 'lock contention stays local to each function',
      },
    ],
    blocked: {
      label: 'what a colliding agent sees',
      lines: [
        '[livehub] BLOCKED: src/schema.ts (node fn:userSchema) is locked',
        'by agent "claude-code-sess-a1b2c3d4" since 2026-04-23T00:41:12Z',
        '(4s ago, reason: Claude Code Edit). Wait, work on a different node,',
        'or set CLAUDE_FILE_LOCK=0 to override.',
      ],
    },
    table: {
      head: ['', 'branch / PR', 'livehub'],
      rows: [
        ['unit of coordination', 'the file',         'the top-level declaration'],
        ['when coordination happens', 'at merge (after work)', 'at edit (before work)'],
        ['failure mode',         'silent merge regressions', 'loud BLOCKED exit, agent retries'],
        ['state visible in',     'git log, PR diffs', 'the file itself'],
        ['scaling N agents',     'merge pain ~ O(N²)', 'lock contention stays local'],
      ],
    },
  },

  marker: {
    id: 'how',
    eyebrow: 'how it works',
    title: 'The lock is the comment',
    body:
      'When an agent starts editing, livehub adds one comment line at the ' +
      'top of the file naming the agent and the node. Another agent reads ' +
      'the same line, sees a different node is wanted, adds its own marker ' +
      'and proceeds.',
    file: {
      name: 'src/api/handlers.ts',
      lines: [
        { kind: 'marker', text: '// livehub lock: agent=claude-code-sess-a1b2c3d4 node=fn:handleRequest started=2026-04-24T08:41:12Z reason=Claude Code Edit', agent: 'A' },
        { kind: 'marker', text: '// livehub lock: agent=claude-code-sess-e5f6g7h8 node=fn:formatResponse started=2026-04-24T08:41:15Z reason=Claude Code Edit', agent: 'B' },
        { kind: 'code', text: "import express from 'express';" },
        { kind: 'code', text: '' },
        { kind: 'code', text: 'function handleRequest(req, res) {', tag: 'A' },
        { kind: 'code', text: '  // sess-A is editing this body' },
        { kind: 'code', text: '  return res.json({ ok: true });' },
        { kind: 'code', text: '}' },
        { kind: 'code', text: '' },
        { kind: 'code', text: 'function formatResponse(data) {', tag: 'B' },
        { kind: 'code', text: '  // sess-B is editing this body' },
        { kind: 'code', text: '  return JSON.stringify(data);' },
        { kind: 'code', text: '}' },
        { kind: 'code', text: '' },
        { kind: 'code', text: 'function parseInput(raw) {', tag: 'free' },
        { kind: 'code', text: '  // anyone else can edit this' },
        { kind: 'code', text: '}' },
      ],
    },
    legend: [
      { swatch: 'A',    label: 'session A holds fn:handleRequest' },
      { swatch: 'B',    label: 'session B holds fn:formatResponse' },
      { swatch: 'free', label: 'parseInput is free — any third agent can take it' },
    ],
  },

  architecture: {
    id: 'architecture',
    eyebrow: 'architecture',
    title: 'Four layers, one tree',
    body:
      'Agents make tool calls. Hooks intercept them. Rules decide. State ' +
      'lives inside the source files themselves — `grep "livehub lock:"` ' +
      'is the source of truth.',
    layers: [
      {
        name: 'Agents',
        path: 'Claude Code sessions',
        role: 'Issue Write / Edit / MultiEdit tool calls. Identified by session id.',
      },
      {
        name: 'Hooks',
        path: '.livehub/hooks/*.mjs',
        role: 'PreToolUse acquires or blocks. PostToolUse releases. SessionEnd purges stale.',
      },
      {
        name: 'Rules',
        path: '.livehub/lock/*.cjs',
        role: 'semantic.cjs locates the node. file-lock.cjs decides collisions and staleness. Pure Node, zero deps.',
      },
      {
        name: 'State',
        path: 'the source files themselves',
        role: 'One marker comment per active lock at the top of the file. No sidecar database.',
      },
    ],
    sidecar: {
      name: 'Skill (sidecar)',
      path: '.claude/skills/file-lock/SKILL.md',
      role: 'Protocol doc Claude reads on session start. Teaches the agent to recover gracefully from BLOCKED.',
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
          'curl -fsSL https://raw.githubusercontent.com/lovieco/livehub/main/install.sh | bash',
        note: 'clones to ~/.livehub-core, symlinks `livehub` onto your PATH',
      },
      {
        n: '02',
        label: 'Wire a project',
        command: 'livehub install /path/to/your/project',
        note: 'copies hooks + skill, patches .claude/settings.json',
      },
      {
        n: '03',
        label: 'Verify',
        command: 'livehub test',
        note: 'simulates acquire / collision / release end-to-end',
      },
    ],
    requirements: ['Node ≥ 16', 'git', 'macOS or Linux'],
  },

  footer: {
    text: 'livehub · MIT-licensed · built for Claude Code',
    link: { label: 'lovieco/livehub on GitHub', href: 'https://github.com/lovieco/livehub' },
  },
};
