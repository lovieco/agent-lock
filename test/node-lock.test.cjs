// Tests for node-level (semantic-scope) locking.
//
// Covers both the lock module (acquireLock/releaseLock/listLocks/withLock)
// and the pre/post hook scripts, which translate Claude tool payloads into
// scoped lock acquisitions.

'use strict';

process.env.CLAUDE_FILE_LOCK_TAG = '0';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const {
  makeTmpDir,
  writeFile,
  readFile,
  repo,
  runHook,
} = require('./helpers/tmp.cjs');

const lock = require(path.resolve(__dirname, '..', 'lock', 'file-lock.cjs'));
const PRE  = repo('hooks/file-lock-pre.mjs');
const POST = repo('hooks/file-lock-post.mjs');

// Every runHook subprocess must also see the disabled-tag env var.
function envBase(extra = {}) {
  return { ...process.env, CLAUDE_FILE_LOCK_TAG: '0', ...extra };
}

// The canonical multi-function fixture used across the test file.
// return 1 / 2 / 3 land inside alpha / beta / gamma respectively.
const FIXTURE =
`function alpha() {
  return 1;
}

function beta() {
  return 2;
}

const gamma = () => {
  return 3;
};
`;

/** Count lock marker lines in a file. */
function countMarkers(p) {
  const txt = readFile(p);
  return txt.split('\n').filter(l => l.includes('livehub lock:')).length;
}

/** Return all marker lines in the file as strings. */
function markerLines(p) {
  return readFile(p).split('\n').filter(l => l.includes('livehub lock:'));
}

// ---------------------------------------------------------------------------
// lock module — node-level acquire/release
// ---------------------------------------------------------------------------

