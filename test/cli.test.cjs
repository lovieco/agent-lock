'use strict';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { makeTmpDir, writeFile, readFile, runCli, repo } = require('./helpers/tmp.cjs');

function readSettings(t) { return JSON.parse(readFile(path.join(t, '.claude/settings.json'))); }
function loadCopiedLock(t) {
  const p = path.join(t, '.agent-lock/lock/agent-lock.cjs');
  delete require.cache[require.resolve(p)];
  return require(p);
}

describe('help / unknown', () => {
  it('help prints usage', () => {
    const r = runCli(['help']);
    assert.equal(r.status, 0);
    for (const c of ['install', 'uninstall', 'status', 'list', 'release', 'doctor', 'watch']) {
      assert.ok(r.stdout.includes(c), `missing ${c}: ${r.stdout}`);
    }
  });
  it('unknown command exits 1', () => {
    const r = runCli(['nonsense']);
    assert.equal(r.status, 1);
    assert.match(r.stderr, /unknown command/);
  });
});

describe('install / uninstall', () => {
  it('install copies files, creates lockdir, and wires hooks (no `sh -c`)', () => {
    const tmp = makeTmpDir();
    const r = runCli(['install', tmp]);
    assert.equal(r.status, 0, r.stderr);
    assert.ok(fs.existsSync(path.join(tmp, '.agent-lock/lock/agent-lock.cjs')));
    assert.ok(fs.existsSync(path.join(tmp, '.agent-lock/locks')));
    assert.ok(fs.existsSync(path.join(tmp, '.agent-lock/hooks/agent-lock-pre.mjs')));
    assert.ok(fs.existsSync(path.join(tmp, '.claude/skills/agent-lock')));

    const s = readSettings(tmp);
    const allHooks = ['PreToolUse', 'PostToolUse', 'SessionEnd']
      .flatMap(e => (s.hooks[e] || []).flatMap(g => g.hooks || []));
    for (const h of allHooks) {
      assert.ok(!h.command.includes('sh -c'), `hook still uses sh -c: ${h.command}`);
    }
    const pre = s.hooks.PreToolUse.flatMap(g => g.hooks);
    assert.ok(pre.some(h => h.command.includes('agent-lock-pre.mjs')));
  });

  it('idempotent — install twice yields no duplicate hooks', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    runCli(['install', tmp]);
    const s = readSettings(tmp);
    const count = (e, n) => (s.hooks[e] || []).flatMap(g => g.hooks || [])
      .filter(h => h.command.includes(n)).length;
    assert.equal(count('PreToolUse', 'agent-lock-pre.mjs'), 1);
    assert.equal(count('PostToolUse', 'agent-lock-post.mjs'), 1);
    assert.equal(count('SessionEnd', 'agent-lock-purge.mjs'), 1);
  });

  it('uninstall removes .agent-lock and strips hooks', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const r = runCli(['uninstall', tmp]);
    assert.equal(r.status, 0);
    assert.equal(fs.existsSync(path.join(tmp, '.agent-lock')), false);
    const s = readSettings(tmp);
    const left = ['PreToolUse', 'PostToolUse', 'SessionEnd']
      .flatMap(e => (s.hooks?.[e] || []).flatMap(g => g.hooks || []))
      .filter(h => h.command.includes('.agent-lock/hooks/'));
    assert.equal(left.length, 0);
  });

  it('preserves existing hooks alongside agent-lock', () => {
    const tmp = makeTmpDir();
    fs.mkdirSync(path.join(tmp, '.claude'), { recursive: true });
    fs.writeFileSync(path.join(tmp, '.claude/settings.json'), JSON.stringify({
      hooks: { PreToolUse: [{ matcher: 'Write|Edit|MultiEdit', hooks: [{ type: 'command', command: 'echo keep' }] }] },
    }));
    runCli(['install', tmp]);
    const s = readSettings(tmp);
    const grp = s.hooks.PreToolUse.find(g => g.matcher === 'Write|Edit|MultiEdit');
    assert.equal(grp.hooks.length, 2);
    assert.ok(grp.hooks[0].command.includes('agent-lock-pre.mjs'));
    assert.equal(grp.hooks[1].command, 'echo keep');
  });
});

