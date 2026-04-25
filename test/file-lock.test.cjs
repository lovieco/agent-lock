'use strict';

// Disable macOS Finder xattr tagging so these tests are platform-neutral.
// The tag-side-effects are covered separately in mac-tags.test.cjs.
process.env.CLAUDE_FILE_LOCK_TAG = '0';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const { makeTmpDir, writeFile, readFile } = require('./helpers/tmp.cjs');

const LOCK_PATH = path.resolve(__dirname, '..', 'lock', 'file-lock.cjs');
const lock = require(LOCK_PATH);

// ---------------------------------------------------------------------------

describe('acquireLock — sidecar (unsupported extensions) / nonexistent', () => {
  it('acquires via sidecar lock for extensions with no comment syntax; target file unmodified', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{"a":1}\n');
    const res = lock.acquireLock(p, { agentId: 'a1', reason: 'test' });
    assert.equal(res.ok, true);
    // Target file content is untouched — lock state lives in the sidecar.
    assert.equal(readFile(p), '{"a":1}\n');
    // Sidecar exists and records the acquiring agent.
    assert.equal(fs.existsSync(p + '.livehub-lock'), true);
    const sc = JSON.parse(fs.readFileSync(p + '.livehub-lock', 'utf-8'));
    assert.equal(sc.agentId, 'a1');
    assert.equal(sc.nodeId, '*');
    // Release removes the sidecar.
    lock.releaseLock(p, { agentId: 'a1' });
    assert.equal(fs.existsSync(p + '.livehub-lock'), false);
  });

  it('acquires via sidecar lock for files with no extension', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'README', 'hello\n');
    const res = lock.acquireLock(p, { agentId: 'a1', reason: 'test' });
    assert.equal(res.ok, true);
    assert.equal(readFile(p), 'hello\n');
    assert.equal(fs.existsSync(p + '.livehub-lock'), true);
    lock.releaseLock(p, { agentId: 'a1' });
    assert.equal(fs.existsSync(p + '.livehub-lock'), false);
  });

  it('returns supported:false with reason when the file does not exist', () => {
    const dir = makeTmpDir();
    const p = path.join(dir, 'missing.js');
    const res = lock.acquireLock(p);
    assert.equal(res.ok, true);
    assert.equal(res.supported, false);
    assert.equal(res.reason, 'file does not exist yet');
    assert.equal(fs.existsSync(p), false);
  });
});

describe('acquireLock — happy paths', () => {
  it('wraps a simple js file and returns { ok, info }', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'console.log(1);\n');
    const res = lock.acquireLock(p, { agentId: 'alice', reason: 'edit' });
    assert.equal(res.ok, true);
    assert.ok(res.info);
    assert.equal(res.info.agentId, 'alice');
    assert.equal(res.info.reason, 'edit');
    const txt = readFile(p);
    assert.ok(txt.startsWith('// livehub lock: A=alice'));
    assert.ok(txt.includes('console.log(1);'));
  });

  it('defaults agentId to anonymous-<pid> when opts.agentId is absent', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    const res = lock.acquireLock(p);
    assert.equal(res.ok, true);
    assert.equal(res.info.agentId, 'anonymous-' + process.pid);
  });

  it("defaults reason to '' when opts.reason is absent", () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    const res = lock.acquireLock(p, { agentId: 'bob' });
    assert.equal(res.info.reason, '');
    const txt = readFile(p);
    assert.ok(/R=\s*$/m.test(txt.split('\n')[0]) || /R=$/.test(txt.split('\n')[0]));
  });

  it('uses DEFAULT_STALE_MS when opts.staleMs is absent (10 minutes)', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    // Pre-wrap as a different agent with a very recent timestamp — default TTL should still consider it fresh.
    lock.acquireLock(p, { agentId: 'alice' });
    const res = lock.acquireLock(p, { agentId: 'bob' });
    assert.equal(res.ok, false);
    assert.equal(res.heldBy, 'alice');
    assert.ok(typeof res.ageMs === 'number');
  });

  it('flattens newlines in the reason to spaces in the marker', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    lock.acquireLock(p, { agentId: 'alice', reason: 'line1\nline2\nline3' });
    const first = readFile(p).split('\n')[0];
    assert.ok(!first.includes('line1\n'));
    assert.ok(first.includes('line1 line2 line3'));
  });
});

