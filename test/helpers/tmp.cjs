// Shared test helpers: temp directories, temp files, and subprocess runners.
//
// Every test that touches the filesystem should use `makeTmpDir()` so the
// working tree never gets polluted. The dir is registered for automatic
// cleanup via `process.on('exit')`.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const _cleanup = [];
let _exitHooked = false;
function _hookExit() {
  if (_exitHooked) return;
  _exitHooked = true;
  process.on('exit', () => {
    for (const p of _cleanup) {
      try { fs.rmSync(p, { recursive: true, force: true }); } catch {}
    }
  });
}

/** Create a temp directory that is auto-cleaned at process exit. */
function makeTmpDir(prefix = 'agent-lock-test-') {
  _hookExit();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  _cleanup.push(dir);
  return dir;
}

/** Create a file in `dir` with given `contents` and return its absolute path. */
function writeFile(dir, name, contents) {
  const p = path.join(dir, name);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.writeFileSync(p, contents);
  return p;
}

/** Read a file as utf-8. */
function readFile(p) {
  return fs.readFileSync(p, 'utf-8');
}

/** Absolute path to the agent-lock repo root. */
const REPO_ROOT = path.resolve(__dirname, '..', '..');

/** Absolute path to a file inside the repo. */
function repo(...parts) {
  return path.join(REPO_ROOT, ...parts);
}

/**
 * Run a hook script by piping a JSON payload on stdin.
 * Returns { status, stdout, stderr }.
 */
function runHook(scriptPath, payload, { env } = {}) {
  const r = spawnSync('node', [scriptPath], {
    input: typeof payload === 'string' ? payload : JSON.stringify(payload ?? {}),
    encoding: 'utf-8',
    env: { ...process.env, ...(env || {}) },
  });
  return { status: r.status, stdout: r.stdout || '', stderr: r.stderr || '' };
}

/**
 * Run the agent-lock CLI with the given args. Returns { status, stdout, stderr }.
 */
function runCli(args, { cwd, env } = {}) {
  const r = spawnSync('node', [repo('bin/agent-lock'), ...args], {
    cwd: cwd || process.cwd(),
    encoding: 'utf-8',
    env: { ...process.env, ...(env || {}) },
  });
  return { status: r.status, stdout: r.stdout || '', stderr: r.stderr || '' };
}

/**
 * Load a fresh copy of a CommonJS module (clears it from the require cache
 * first so module-level state doesn't bleed between tests).
 */
function freshRequire(modPath) {
  const resolved = require.resolve(modPath);
  delete require.cache[resolved];
  return require(resolved);
}

module.exports = {
  makeTmpDir,
  writeFile,
  readFile,
  repo,
  REPO_ROOT,
  runHook,
  runCli,
  freshRequire,
};