describe('lock module — node-level acquire/release', () => {
  it('acquires a per-node lock on a clean file and writes N=fn:foo', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const res = lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:foo', reason: 'test' });
    assert.equal(res.ok, true);
    assert.equal(res.info.nodeId, 'fn:foo');

    const txt = readFile(p);
    assert.match(txt, /^\/\/ livehub lock: A=A N=fn:foo /m);
    assert.equal(countMarkers(p), 1);
  });

  it('two agents can lock DIFFERENT nodes on the same file simultaneously', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const a = lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:alpha' });
    const b = lock.acquireLock(p, { agentId: 'B', nodeId: 'fn:beta' });
    assert.equal(a.ok, true);
    assert.equal(b.ok, true);

    const lines = markerLines(p);
    assert.equal(lines.length, 2);
    assert.ok(lines.some(l => /A=A .*N=fn:alpha/.test(l)));
    assert.ok(lines.some(l => /A=B .*N=fn:beta/.test(l)));
  });

  it('two agents requesting the SAME nodeId → second blocked with heldBy+nodeId', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const a = lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:foo' });
    assert.equal(a.ok, true);

    const b = lock.acquireLock(p, { agentId: 'B', nodeId: 'fn:foo' });
    assert.equal(b.ok, false);
    assert.equal(b.heldBy, 'A');
    assert.equal(b.nodeId, 'fn:foo');
    assert.equal(countMarkers(p), 1);
  });

  it('same agent re-acquires the same nodeId → still exactly one marker', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const a1 = lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:foo' });
    const a2 = lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:foo' });
    assert.equal(a1.ok, true);
    assert.equal(a2.ok, true);

    const lines = markerLines(p);
    assert.equal(lines.length, 1);
    assert.match(lines[0], /A=A .*N=fn:foo/);
  });

  it('same agent acquires TWO different nodes → two markers both owned by that agent', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    assert.equal(lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:foo' }).ok, true);
    assert.equal(lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:bar' }).ok, true);

    const lines = markerLines(p);
    assert.equal(lines.length, 2);
    assert.ok(lines.every(l => /A=A /.test(l)));
    assert.ok(lines.some(l => /N=fn:foo/.test(l)));
    assert.ok(lines.some(l => /N=fn:bar/.test(l)));
  });

  it('nodeId="*" (whole file) blocked while any per-node lock exists', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    assert.equal(lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:foo' }).ok, true);
    const res = lock.acquireLock(p, { agentId: 'B', nodeId: '*' });
    assert.equal(res.ok, false);
    assert.equal(res.heldBy, 'A');
    assert.equal(res.nodeId, 'fn:foo');
  });

  it('per-node acquire blocked while a nodeId="*" whole-file lock exists', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    assert.equal(lock.acquireLock(p, { agentId: 'A', nodeId: '*' }).ok, true);
    const res = lock.acquireLock(p, { agentId: 'B', nodeId: 'fn:foo' });
    assert.equal(res.ok, false);
    assert.equal(res.heldBy, 'A');
    assert.equal(res.nodeId, '*');
  });

  it('omitted nodeId defaults to "*" and marker reflects that', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const res = lock.acquireLock(p, { agentId: 'A' });
    assert.equal(res.ok, true);
    assert.equal(res.info.nodeId, '*');

    const line = markerLines(p)[0];
    assert.match(line, /N=\*/);
  });

  it('releaseLock with no nodeId removes ALL markers held by that agent', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    assert.equal(lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:foo' }).ok, true);
    assert.equal(lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:bar' }).ok, true);
    assert.equal(countMarkers(p), 2);

    const r = lock.releaseLock(p, { agentId: 'A' });
    assert.equal(r.ok, true);
    assert.equal(r.wasHeld, true);
    assert.equal(countMarkers(p), 0);
  });

  it('releaseLock with a specific nodeId removes ONLY that marker', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    assert.equal(lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:foo' }).ok, true);
    assert.equal(lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:bar' }).ok, true);

    const r = lock.releaseLock(p, { agentId: 'A', nodeId: 'fn:foo' });
    assert.equal(r.ok, true);
    assert.equal(r.wasHeld, true);

    const lines = markerLines(p);
    assert.equal(lines.length, 1);
    assert.match(lines[0], /N=fn:bar/);
  });

  it('releaseLock refuses when nodeId is held by a different agent', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    assert.equal(lock.acquireLock(p, { agentId: 'B', nodeId: 'fn:foo' }).ok, true);

    const r = lock.releaseLock(p, { agentId: 'A', nodeId: 'fn:foo' });
    assert.equal(r.ok, false);
    assert.match(r.error, /not owner \(held by B\)/);
    assert.equal(countMarkers(p), 1);
  });

  it('stale marker on fn:foo is cleared on next acquire of any node in that file', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    assert.equal(lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:foo' }).ok, true);
    // Rewrite the started timestamp to something ancient so it's stale.
    const txt = readFile(p).replace(/@\S+/, '@2000-01-01T00:00:00.000Z');
    fs.writeFileSync(p, txt);

    // Different agent acquires a different node: the stale fn:foo marker
    // must be swept before the collision check.
    const res = lock.acquireLock(p, { agentId: 'B', nodeId: 'fn:bar' });
    assert.equal(res.ok, true);

    const lines = markerLines(p);
    assert.equal(lines.length, 1);
    assert.match(lines[0], /A=B .*N=fn:bar/);
  });

  it('re-acquire does not duplicate marker even when called many times', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);
    for (let i = 0; i < 5; i++) {
      assert.equal(lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:foo' }).ok, true);
    }
    assert.equal(countMarkers(p), 1);
  });

  it('release of a non-existent node is a no-op and returns wasHeld=false', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    assert.equal(lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:foo' }).ok, true);
    const r = lock.releaseLock(p, { agentId: 'A', nodeId: 'fn:nonexistent' });
    // The agent owns at least one marker on the file, so the owner-mismatch
    // check passes; but no marker matches the nodeId filter so nothing is removed.
    assert.equal(r.ok, true);
    assert.equal(r.wasHeld, false);
    assert.equal(countMarkers(p), 1);
  });
});

// ---------------------------------------------------------------------------
// lock module — listLocks with node entries
// ---------------------------------------------------------------------------

describe('lock module — listLocks with node entries', () => {
  it('returns one entry per marker across the tree', () => {
    const dir = makeTmpDir();
    const p1 = writeFile(dir, 'one.js', FIXTURE);
    const p2 = writeFile(dir, 'sub/two.js', FIXTURE);

    assert.equal(lock.acquireLock(p1, { agentId: 'A', nodeId: 'fn:alpha', reason: 'ra' }).ok, true);
    assert.equal(lock.acquireLock(p1, { agentId: 'B', nodeId: 'fn:beta',  reason: 'rb' }).ok, true);
    assert.equal(lock.acquireLock(p2, { agentId: 'C', nodeId: '*',        reason: 'rc' }).ok, true);

    const all = lock.listLocks(dir);
    assert.equal(all.length, 3);

    for (const entry of all) {
      assert.ok(entry.target);
      assert.ok(entry.agentId);
      assert.ok(entry.nodeId);
      assert.ok(entry.startedAt);
      assert.ok('reason' in entry);
    }
    const ids = all.map(e => `${path.basename(e.target)}:${e.agentId}:${e.nodeId}`).sort();
    assert.deepEqual(ids, [
      'one.js:A:fn:alpha',
      'one.js:B:fn:beta',
      'two.js:C:*',
    ]);
  });

  it('returns empty list for a clean tree', () => {
    const dir = makeTmpDir();
    writeFile(dir, 'clean.js', FIXTURE);
    assert.deepEqual(lock.listLocks(dir), []);
  });
});

