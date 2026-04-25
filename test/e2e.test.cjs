'use strict';

// End-to-end tests exercising the full livehub CLI + hooks surface as it
// would actually be used by two or more Claude Code sessions collaborating
// on the same project. Every test installs into a fresh tmp dir, runs a
// realistic sequence of PreToolUse / PostToolUse / SessionEnd payloads,
// and asserts the combined effect on markers, settings.json, and the
// CLI's own output.
//
// macOS Finder-tag xattrs are disabled globally so these tests are
// platform-neutral.

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
  runCli,
  REPO_ROOT,
} = require('./helpers/tmp.cjs');

const PRE   = repo('hooks/file-lock-pre.mjs');
const POST  = repo('hooks/file-lock-post.mjs');
const PURGE = repo('hooks/file-lock-purge.mjs');

// ---------------------------------------------------------------------------
// Shared helpers — used across every describe block below.
// ---------------------------------------------------------------------------

function envBase(extra = {}) {
  return { ...process.env, CLAUDE_FILE_LOCK_TAG: '0', ...extra };
}

/** Install livehub into a fresh tmp dir and return its path. */
function freshProject() {
  const dir = makeTmpDir();
  const r = runCli(['install', dir], { env: envBase() });
  assert.equal(r.status, 0, r.stderr);
  return dir;
}

/** Load the copied file-lock.cjs from the project's .livehub/lock. */
function loadLock(projectDir) {
  const p = path.join(projectDir, '.livehub', 'lock', 'file-lock.cjs');
  delete require.cache[require.resolve(p)];
  return require(p);
}

function readSettings(target) {
  return JSON.parse(readFile(path.join(target, '.claude/settings.json')));
}

/** Count the number of `livehub lock:` marker lines present in a file. */
function countMarkers(filePath) {
  return (readFile(filePath).match(/livehub lock:/g) || []).length;
}

/** Rewrite the `@` timestamp of the marker on the nodeId (or first). */
function rewriteStartedOnNode(filePath, nodeId, iso) {
  const text = readFile(filePath);
  const lines = text.split('\n');
  let replaced = false;
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].includes('livehub lock:') && (!nodeId || lines[i].includes(`N=${nodeId}`))) {
      lines[i] = lines[i].replace(/@\S+/, `@${iso}`);
      replaced = true;
      break;
    }
  }
  assert.ok(replaced, `no marker to rewrite on ${filePath} (nodeId=${nodeId || 'any'})`);
  fs.writeFileSync(filePath, lines.join('\n'));
}

const FIXTURE = `
function alpha() {
  return 1;
}

function beta() {
  return 2;
}

const gamma = () => {
  return 3;
};
`.trimStart();

// ---------------------------------------------------------------------------
// multi-agent — happy paths
// ---------------------------------------------------------------------------

