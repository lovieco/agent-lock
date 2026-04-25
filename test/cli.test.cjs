'use strict';

// Disable macOS Finder xattr tagging so these tests are platform-neutral.
process.env.CLAUDE_FILE_LOCK_TAG = '0';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { makeTmpDir, writeFile, readFile, runCli, repo } = require('./helpers/tmp.cjs');

const CLI_ENV = { CLAUDE_FILE_LOCK_TAG: '0' };

function runCliEnv(args, opts = {}) {
  return runCli(args, { ...opts, env: { ...CLI_ENV, ...(opts.env || {}) } });
}

function readSettings(target) {
  return JSON.parse(readFile(path.join(target, '.claude/settings.json')));
}

function loadCopiedLock(target) {
  const p = path.join(target, '.livehub/lock/file-lock.cjs');
  delete require.cache[require.resolve(p)];
  return require(p);
}

// ---------------------------------------------------------------------------

describe('help / unknown command', () => {
  it('`help` prints the usage banner', () => {
    const r = runCliEnv(['help']);
    assert.equal(r.status, 0);
    for (const cmd of ['install', 'uninstall', 'status', 'list', 'release', 'purge-all', 'on', 'off', 'test', 'help']) {
      assert.ok(r.stdout.includes(cmd), `expected usage to mention "${cmd}":\n${r.stdout}`);
    }
  });

  it('no args prints the usage banner', () => {
    const r = runCliEnv([]);
    assert.equal(r.status, 0);
    assert.ok(r.stdout.includes('install'));
    assert.ok(r.stdout.includes('uninstall'));
  });

  it('unknown command exits 1 and prints error + banner', () => {
    const r = runCliEnv(['nonsense-cmd']);
    assert.equal(r.status, 1);
    assert.ok(r.stderr.includes('unknown command: nonsense-cmd'), `stderr=${r.stderr}`);
    assert.ok(r.stdout.includes('install'), `stdout=${r.stdout}`);
  });
});

// ---------------------------------------------------------------------------