// ---------------------------------------------------------------------------
// withLock — per-node
// ---------------------------------------------------------------------------

describe('withLock — per-node', () => {
  it('releases the per-node marker after the callback completes', async () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    await lock.withLock(p, { agentId: 'A', nodeId: 'fn:foo' }, async () => {
      assert.equal(countMarkers(p), 1);
    });
    assert.equal(countMarkers(p), 0);
  });

  it('throws ELOCKED when whole-file lock requested while a per-node lock exists', async () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    assert.equal(lock.acquireLock(p, { agentId: 'A', nodeId: 'fn:foo' }).ok, true);

    await assert.rejects(
      lock.withLock(p, { agentId: 'B' /* default nodeId '*' */ }, async () => { /* unused */ }),
      (err) => {
        assert.equal(err.code, 'ELOCKED');
        assert.equal(err.details.heldBy, 'A');
        assert.equal(err.details.nodeId, 'fn:foo');
        return true;
      },
    );
    // The pre-existing marker is still there, and no stray marker was written.
    assert.equal(countMarkers(p), 1);
  });

  it('two withLock calls on different nodes proceed in parallel', async () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    let inside = 0;
    const work = (nodeId) => lock.withLock(p, { agentId: nodeId, nodeId }, async () => {
      inside++;
      assert.ok(inside <= 2);
      await new Promise(r => setTimeout(r, 10));
    });
    await Promise.all([work('fn:alpha'), work('fn:beta')]);
    assert.equal(countMarkers(p), 0);
  });
});

// ---------------------------------------------------------------------------
// pre-hook — per-node acquire
// ---------------------------------------------------------------------------

describe('pre-hook — per-node acquire', () => {
  it('Edit whose old_string is inside alpha → acquires N=fn:alpha', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const r = runHook(PRE, {
      session_id: 'sessAAAA-xxx',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: 'return 1;', new_string: 'return 11;' },
    }, { env: envBase() });

    assert.equal(r.status, 0, r.stderr);
    const lines = markerLines(p);
    assert.equal(lines.length, 1);
    assert.match(lines[0], /A=claude-code-sess-sessAAAA .*N=fn:alpha/);
  });

  it('two different sessions editing alpha and beta both succeed', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const a = runHook(PRE, {
      session_id: 'aaaaaaaa-xxx',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: 'return 1;', new_string: 'return 11;' },
    }, { env: envBase() });
    assert.equal(a.status, 0, a.stderr);

    const b = runHook(PRE, {
      session_id: 'bbbbbbbb-yyy',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: 'return 2;', new_string: 'return 22;' },
    }, { env: envBase() });
    assert.equal(b.status, 0, b.stderr);

    const lines = markerLines(p);
    assert.equal(lines.length, 2);
    assert.ok(lines.some(l => /sess-aaaaaaaa .*N=fn:alpha/.test(l)));
    assert.ok(lines.some(l => /sess-bbbbbbbb .*N=fn:beta/.test(l)));
  });

  it('second session editing the SAME node → exit 2 and stderr mentions the node', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const a = runHook(PRE, {
      session_id: 'aaaaaaaa-xxx',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: 'return 1;', new_string: 'return 11;' },
    }, { env: envBase() });
    assert.equal(a.status, 0, a.stderr);

    const b = runHook(PRE, {
      session_id: 'bbbbbbbb-yyy',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: 'return 1;', new_string: 'return 111;' },
    }, { env: envBase() });
    assert.equal(b.status, 2);
    assert.match(b.stderr, /node fn:alpha/);
    assert.match(b.stderr, /BLOCKED/);
    assert.equal(countMarkers(p), 1);
  });

  it('Write (no old_string) acquires N=*', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const r = runHook(PRE, {
      session_id: 'ssssssss-xxx',
      tool_name: 'Write',
      tool_input: { file_path: p },
    }, { env: envBase() });
    assert.equal(r.status, 0, r.stderr);

    const lines = markerLines(p);
    assert.equal(lines.length, 1);
    assert.match(lines[0], /N=\*/);
  });

  it('Write from a second session when first holds a per-node lock → exit 2, stderr says whole file', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const a = runHook(PRE, {
      session_id: 'aaaaaaaa-xxx',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: 'return 1;', new_string: 'return 11;' },
    }, { env: envBase() });
    assert.equal(a.status, 0, a.stderr);

    const b = runHook(PRE, {
      session_id: 'bbbbbbbb-yyy',
      tool_name: 'Write',
      tool_input: { file_path: p, content: 'rewrite' },
    }, { env: envBase() });
    assert.equal(b.status, 2);
    // The BLOCKED message reports the existing lock's scope (more useful
    // to the incoming agent than echoing its own attempted scope).
    assert.match(b.stderr, /fn:alpha/);
    assert.match(b.stderr, /locked by agent/);
    assert.equal(countMarkers(p), 1);
  });
});