describe('multi-agent — happy paths', () => {
  it('two agents, two different functions, clean end-state', () => {
    const tmp = freshProject();
    const file = writeFile(tmp, 'src/foo.js', FIXTURE);

    // sess-A acquires fn:alpha
    const a = runHook(PRE, {
      session_id: 'aaaaaaaa-sessA',
      tool_name: 'Edit',
      tool_input: { file_path: file, old_string: 'return 1;' },
    }, { env: envBase() });
    assert.equal(a.status, 0, a.stderr);

    // sess-B acquires fn:beta
    const b = runHook(PRE, {
      session_id: 'bbbbbbbb-sessB',
      tool_name: 'Edit',
      tool_input: { file_path: file, old_string: 'return 2;' },
    }, { env: envBase() });
    assert.equal(b.status, 0, b.stderr);

    // Two marker lines at the top.
    assert.equal(countMarkers(file), 2, readFile(file));
    const txt = readFile(file);
    assert.match(txt, /N=fn:alpha/);
    assert.match(txt, /N=fn:beta/);

    // livehub list reports both locks. The list output doesn't embed the
    // nodeId, but both agent ids should appear, and the count is 2.
    const listOut = runCli(['list', tmp], { env: envBase() });
    assert.equal(listOut.status, 0, listOut.stderr);
    assert.ok(listOut.stdout.includes('active locks: 2'), listOut.stdout);
    assert.ok(listOut.stdout.includes('claude-code-sess-aaaaaaaa'), listOut.stdout);
    assert.ok(listOut.stdout.includes('claude-code-sess-bbbbbbbb'), listOut.stdout);

    // sess-A releases — one marker remains (fn:beta).
    const postA = runHook(POST, {
      session_id: 'aaaaaaaa-sessA',
      tool_name: 'Edit',
      tool_input: { file_path: file },
    }, { env: envBase() });
    assert.equal(postA.status, 0, postA.stderr);
    assert.equal(countMarkers(file), 1, readFile(file));
    assert.match(readFile(file), /N=fn:beta/);

    // sess-B releases — zero markers, file restored.
    const postB = runHook(POST, {
      session_id: 'bbbbbbbb-sessB',
      tool_name: 'Edit',
      tool_input: { file_path: file },
    }, { env: envBase() });
    assert.equal(postB.status, 0, postB.stderr);
    assert.equal(countMarkers(file), 0);
    assert.equal(readFile(file), FIXTURE);
  });

  it('whole-file Write is blocked while a per-node lock is held', () => {
    const tmp = freshProject();
    const file = writeFile(tmp, 'src/foo.js', FIXTURE);

    // sess-A holds fn:alpha.
    const a = runHook(PRE, {
      session_id: 'aaaaaaaa-sessA',
      tool_name: 'Edit',
      tool_input: { file_path: file, old_string: 'return 1;' },
    }, { env: envBase() });
    assert.equal(a.status, 0);

    // sess-B tries a whole-file Write → blocked.
    const b1 = runHook(PRE, {
      session_id: 'bbbbbbbb-sessB',
      tool_name: 'Write',
      tool_input: { file_path: file },
    }, { env: envBase() });
    assert.equal(b1.status, 2);
    assert.match(b1.stderr, /fn:alpha/);
    assert.match(b1.stderr, /BLOCKED/);

    // sess-A releases — file clean, no markers.
    const postA = runHook(POST, {
      session_id: 'aaaaaaaa-sessA',
      tool_name: 'Edit',
      tool_input: { file_path: file },
    }, { env: envBase() });
    assert.equal(postA.status, 0);
    assert.equal(countMarkers(file), 0);

    // sess-B retries the Write → now succeeds.
    const b2 = runHook(PRE, {
      session_id: 'bbbbbbbb-sessB',
      tool_name: 'Write',
      tool_input: { file_path: file },
    }, { env: envBase() });
    assert.equal(b2.status, 0, b2.stderr);

    const postB = runHook(POST, {
      session_id: 'bbbbbbbb-sessB',
      tool_name: 'Write',
      tool_input: { file_path: file },
    }, { env: envBase() });
    assert.equal(postB.status, 0);
    assert.equal(countMarkers(file), 0);
  });

  it('same-node collision then recovery after the holder releases', () => {
    const tmp = freshProject();
    const file = writeFile(tmp, 'src/foo.js', FIXTURE);

    const a = runHook(PRE, {
      session_id: 'aaaaaaaa-sessA',
      tool_name: 'Edit',
      tool_input: { file_path: file, old_string: 'return 1;' },
    }, { env: envBase() });
    assert.equal(a.status, 0);

    // sess-B collides on fn:alpha.
    const b1 = runHook(PRE, {
      session_id: 'bbbbbbbb-sessB',
      tool_name: 'Edit',
      tool_input: { file_path: file, old_string: 'return 1;' },
    }, { env: envBase() });
    assert.equal(b1.status, 2);
    assert.match(b1.stderr, /BLOCKED/);

    const postA = runHook(POST, {
      session_id: 'aaaaaaaa-sessA',
      tool_name: 'Edit',
      tool_input: { file_path: file },
    }, { env: envBase() });
    assert.equal(postA.status, 0);
    assert.equal(countMarkers(file), 0);

    // sess-B retries the same node → now succeeds.
    const b2 = runHook(PRE, {
      session_id: 'bbbbbbbb-sessB',
      tool_name: 'Edit',
      tool_input: { file_path: file, old_string: 'return 1;' },
    }, { env: envBase() });
    assert.equal(b2.status, 0, b2.stderr);
    assert.match(readFile(file), /A=claude-code-sess-bbbbbbbb.*N=fn:alpha/);

    const postB = runHook(POST, {
      session_id: 'bbbbbbbb-sessB',
      tool_name: 'Edit',
      tool_input: { file_path: file },
    }, { env: envBase() });
    assert.equal(postB.status, 0);
    assert.equal(countMarkers(file), 0);
  });

  it('many sequential acquire/release cycles across many files', () => {
    const tmp = freshProject();
    const files = [];
    for (let i = 0; i < 10; i++) {
      const f = writeFile(tmp, `src/m${i}.js`, FIXTURE);
      files.push(f);
      const pre = runHook(PRE, {
        session_id: `sess-${String(i).padStart(4, '0')}`,
        tool_name: 'Edit',
        tool_input: { file_path: f, old_string: 'return 1;' },
      }, { env: envBase() });
      assert.equal(pre.status, 0, pre.stderr);
      const post = runHook(POST, {
        session_id: `sess-${String(i).padStart(4, '0')}`,
        tool_name: 'Edit',
        tool_input: { file_path: f },
      }, { env: envBase() });
      assert.equal(post.status, 0, post.stderr);
    }

    for (const f of files) {
      assert.equal(countMarkers(f), 0, `leftover marker in ${f}`);
      assert.equal(readFile(f), FIXTURE);
    }

    const listOut = runCli(['list', tmp], { env: envBase() });
    assert.equal(listOut.status, 0, listOut.stderr);
    assert.ok(listOut.stdout.includes('(no active locks)'), listOut.stdout);
  });

  it('whole-file lock lands on a non-JS file via the # comment style', () => {
    const tmp = freshProject();
    const orig = '#!/bin/sh\necho hi\n';
    const file = writeFile(tmp, 'scripts/deploy.sh', orig);

    const pre = runHook(PRE, {
      session_id: 'shshshsh',
      tool_name: 'Write',
      tool_input: { file_path: file },
    }, { env: envBase() });
    assert.equal(pre.status, 0, pre.stderr);

    // .sh uses "# livehub lock: ..." — and the shebang stays on line 1.
    const withLock = readFile(file);
    assert.match(withLock, /^#!\/bin\/sh\n# livehub lock:/);
    assert.equal(countMarkers(file), 1);

    const post = runHook(POST, {
      session_id: 'shshshsh',
      tool_name: 'Write',
      tool_input: { file_path: file },
    }, { env: envBase() });
    assert.equal(post.status, 0);
    assert.equal(readFile(file), orig, 'file restored to original after release');
  });
});

// ---------------------------------------------------------------------------
// multi-agent — collisions and recovery
// ---------------------------------------------------------------------------

describe('multi-agent — collisions and recovery', () => {
  it('SessionEnd purge cleans up a crashed session\'s stale marker', () => {
    const tmp = freshProject();
    const file = writeFile(tmp, 'src/foo.js', FIXTURE);

    // sess-A acquires fn:alpha — and then "crashes" (no PostToolUse).
    const a = runHook(PRE, {
      session_id: 'crashaaa',
      tool_name: 'Edit',
      tool_input: { file_path: file, old_string: 'return 1;' },
    }, { env: envBase() });
    assert.equal(a.status, 0);
    assert.equal(countMarkers(file), 1);

    // Backdate the marker so the purge treats it as stale.
    const stale = new Date(Date.now() - 20 * 60 * 1000).toISOString();
    rewriteStartedOnNode(file, 'fn:alpha', stale);

    const r = runHook(PURGE, {}, { env: envBase({ CLAUDE_PROJECT_DIR: tmp }) });
    assert.equal(r.status, 0, r.stderr);
    assert.match(r.stderr, /purged 1 stale lock\(s\)/);
    assert.equal(countMarkers(file), 0);
  });

  it('stale and fresh locks are handled differently by SessionEnd purge', () => {
    const tmp = freshProject();
    const f1 = writeFile(tmp, 'src/one.js', FIXTURE);
    const f2 = writeFile(tmp, 'src/two.js', FIXTURE);

    // Acquire a lock on each file.
    assert.equal(runHook(PRE, {
      session_id: 'aaaaaaaa-one',
      tool_name: 'Edit',
      tool_input: { file_path: f1, old_string: 'return 1;' },
    }, { env: envBase() }).status, 0);

    assert.equal(runHook(PRE, {
      session_id: 'bbbbbbbb-two',
      tool_name: 'Edit',
      tool_input: { file_path: f2, old_string: 'return 2;' },
    }, { env: envBase() }).status, 0);

    // Flip f1 to stale, leave f2 fresh.
    rewriteStartedOnNode(f1, 'fn:alpha', new Date(Date.now() - 20 * 60 * 1000).toISOString());

    const r = runHook(PURGE, {}, { env: envBase({ CLAUDE_PROJECT_DIR: tmp }) });
    assert.equal(r.status, 0, r.stderr);
    assert.match(r.stderr, /purged 1 stale lock\(s\)/);
    assert.equal(countMarkers(f1), 0, 'stale lock gone');
    assert.equal(countMarkers(f2), 1, 'fresh lock still here');
  });
});

// ---------------------------------------------------------------------------
// CLI state — status / list / release / purge-all
// ---------------------------------------------------------------------------

describe('CLI state — status/list/release/purge-all', () => {
  it('status reflects live locks after acquire via the copied module', () => {
    const tmp = freshProject();
    const file = writeFile(tmp, 'src/foo.js', FIXTURE);
    const lock = loadLock(tmp);
    const res = lock.acquireLock(file, {
      agentId: 'claude-code-sess-statusZZ',
      nodeId: 'fn:alpha',
      reason: 'status test',
    });
    assert.equal(res.ok, true);

    const r = runCli(['status', tmp], { env: envBase() });
    assert.equal(r.status, 0, r.stderr);
    assert.ok(r.stdout.includes('files installed     : yes'), r.stdout);
    assert.ok(r.stdout.includes('settings.json wired : yes'), r.stdout);
    assert.ok(r.stdout.includes('active locks: 1'), r.stdout);
    assert.ok(r.stdout.includes('claude-code-sess-statusZZ'), r.stdout);

    // Sanity: the nodeId made it into the actual marker, even though list
    // itself doesn't render it.
    assert.match(readFile(file), /N=fn:alpha/);

    lock.releaseLock(file, {});
  });

  it('release resolves a relative path when run from inside the project', () => {
    const tmp = freshProject();
    const file = writeFile(tmp, 'src/foo.js', FIXTURE);
    const lock = loadLock(tmp);
    assert.equal(lock.acquireLock(file, { agentId: 'claude-code-sess-relXX' }).ok, true);
    assert.equal(countMarkers(file), 1);

    const r = runCli(['release', 'src/foo.js'], { cwd: tmp, env: envBase() });
    assert.equal(r.status, 0, r.stderr);
    assert.equal(countMarkers(file), 0);
  });

  it('purge-all nukes both stale and fresh locks regardless of age', () => {
    const tmp = freshProject();
    const f1 = writeFile(tmp, 'src/stale.js', FIXTURE);
    const f2 = writeFile(tmp, 'src/fresh.js', FIXTURE);
    const lock = loadLock(tmp);
    assert.equal(lock.acquireLock(f1, { agentId: 'stale-agent' }).ok, true);
    assert.equal(lock.acquireLock(f2, { agentId: 'fresh-agent' }).ok, true);

    rewriteStartedOnNode(f1, null, new Date(Date.now() - 20 * 60 * 1000).toISOString());

    const r = runCli(['purge-all', tmp], { env: envBase() });
    assert.equal(r.status, 0, r.stderr);
    assert.ok(r.stdout.includes('purged 2 lock(s)'), r.stdout);
    assert.equal(countMarkers(f1), 0);
    assert.equal(countMarkers(f2), 0);
  });
});

// ---------------------------------------------------------------------------
// install / uninstall edge cases
// ---------------------------------------------------------------------------

describe('install/uninstall edge cases', () => {
  it('uninstall leaves any in-file markers behind (documented behavior)', () => {
    const tmp = freshProject();
    const file = writeFile(tmp, 'src/foo.js', FIXTURE);
    const lock = loadLock(tmp);
    assert.equal(lock.acquireLock(file, { agentId: 'pre-uninstall' }).ok, true);
    assert.equal(countMarkers(file), 1);

    const r = runCli(['uninstall', tmp], { env: envBase() });
    assert.equal(r.status, 0, r.stderr);
    assert.equal(fs.existsSync(path.join(tmp, '.livehub')), false);
    assert.equal(fs.existsSync(path.join(tmp, '.claude/skills/file-lock')), false);
    const s = readSettings(tmp);
    const anyLivehub = ['PreToolUse', 'PostToolUse', 'SessionEnd']
      .flatMap(e => (s.hooks?.[e] || []).flatMap(g => g.hooks || []))
      .some(h => typeof h.command === 'string' && h.command.includes('.livehub/hooks/file-lock-'));
    assert.equal(anyLivehub, false);

    // Uninstall does NOT clean markers — the user can `purge-all` first if
    // they want a pristine tree.
    assert.equal(countMarkers(file), 1, 'marker survives uninstall');
  });

  it('re-installing over an existing install does not duplicate hook entries', () => {
    const tmp = freshProject();
    const r = runCli(['install', tmp], { env: envBase() });
    assert.equal(r.status, 0, r.stderr);

    const s = readSettings(tmp);
    const countByScript = (event, scriptName) =>
      (s.hooks?.[event] || [])
        .flatMap(g => g.hooks || [])
        .filter(h => typeof h.command === 'string' && h.command.includes(scriptName))
        .length;
    assert.equal(countByScript('PreToolUse', 'file-lock-pre.mjs'), 1);
    assert.equal(countByScript('PostToolUse', 'file-lock-post.mjs'), 1);
    assert.equal(countByScript('SessionEnd', 'file-lock-purge.mjs'), 1);
  });

  it('install preserves unrelated env vars already present in settings.json', () => {
    const tmp = makeTmpDir();
    const settingsPath = path.join(tmp, '.claude/settings.json');
    fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
    fs.writeFileSync(settingsPath, JSON.stringify({ env: { MY_VAR: 'hello' } }, null, 2));

    const r = runCli(['install', tmp], { env: envBase() });
    assert.equal(r.status, 0, r.stderr);
    const s = readSettings(tmp);
    assert.equal(s.env?.MY_VAR, 'hello');
  });
});

// ---------------------------------------------------------------------------
// kill switch end-to-end
// ---------------------------------------------------------------------------

describe('kill switch end-to-end', () => {
  it('`livehub off` persists the kill switch and the pre-hook respects it', () => {
    const tmp = freshProject();
    const file = writeFile(tmp, 'src/foo.js', FIXTURE);

    const off = runCli(['off', tmp], { env: envBase() });
    assert.equal(off.status, 0, off.stderr);
    assert.equal(readSettings(tmp).env.CLAUDE_FILE_LOCK, '0');

    // Simulate Claude Code reading settings.env and inheriting it into the hook.
    const before = readFile(file);
    const pre = runHook(PRE, {
      session_id: 'killswit',
      tool_name: 'Write',
      tool_input: { file_path: file },
    }, { env: envBase({ CLAUDE_FILE_LOCK: '0' }) });
    assert.equal(pre.status, 0);
    assert.equal(readFile(file), before, 'kill-switched hook must not write a marker');

    // Flip it back on.
    const on = runCli(['on', tmp], { env: envBase() });
    assert.equal(on.status, 0, on.stderr);
    const s = readSettings(tmp);
    assert.ok(!('CLAUDE_FILE_LOCK' in (s.env || {})), `env=${JSON.stringify(s.env)}`);
  });

  it('live-env kill switch works even when not persisted to settings.json', () => {
    const tmp = freshProject();
    const file = writeFile(tmp, 'src/foo.js', FIXTURE);
    const settingsBefore = readFile(path.join(tmp, '.claude/settings.json'));
    const fileBefore = readFile(file);

    const pre = runHook(PRE, {
      session_id: 'livekill',
      tool_name: 'Write',
      tool_input: { file_path: file },
    }, { env: envBase({ CLAUDE_FILE_LOCK: '0' }) });
    assert.equal(pre.status, 0);

    // Neither the file nor settings.json changed.
    assert.equal(readFile(file), fileBefore, 'file untouched by kill-switched hook');
    assert.equal(readFile(path.join(tmp, '.claude/settings.json')), settingsBefore,
      'settings.json untouched by a pure env-var kill switch');
  });

  it('`livehub test` smoke test passes end-to-end right after install', () => {
    const tmp = freshProject();
    const r = runCli(['test', tmp], { env: envBase() });
    assert.equal(r.status, 0, r.stderr);
    assert.ok(r.stdout.includes('semantic:'), r.stdout);
    assert.ok(r.stdout.includes('markers stripped'), r.stdout);
    const okCount = r.stdout.split('\n').filter(l => /\bOK\b/.test(l)).length;
    assert.ok(okCount >= 6, `expected >=6 OK lines, got ${okCount}:\n${r.stdout}`);
    assert.ok(!r.stdout.includes('FAIL'), `unexpected FAIL in output:\n${r.stdout}`);
  });
});
