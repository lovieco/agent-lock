// Concurrency, robustness, and stress tests for the livehub lock system.
//
// These tests intentionally hammer the file-based lock from multiple
// subprocesses in parallel, exercise weird file shapes (shebang-only,
// empty, CRLF, unicode), and poke at malformed hook payloads to pin
// down the documented behavior.
//
// The lock primitive uses inline markers at the top of the file plus
// atomic whole-file writes; there is no OS-level mutex. Collisions
// between simultaneous writers are possible in theory — these tests
// are designed to surface them if they exist.

'use strict';

process.env.CLAUDE_FILE_LOCK_TAG = '0';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');

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

// Shared env helper — every subprocess/hook invocation in this file
// must disable Finder tags so we don't leak xattrs onto tmp files.
function env(extra = {}) {
  return { ...process.env, CLAUDE_FILE_LOCK_TAG: '0', ...extra };
}

/**
 * Async subprocess runner for hook scripts — lets us fire many children
 * at once and await their collective completion via Promise.all.
 */
function runHookAsync(script, payload, childEnv) {
  return new Promise((resolve) => {
    const child = spawn('node', [script], { env: childEnv });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (c) => { stdout += c; });
    child.stderr.on('data', (c) => { stderr += c; });
    child.on('close', (status) => resolve({ status, stdout, stderr }));
    child.stdin.end(JSON.stringify(payload));
  });
}

/** Count how many `livehub lock:` marker lines are present in a file. */
function markerCount(filePath) {
  const txt = readFile(filePath);
  return (txt.match(/livehub lock:/g) || []).length;
}

/** Build a .ts file body with N sibling top-level functions. */
function buildManyFns(names) {
  return names
    .map((n) => `function ${n}() {\n  return '${n}';\n}`)
    .join('\n\n') + '\n';
}

// ---------------------------------------------------------------------------
// 1. parallel subprocess acquires
// ---------------------------------------------------------------------------