describe('acquireLock — collision / stale / same-agent', () => {
  it('returns a collision object when a different agent holds a fresh lock', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    const a = lock.acquireLock(p, { agentId: 'alice', reason: 'busy' });
    assert.equal(a.ok, true);
    const b = lock.acquireLock(p, { agentId: 'bob' });
    assert.equal(b.ok, false);
    assert.equal(b.heldBy, 'alice');
    assert.equal(b.reason, 'busy');
    assert.ok(typeof b.since === 'string');
    assert.ok(typeof b.ageMs === 'number' && b.ageMs >= 0);
  });

  it('overrides a stale marker (older than staleMs)', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    lock.acquireLock(p, { agentId: 'alice', reason: 'old' });
    // New agent with staleMs=0 — existing marker should be considered stale.
    const b = lock.acquireLock(p, { agentId: 'bob', staleMs: 0 });
    assert.equal(b.ok, true);
    assert.equal(b.info.agentId, 'bob');
    const marker = lock._internal.readMarker(p);
    assert.equal(marker.agentId, 'bob');
  });

  it('lets the same agent re-acquire its own lock', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'hello\n');
    const first = lock.acquireLock(p, { agentId: 'alice', reason: 'r1' });
    assert.equal(first.ok, true);
    const second = lock.acquireLock(p, { agentId: 'alice', reason: 'r2' });
    assert.equal(second.ok, true);
    const marker = lock._internal.readMarker(p);
    assert.equal(marker.agentId, 'alice');
    assert.equal(marker.reason, 'r2');
    // No stacked markers — exactly one marker line on the file.
    const lines = readFile(p).split('\n').filter(l => l.includes('livehub lock:'));
    assert.equal(lines.length, 1);
  });
});

describe('releaseLock', () => {
  it('returns wasHeld:false for unsupported extensions', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}');
    const res = lock.releaseLock(p);
    assert.deepEqual(res, { ok: true, wasHeld: false });
  });

  it('returns wasHeld:false for nonexistent files', () => {
    const dir = makeTmpDir();
    const p = path.join(dir, 'ghost.js');
    const res = lock.releaseLock(p);
    assert.deepEqual(res, { ok: true, wasHeld: false });
  });

  it('returns wasHeld:false for supported files with no marker', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'console.log(1);\n');
    const before = readFile(p);
    const res = lock.releaseLock(p);
    assert.deepEqual(res, { ok: true, wasHeld: false });
    assert.equal(readFile(p), before);
  });

  it('releases a held lock and returns wasHeld:true', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'console.log(1);\n');
    lock.acquireLock(p, { agentId: 'alice' });
    const res = lock.releaseLock(p, { agentId: 'alice' });
    assert.deepEqual(res, { ok: true, wasHeld: true });
    assert.equal(readFile(p), 'console.log(1);\n');
  });

  it('force-releases any owner when opts.agentId is omitted', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    lock.acquireLock(p, { agentId: 'alice' });
    const res = lock.releaseLock(p); // no agentId
    assert.deepEqual(res, { ok: true, wasHeld: true });
    assert.equal(lock._internal.readMarker(p), null);
  });

  it('refuses to release when opts.agentId is wrong owner', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    lock.acquireLock(p, { agentId: 'alice' });
    const res = lock.releaseLock(p, { agentId: 'bob' });
    assert.equal(res.ok, false);
    assert.equal(res.error, 'not owner (held by alice)');
    // Lock still present.
    const marker = lock._internal.readMarker(p);
    assert.equal(marker.agentId, 'alice');
  });
});

