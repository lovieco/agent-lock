'use strict';

// Targets the remaining uncovered branches/lines so the coverage report
// reaches 100% on lines and functions. Each `it` is a minimum-viable test
// that exercises a specific path the broader suites don't happen to hit.

process.env.CLAUDE_FILE_LOCK_TAG = '0';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const { makeTmpDir, writeFile, readFile, repo, runHook, runCli } = require('./helpers/tmp.cjs');

const PRE    = repo('hooks/file-lock-pre.mjs');
const POST   = repo('hooks/file-lock-post.mjs');
const PURGE  = repo('hooks/file-lock-purge.mjs');
const LOCK   = repo('lock/file-lock.cjs');

function env(extra = {}) {
  return { ...process.env, CLAUDE_FILE_LOCK_TAG: '0', ...extra };
}

// ---------------------------------------------------------------------------

describe('pre-hook — reachable branch edges', () => {
  it('Edit on a nonexistent file falls back to whole-file (targetNodes existsSync branch)', () => {
    const dir = makeTmpDir();
    const ghost = path.join(dir, 'ghost.ts');
    const r = runHook(PRE, {
      session_id: 'ghostpth',
      tool_name: 'Edit',
      tool_input: { file_path: ghost, old_string: 'anything' },
    }, { env: env() });
    // acquireLock on a nonexistent file returns { ok: true, supported: false },
    // so the hook exits 0 and writes no marker.
    assert.equal(r.status, 0);
    assert.equal(fs.existsSync(ghost), false);
  });

  it('BLOCKED stderr message renders the empty-reason "-" fallback', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'r.ts', 'const x = 1;\n');
    // Seed a marker with an empty reason by acquiring via the module directly.
    const lock = require(LOCK);
    // Clear the require cache for tests running in the same process.
    lock.acquireLock(p, { agentId: 'pre-owner', reason: '' });

    const r = runHook(PRE, {
      session_id: 'otherSID',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: 'const x = 1;' },
    }, { env: env() });
    assert.equal(r.status, 2);
    assert.match(r.stderr, /reason: -/);
  });
});

// ---------------------------------------------------------------------------

describe('purge-hook — reachable branch edges', () => {
  it('reports purged count on stderr when stale markers exist', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.ts', 'const x = 1;\n');
    // Acquire a real marker, then rewrite its timestamp to be 1h in the past.
    const lock = require(LOCK);
    lock.acquireLock(p, { agentId: 'ghost', reason: 'abandoned' });
    const stale = new Date(Date.now() - 60 * 60 * 1000).toISOString();
    const rewritten = readFile(p).replace(/@\S+/, `@${stale}`);
    fs.writeFileSync(p, rewritten);
    const r = runHook(PURGE, {}, { env: env({ CLAUDE_PROJECT_DIR: dir }) });
    assert.equal(r.status, 0);
    assert.match(r.stderr, /purged 1 stale lock\(s\)/);
    assert.doesNotMatch(readFile(p), /livehub lock:/);
  });

  it('runs with CLAUDE_PROJECT_DIR unset — falls back to ../.. of the script', () => {
    // We deliberately do NOT pass CLAUDE_PROJECT_DIR. The purge hook will
    // resolve `path.resolve(here, '../..')` — on this repo that's the
    // livehub root itself. As long as the repo has no stale markers (it
    // doesn't), exit 0 with empty stderr.
    const copiedEnv = { ...process.env, CLAUDE_FILE_LOCK_TAG: '0' };
    delete copiedEnv.CLAUDE_PROJECT_DIR;
    const r = runHook(PURGE, {}, { env: copiedEnv });
    assert.equal(r.status, 0);
    // We don't assert on stderr contents because the fallback dir's contents
    // are not under our control; we only assert that the hook doesn't crash.
  });
});

// ---------------------------------------------------------------------------