describe('install', () => {
  it('fresh install copies files and wires settings.json', () => {
    const tmp = makeTmpDir();
    const r = runCliEnv(['install', tmp]);
    assert.equal(r.status, 0, `stderr=${r.stderr}`);
    assert.ok(r.stdout.includes('copied to .livehub/') || r.stdout.includes('copied to'), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('wired .claude/settings.json') || r.stdout.includes('wired'));

    assert.ok(fs.existsSync(path.join(tmp, '.livehub/lock/file-lock.cjs')));
    assert.ok(fs.existsSync(path.join(tmp, '.livehub/lock/mac-tags.cjs')));
    assert.ok(fs.existsSync(path.join(tmp, '.livehub/hooks/file-lock-pre.mjs')));
    assert.ok(fs.existsSync(path.join(tmp, '.livehub/hooks/file-lock-post.mjs')));
    assert.ok(fs.existsSync(path.join(tmp, '.livehub/hooks/file-lock-purge.mjs')));
    assert.ok(fs.existsSync(path.join(tmp, '.claude/skills/file-lock/SKILL.md')));

    const s = readSettings(tmp);
    assert.ok(s.hooks, 'hooks present');
    assert.ok(Array.isArray(s.hooks.PreToolUse));
    assert.ok(Array.isArray(s.hooks.PostToolUse));
    assert.ok(Array.isArray(s.hooks.SessionEnd));

    const pre = s.hooks.PreToolUse.find(g => g.matcher === 'Write|Edit|MultiEdit');
    const post = s.hooks.PostToolUse.find(g => g.matcher === 'Write|Edit|MultiEdit');
    assert.ok(pre && pre.hooks.some(h => typeof h.command === 'string' && h.command.includes('.livehub/hooks/file-lock-pre.mjs')));
    assert.ok(post && post.hooks.some(h => typeof h.command === 'string' && h.command.includes('.livehub/hooks/file-lock-post.mjs')));
    const sessEnd = s.hooks.SessionEnd.flatMap(g => g.hooks || []);
    assert.ok(sessEnd.some(h => typeof h.command === 'string' && h.command.includes('.livehub/hooks/file-lock-purge.mjs')));
  });

  it('is idempotent — running install twice yields no duplicates', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    assert.equal(runCliEnv(['install', tmp]).status, 0);

    const s = readSettings(tmp);
    const countByScript = (event, scriptName) =>
      (s.hooks[event] || [])
        .flatMap(g => g.hooks || [])
        .filter(h => typeof h.command === 'string' && h.command.includes(scriptName))
        .length;

    assert.equal(countByScript('PreToolUse', 'file-lock-pre.mjs'), 1);
    assert.equal(countByScript('PostToolUse', 'file-lock-post.mjs'), 1);
    assert.equal(countByScript('SessionEnd', 'file-lock-purge.mjs'), 1);
  });

  it('preserves existing matcher-group hooks; livehub hook is prepended', () => {
    const tmp = makeTmpDir();
    const settingsPath = path.join(tmp, '.claude/settings.json');
    fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
    const initial = {
      hooks: {
        PreToolUse: [
          {
            matcher: 'Write|Edit|MultiEdit',
            hooks: [{ type: 'command', command: 'echo pre-existing' }],
          },
        ],
      },
    };
    fs.writeFileSync(settingsPath, JSON.stringify(initial, null, 2));

    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const s = readSettings(tmp);
    const group = s.hooks.PreToolUse.find(g => g.matcher === 'Write|Edit|MultiEdit');
    assert.ok(group);
    assert.equal(group.hooks.length, 2);
    assert.ok(group.hooks[0].command.includes('file-lock-pre.mjs'), 'livehub at index 0');
    assert.equal(group.hooks[1].command, 'echo pre-existing');
  });

  it('preserves existing SessionEnd group without matcher and prepends livehub', () => {
    const tmp = makeTmpDir();
    const settingsPath = path.join(tmp, '.claude/settings.json');
    fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
    const initial = {
      hooks: {
        SessionEnd: [
          { hooks: [{ type: 'command', command: 'echo pre' }] },
        ],
      },
    };
    fs.writeFileSync(settingsPath, JSON.stringify(initial, null, 2));

    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const s = readSettings(tmp);
    // Livehub should have reused the same no-matcher group (not created a new one).
    const noMatcherGroups = s.hooks.SessionEnd.filter(g => !g.matcher);
    assert.equal(noMatcherGroups.length, 1, 'exactly one no-matcher SessionEnd group');
    const group = noMatcherGroups[0];
    assert.equal(group.hooks.length, 2);
    assert.ok(group.hooks[0].command.includes('file-lock-purge.mjs'), 'livehub prepended');
    assert.equal(group.hooks[1].command, 'echo pre');
  });

  it('copies .mjs with mode 0o755 and other files with mode 0o644', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const mjs = fs.statSync(path.join(tmp, '.livehub/hooks/file-lock-pre.mjs')).mode & 0o777;
    const cjs = fs.statSync(path.join(tmp, '.livehub/lock/file-lock.cjs')).mode & 0o777;
    assert.equal(mjs, 0o755, `expected 0o755, got 0o${mjs.toString(8)}`);
    assert.equal(cjs, 0o644, `expected 0o644, got 0o${cjs.toString(8)}`);
  });
});

// ---------------------------------------------------------------------------