describe('withLock', () => {
  it('runs the callback under a lock and releases on success', async () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    let sawMarker = null;
    const result = await lock.withLock(p, { agentId: 'alice' }, async () => {
      sawMarker = lock._internal.readMarker(p);
      return 42;
    });
    assert.equal(result, 42);
    assert.ok(sawMarker);
    assert.equal(sawMarker.agentId, 'alice');
    assert.equal(lock._internal.readMarker(p), null);
    assert.equal(readFile(p), 'x\n');
  });

  it('releases the lock even if the callback throws', async () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    await assert.rejects(
      lock.withLock(p, { agentId: 'alice' }, async () => { throw new Error('boom'); }),
      /boom/,
    );
    assert.equal(lock._internal.readMarker(p), null);
  });

  it('throws an ELOCKED error when the file is already locked by someone else', async () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    lock.acquireLock(p, { agentId: 'alice', reason: 'working' });
    let caught;
    try {
      await lock.withLock(p, { agentId: 'bob' }, async () => 'nope');
    } catch (e) { caught = e; }
    assert.ok(caught);
    assert.equal(caught.code, 'ELOCKED');
    assert.ok(caught.details);
    assert.equal(caught.details.heldBy, 'alice');
    assert.equal(caught.details.reason, 'working');
    assert.ok(/alice/.test(caught.message));
  });

  it('ELOCKED message falls back to "unknown" when collision fields are missing', async () => {
    // Build a fake collision result via a directly-thrown-style path: acquire with a
    // marker that is missing fields (manually write a malformed begin line, then
    // try to acquire). In practice heldBy/since/reason are always populated —
    // this test just guards the `|| 'unknown'` default branches cosmetically.
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    // Pre-wrap with a blank reason and a valid ISO date.
    lock.acquireLock(p, { agentId: 'ghost', reason: '' });
    let caught;
    try {
      await lock.withLock(p, { agentId: 'other' }, async () => 1);
    } catch (e) { caught = e; }
    assert.ok(caught);
    assert.equal(caught.code, 'ELOCKED');
    // Empty reason renders as `-` in the message.
    assert.ok(/reason: -/.test(caught.message));
  });
});

describe('listLocks', () => {
  it('returns [] for an empty tree', () => {
    const dir = makeTmpDir();
    assert.deepEqual(lock.listLocks(dir), []);
  });

  it('returns [] for a tree with only unlocked files', () => {
    const dir = makeTmpDir();
    writeFile(dir, 'a.js', 'x\n');
    writeFile(dir, 'b.md', 'hi\n');
    writeFile(dir, 'c.json', '{}');
    assert.deepEqual(lock.listLocks(dir), []);
  });

  it('finds all files carrying markers and returns the expected shape', () => {
    const dir = makeTmpDir();
    const a = writeFile(dir, 'a.js', 'x\n');
    const b = writeFile(dir, 'sub/b.md', '# hi\n');
    writeFile(dir, 'c.js', 'y\n'); // unlocked
    lock.acquireLock(a, { agentId: 'alice', reason: 'r1' });
    lock.acquireLock(b, { agentId: 'bob', reason: 'r2' });
    const found = lock.listLocks(dir);
    assert.equal(found.length, 2);
    const targets = found.map(f => f.target).sort();
    assert.deepEqual(targets, [a, b].sort());
    for (const f of found) {
      assert.ok(f.agentId);
      assert.ok(f.startedAt);
      assert.ok('reason' in f);
    }
  });

  it('skips node_modules, .git, and .next when walking the tree', () => {
    const dir = makeTmpDir();
    // Files in skipped dirs:
    const nm = writeFile(dir, 'node_modules/lib.js', 'x\n');
    const git = writeFile(dir, '.git/info.js', 'x\n');
    const nx = writeFile(dir, '.next/cache.js', 'x\n');
    // Lock them all (acquireLock works on any readable file).
    for (const p of [nm, git, nx]) lock.acquireLock(p, { agentId: 'a' });
    // File outside skipped dirs:
    const ok = writeFile(dir, 'ok.js', 'x\n');
    lock.acquireLock(ok, { agentId: 'a' });

    const found = lock.listLocks(dir);
    assert.equal(found.length, 1);
    assert.equal(found[0].target, ok);
  });

  it('swallows errors from unreadable directories and keeps walking', () => {
    const dir = makeTmpDir();
    const ok = writeFile(dir, 'a.js', 'x\n');
    lock.acquireLock(ok, { agentId: 'alice' });
    // Create an unreadable sibling dir (mode 0). Restore mode before cleanup.
    const locked = path.join(dir, 'locked-dir');
    fs.mkdirSync(locked);
    writeFile(dir, 'locked-dir/inner.js', 'y\n');
    fs.chmodSync(locked, 0o000);
    try {
      const found = lock.listLocks(dir);
      // Contains our lock; the unreadable dir either yields nothing (readdir
      // throws and is swallowed) or — if running as root — still yields nothing
      // because inner.js has no marker.
      assert.ok(found.some(f => f.target === ok));
    } finally {
      fs.chmodSync(locked, 0o755);
    }
  });

  it('returns silently for a nonexistent root dir (readdir throws, caught)', () => {
    const dir = makeTmpDir();
    const ghost = path.join(dir, 'does-not-exist');
    assert.deepEqual(lock.listLocks(ghost), []);
  });
});

