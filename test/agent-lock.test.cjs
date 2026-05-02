'use strict';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { makeTmpDir, writeFile, readFile, repo } = require('./helpers/tmp.cjs');
const lock = require(repo('lock/agent-lock.cjs'));

function setupProject() {
  const dir = makeTmpDir();
  fs.mkdirSync(path.join(dir, '.agent-lock/locks'), { recursive: true });
  return dir;
}

describe('acquireLock / releaseLock — basic', () => {
  it('does not modify the target file', () => {
    const root = setupProject();
    const p = writeFile(root, 'a.ts', 'export const x = 1;\n');
    const before = readFile(p);
    const r = lock.acquireLock(p, { agentId: 'a1', root });
    assert.equal(r.ok, true);
    assert.equal(readFile(p), before);
    lock.releaseLock(p, { agentId: 'a1', root });
    assert.equal(readFile(p), before);
  });

  it('writes a JSON lockfile in the central lockdir', () => {
    const root = setupProject();
    const p = writeFile(root, 'a.ts', 'x\n');
    lock.acquireLock(p, { agentId: 'a1', reason: 'test', root });
    const entries = fs.readdirSync(path.join(root, '.agent-lock/locks'));
    assert.equal(entries.length, 1);
    const obj = JSON.parse(readFile(path.join(root, '.agent-lock/locks', entries[0])));
    assert.equal(obj.agentId, 'a1');
    assert.equal(obj.path, p);
  });

  it('blocks a second agent', () => {
    const root = setupProject();
    const p = writeFile(root, 'a.ts', 'x\n');
    assert.equal(lock.acquireLock(p, { agentId: 'a1', root }).ok, true);
    const r = lock.acquireLock(p, { agentId: 'a2', root });
    assert.equal(r.ok, false);
    assert.equal(r.heldBy, 'a1');
  });

  it('idempotent self-reacquire', () => {
    const root = setupProject();
    const p = writeFile(root, 'a.ts', 'x\n');
    assert.equal(lock.acquireLock(p, { agentId: 'a1', root }).ok, true);
    assert.equal(lock.acquireLock(p, { agentId: 'a1', root }).ok, true);
  });

  it('overrides a stale lock', () => {
    const root = setupProject();
    const p = writeFile(root, 'a.ts', 'x\n');
    lock.acquireLock(p, { agentId: 'a1', root });
    // forge a stale timestamp directly in the lockfile
    const lf = lock._internal.lockFileFor(p, { root });
    const obj = JSON.parse(readFile(lf));
    obj.startedAt = new Date(Date.now() - 11 * 60 * 1000).toISOString();
    fs.writeFileSync(lf, JSON.stringify(obj));
    const r = lock.acquireLock(p, { agentId: 'a2', root });
    assert.equal(r.ok, true);
  });

  it('FUTURE timestamp is treated as stale (cannot pin lock)', () => {
    const root = setupProject();
    const p = writeFile(root, 'a.ts', 'x\n');
    const lf = lock._internal.lockFileFor(p, { root });
    fs.writeFileSync(lf, JSON.stringify({
      path: p, agentId: 'attacker', nodeId: '*',
      startedAt: '2099-01-01T00:00:00.000Z', reason: 'pin',
    }));
    const r = lock.acquireLock(p, { agentId: 'good', root });
    assert.equal(r.ok, true, 'future-dated marker must not block acquire');
  });

  it('invalid date string is treated as stale', () => {
    const root = setupProject();
    const p = writeFile(root, 'a.ts', 'x\n');
    const lf = lock._internal.lockFileFor(p, { root });
    fs.writeFileSync(lf, JSON.stringify({
      path: p, agentId: 'bogus', startedAt: 'not-a-date',
    }));
    assert.equal(lock.acquireLock(p, { agentId: 'good', root }).ok, true);
  });

  it('release refuses wrong owner', () => {
    const root = setupProject();
    const p = writeFile(root, 'a.ts', 'x\n');
    lock.acquireLock(p, { agentId: 'a1', root });
    const r = lock.releaseLock(p, { agentId: 'a2', root });
    assert.equal(r.ok, false);
  });

  it('release without agentId force-releases', () => {
    const root = setupProject();
    const p = writeFile(root, 'a.ts', 'x\n');
    lock.acquireLock(p, { agentId: 'a1', root });
    assert.equal(lock.releaseLock(p, { root }).ok, true);
  });
});

describe('listLocks', () => {
  it('returns [] when no locks', () => {
    const root = setupProject();
    assert.deepEqual(lock.listLocks(root), []);
  });

  it('lists every active lock', () => {
    const root = setupProject();
    const a = writeFile(root, 'a.ts', 'x\n');
    const b = writeFile(root, 'b.json', '{}');
    lock.acquireLock(a, { agentId: 'a1', reason: 'r1', root });
    lock.acquireLock(b, { agentId: 'a2', reason: 'r2', root });
    const all = lock.listLocks(root);
    assert.equal(all.length, 2);
    const targets = all.map(l => l.target).sort();
    assert.deepEqual(targets, [a, b].sort());
  });

  it('works for any extension uniformly (json/binary/no-ext)', () => {
    const root = setupProject();
    const j = writeFile(root, 'data.json', '{}');
    const r = writeFile(root, 'README', 'hi');
    assert.equal(lock.acquireLock(j, { agentId: 'a', root }).ok, true);
    assert.equal(lock.acquireLock(r, { agentId: 'b', root }).ok, true);
    assert.equal(readFile(j), '{}');
    assert.equal(readFile(r), 'hi');
    assert.equal(lock.listLocks(root).length, 2);
  });
});

describe('withLock', () => {
  it('releases on success and on throw', async () => {
    const root = setupProject();
    const p = writeFile(root, 'a.ts', 'x\n');
    const v = await lock.withLock(p, { agentId: 'a1', root }, async () => 42);
    assert.equal(v, 42);
    assert.equal(lock.listLocks(root).length, 0);
    await assert.rejects(
      lock.withLock(p, { agentId: 'a1', root }, async () => { throw new Error('x'); }),
      /x/,
    );
    assert.equal(lock.listLocks(root).length, 0);
  });

  it('throws ELOCKED when held by another', async () => {
    const root = setupProject();
    const p = writeFile(root, 'a.ts', 'x\n');
    lock.acquireLock(p, { agentId: 'a1', root });
    await assert.rejects(
      lock.withLock(p, { agentId: 'a2', root }, async () => 1),
      e => e.code === 'ELOCKED',
    );
  });
});

describe('O_EXCL race', () => {
  it('100 races: exactly one acquire wins each round', () => {
    const root = setupProject();
    const p = writeFile(root, 'race.ts', 'x\n');
    for (let i = 0; i < 100; i++) {
      const a = lock.acquireLock(p, { agentId: 'A', root });
      const b = lock.acquireLock(p, { agentId: 'B', root });
      // one wins as fresh, the other returns ok via idempotent self or fails
      assert.equal(a.ok, true);
      assert.equal(b.ok, false); // B is a different agent → blocked
      lock.releaseLock(p, { root });
    }
  });
});