describe('uninstall', () => {
  it('removes .livehub/, .claude/skills/file-lock/, and strips livehub hooks', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const r = runCliEnv(['uninstall', tmp]);
    assert.equal(r.status, 0);
    assert.equal(fs.existsSync(path.join(tmp, '.livehub')), false);
    assert.equal(fs.existsSync(path.join(tmp, '.claude/skills/file-lock')), false);
    const s = readSettings(tmp);
    const allHooks = [
      ...(s.hooks?.PreToolUse || []),
      ...(s.hooks?.PostToolUse || []),
      ...(s.hooks?.SessionEnd || []),
    ].flatMap(g => g.hooks || []);
    for (const h of allHooks) {
      assert.ok(!(typeof h.command === 'string' && h.command.includes('.livehub/hooks/file-lock-')),
        `leftover livehub hook: ${h.command}`);
    }
  });

  it('succeeds on a fresh dir with no prior install', () => {
    const tmp = makeTmpDir();
    const r = runCliEnv(['uninstall', tmp]);
    assert.equal(r.status, 0, `stderr=${r.stderr}`);
    assert.equal(fs.existsSync(path.join(tmp, '.claude')), false);
  });

  it('leaves non-livehub hooks intact when stripping', () => {
    const tmp = makeTmpDir();
    const settingsPath = path.join(tmp, '.claude/settings.json');
    fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
    const initial = {
      hooks: {
        PreToolUse: [
          {
            matcher: 'Write|Edit|MultiEdit',
            hooks: [{ type: 'command', command: 'echo keep-me' }],
          },
        ],
      },
    };
    fs.writeFileSync(settingsPath, JSON.stringify(initial, null, 2));

    // Uninstall on settings.json with no livehub entries — should be a no-op shape-wise.
    const r = runCliEnv(['uninstall', tmp]);
    assert.equal(r.status, 0);
    const s = readSettings(tmp);
    const group = s.hooks.PreToolUse.find(g => g.matcher === 'Write|Edit|MultiEdit');
    assert.ok(group);
    assert.equal(group.hooks.length, 1);
    assert.equal(group.hooks[0].command, 'echo keep-me');
  });
});

// ---------------------------------------------------------------------------

describe('status', () => {
  it('before install → files NO, wired NO, kill on, does not call list', () => {
    const tmp = makeTmpDir();
    const r = runCliEnv(['status', tmp]);
    assert.equal(r.status, 0);
    assert.ok(r.stdout.includes('files installed     : NO'), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('settings.json wired : NO'));
    assert.ok(r.stdout.includes('kill switch         : on'));
    // list would either print "(no active locks)" or crash — since files absent,
    // status should NOT call list.
    assert.ok(!r.stdout.includes('(no active locks)'));
    assert.ok(!r.stdout.includes('active locks:'));
  });

  it('after install on a clean project → yes, yes, on, (no active locks)', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const r = runCliEnv(['status', tmp]);
    assert.equal(r.status, 0);
    assert.ok(r.stdout.includes('files installed     : yes'), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('settings.json wired : yes'));
    assert.ok(r.stdout.includes('kill switch         : on'));
    assert.ok(r.stdout.includes('(no active locks)'));
  });

  it('reports kill switch OFF when CLAUDE_FILE_LOCK=0 in settings', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    assert.equal(runCliEnv(['off', tmp]).status, 0);
    const r = runCliEnv(['status', tmp]);
    assert.equal(r.status, 0);
    assert.ok(r.stdout.includes('kill switch         : OFF (CLAUDE_FILE_LOCK=0)'), `stdout=${r.stdout}`);
  });
});

// ---------------------------------------------------------------------------

describe('list', () => {
  it('before install → exit 1 with "not installed"', () => {
    const tmp = makeTmpDir();
    const r = runCliEnv(['list', tmp]);
    assert.equal(r.status, 1);
    assert.ok(r.stderr.includes('not installed'), `stderr=${r.stderr}`);
  });

  it('after install with one active lock → prints agent id and count', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const target = writeFile(tmp, 'src/mod.ts', 'export const x = 1;\n');
    const lock = loadCopiedLock(tmp);
    const res = lock.acquireLock(target, { agentId: 'test-agent-XYZ', reason: 'unit test' });
    assert.equal(res.ok, true);

    try {
      const r = runCliEnv(['list', tmp]);
      assert.equal(r.status, 0, `stderr=${r.stderr}`);
      assert.ok(r.stdout.includes('active locks: 1'), `stdout=${r.stdout}`);
      assert.ok(r.stdout.includes('test-agent-XYZ'), `stdout=${r.stdout}`);
    } finally {
      lock.releaseLock(target, {});
    }
  });
});

// ---------------------------------------------------------------------------