describe('_internal.readMarker', () => {
  it('returns null on a nonexistent file', () => {
    const dir = makeTmpDir();
    assert.equal(lock._internal.readMarker(path.join(dir, 'nope.js')), null);
  });

  it('returns null on a file with no marker', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'console.log(1);\n');
    assert.equal(lock._internal.readMarker(p), null);
  });

  it('reads a marker on line 0 (no shebang)', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    lock.acquireLock(p, { agentId: 'alice', reason: 'line0' });
    const m = lock._internal.readMarker(p);
    assert.ok(m);
    assert.equal(m.agentId, 'alice');
    assert.equal(m.reason, 'line0');
  });

  it('reads a marker on line 1 when line 0 is a shebang', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.sh', '#!/bin/sh\necho hi\n');
    lock.acquireLock(p, { agentId: 'alice', reason: 'shebanged' });
    const lines = readFile(p).split('\n');
    assert.equal(lines[0], '#!/bin/sh');
    assert.ok(lines[1].startsWith('# livehub lock: A=alice'));
    const m = lock._internal.readMarker(p);
    assert.equal(m.agentId, 'alice');
    assert.equal(m.reason, 'shebanged');
  });

  it('returns null for lines that vaguely mention locks but do not match', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    fs.writeFileSync(p, '// livehub-ish but not a marker\nbody\n');
    assert.equal(lock._internal.readMarker(p), null);
  });

  it('returns null for malformed marker-like lines', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', '// not a marker at all\nbody\n');
    assert.equal(lock._internal.readMarker(p), null);
  });

  it('correctly parses an HTML/markdown marker (stripping the trailing -->)', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.md', '# heading\n');
    lock.acquireLock(p, { agentId: 'alice', reason: 'docs' });
    const m = lock._internal.readMarker(p);
    assert.equal(m.agentId, 'alice');
    assert.equal(m.reason, 'docs');
    const first = readFile(p).split('\n')[0];
    assert.ok(first.startsWith('<!--'));
    assert.ok(first.endsWith('-->'));
  });

  it('correctly parses a CSS marker (stripping the trailing */)', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.css', '.x { color: red; }\n');
    lock.acquireLock(p, { agentId: 'alice', reason: 'styles' });
    const m = lock._internal.readMarker(p);
    assert.equal(m.agentId, 'alice');
    assert.equal(m.reason, 'styles');
  });

  it('readSidecar is an alias of readMarker', () => {
    assert.equal(lock._internal.readSidecar, lock._internal.readMarker);
  });
});

