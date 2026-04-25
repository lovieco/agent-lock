// Tests for the three Claude Code hook scripts under hooks/:
//   file-lock-pre.mjs    (PreToolUse)
//   file-lock-post.mjs   (PostToolUse)
//   file-lock-purge.mjs  (SessionEnd)
//
// Every test runs under a fresh tmp dir (never inside the repo) and sets
// CLAUDE_FILE_LOCK_TAG=0 to skip mac Finder tag side-effects.

'use strict';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');

const {
  makeTmpDir,
  writeFile,
  readFile,
  repo,
  runHook,
} = require('./helpers/tmp.cjs');

const lock = require(repo('lock/file-lock.cjs'));

const PRE = repo('hooks/file-lock-pre.mjs');
const POST = repo('hooks/file-lock-post.mjs');
const PURGE = repo('hooks/file-lock-purge.mjs');

// Always pass CLAUDE_FILE_LOCK_TAG=0 so Finder-tag xattrs never get written.
const BASE_ENV = { CLAUDE_FILE_LOCK_TAG: '0' };

function env(extra = {}) {
  return { ...BASE_ENV, ...extra };
}

/** Rewrite the `@<iso>` timestamp on an existing marker to a fixed ISO. */
function rewriteStarted(filePath, iso) {
  const txt = readFile(filePath).replace(/@\S+/, `@${iso}`);
  fs.writeFileSync(filePath, txt);
}

// ---------------------------------------------------------------------------
// file-lock-pre.mjs
// ---------------------------------------------------------------------------

describe('file-lock-pre.mjs', () => {
  it('acquires a fresh lock and exits 0', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.ts', 'const x = 1;\n');

    const r = runHook(PRE, {
      session_id: 'sessAAAA-rest-ignored',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env() });

    assert.equal(r.status, 0);
    assert.equal(r.stderr, '');
    const txt = readFile(p);
    assert.match(txt, /livehub lock: A=claude-code-sess-sessAAAA/);
  });

  it('blocks a second session with exit 2 and informative stderr', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'b.ts', 'const y = 2;\n');

    const a = runHook(PRE, {
      session_id: 'aaaaaaaa-xxx',
      tool_name: 'Edit',
      tool_input: { file_path: p },
    }, { env: env() });
    assert.equal(a.status, 0);

    const b = runHook(PRE, {
      session_id: 'bbbbbbbb-yyy',
      tool_name: 'Edit',
      tool_input: { file_path: p },
    }, { env: env() });

    assert.equal(b.status, 2);
    assert.match(b.stderr, /BLOCKED/);
    assert.match(b.stderr, /locked by agent/);
    assert.ok(b.stderr.includes(p), 'stderr should include the file path');
  });

  it('allows the same session to re-acquire (self-release) without nesting', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'c.ts', 'const z = 3;\n');

    const payload = {
      session_id: 'same1234-rest',
      tool_name: 'Edit',
      tool_input: { file_path: p },
    };

    const a = runHook(PRE, payload, { env: env() });
    assert.equal(a.status, 0);

    const b = runHook(PRE, payload, { env: env() });
    assert.equal(b.status, 0);

    const txt = readFile(p);
    const markerLines = txt.match(/^\/\/ livehub lock: A=claude-code-sess-same1234/gm) || [];
    // Exactly one marker line — no nesting, no stacking.
    assert.equal(markerLines.length, 1, 'exactly one marker line after self re-acquire');
  });

  it('honors the CLAUDE_FILE_LOCK=0 kill switch (file untouched)', () => {
    const dir = makeTmpDir();
    const original = 'const untouched = true;\n';
    const p = writeFile(dir, 'd.ts', original);

    const r = runHook(PRE, {
      session_id: 'killswit',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env({ CLAUDE_FILE_LOCK: '0' }) });

    assert.equal(r.status, 0);
    assert.equal(readFile(p), original);
  });

  it('exits 0 on malformed JSON stdin', () => {
    const r = runHook(PRE, 'not json', { env: env() });
    assert.equal(r.status, 0);
  });

  it('exits 0 on empty stdin (parses as {})', () => {
    const r = runHook(PRE, '', { env: env() });
    assert.equal(r.status, 0);
  });

  it('exits 0 when payload is missing tool_input.file_path', () => {
    const r = runHook(PRE, {
      session_id: 'xxxxxxxx',
      tool_name: 'Write',
      tool_input: {},
    }, { env: env() });
    assert.equal(r.status, 0);
  });

  it('falls back to "unknown" agent prefix when session_id is missing', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'e.ts', 'const w = 4;\n');

    const r = runHook(PRE, {
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env() });

    assert.equal(r.status, 0);
    const txt = readFile(p);
    assert.match(txt, /livehub lock: A=claude-code-sess-unknown/);
  });

  it('leaves unsupported extensions untouched (e.g. .json) and exits 0', () => {
    const dir = makeTmpDir();
    const original = '{"hello":"world"}\n';
    const p = writeFile(dir, 'data.json', original);

    const r = runHook(PRE, {
      session_id: 'jsonsess',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env() });

    assert.equal(r.status, 0);
    assert.equal(readFile(p), original);
  });

  it('defaults the reason to "Claude Code Edit" when tool_name is absent', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.ts', 'const q = 5;\n');

    const r = runHook(PRE, {
      session_id: 'noname12',
      tool_input: { file_path: p },
    }, { env: env() });

    assert.equal(r.status, 0);
    const txt = readFile(p);
    assert.match(txt, /R=Claude Code Edit/);
  });

  it('truncates session_id to 8 chars in the agentId', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'g.ts', 'const r = 6;\n');

    const r = runHook(PRE, {
      session_id: 'abcdefghijklmnop',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env() });

    assert.equal(r.status, 0);
    const txt = readFile(p);
    assert.match(txt, /claude-code-sess-abcdefgh\b/);
    assert.doesNotMatch(txt, /claude-code-sess-abcdefghi/);
  });
});