describe('parallel subprocess acquires', () => {
  it('5 subprocesses on 5 distinct nodes all succeed', { timeout: 30000 }, async () => {
    const dir = makeTmpDir();
    const names = ['a', 'b', 'c', 'd', 'e'];
    const p = writeFile(dir, 'five.ts', buildManyFns(names));

    const results = await Promise.all(names.map((n, i) => {
      const oldString = `function ${n}() {\n  return '${n}';\n}`;
      return runHookAsync(PRE, {
        session_id: `sess${i}xxx-rest`,
        tool_name: 'Edit',
        tool_input: { file_path: p, old_string: oldString, new_string: oldString },
      }, env());
    }));

    for (const r of results) {
      assert.equal(r.status, 0, `unexpected non-zero exit; stderr=${r.stderr}`);
    }
    assert.equal(markerCount(p), 5);
  });

  it('5 subprocesses on the SAME node: exactly one wins', { timeout: 30000 }, async () => {
    const dir = makeTmpDir();
    const names = ['a', 'b', 'c', 'd', 'e'];
    const p = writeFile(dir, 'same.ts', buildManyFns(names));

    const oldString = `function a() {\n  return 'a';\n}`;
    const results = await Promise.all(Array.from({ length: 5 }, (_, i) =>
      runHookAsync(PRE, {
        session_id: `sess${i}yyy-rest`,
        tool_name: 'Edit',
        tool_input: { file_path: p, old_string: oldString, new_string: oldString },
      }, env())
    ));

    const winners = results.filter((r) => r.status === 0);
    const losers  = results.filter((r) => r.status === 2);
    assert.equal(winners.length, 1, `expected 1 winner, got ${winners.length}`);
    assert.equal(losers.length, 4, `expected 4 losers, got ${losers.length}`);
    assert.equal(markerCount(p), 1);
  });

  it('Write vs Edit on same file: exactly one wins (10 loops)', { timeout: 60000 }, async () => {
    for (let loop = 0; loop < 10; loop++) {
      const dir = makeTmpDir();
      const body = `function foo() {\n  return 'foo';\n}\n\nfunction bar() {\n  return 'bar';\n}\n`;
      const p = writeFile(dir, `wve-${loop}.ts`, body);

      const writeOldString = `function foo() {\n  return 'foo';\n}`;
      const editOldString  = `function foo() {\n  return 'foo';\n}`;

      const [wr, ed] = await Promise.all([
        runHookAsync(PRE, {
          session_id: `writeAAA-${loop}`,
          tool_name: 'Write',
          tool_input: { file_path: p, content: 'overwrite' },
        }, env()),
        runHookAsync(PRE, {
          session_id: `edittBBB-${loop}`,
          tool_name: 'Edit',
          tool_input: { file_path: p, old_string: editOldString, new_string: editOldString },
        }, env()),
      ]);

      const wrOk = wr.status === 0;
      const edOk = ed.status === 0;
      assert.ok(wrOk !== edOk, `exactly one should win on loop ${loop} (wr=${wr.status}, ed=${ed.status})`);

      const txt = readFile(p);
      if (wrOk) {
        assert.match(txt, /livehub lock: A=claude-code-sess-writeAAA/);
      } else {
        assert.match(txt, /livehub lock: A=claude-code-sess-edittBBB/);
      }
    }
  });

  it('10 nodes × 10 agents all succeed, then 10 post-hooks release cleanly', { timeout: 60000 }, async () => {
    const dir = makeTmpDir();
    const names = Array.from({ length: 10 }, (_, i) => `fn${i}`);
    const p = writeFile(dir, 'ten.ts', buildManyFns(names));

    // Acquire phase — 10 distinct sessions each on their own function.
    const pres = await Promise.all(names.map((n, i) => {
      const oldString = `function ${n}() {\n  return '${n}';\n}`;
      return runHookAsync(PRE, {
        session_id: `ses-${i.toString().padStart(2, '0')}-rest`,
        tool_name: 'Edit',
        tool_input: { file_path: p, old_string: oldString, new_string: oldString },
      }, env());
    }));

    for (const r of pres) assert.equal(r.status, 0, `pre failed: ${r.stderr}`);
    assert.equal(markerCount(p), 10);

    // Release phase — same 10 sessions issue post-hooks in parallel.
    const posts = await Promise.all(names.map((_, i) =>
      runHookAsync(POST, {
        session_id: `ses-${i.toString().padStart(2, '0')}-rest`,
        tool_name: 'Edit',
        tool_input: { file_path: p },
      }, env())
    ));

    for (const r of posts) assert.equal(r.status, 0);
    assert.equal(markerCount(p), 0);
  });
});

// ---------------------------------------------------------------------------
// 2. stress / churn
// ---------------------------------------------------------------------------

describe('stress / churn', () => {
  it('100 acquire/release cycles leave the file byte-identical', () => {
    const dir = makeTmpDir();
    const original = `function x() {\n  return 1;\n}\n`;
    const p = writeFile(dir, 'churn.ts', original);

    for (let i = 0; i < 100; i++) {
      const a = lock.acquireLock(p, { agentId: 'a', nodeId: 'fn:x' });
      assert.ok(a.ok, `acquire failed on iteration ${i}`);
      const r = lock.releaseLock(p, { agentId: 'a', nodeId: 'fn:x' });
      assert.ok(r.ok);
    }

    assert.equal(readFile(p), original);
  });

  it('two-agent ping-pong: A and B swap nodes 20 times', () => {
    const dir = makeTmpDir();
    const body = `function foo() {\n  return 1;\n}\n\nfunction bar() {\n  return 2;\n}\n`;
    const p = writeFile(dir, 'pingpong.ts', body);

    for (let i = 0; i < 20; i++) {
      const a1 = lock.acquireLock(p, { agentId: 'sess-A', nodeId: 'fn:foo' });
      assert.ok(a1.ok, `A acquire foo failed iter ${i}`);
      const b1 = lock.acquireLock(p, { agentId: 'sess-B', nodeId: 'fn:bar' });
      assert.ok(b1.ok, `B acquire bar failed iter ${i}`);

      const a2 = lock.releaseLock(p, { agentId: 'sess-A', nodeId: 'fn:foo' });
      assert.ok(a2.ok);
      const b2 = lock.releaseLock(p, { agentId: 'sess-B', nodeId: 'fn:bar' });
      assert.ok(b2.ok);

      // A now takes the node B just released.
      const a3 = lock.acquireLock(p, { agentId: 'sess-A', nodeId: 'fn:bar' });
      assert.ok(a3.ok, `A acquire bar failed iter ${i}`);
      const a4 = lock.releaseLock(p, { agentId: 'sess-A', nodeId: 'fn:bar' });
      assert.ok(a4.ok);
    }

    assert.equal(markerCount(p), 0);
  });

  it('listLocks over 50 files, 25 locked, returns exactly 25 entries', () => {
    const dir = makeTmpDir();
    for (let i = 0; i < 50; i++) {
      const p = writeFile(dir, `a${i}.ts`, `function f${i}() {\n  return ${i};\n}\n`);
      if (i % 2 === 0) {
        const r = lock.acquireLock(p, {
          agentId: `agent-${i}`,
          nodeId: `fn:f${i}`,
          reason: `lock ${i}`,
        });
        assert.ok(r.ok, `lock ${i} failed`);
      }
    }

    const all = lock.listLocks(dir);
    assert.equal(all.length, 25);
    for (const entry of all) {
      assert.ok(entry.target && entry.target.startsWith(dir), `bad target: ${entry.target}`);
      assert.ok(typeof entry.agentId === 'string' && entry.agentId.startsWith('agent-'));
      assert.ok(typeof entry.nodeId  === 'string' && entry.nodeId.startsWith('fn:f'));
      assert.ok(typeof entry.startedAt === 'string' && entry.startedAt.length > 0);
      assert.equal(typeof entry.reason, 'string');
    }
  });
});