// ---------------------------------------------------------------------------
// pre-hook — MultiEdit
// ---------------------------------------------------------------------------

describe('pre-hook — MultiEdit', () => {
  it('three edits hitting three different functions → three node locks written', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const r = runHook(PRE, {
      session_id: 'multiplx-xxx',
      tool_name: 'MultiEdit',
      tool_input: {
        file_path: p,
        edits: [
          { old_string: 'return 1;', new_string: 'return 11;' },
          { old_string: 'return 2;', new_string: 'return 22;' },
          { old_string: 'return 3;', new_string: 'return 33;' },
        ],
      },
    }, { env: envBase() });

    assert.equal(r.status, 0, r.stderr);
    const lines = markerLines(p);
    assert.equal(lines.length, 3);
    assert.ok(lines.some(l => /N=fn:alpha/.test(l)));
    assert.ok(lines.some(l => /N=fn:beta/.test(l)));
    assert.ok(lines.some(l => /N=var:gamma/.test(l)));
    assert.ok(lines.every(l => /A=claude-code-sess-multiplx/.test(l)));
  });

  it('MultiEdit where one edit hits an already-locked node → exit 2 AND innocent nodes are not partially locked', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    // Session A locks beta.
    const a = runHook(PRE, {
      session_id: 'aaaaaaaa-xxx',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: 'return 2;', new_string: 'return 22;' },
    }, { env: envBase() });
    assert.equal(a.status, 0, a.stderr);
    assert.equal(countMarkers(p), 1);

    // Session B tries MultiEdit touching alpha + beta + gamma; beta collides.
    const b = runHook(PRE, {
      session_id: 'bbbbbbbb-yyy',
      tool_name: 'MultiEdit',
      tool_input: {
        file_path: p,
        edits: [
          { old_string: 'return 1;', new_string: 'return 11;' },
          { old_string: 'return 2;', new_string: 'return 222;' },
          { old_string: 'return 3;', new_string: 'return 33;' },
        ],
      },
    }, { env: envBase() });
    assert.equal(b.status, 2);
    assert.match(b.stderr, /node fn:beta/);

    // Only A's lock should remain. B must not have kept partial alpha/gamma locks.
    const lines = markerLines(p);
    assert.equal(lines.length, 1);
    assert.match(lines[0], /sess-aaaaaaaa .*N=fn:beta/);
  });

  it('MultiEdit with an old_string that cannot be located → dominates to N=*', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const r = runHook(PRE, {
      session_id: 'domintex-xxx',
      tool_name: 'MultiEdit',
      tool_input: {
        file_path: p,
        edits: [
          { old_string: 'return 1;', new_string: 'return 11;' },
          { old_string: 'THIS STRING IS NOWHERE', new_string: 'x' },
        ],
      },
    }, { env: envBase() });

    assert.equal(r.status, 0, r.stderr);
    const lines = markerLines(p);
    assert.equal(lines.length, 1);
    assert.match(lines[0], /N=\*/);
  });

  it('MultiEdit where every edit maps to the same node → exactly one marker', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const r = runHook(PRE, {
      session_id: 'dedupedx-xxx',
      tool_name: 'MultiEdit',
      tool_input: {
        file_path: p,
        edits: [
          { old_string: 'return 1;', new_string: 'return 11;' },
          { old_string: 'function alpha() {', new_string: 'function alpha() { // tagged' },
        ],
      },
    }, { env: envBase() });

    assert.equal(r.status, 0, r.stderr);
    const lines = markerLines(p);
    assert.equal(lines.length, 1);
    assert.match(lines[0], /N=fn:alpha/);
  });
});