// ---------------------------------------------------------------------------
// file-lock-post.mjs
// ---------------------------------------------------------------------------

describe('file-lock-post.mjs', () => {
  it('releases a lock after pre/post pair, restoring file body', () => {
    const dir = makeTmpDir();
    const original = 'const original = 1;\n';
    const p = writeFile(dir, 'a.ts', original);

    const pre = runHook(PRE, {
      session_id: 'postaaaa',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env() });
    assert.equal(pre.status, 0);
    assert.notEqual(readFile(p), original);

    const post = runHook(POST, {
      session_id: 'postaaaa',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env() });
    assert.equal(post.status, 0);
    assert.equal(readFile(p), original);
  });

  it('exits 0 when called without any prior acquire (no-op)', () => {
    const dir = makeTmpDir();
    const original = 'const nope = 0;\n';
    const p = writeFile(dir, 'b.ts', original);

    const r = runHook(POST, {
      session_id: 'nolockss',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env() });

    assert.equal(r.status, 0);
    assert.equal(readFile(p), original);
  });

  it('silently does nothing when the releasing session is not the owner', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'c.ts', 'const owned = 1;\n');

    const pre = runHook(PRE, {
      session_id: 'ownerAAA',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env() });
    assert.equal(pre.status, 0);

    const locked = readFile(p);
    assert.match(locked, /livehub lock: A=claude-code-sess-ownerAAA/);

    const r = runHook(POST, {
      session_id: 'strangeX',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env() });
    assert.equal(r.status, 0);

    // Marker must still be present — owner mismatch is a silent no-op.
    assert.equal(readFile(p), locked);
  });

  it('honors CLAUDE_FILE_LOCK=0 (file untouched, exit 0)', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'd.ts', 'const guarded = 1;\n');

    // Acquire so there IS a marker, then run post with kill switch.
    const pre = runHook(PRE, {
      session_id: 'guardeee',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env() });
    assert.equal(pre.status, 0);
    const before = readFile(p);

    const r = runHook(POST, {
      session_id: 'guardeee',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env({ CLAUDE_FILE_LOCK: '0' }) });

    assert.equal(r.status, 0);
    assert.equal(readFile(p), before);
  });

  it('exits 0 on malformed JSON stdin', () => {
    const r = runHook(POST, 'not json', { env: env() });
    assert.equal(r.status, 0);
  });

  it('exits 0 when payload is missing tool_input.file_path', () => {
    const r = runHook(POST, {
      session_id: 'xxxxxxxx',
      tool_name: 'Write',
      tool_input: {},
    }, { env: env() });
    assert.equal(r.status, 0);
  });

  it('uses "unknown" prefix and exits 0 when session_id is missing', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'e.ts', 'const u = 1;\n');
    // No prior acquire — post should just silently no-op.
    const r = runHook(POST, {
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env() });
    assert.equal(r.status, 0);
  });
});