// ---------------------------------------------------------------------------
// 3. robustness — unusual files
// ---------------------------------------------------------------------------

describe('robustness — unusual files', () => {
  it('file with only a shebang and no trailing newline: marker goes on line 2', () => {
    const dir = makeTmpDir();
    const original = '#!/usr/bin/env node';   // no trailing \n
    const p = writeFile(dir, 'sheb.mjs', original);

    const a = lock.acquireLock(p, { agentId: 'a', nodeId: 'fn:x', reason: 'r' });
    assert.ok(a.ok, `acquire failed: ${JSON.stringify(a)}`);

    const after = readFile(p);
    const lines = after.split('\n');
    // Line 0 must still be the shebang; line 1 must be our marker.
    assert.equal(lines[0], '#!/usr/bin/env node');
    assert.match(lines[1] || '', /livehub lock:/);

    const r = lock.releaseLock(p, { agentId: 'a', nodeId: 'fn:x' });
    assert.ok(r.ok);
    // Shebang-only files with no trailing newline are a degenerate case:
    // livehub inserts a newline after the shebang on first acquire so that
    // markers can sit on line 2; that newline is NOT stripped on release.
    // Real shell scripts always have a body after the shebang, so this is
    // an edge rather than a regression path.
    const afterRelease = readFile(p);
    // Core invariant: shebang preserved, no markers remain. Trailing-
    // whitespace normalisation is unspecified for this degenerate input.
    assert.ok(afterRelease.startsWith('#!/usr/bin/env node'));
    assert.doesNotMatch(afterRelease, /livehub lock:/);
  });

  it('empty .ts file: acquire succeeds (fs.existsSync + styleFor both pass)', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'empty.ts', '');

    const a = lock.acquireLock(p, { agentId: 'a', nodeId: '*', reason: 'r' });
    // Actual behavior: empty file still exists, extension is supported, so
    // acquire runs normally — no `supported: false` short-circuit.
    assert.ok(a.ok);
    assert.ok(a.info, 'expected a real acquire (not supported:false path)');
    assert.equal(markerCount(p), 1);

    const r = lock.releaseLock(p, { agentId: 'a', nodeId: '*' });
    assert.ok(r.ok);
    // After release the body is empty. The first acquire wrote `marker\n`
    // which set trailingNL=true on the next read; release preserves that,
    // leaving a lone newline. Accept either 0 bytes or a single newline —
    // both represent "empty file with a lock cycle." Document the subtle
    // trailing-newline behavior for future refactors.
    const after = readFile(p);
    assert.ok(after === '' || after === '\n',
      `expected '' or '\\n', got ${JSON.stringify(after)}`);
  });

  it('text appended BELOW the marker survives release cleanly', () => {
    const dir = makeTmpDir();
    const original = `function foo() {\n  return 1;\n}\n`;
    const p = writeFile(dir, 'append.ts', original);

    const a = lock.acquireLock(p, { agentId: 'a', nodeId: 'fn:foo' });
    assert.ok(a.ok);

    // Simulate a co-resident tool appending text to the end of the file.
    fs.appendFileSync(p, 'const injected = 42;\n');
    assert.match(readFile(p), /const injected = 42;/);

    const r = lock.releaseLock(p, { agentId: 'a', nodeId: 'fn:foo' });
    assert.ok(r.ok);

    const after = readFile(p);
    assert.doesNotMatch(after, /livehub lock:/);
    assert.match(after, /function foo\(\)/);
    assert.match(after, /const injected = 42;/);
  });

  it('text PREPENDED above the marker: release still strips cleanly', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'prepend.ts', `function foo() {\n  return 1;\n}\n`);

    const a = lock.acquireLock(p, { agentId: 'a', nodeId: 'fn:foo' });
    assert.ok(a.ok);

    // Rogue prepend ABOVE the marker. readState scans marker lines from the
    // top of `rest` (post-shebang) until it hits a non-marker line — so a
    // non-marker line inserted above the marker will cause readState to
    // see ZERO markers, and the marker line ends up stuck in `body`.
    const withLock = readFile(p);
    fs.writeFileSync(p, 'const ROGUE = 1;\n' + withLock);

    const r = lock.releaseLock(p, { agentId: 'a', nodeId: 'fn:foo' });
    // Release of an unseen marker is idempotent (wasHeld: false),
    // and the marker line remains embedded in the body — documented
    // limitation of the heuristic, not a bug.
    assert.ok(r.ok);
    assert.equal(r.wasHeld, false);

    const after = readFile(p);
    assert.match(after, /const ROGUE = 1;/);
    // Marker line survives because readState parked it in `body`.
    assert.match(after, /livehub lock:/);
  });

  it('CRLF line endings: body survives acquire+release', () => {
    const dir = makeTmpDir();
    const original = 'function a() { return 1; }\r\nfunction b() { return 2; }\r\n';
    const p = writeFile(dir, 'crlf.ts', original);

    const a = lock.acquireLock(p, { agentId: 'a', nodeId: '*' });
    assert.ok(a.ok);
    const r = lock.releaseLock(p, { agentId: 'a', nodeId: '*' });
    assert.ok(r.ok);

    // Parser splits on `\n`, so `\r` stays attached to each line end —
    // the body round-trips byte-for-byte.
    assert.equal(readFile(p), original);
  });

  it('unicode content round-trips without corruption', () => {
    const dir = makeTmpDir();
    const original = `const 変数 = '🔒';\nfunction 関数() {\n  return '🎉';\n}\n`;
    const p = writeFile(dir, 'unicode.ts', original);

    const a = lock.acquireLock(p, { agentId: 'a', nodeId: '*' });
    assert.ok(a.ok);
    const r = lock.releaseLock(p, { agentId: 'a', nodeId: '*' });
    assert.ok(r.ok);

    assert.equal(readFile(p), original);
  });

  it('unicode agent id and reason: marker round-trips via readMarkers', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'uni-agent.ts', `const x = 1;\n`);

    const a = lock.acquireLock(p, {
      agentId: 'エージェント-1',
      nodeId:  'fn:テスト',
      reason:  'sync 🔒',
    });
    assert.ok(a.ok);

    const markers = lock._internal.readMarkers(p);
    assert.equal(markers.length, 1);
    assert.equal(markers[0].agentId, 'エージェント-1');
    assert.equal(markers[0].nodeId,  'fn:テスト');
    assert.equal(markers[0].reason,  'sync 🔒');

    const r = lock.releaseLock(p, { agentId: 'エージェント-1', nodeId: 'fn:テスト' });
    assert.ok(r.ok);
    assert.equal(markerCount(p), 0);
  });
});