describe('release', () => {
  it('without file arg → exit 1 with usage message', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const r = runCliEnv(['release'], { cwd: tmp });
    assert.equal(r.status, 1);
    assert.ok(r.stderr.includes('usage: livehub release <file>'), `stderr=${r.stderr}`);
  });

  it('strips the marker from a locked file', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const target = writeFile(tmp, 'src/thing.ts', 'export const y = 2;\n');
    const lock = loadCopiedLock(tmp);
    const res = lock.acquireLock(target, { agentId: 'agent-A', reason: 'r' });
    assert.equal(res.ok, true);
    assert.ok(readFile(target).includes('livehub lock:'), 'marker present before release');

    const r = runCliEnv(['release', target], { cwd: tmp });
    assert.equal(r.status, 0, `stderr=${r.stderr}`);
    assert.ok(!readFile(target).includes('livehub lock:'), 'marker stripped');
  });
});

// ---------------------------------------------------------------------------

describe('purge-all', () => {
  it('removes every active lock and reports the count', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const f1 = writeFile(tmp, 'src/a.ts', 'export const a = 1;\n');
    const f2 = writeFile(tmp, 'src/b.ts', 'export const b = 2;\n');
    const lock = loadCopiedLock(tmp);
    assert.equal(lock.acquireLock(f1, { agentId: 'agent-1' }).ok, true);
    assert.equal(lock.acquireLock(f2, { agentId: 'agent-2' }).ok, true);

    const r = runCliEnv(['purge-all', tmp]);
    assert.equal(r.status, 0, `stderr=${r.stderr}`);
    assert.ok(r.stdout.includes('purged 2 lock(s)'), `stdout=${r.stdout}`);
    assert.ok(!readFile(f1).includes('livehub lock:'));
    assert.ok(!readFile(f2).includes('livehub lock:'));
  });

  it('prints "purged 0 lock(s)" when none active', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const r = runCliEnv(['purge-all', tmp]);
    assert.equal(r.status, 0);
    assert.ok(r.stdout.includes('purged 0 lock(s)'), `stdout=${r.stdout}`);
  });
});

// ---------------------------------------------------------------------------

describe('on / off', () => {
  it('off writes env.CLAUDE_FILE_LOCK=0 and prints OFF', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const r = runCliEnv(['off', tmp]);
    assert.equal(r.status, 0);
    assert.ok(r.stdout.includes('OFF'), `stdout=${r.stdout}`);
    const s = readSettings(tmp);
    assert.equal(s.env.CLAUDE_FILE_LOCK, '0');
  });

  it('on deletes env.CLAUDE_FILE_LOCK and prints ON', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    assert.equal(runCliEnv(['off', tmp]).status, 0);
    const r = runCliEnv(['on', tmp]);
    assert.equal(r.status, 0);
    assert.ok(r.stdout.includes('ON'), `stdout=${r.stdout}`);
    const s = readSettings(tmp);
    assert.ok(!('CLAUDE_FILE_LOCK' in (s.env || {})), `env=${JSON.stringify(s.env)}`);
  });

  it('on creates settings.json when none exists', () => {
    const tmp = makeTmpDir();
    assert.equal(fs.existsSync(path.join(tmp, '.claude/settings.json')), false);
    const r = runCliEnv(['on', tmp]);
    assert.equal(r.status, 0);
    assert.ok(fs.existsSync(path.join(tmp, '.claude/settings.json')));
    const s = readSettings(tmp);
    assert.ok(!('CLAUDE_FILE_LOCK' in (s.env || {})));
  });
});

// ---------------------------------------------------------------------------