describe('lock module CLI — "list" with active locks', () => {
  it('prints each active lock line when the scanned tree has markers', () => {
    // Put a locked file under a tmp dir, then invoke the REPO's lock
    // module with the tmp dir as the scan root (so c8 sees coverage).
    const tmp = makeTmpDir();
    const target = writeFile(tmp, 'src/foo.ts', 'const z = 9;\n');
    const lock = require(LOCK);
    assert.equal(lock.acquireLock(target, { agentId: 'cli-list-test', reason: 'r' }).ok, true);

    const r = spawnSync('node', [LOCK, 'list', tmp], { encoding: 'utf-8', env: env() });
    assert.equal(r.status, 0);
    assert.match(r.stdout, /cli-list-test/);
    assert.match(r.stdout, /node=\*/);
    assert.ok(r.stdout.includes(target), `expected target path in stdout: ${r.stdout}`);
    // Clean up the lock we just took so later tests don't see it.
    lock.releaseLock(target);
  });
});

// ---------------------------------------------------------------------------

describe('semantic — remaining branches', () => {
  const semantic = require(repo('lock/semantic.cjs'));

  it('findEnclosingNode with ext = undefined returns null (ext || "" branch)', () => {
    const src = 'function foo() { return 1; }\n';
    assert.equal(semantic.findEnclosingNode(src, 'return 1;', undefined), null);
  });

  it('listTopLevelNodes skips a line containing `livehub lock:` at the top', () => {
    const src = [
      '// livehub lock: agent=x node=* started=2026-04-23T00:00:00Z reason=r',
      'function foo() {',
      '  return 1;',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:foo');
    // `foo` starts on line 1 (after the marker line), not line 0.
    assert.equal(nodes[0].startLine, 1);
  });

  it('bracesOnly swallows an unterminated block comment gracefully', () => {
    // A `/*` without closing `*/` inside a line triggers the `end < 0 break`
    // path in bracesOnly. The function whose body starts that line is still
    // classified; the `break` just means we stop scanning THIS line's tail.
    const src = [
      'function foo() {',
      '  const x = 1; /* never closed',
      '  return x;',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    // `foo` is still classified (we exit bracesOnly mid-line, but the outer
    // scan proceeds to the next line).
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:foo');
  });
});

// ---------------------------------------------------------------------------

describe('file-lock — Finder-tag side-effect branches', () => {
  it('acquire with tags ENABLED adds the Locked tag; release strips it',
     { skip: process.platform !== 'darwin' },
     () => {
       const dir = makeTmpDir();
       const p = writeFile(dir, 'tagged.js', 'const z = 1;\n');
       // Freshly require the lock module with the tag kill-switch OFF —
       // this exercises the `if (finderTagsEnabled())` true branch and the
       // try/catch around the real xattr call.
       const prev = process.env.CLAUDE_FILE_LOCK_TAG;
       delete process.env.CLAUDE_FILE_LOCK_TAG;
       try {
         delete require.cache[require.resolve(LOCK)];
         const freshLock = require(LOCK);
         assert.equal(freshLock.acquireLock(p, { agentId: 'tag-test' }).ok, true);
         const tagsMod = require(repo('lock/mac-tags.cjs'));
         const afterAcquire = tagsMod.readTags(p);
         assert.ok(afterAcquire.some(t => t.name === 'Locked'),
           `expected Locked tag, got ${JSON.stringify(afterAcquire)}`);
         freshLock.releaseLock(p);
         const afterRelease = tagsMod.readTags(p);
         assert.ok(!afterRelease.some(t => t.name === 'Locked'));
       } finally {
         if (prev !== undefined) process.env.CLAUDE_FILE_LOCK_TAG = prev;
         else process.env.CLAUDE_FILE_LOCK_TAG = '0';
         delete require.cache[require.resolve(LOCK)];
       }
     });
});

describe('mac-tags — reachable branch edges', () => {
  const tags = require(repo('lock/mac-tags.cjs'));

  it('readTags returns [] when the xattr is present but empty', { skip: process.platform !== 'darwin' }, () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'empty-xattr.txt', 'hi\n');
    // Seed an empty hex string into the xattr via the `xattr -wx` tool.
    // This forces the `if (!hex) return []` branch to fire.
    const r = spawnSync('xattr', ['-wx', 'com.apple.metadata:_kMDItemUserTags', '', p]);
    // If we can't seed (some filesystems reject empty hex), skip gracefully.
    if (r.status !== 0) return;
    assert.deepEqual(tags.readTags(p), []);
  });
});