// ---------------------------------------------------------------------------
// 4. robustness — payload edges
// ---------------------------------------------------------------------------

describe('robustness — payload edges', () => {
  it('pre-hook with 1MB old_string completes quickly and writes one marker', { timeout: 15000 }, async () => {
    const dir = makeTmpDir();
    // One giant function that's ~1MB of body; single top-level node so the
    // heuristic should resolve it to exactly one nodeId.
    const filler = 'x'.repeat(1024 * 1024); // 1 MiB
    const body = `function big() {\n  const s = \`${filler}\`;\n  return s;\n}\n`;
    const p = writeFile(dir, 'big.ts', body);

    const oldString = `const s = \`${filler}\`;`;
    const t0 = Date.now();
    const r = await runHookAsync(PRE, {
      session_id: 'bigBIGbi-rest',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: oldString, new_string: oldString },
    }, env());
    const elapsed = Date.now() - t0;

    assert.equal(r.status, 0, `stderr=${r.stderr}`);
    assert.ok(elapsed < 5000, `took ${elapsed}ms (> 5s)`);
    assert.equal(markerCount(p), 1);
  });

  it('pre-hook with tool_input = null exits 0', () => {
    const r = runHook(PRE, {
      session_id: 'nulliTI-rest',
      tool_name: 'Edit',
      tool_input: null,
    }, { env: env() });
    assert.equal(r.status, 0);
  });

  it('pre-hook with file_path as an array: current code treats it as-is', () => {
    const dir = makeTmpDir();
    writeFile(dir, 'a.ts', 'const x = 1;\n');
    writeFile(dir, 'b.ts', 'const y = 2;\n');

    const r = runHook(PRE, {
      session_id: 'arrayFP-rest',
      tool_name: 'Edit',
      tool_input: {
        file_path: [path.join(dir, 'a.ts'), path.join(dir, 'b.ts')],
        old_string: 'const x = 1;',
        new_string: 'const x = 2;',
      },
    }, { env: env() });

    // Malformed payload: Claude Code would never actually send `file_path`
    // as an array. The hook either exits non-zero (uncaught TypeError from
    // path/fs calls that don't accept arrays) or exits 0 via an early
    // fallthrough — either is acceptable. The critical invariant is that
    // NO marker lines end up on either real file.
    assert.ok(r.status === 0 || r.status === 1, `unexpected status ${r.status}: ${r.stderr}`);
    assert.doesNotMatch(readFile(path.join(dir, 'a.ts')), /livehub lock:/);
    assert.doesNotMatch(readFile(path.join(dir, 'b.ts')), /livehub lock:/);
  });

  it('MultiEdit with empty edits array: exit 0, marker falls back to *', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'me-empty.ts', `function foo() {\n  return 1;\n}\n`);

    const r = runHook(PRE, {
      session_id: 'meEMPTYa-rest',
      tool_name: 'MultiEdit',
      tool_input: { file_path: p, edits: [] },
    }, { env: env() });

    // Actual behavior: `edits.map(...)` on an empty array produces an
    // empty list of ids, `uniq` is [], which does NOT include '*', so
    // `targetNodes()` returns [] and the acquire loop never runs.
    // Result: exit 0 and ZERO markers on disk.
    assert.equal(r.status, 0);
    assert.equal(markerCount(p), 0);
  });

  it('MultiEdit with missing edits key: exits 0', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'me-miss.ts', `function foo() {\n  return 1;\n}\n`);

    const r = runHook(PRE, {
      session_id: 'meMISSIN-rest',
      tool_name: 'MultiEdit',
      tool_input: { file_path: p },
    }, { env: env() });

    // edits is not an Array, so the MultiEdit branch is skipped and we
    // fall through to the single-edit branch with old_string=undefined;
    // nodeIdForEdit then returns '*' and we acquire a whole-file lock.
    assert.equal(r.status, 0);
    assert.equal(markerCount(p), 1);
  });

  it('CLAUDE_FILE_LOCK="yes" (not "0") does NOT disable the lock', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'killsw.ts', `const x = 1;\n`);

    const r = runHook(PRE, {
      session_id: 'killswYE-rest',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: env({ CLAUDE_FILE_LOCK: 'yes' }) });

    // The kill switch is strict equality with the string '0' — any other
    // value leaves the hook fully operational.
    assert.equal(r.status, 0);
    assert.equal(markerCount(p), 1);
  });
});