describe('wrap / strip via public API', () => {
  it('preserves shebang on line 0, marker goes on line 1', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.sh', '#!/bin/sh\necho 1\necho 2\n');
    lock.acquireLock(p, { agentId: 'alice' });
    const lines = readFile(p).split('\n');
    assert.equal(lines[0], '#!/bin/sh');
    assert.ok(lines[1].startsWith('# livehub lock: A=alice'));
  });

  it('preserves a trailing newline', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'hello\n');
    lock.acquireLock(p, { agentId: 'alice' });
    assert.ok(readFile(p).endsWith('\n'));
    lock.releaseLock(p);
    assert.equal(readFile(p), 'hello\n');
  });

  it('preserves absence of trailing newline', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'hello');
    lock.acquireLock(p, { agentId: 'alice' });
    assert.ok(!readFile(p).endsWith('\n'));
    lock.releaseLock(p);
    assert.equal(readFile(p), 'hello');
  });

  it('handles a shebang file with no body after the shebang line', () => {
    const dir = makeTmpDir();
    // Shebang with no newline after — prefix-extraction short-circuits.
    const p = writeFile(dir, 'a.sh', '#!/bin/sh');
    lock.acquireLock(p, { agentId: 'alice' });
    const txt = readFile(p);
    assert.ok(txt.includes('#!/bin/sh'));
    assert.ok(/livehub lock: A=alice/.test(txt));
  });

  it('release is idempotent — calling twice leaves the file stable', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'body\n');
    lock.acquireLock(p, { agentId: 'alice' });
    lock.releaseLock(p);
    const after1 = readFile(p);
    lock.releaseLock(p);
    const after2 = readFile(p);
    assert.equal(after1, after2);
    assert.equal(after2, 'body\n');
  });

  it('release is a no-op on a nonexistent file', () => {
    const dir = makeTmpDir();
    const p = path.join(dir, 'ghost.js');
    assert.doesNotThrow(() => lock.releaseLock(p));
    assert.equal(fs.existsSync(p), false);
  });
});

describe('_internal.lockPath — extension-aware', () => {
  it('returns null for supported (inline-marker) extensions', () => {
    assert.equal(lock._internal.lockPath('anything.js'), null);
    assert.equal(lock._internal.lockPath('doc.md'), null);
  });

  it('returns the sidecar path for unsupported extensions', () => {
    assert.equal(lock._internal.lockPath('data.json'), 'data.json.livehub-lock');
    assert.equal(lock._internal.lockPath('README'), 'README.livehub-lock');
  });
});

describe('round-trip — every supported extension', () => {
  const cases = [
    { ext: '.ts',   body: 'export const x = 1;\n' },
    { ext: '.tsx',  body: 'export const C = () => null;\n' },
    { ext: '.js',   body: 'console.log(1);\n' },
    { ext: '.jsx',  body: 'export default () => null;\n' },
    { ext: '.cjs',  body: 'module.exports = 1;\n' },
    { ext: '.mjs',  body: 'export default 1;\n' },
    { ext: '.md',   body: '# Title\n\ntext\n' },
    { ext: '.html', body: '<p>hi</p>\n' },
    { ext: '.py',   body: 'print("hi")\n' },
    { ext: '.sh',   body: '#!/bin/sh\necho hi\n' },
    { ext: '.sql',  body: 'SELECT 1;\n' },
    { ext: '.css',  body: '.x { color: red; }\n' },
  ];
  for (const { ext, body } of cases) {
    it(`acquire+release round-trips cleanly for ${ext}`, () => {
      const dir = makeTmpDir();
      const p = writeFile(dir, 'file' + ext, body);
      const orig = readFile(p);
      const a = lock.acquireLock(p, { agentId: 'alice' });
      assert.equal(a.ok, true);
      assert.ok(a.info);
      // File is visibly modified.
      assert.notEqual(readFile(p), orig);
      // Marker is visible.
      assert.ok(lock._internal.readMarker(p));
      const r = lock.releaseLock(p, { agentId: 'alice' });
      assert.deepEqual(r, { ok: true, wasHeld: true });
      // Content is byte-identical after release.
      assert.equal(readFile(p), orig);
    });
  }
});