// ---------------------------------------------------------------------------
// file-lock-purge.mjs
// ---------------------------------------------------------------------------

describe('file-lock-purge.mjs', () => {
  it('purges a stale marker (>10 min old) and reports on stderr', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'stale.ts', 'const s = 1;\n');

    lock.acquireLock(p, { agentId: 'stale-test', reason: 'test' });
    const stale = new Date(Date.now() - 11 * 60 * 1000).toISOString();
    rewriteStarted(p, stale);

    const r = runHook(PURGE, {}, { env: env({ CLAUDE_PROJECT_DIR: dir }) });

    assert.equal(r.status, 0);
    assert.match(r.stderr, /purged 1 stale lock\(s\)/);
    const txt = readFile(p);
    assert.doesNotMatch(txt, /livehub lock:/);
  });

  it('skips fresh markers (<10 min old) and stays quiet', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'fresh.ts', 'const f = 1;\n');

    lock.acquireLock(p, { agentId: 'fresh-test', reason: 'test' });
    const fresh = new Date(Date.now() - 30 * 1000).toISOString();
    rewriteStarted(p, fresh);

    const before = readFile(p);
    const r = runHook(PURGE, {}, { env: env({ CLAUDE_PROJECT_DIR: dir }) });

    assert.equal(r.status, 0);
    assert.doesNotMatch(r.stderr, /purged/);
    assert.equal(readFile(p), before);
  });

  it('exits 0 with empty stderr when no markers exist in tree', () => {
    const dir = makeTmpDir();
    writeFile(dir, 'plain.ts', 'const p = 1;\n');

    const r = runHook(PURGE, {}, { env: env({ CLAUDE_PROJECT_DIR: dir }) });

    assert.equal(r.status, 0);
    assert.equal(r.stderr, '');
  });

  it('reports the correct count when multiple stale markers are purged', () => {
    const dir = makeTmpDir();
    const stale = new Date(Date.now() - 11 * 60 * 1000).toISOString();
    const files = ['one.ts', 'two.ts', 'three.ts'].map((n, i) => {
      const p = writeFile(dir, n, `const x${i} = ${i};\n`);
      lock.acquireLock(p, { agentId: `stale-${i}`, reason: 'test' });
      rewriteStarted(p, stale);
      return p;
    });

    const r = runHook(PURGE, {}, { env: env({ CLAUDE_PROJECT_DIR: dir }) });

    assert.equal(r.status, 0);
    assert.match(r.stderr, /purged 3 stale lock\(s\)/);
    for (const p of files) {
      assert.doesNotMatch(readFile(p), /livehub lock:/);
    }
  });

  it('uses CLAUDE_PROJECT_DIR when set (env var wins over fallback)', () => {
    // Covered indirectly by every test above, but assert explicitly: a stale
    // marker in the supplied dir gets purged, proving the env var is honored.
    const dir = makeTmpDir();
    const p = writeFile(dir, 'env-win.ts', 'const e = 1;\n');
    lock.acquireLock(p, { agentId: 'env-test', reason: 'env' });
    const stale = new Date(Date.now() - 11 * 60 * 1000).toISOString();
    rewriteStarted(p, stale);

    const r = runHook(PURGE, {}, { env: env({ CLAUDE_PROJECT_DIR: dir }) });
    assert.equal(r.status, 0);
    assert.match(r.stderr, /purged 1 stale lock\(s\)/);
  });

  it('releases the ending session\'s OWN markers regardless of age (self-heal)', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'mine.ts', 'const m = 1;\n');
    // Hand-craft a marker with the same agentId the purge hook will derive
    // from session_id below — so the "own session" branch fires.
    lock.acquireLock(p, { agentId: 'claude-code-sess-mineAAAA', reason: 'edit' });
    // Marker is fresh (just acquired), so the stale-only path would skip it;
    // the own-session path must catch it.
    const r = runHook(PURGE, { session_id: 'mineAAAA-extra-ignored' }, { env: env({ CLAUDE_PROJECT_DIR: dir }) });
    assert.equal(r.status, 0);
    assert.match(r.stderr, /purged 1 own lock\(s\) from this session/);
    assert.doesNotMatch(readFile(p), /livehub lock:/);
  });

  it('combines own-session and stale purges in a single run', () => {
    const dir = makeTmpDir();
    // One file: own marker (fresh).
    const own = writeFile(dir, 'own.ts', 'const a = 1;\n');
    lock.acquireLock(own, { agentId: 'claude-code-sess-bothAAAA', reason: 'mine' });
    // Another file: stale marker from a different (crashed) agent.
    const stale = writeFile(dir, 'stale.ts', 'const b = 2;\n');
    lock.acquireLock(stale, { agentId: 'claude-code-sess-zombieXX', reason: 'gone' });
    rewriteStarted(stale, new Date(Date.now() - 11 * 60 * 1000).toISOString());

    const r = runHook(PURGE, { session_id: 'bothAAAA-extra' }, { env: env({ CLAUDE_PROJECT_DIR: dir }) });
    assert.equal(r.status, 0);
    assert.match(r.stderr, /purged 1 own lock\(s\) from this session/);
    assert.match(r.stderr, /purged 1 stale lock\(s\)/);
    assert.doesNotMatch(readFile(own),   /livehub lock:/);
    assert.doesNotMatch(readFile(stale), /livehub lock:/);
  });

  it('exits 0 on malformed JSON stdin (falls through to stale-only sweep)', () => {
    const dir = makeTmpDir();
    const r = runHook(PURGE, 'not json', { env: env({ CLAUDE_PROJECT_DIR: dir }) });
    assert.equal(r.status, 0);
    assert.equal(r.stderr, '');
  });

  it('exits 0 on empty stdin (raw || "{}" falsy branch)', () => {
    const dir = makeTmpDir();
    const r = runHook(PURGE, '', { env: env({ CLAUDE_PROJECT_DIR: dir }) });
    assert.equal(r.status, 0);
    assert.equal(r.stderr, '');
  });

  it('falls through to stale-only when session_id is not a string', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'odd.ts', 'const c = 3;\n');
    lock.acquireLock(p, { agentId: 'claude-code-sess-someoneX', reason: 'edit' });
    // session_id is not a string — myAgentId stays null, own-session branch
    // skipped. Marker is fresh → stale path also skips → no purge.
    const r = runHook(PURGE, { session_id: 12345 }, { env: env({ CLAUDE_PROJECT_DIR: dir }) });
    assert.equal(r.status, 0);
    assert.equal(r.stderr, '');
    assert.match(readFile(p), /livehub lock:/);
    lock.releaseLock(p, { agentId: 'claude-code-sess-someoneX' });
  });

  // The CLAUDE_PROJECT_DIR-unset branch (falls back to `../..` of the hook)
  // would scan the livehub repo itself, which could pick up unrelated
  // markers and would require creating files under the repo. That would
  // violate the "tests MUST NOT create files in the livehub repo" rule,
  // so the fallback branch is left unexercised here by design.
});