describe('test command', () => {
  it('after install → all six steps OK, exit 0', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const r = runCliEnv(['test', tmp]);
    assert.equal(r.status, 0, `stderr=${r.stderr}`);
    assert.ok(r.stdout.includes('1. whole-file acquire'), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('2. whole-file collision'));
    assert.ok(r.stdout.includes('3. release'));
    assert.ok(r.stdout.includes('4. semantic:'));
    assert.ok(r.stdout.includes('5. semantic collision:'));
    assert.ok(r.stdout.includes('6. markers stripped'));

    // Count lines matching "OK" — every step should succeed.
    const lines = r.stdout.split('\n');
    const okCount = lines.filter(l => /\bOK\b/.test(l)).length;
    assert.ok(okCount >= 6, `expected >=6 OK lines, got ${okCount}:\n${r.stdout}`);
    assert.ok(!r.stdout.includes('FAIL'), `unexpected FAIL in output:\n${r.stdout}`);
  });

  it('before install → exit 1 with "not installed"', () => {
    const tmp = makeTmpDir();
    const r = runCliEnv(['test', tmp]);
    assert.equal(r.status, 1);
    assert.ok(r.stderr.includes('not installed'), `stderr=${r.stderr}`);
  });
});

// ---------------------------------------------------------------------------

describe('watch command', () => {
  it('--once before install → exit 1 with "not installed"', () => {
    const tmp = makeTmpDir();
    const r = runCliEnv(['watch', tmp, '--once']);
    assert.equal(r.status, 1);
    assert.ok(r.stderr.includes('not installed'), `stderr=${r.stderr}`);
  });

  it('--once after install with no locks → "(no active locks)"', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const r = runCliEnv(['watch', tmp, '--once']);
    assert.equal(r.status, 0, `stderr=${r.stderr}`);
    assert.ok(r.stdout.includes('livehub watch'), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('0 active lock(s)'), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('(no active locks)'));
  });

  it('--once after install with one lock → renders one row in the table', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const lockMod = loadCopiedLock(tmp);
    const target = writeFile(tmp, 'src/foo.ts', 'const z = 1;\n');
    assert.equal(lockMod.acquireLock(target, { agentId: 'watch-agent', reason: 'editing' }).ok, true);

    const r = runCliEnv(['watch', tmp, '--once']);
    assert.equal(r.status, 0, `stderr=${r.stderr}`);
    assert.ok(r.stdout.includes('1 active lock(s)'), `stdout=${r.stdout}`);
    assert.ok(/PATH\s+AGENT\s+NODE\s+AGE\s+REASON/.test(r.stdout), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('watch-agent'));
    assert.ok(r.stdout.includes('editing'));
    assert.ok(/src\/foo\.ts/.test(r.stdout));
  });

  it('--once with --once as the only positional arg uses cwd', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install'], { cwd: tmp }).status, 0);
    const r = runCliEnv(['watch', '--once'], { cwd: tmp });
    assert.equal(r.status, 0, `stderr=${r.stderr}`);
    assert.ok(r.stdout.includes('livehub watch'), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('(no active locks)'));
  });
});

// ---------------------------------------------------------------------------