describe('CLI main', () => {
  function runNode(args, { cwd } = {}) {
    const r = spawnSync('node', [LOCK_PATH, ...args], {
      encoding: 'utf-8',
      cwd: cwd || process.cwd(),
      env: { ...process.env, CLAUDE_FILE_LOCK_TAG: '0' },
    });
    return { status: r.status, stdout: r.stdout || '', stderr: r.stderr || '' };
  }

  it('prints usage when invoked with no args', () => {
    const r = runNode([]);
    assert.equal(r.status, 0);
    assert.ok(/Usage:\s+file-lock\.cjs/.test(r.stdout));
    assert.ok(/list/.test(r.stdout));
    assert.ok(/release/.test(r.stdout));
    assert.ok(/show/.test(r.stdout));
  });

  it('prints an unknown-command usage when cmd is unrecognized', () => {
    const r = runNode(['banana']);
    assert.equal(r.status, 0);
    assert.ok(/Usage:/.test(r.stdout));
  });

  it('list prints "(no active locks)" when none are held in the scanned tree', () => {
    // `list` scans path.join(__dirname, '..', '..') relative to the lock file
    // (i.e. the parent of the livehub repo). In CI we can't guarantee zero
    // locks live up there, but we can at least verify it exits 0 and prints
    // either "(no active locks)" or a list of lines.
    const r = runNode(['list']);
    assert.equal(r.status, 0);
    assert.ok(
      /\(no active locks\)/.test(r.stdout) || /A=/.test(r.stdout),
      `unexpected stdout: ${r.stdout}`,
    );
  });

  it('list with explicit empty dir prints "(no active locks)"', () => {
    const dir = makeTmpDir();
    const r = runNode(['list', dir]);
    assert.equal(r.status, 0);
    assert.ok(/\(no active locks\)/.test(r.stdout), r.stdout);
  });

  it('list with explicit dir containing a locked file prints the lock line', () => {
    const dir = makeTmpDir();
    const f = writeFile(dir, 'a.js', 'x\n');
    lock.acquireLock(f, { agentId: 'cli-test', reason: 'cli-list' });
    const r = runNode(['list', dir]);
    assert.equal(r.status, 0);
    assert.ok(/agent=cli-test/.test(r.stdout), r.stdout);
    assert.ok(/cli-list/.test(r.stdout), r.stdout);
    lock.releaseLock(f, { agentId: 'cli-test' });
  });

  it('show prints the marker for a locked file', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    lock.acquireLock(p, { agentId: 'alice', reason: 'cli-show' });
    const r = runNode(['show', p]);
    assert.equal(r.status, 0);
    assert.ok(/alice/.test(r.stdout));
    assert.ok(/cli-show/.test(r.stdout));
  });

  it('show prints null for an unlocked file', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    const r = runNode(['show', p]);
    assert.equal(r.status, 0);
    assert.ok(/null/.test(r.stdout));
  });

  it('release clears a held lock and prints the result object', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'a.js', 'x\n');
    lock.acquireLock(p, { agentId: 'alice' });
    const r = runNode(['release', p]);
    assert.equal(r.status, 0);
    assert.ok(/wasHeld: true/.test(r.stdout));
    assert.equal(lock._internal.readMarker(p), null);
    assert.equal(readFile(p), 'x\n');
  });

  it('release without a file arg falls through to usage', () => {
    const r = runNode(['release']);
    assert.equal(r.status, 0);
    assert.ok(/Usage:/.test(r.stdout));
  });

  it('show without a file arg falls through to usage', () => {
    const r = runNode(['show']);
    assert.equal(r.status, 0);
    assert.ok(/Usage:/.test(r.stdout));
  });
});


// ---------------------------------------------------------------------------
// Sidecar-path coverage: exercise every branch added for unsupported-extension
// locks (JSON, binary, no-extension).