describe('status / list / release / purge-all', () => {
  it('status reflects no install', () => {
    const tmp = makeTmpDir();
    const r = runCli(['status', tmp]);
    assert.equal(r.status, 0);
    assert.match(r.stdout, /files installed     : NO/);
    assert.match(r.stdout, /settings\.json wired : NO/);
  });

  it('status after install', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const r = runCli(['status', tmp]);
    assert.equal(r.status, 0);
    assert.match(r.stdout, /files installed     : yes/);
    assert.match(r.stdout, /settings\.json wired : yes/);
    assert.match(r.stdout, /\(no active locks\)/);
  });

  it('list shows active lock', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const target = writeFile(tmp, 'src/a.ts', 'x\n');
    const lock = loadCopiedLock(tmp);
    lock.acquireLock(target, { agentId: 'test-XYZ', root: tmp });
    const r = runCli(['list', tmp]);
    assert.equal(r.status, 0);
    assert.match(r.stdout, /active locks: 1/);
    assert.match(r.stdout, /test-XYZ/);
  });

  it('release without arg → exit 1', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const r = runCli(['release'], { cwd: tmp });
    assert.equal(r.status, 1);
    assert.match(r.stderr, /usage: agent-lock release/);
  });

  it('release frees the lock', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const target = writeFile(tmp, 'src/a.ts', 'x\n');
    const lock = loadCopiedLock(tmp);
    lock.acquireLock(target, { agentId: 'A', root: tmp });
    assert.equal(lock.listLocks(tmp).length, 1);
    const r = runCli(['release', target], { cwd: tmp });
    assert.equal(r.status, 0);
    assert.equal(lock.listLocks(tmp).length, 0);
  });

  it('purge-all reports count', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const a = writeFile(tmp, 'a.ts', 'x\n');
    const b = writeFile(tmp, 'b.ts', 'y\n');
    const lock = loadCopiedLock(tmp);
    lock.acquireLock(a, { agentId: '1', root: tmp });
    lock.acquireLock(b, { agentId: '2', root: tmp });
    const r = runCli(['purge-all', tmp]);
    assert.match(r.stdout, /purged 2 lock\(s\)/);
  });
});

describe('on / off', () => {
  it('off then on toggles env', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    runCli(['off', tmp]);
    assert.equal(readSettings(tmp).env.CLAUDE_FILE_LOCK, '0');
    runCli(['on', tmp]);
    assert.ok(!('CLAUDE_FILE_LOCK' in (readSettings(tmp).env || {})));
  });
});

describe('test command', () => {
  it('all steps OK', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const r = runCli(['test', tmp]);
    assert.equal(r.status, 0, r.stderr);
    const okCount = r.stdout.split('\n').filter(l => /\bOK\b/.test(l)).length;
    assert.ok(okCount >= 6, `got ${okCount} OK:\n${r.stdout}`);
    assert.ok(!r.stdout.includes('FAIL'), r.stdout);
  });
});

describe('watch --once', () => {
  it('renders empty', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const r = runCli(['watch', tmp, '--once']);
    assert.equal(r.status, 0);
    assert.match(r.stdout, /\(no active locks\)/);
  });

  it('renders one row', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const target = writeFile(tmp, 'src/foo.ts', 'x\n');
    const lock = loadCopiedLock(tmp);
    lock.acquireLock(target, { agentId: 'watch-agent', reason: 'editing', root: tmp });
    const r = runCli(['watch', tmp, '--once']);
    assert.equal(r.status, 0);
    assert.match(r.stdout, /1 active lock\(s\)/);
    assert.match(r.stdout, /watch-agent/);
    assert.match(r.stdout, /editing/);
  });
});

describe('doctor', () => {
  it('clean tree exits 0', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const r = runCli(['doctor', tmp]);
    assert.equal(r.status, 0);
    assert.match(r.stdout, /clean\./);
  });

  it('flags stale and exits 1', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const target = writeFile(tmp, 'src/a.ts', 'x\n');
    const lock = loadCopiedLock(tmp);
    lock.acquireLock(target, { agentId: 'old', root: tmp });
    const lf = lock._internal.lockFileFor(target, { root: tmp });
    const obj = JSON.parse(readFile(lf));
    obj.startedAt = new Date(Date.now() - 11 * 60 * 1000).toISOString();
    fs.writeFileSync(lf, JSON.stringify(obj));
    const r = runCli(['doctor', tmp]);
    assert.equal(r.status, 1);
    assert.match(r.stdout, /\[stale\]/);
  });

  it('flags future-dated and exits 1', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const target = writeFile(tmp, 'src/a.ts', 'x\n');
    const lock = loadCopiedLock(tmp);
    lock.acquireLock(target, { agentId: 'forge', root: tmp });
    const lf = lock._internal.lockFileFor(target, { root: tmp });
    const obj = JSON.parse(readFile(lf));
    obj.startedAt = '2099-01-01T00:00:00.000Z';
    fs.writeFileSync(lf, JSON.stringify(obj));
    const r = runCli(['doctor', tmp]);
    assert.equal(r.status, 1);
    assert.match(r.stdout, /\[future\]/);
  });

  it('flags orphan (target file deleted) and exits 1', () => {
    const tmp = makeTmpDir();
    runCli(['install', tmp]);
    const target = writeFile(tmp, 'src/gone.ts', 'x\n');
    const lock = loadCopiedLock(tmp);
    lock.acquireLock(target, { agentId: 'a', root: tmp });
    fs.unlinkSync(target);
    const r = runCli(['doctor', tmp]);
    assert.equal(r.status, 1);
    assert.match(r.stdout, /\[orphan\]/);
  });
});