// ---------------------------------------------------------------------------
// pre-hook — fallback to whole-file
// ---------------------------------------------------------------------------

describe('pre-hook — fallback to whole-file', () => {
  it('Edit whose old_string lies outside any top-level node (e.g. imports) → N=*', () => {
    const dir = makeTmpDir();
    const body =
`import fs from 'node:fs';
import path from 'node:path';

function hello() {
  return 1;
}
`;
    const p = writeFile(dir, 'f.js', body);

    const r = runHook(PRE, {
      session_id: 'importsx-xxx',
      tool_name: 'Edit',
      tool_input: {
        file_path: p,
        old_string: "import fs from 'node:fs';",
        new_string: "import fs from 'node:fs/promises';",
      },
    }, { env: envBase() });

    assert.equal(r.status, 0, r.stderr);
    const lines = markerLines(p);
    assert.equal(lines.length, 1);
    assert.match(lines[0], /N=\*/);
  });

  it('Edit on a .py file (unsupported language for node detection) → N=*', () => {
    const dir = makeTmpDir();
    const py =
`def hello():
    return 1

def world():
    return 2
`;
    const p = writeFile(dir, 'f.py', py);

    const r = runHook(PRE, {
      session_id: 'pythonxx-xxx',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: 'return 1', new_string: 'return 11' },
    }, { env: envBase() });

    assert.equal(r.status, 0, r.stderr);
    const txt = readFile(p);
    assert.match(txt, /^# livehub lock: A=claude-code-sess-pythonxx N=\* /m);
  });

  it('Edit on an unknown extension with no comment syntax → unsupported, no marker', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{"a": 1}\n');

    const r = runHook(PRE, {
      session_id: 'jsonfile-xxx',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: '1', new_string: '2' },
    }, { env: envBase() });

    assert.equal(r.status, 0, r.stderr);
    assert.equal(countMarkers(p), 0);
  });
});

// ---------------------------------------------------------------------------
// post-hook — multi-release
// ---------------------------------------------------------------------------

describe('post-hook — multi-release', () => {
  it('releases every node lock a session held on the file', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    // Use MultiEdit to acquire three node locks for the same session.
    const pre = runHook(PRE, {
      session_id: 'relesxxx-xxx',
      tool_name: 'MultiEdit',
      tool_input: {
        file_path: p,
        edits: [
          { old_string: 'return 1;', new_string: 'return 11;' },
          { old_string: 'return 2;', new_string: 'return 22;' },
          { old_string: 'return 3;', new_string: 'return 33;' },
        ],
      },
    }, { env: envBase() });
    assert.equal(pre.status, 0, pre.stderr);
    assert.equal(countMarkers(p), 3);

    const post = runHook(POST, {
      session_id: 'relesxxx-xxx',
      tool_name: 'MultiEdit',
      tool_input: { file_path: p },
    }, { env: envBase() });
    assert.equal(post.status, 0, post.stderr);
    assert.equal(countMarkers(p), 0);
  });

  it('leaves markers owned by other sessions untouched', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const a = runHook(PRE, {
      session_id: 'aaaaaaaa-xxx',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: 'return 1;', new_string: 'return 11;' },
    }, { env: envBase() });
    assert.equal(a.status, 0, a.stderr);

    const b = runHook(PRE, {
      session_id: 'bbbbbbbb-yyy',
      tool_name: 'Edit',
      tool_input: { file_path: p, old_string: 'return 2;', new_string: 'return 22;' },
    }, { env: envBase() });
    assert.equal(b.status, 0, b.stderr);
    assert.equal(countMarkers(p), 2);

    const post = runHook(POST, {
      session_id: 'aaaaaaaa-xxx',
      tool_name: 'Edit',
      tool_input: { file_path: p },
    }, { env: envBase() });
    assert.equal(post.status, 0, post.stderr);

    const lines = markerLines(p);
    assert.equal(lines.length, 1);
    assert.match(lines[0], /sess-bbbbbbbb .*N=fn:beta/);
  });

  it('post with no matching marker is a no-op (exit 0, no error)', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'f.js', FIXTURE);

    const post = runHook(POST, {
      session_id: 'ghostxxx-xxx',
      tool_name: 'Edit',
      tool_input: { file_path: p },
    }, { env: envBase() });
    assert.equal(post.status, 0, post.stderr);
    assert.equal(countMarkers(p), 0);
  });
});
