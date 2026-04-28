'use strict';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { makeTmpDir, writeFile, readFile, repo, runHook } = require('./helpers/tmp.cjs');
const lock = require(repo('lock/file-lock.cjs'));

const PRE = repo('hooks/file-lock-pre.mjs');
const POST = repo('hooks/file-lock-post.mjs');
const PURGE = repo('hooks/file-lock-purge.mjs');

function setup() {
  const dir = makeTmpDir();
  fs.mkdirSync(path.join(dir, '.agent-lock/locks'), { recursive: true });
  return dir;
}

function envWith(root, extra = {}) { return { CLAUDE_PROJECT_DIR: root, ...extra }; }

describe('pre hook', () => {
  it('acquires and exits 0; target file untouched', () => {
    const root = setup();
    const p = writeFile(root, 'a.ts', 'x\n');
    const before = readFile(p);
    const r = runHook(PRE, { session_id: 'AAAAAAAA', tool_name: 'Write', tool_input: { file_path: p } }, { env: envWith(root) });
    assert.equal(r.status, 0, r.stderr);
    assert.equal(readFile(p), before);
    assert.equal(lock.listLocks(root).length, 1);
  });

  it('blocks a different session with exit 2', () => {
    const root = setup();
    const p = writeFile(root, 'a.ts', 'x\n');
    runHook(PRE, { session_id: 'AAAAAAAA', tool_name: 'Write', tool_input: { file_path: p } }, { env: envWith(root) });
    const r = runHook(PRE, { session_id: 'BBBBBBBB', tool_name: 'Write', tool_input: { file_path: p } }, { env: envWith(root) });
    assert.equal(r.status, 2);
    assert.match(r.stderr, /BLOCKED/);
  });

  it('idempotent self re-acquire', () => {
    const root = setup();
    const p = writeFile(root, 'a.ts', 'x\n');
    const payload = { session_id: 'SAME0000', tool_name: 'Edit', tool_input: { file_path: p } };
    assert.equal(runHook(PRE, payload, { env: envWith(root) }).status, 0);
    assert.equal(runHook(PRE, payload, { env: envWith(root) }).status, 0);
    assert.equal(lock.listLocks(root).length, 1);
  });

  it('CLAUDE_FILE_LOCK=0 short-circuits', () => {
    const root = setup();
    const p = writeFile(root, 'a.ts', 'x\n');
    const r = runHook(PRE, { session_id: 'A', tool_name: 'Write', tool_input: { file_path: p } },
      { env: envWith(root, { CLAUDE_FILE_LOCK: '0' }) });
    assert.equal(r.status, 0);
    assert.equal(lock.listLocks(root).length, 0);
  });

  it('malformed JSON: exit 0, logs to stderr, no lock created', () => {
    const root = setup();
    const r = runHook(PRE, 'not-json', { env: envWith(root) });
    assert.equal(r.status, 0);
    assert.match(r.stderr, /malformed JSON/);
    assert.equal(lock.listLocks(root).length, 0);
  });
});

describe('post hook', () => {
  it('releases the session\'s own lock', () => {
    const root = setup();
    const p = writeFile(root, 'a.ts', 'x\n');
    const payload = { session_id: 'XYZ00000', tool_name: 'Write', tool_input: { file_path: p } };
    runHook(PRE, payload, { env: envWith(root) });
    assert.equal(lock.listLocks(root).length, 1);
    runHook(POST, payload, { env: envWith(root) });
    assert.equal(lock.listLocks(root).length, 0);
  });

  it('does not release another agent\'s lock', () => {
    const root = setup();
    const p = writeFile(root, 'a.ts', 'x\n');
    runHook(PRE, { session_id: 'OWNER000', tool_name: 'Write', tool_input: { file_path: p } }, { env: envWith(root) });
    runHook(POST, { session_id: 'OTHER000', tool_name: 'Write', tool_input: { file_path: p } }, { env: envWith(root) });
    assert.equal(lock.listLocks(root).length, 1);
  });
});

describe('purge hook', () => {
  it('releases own session locks regardless of age', () => {
    const root = setup();
    const p = writeFile(root, 'a.ts', 'x\n');
    lock.acquireLock(p, { agentId: 'claude-code-sess-MINEAAAA', root });
    const r = runHook(PURGE, { session_id: 'MINEAAAA-rest' }, { env: envWith(root) });
    assert.equal(r.status, 0);
    assert.match(r.stderr, /purged 1 own lock/);
    assert.equal(lock.listLocks(root).length, 0);
  });

  it('purges stale locks regardless of owner', () => {
    const root = setup();
    const p = writeFile(root, 'a.ts', 'x\n');
    lock.acquireLock(p, { agentId: 'zombie', root });
    const lf = lock._internal.lockFileFor(p, { root });
    const obj = JSON.parse(readFile(lf));
    obj.startedAt = new Date(Date.now() - 11 * 60 * 1000).toISOString();
    fs.writeFileSync(lf, JSON.stringify(obj));
    const r = runHook(PURGE, { session_id: 'someone' }, { env: envWith(root) });
    assert.equal(r.status, 0);
    assert.match(r.stderr, /purged 1 stale lock/);
    assert.equal(lock.listLocks(root).length, 0);
  });

  it('keeps fresh locks of other sessions', () => {
    const root = setup();
    const p = writeFile(root, 'a.ts', 'x\n');
    lock.acquireLock(p, { agentId: 'claude-code-sess-OTHER123', root });
    const r = runHook(PURGE, { session_id: 'MINE0000-rest' }, { env: envWith(root) });
    assert.equal(r.status, 0);
    assert.equal(lock.listLocks(root).length, 1);
  });
});