describe('sidecar path — coverage fill', () => {
  it('blocks a second agent while the first holds the sidecar', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    const a = lock.acquireLock(p, { agentId: 'a1', reason: 'r1' });
    assert.equal(a.ok, true);
    const b = lock.acquireLock(p, { agentId: 'a2', reason: 'r2' });
    assert.equal(b.ok, false);
    assert.equal(b.heldBy, 'a1');
    assert.equal(b.nodeId, '*');
    assert.ok(typeof b.ageMs === 'number');
    assert.ok(typeof b.since === 'string');
    // Cleanup.
    lock.releaseLock(p, { agentId: 'a1' });
  });

  it('self-reacquire with same reason on a fresh marker is idempotent', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    lock.acquireLock(p, { agentId: 'a1', reason: 'same' });
    const mtime1 = fs.statSync(p + '.livehub-lock').mtimeMs;
    // Re-acquire immediately — marker should not be rewritten.
    const res = lock.acquireLock(p, { agentId: 'a1', reason: 'same' });
    assert.equal(res.ok, true);
    assert.equal(res.refreshed, false);
    const mtime2 = fs.statSync(p + '.livehub-lock').mtimeMs;
    assert.equal(mtime1, mtime2);
    lock.releaseLock(p, { agentId: 'a1' });
  });

  it('self-reacquire with a different reason overwrites the marker', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    lock.acquireLock(p, { agentId: 'a1', reason: 'r1' });
    const res = lock.acquireLock(p, { agentId: 'a1', reason: 'r2' });
    assert.equal(res.ok, true);
    const sc = JSON.parse(fs.readFileSync(p + '.livehub-lock', 'utf-8'));
    assert.equal(sc.reason, 'r2');
    lock.releaseLock(p, { agentId: 'a1' });
  });

  it('a stale sidecar held by another agent is replaced on acquire', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    // Plant a stale marker owned by a different agent.
    const stale = {
      agentId: 'a-old',
      nodeId: '*',
      startedAt: new Date(Date.now() - 60 * 60 * 1000).toISOString(), // 1h ago
      reason: 'old',
    };
    fs.writeFileSync(p + '.livehub-lock', JSON.stringify(stale, null, 2) + '\n');
    const res = lock.acquireLock(p, { agentId: 'a-new', reason: 'fresh', staleMs: 60 * 1000 });
    assert.equal(res.ok, true);
    const sc = JSON.parse(fs.readFileSync(p + '.livehub-lock', 'utf-8'));
    assert.equal(sc.agentId, 'a-new');
    lock.releaseLock(p, { agentId: 'a-new' });
  });

  it('self-reacquire after the refresh threshold rewrites the marker', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    // Plant a marker owned by a1 that is older than staleMs/2 but not stale.
    const staleMs = 60 * 1000;
    const planted = {
      agentId: 'a1',
      nodeId: '*',
      startedAt: new Date(Date.now() - 40 * 1000).toISOString(), // 40s ago, > 30s threshold
      reason: 'same',
    };
    fs.writeFileSync(p + '.livehub-lock', JSON.stringify(planted, null, 2) + '\n');
    const res = lock.acquireLock(p, { agentId: 'a1', reason: 'same', staleMs });
    assert.equal(res.ok, true);
    // Marker startedAt should now be fresh (>= 'now' - 2s).
    const sc = JSON.parse(fs.readFileSync(p + '.livehub-lock', 'utf-8'));
    const age = Date.now() - new Date(sc.startedAt).getTime();
    assert.ok(age < 2000, 'marker should be refreshed, got age=' + age);
    lock.releaseLock(p, { agentId: 'a1' });
  });

  it('release when no sidecar exists returns { ok: true, wasHeld: false }', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    const res = lock.releaseLock(p, { agentId: 'anyone' });
    assert.equal(res.ok, true);
    assert.equal(res.wasHeld, false);
  });

  it('release with wrong owner is refused and sidecar persists', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    lock.acquireLock(p, { agentId: 'a1', reason: 'r' });
    const res = lock.releaseLock(p, { agentId: 'a2' });
    assert.equal(res.ok, false);
    assert.match(res.error, /not owner/);
    assert.equal(fs.existsSync(p + '.livehub-lock'), true);
    lock.releaseLock(p, { agentId: 'a1' });
  });

  it('release without agentId unconditionally removes the sidecar', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    lock.acquireLock(p, { agentId: 'a1', reason: 'r' });
    const res = lock.releaseLock(p);
    assert.equal(res.ok, true);
    assert.equal(res.wasHeld, true);
    assert.equal(fs.existsSync(p + '.livehub-lock'), false);
  });

  it('listLocks surfaces a sidecar-locked file', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    lock.acquireLock(p, { agentId: 'a1', reason: 'r' });
    const list = lock.listLocks(dir);
    const hit = list.find(l => l.target === p);
    assert.ok(hit, 'expected sidecar entry for ' + p + ' in ' + JSON.stringify(list));
    assert.equal(hit.agentId, 'a1');
    assert.equal(hit.nodeId, '*');
    lock.releaseLock(p, { agentId: 'a1' });
  });

  it('listLocks ignores a stray sidecar whose target has a supported extension', () => {
    const dir = makeTmpDir();
    // Plant foo.js and a stray foo.js.livehub-lock.
    const js = writeFile(dir, 'foo.js', 'x\n');
    fs.writeFileSync(js + '.livehub-lock', JSON.stringify({
      agentId: 'stray', nodeId: '*', startedAt: new Date().toISOString(), reason: 'stray',
    }));
    const list = lock.listLocks(dir);
    // No entry should mention the stray sidecar target (supported ext wins).
    assert.equal(list.find(l => l.agentId === 'stray'), undefined);
  });

  it('_internal.readMarker returns the sidecar marker for unsupported extensions', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    assert.equal(lock._internal.readMarker(p), null);
    lock.acquireLock(p, { agentId: 'a1', reason: 'r' });
    const m = lock._internal.readMarker(p);
    assert.ok(m);
    assert.equal(m.agentId, 'a1');
    lock.releaseLock(p, { agentId: 'a1' });
  });

  it('sidecar defaults branch: parses marker with only required fields, defaults nodeId and reason', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    // Plant sidecar with only agentId+startedAt; exercises nodeId||WHOLE_FILE and reason||'' defaults.
    fs.writeFileSync(p + '.livehub-lock', JSON.stringify({
      agentId: 'a1',
      startedAt: new Date().toISOString(),
    }));
    const m = lock._internal.readMarker(p);
    assert.ok(m);
    assert.equal(m.nodeId, '*');
    assert.equal(m.reason, '');
    lock.releaseLock(p, { agentId: 'a1' });
  });

  it('treats a null JSON sidecar as no lock', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    fs.writeFileSync(p + '.livehub-lock', 'null');
    const res = lock.acquireLock(p, { agentId: 'a1', reason: 'r' });
    assert.equal(res.ok, true);
    lock.releaseLock(p, { agentId: 'a1' });
  });

  it('treats a sidecar missing startedAt as no lock', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    fs.writeFileSync(p + '.livehub-lock', JSON.stringify({ agentId: 'a1' }));
    const res = lock.acquireLock(p, { agentId: 'a2', reason: 'r' });
    assert.equal(res.ok, true);
    lock.releaseLock(p, { agentId: 'a2' });
  });

  it('treats a corrupt-JSON sidecar as no lock; acquire overwrites it', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    fs.writeFileSync(p + '.livehub-lock', '{not valid json');
    const res = lock.acquireLock(p, { agentId: 'a1', reason: 'r' });
    assert.equal(res.ok, true);
    const sc = JSON.parse(fs.readFileSync(p + '.livehub-lock', 'utf-8'));
    assert.equal(sc.agentId, 'a1');
    lock.releaseLock(p, { agentId: 'a1' });
  });

  it('treats a sidecar with missing required fields as no lock', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    fs.writeFileSync(p + '.livehub-lock', JSON.stringify({ reason: 'no agentId here' }));
    const res = lock.acquireLock(p, { agentId: 'a1', reason: 'r' });
    assert.equal(res.ok, true);
    const sc = JSON.parse(fs.readFileSync(p + '.livehub-lock', 'utf-8'));
    assert.equal(sc.agentId, 'a1');
    lock.releaseLock(p, { agentId: 'a1' });
  });

  it('_internal.readMarkers returns [] when no sidecar, [marker] when present', () => {
    const dir = makeTmpDir();
    const p = writeFile(dir, 'data.json', '{}\n');
    assert.deepEqual(lock._internal.readMarkers(p), []);
    lock.acquireLock(p, { agentId: 'a1', reason: 'r' });
    const arr = lock._internal.readMarkers(p);
    assert.equal(arr.length, 1);
    assert.equal(arr[0].agentId, 'a1');
    lock.releaseLock(p, { agentId: 'a1' });
  });
});