describe('doctor command', () => {
  it('before install → exit 1 with "not installed"', () => {
    const tmp = makeTmpDir();
    const r = runCliEnv(['doctor', tmp]);
    assert.equal(r.status, 1);
    assert.ok(r.stderr.includes('not installed'), `stderr=${r.stderr}`);
  });

  it('after install on a clean tree → exits 0 with "clean."', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const r = runCliEnv(['doctor', tmp]);
    assert.equal(r.status, 0, `stderr=${r.stderr}`);
    assert.ok(r.stdout.includes('livehub doctor:'), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('clean.'));
  });

  it('reports stale markers (>10 min) and exits 1', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const lockMod = loadCopiedLock(tmp);
    const target = writeFile(tmp, 'src/old.ts', 'const z = 1;\n');
    assert.equal(lockMod.acquireLock(target, { agentId: 'old-agent', reason: 'r' }).ok, true);
    const stale = new Date(Date.now() - 11 * 60 * 1000).toISOString();
    fs.writeFileSync(target, fs.readFileSync(target, 'utf-8').replace(/@\S+/, `@${stale}`));

    const r = runCliEnv(['doctor', tmp]);
    assert.equal(r.status, 1);
    assert.ok(r.stdout.includes('[stale]'), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('old-agent'));
    assert.ok(/1 anomaly( or anomalies)? found/.test(r.stdout));
  });

  it('reports unparseable marker-shaped lines and exits 1', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    // Marker-shaped (has A= field) but missing N=/@/R= — a half-rewritten
    // marker. Doctor should flag it. Plain prose containing `livehub lock:`
    // (no field tokens) must NOT trigger.
    writeFile(tmp, 'src/bad.ts',
      '// livehub lock: A=corrupt half-written-marker missing fields\n' +
      'const x = 1;\n');

    const r = runCliEnv(['doctor', tmp]);
    assert.equal(r.status, 1);
    assert.ok(r.stdout.includes('[unparseable]'), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('half-written-marker'));
  });

  it('does NOT flag prose mentions of `livehub lock:` (no field tokens)', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    // Comment with the phrase but no A=/agent= field — should be skipped.
    writeFile(tmp, 'src/prose.ts',
      '// see also: `grep "livehub lock:" <file>` is the source of truth\n' +
      'const x = 1;\n');

    const r = runCliEnv(['doctor', tmp]);
    assert.equal(r.status, 0, `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('clean.'));
  });

  it('reports duplicate (agent, node) pairs and exits 1', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    writeFile(tmp, 'src/dup.ts',
      '// livehub lock: A=dupAgent N=fn:foo @2026-04-25T13:00:00.000Z R=Edit\n' +
      '// livehub lock: A=dupAgent N=fn:foo @2026-04-25T13:00:01.000Z R=Edit\n' +
      'function foo() { return 1; }\n');

    const r = runCliEnv(['doctor', tmp]);
    assert.equal(r.status, 1);
    assert.ok(r.stdout.includes('[duplicate]'), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes('dupAgent'));
    assert.ok(r.stdout.includes('(2 markers)'));
  });

  it('reports orphan critical-section files (.lh-crit) and exits 1', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    const orphan = writeFile(tmp, 'src/abandoned.ts.lh-crit', '');

    const r = runCliEnv(['doctor', tmp]);
    assert.equal(r.status, 1);
    assert.ok(r.stdout.includes('[orphan-crit]'), `stdout=${r.stdout}`);
    assert.ok(r.stdout.includes(orphan));
  });

  it('reports multiple anomalies in one run with the correct count', () => {
    const tmp = makeTmpDir();
    assert.equal(runCliEnv(['install', tmp]).status, 0);
    // 1× stale, 1× duplicate (with FRESH timestamps so the dup file isn't
    // ALSO flagged as stale — keeps the count assertion clean).
    const lockMod = loadCopiedLock(tmp);
    const a = writeFile(tmp, 'src/a.ts', 'const a = 1;\n');
    lockMod.acquireLock(a, { agentId: 'stale-A', reason: 'x' });
    const stale = new Date(Date.now() - 11 * 60 * 1000).toISOString();
    fs.writeFileSync(a, fs.readFileSync(a, 'utf-8').replace(/@\S+/, `@${stale}`));
    const fresh1 = new Date(Date.now() -  5 * 1000).toISOString();
    const fresh2 = new Date(Date.now() - 10 * 1000).toISOString();
    writeFile(tmp, 'src/b.ts',
      `// livehub lock: A=dupB N=fn:bar @${fresh1} R=Edit\n` +
      `// livehub lock: A=dupB N=fn:bar @${fresh2} R=Edit\n` +
      'function bar() { return 2; }\n');

    const r = runCliEnv(['doctor', tmp]);
    assert.equal(r.status, 1);
    assert.ok(r.stdout.includes('[stale]'));
    assert.ok(r.stdout.includes('[duplicate]'));
    assert.ok(/2 anomaly( or anomalies)? found/.test(r.stdout), `stdout=${r.stdout}`);
  });
});

// ---------------------------------------------------------------------------

describe('default target (cwd)', () => {
  it('install with no target arg uses cwd', () => {
    const tmp = makeTmpDir();
    const r = runCliEnv(['install'], { cwd: tmp });
    assert.equal(r.status, 0, `stderr=${r.stderr}`);
    assert.ok(fs.existsSync(path.join(tmp, '.livehub/lock/file-lock.cjs')));
  });
});
