#!/usr/bin/env node
// File-lock coordination — CENTRAL LOCKDIR MODE.
//
// Each lock is a single JSON file under <project>/.agent-lock/locks/<sha1>.json
// containing { path, agentId, nodeId, startedAt, ttlMs, reason }. The
// project tree itself is never modified. Acquire is `openSync(..., 'wx')`
// — atomic O_EXCL creation. Release is `unlinkSync`. List is `readdirSync`.
//
// Stale: a lock whose startedAt is invalid, in the future, or older than
// ttlMs is treated as not held. Future-timestamps cannot pin the lock.

const fs = require('fs');
const path = require('path');
const crypto = require('crypto');

const DEFAULT_STALE_MS = 10 * 60 * 1000;
const WHOLE_FILE = '*';
const LOCKDIR_NAME = '.agent-lock/locks';

function findProjectRoot(filePath) {
  let d = path.dirname(path.resolve(filePath));
  while (true) {
    if (fs.existsSync(path.join(d, '.agent-lock'))) return d;
    const parent = path.dirname(d);
    if (parent === d) return path.dirname(path.resolve(filePath));
    d = parent;
  }
}

function lockDirFor(filePath, opts = {}) {
  const root = opts.root || findProjectRoot(filePath);
  return path.join(root, LOCKDIR_NAME);
}

function lockFileFor(filePath, opts = {}) {
  const abs = path.resolve(filePath);
  const hash = crypto.createHash('sha1').update(abs).digest('hex').slice(0, 16);
  return path.join(lockDirFor(filePath, opts), hash + '.json');
}

function isStale(marker, staleMs) {
  if (!marker || typeof marker.startedAt !== 'string') return true;
  const t = new Date(marker.startedAt).getTime();
  if (!Number.isFinite(t) || t <= 0) return true;
  const now = Date.now();
  if (t > now) return true;
  return now - t >= staleMs;
}

function readLockfile(p) {
  try {
    const raw = fs.readFileSync(p, 'utf-8');
    const obj = JSON.parse(raw);
    if (!obj || typeof obj.agentId !== 'string') return null;
    return obj;
  } catch {
    return null;
  }
}

function writeLockfileExcl(p, info) {
  fs.mkdirSync(path.dirname(p), { recursive: true });
  const fd = fs.openSync(p, 'wx');
  try { fs.writeSync(fd, JSON.stringify(info, null, 2) + '\n'); }
  finally { fs.closeSync(fd); }
}

function acquireLock(filePath, opts = {}) {
  const agentId = opts.agentId || 'anonymous-' + process.pid;
  const nodeId  = opts.nodeId  || WHOLE_FILE;
  const reason  = opts.reason  || '';
  const staleMs = opts.staleMs ?? DEFAULT_STALE_MS;
  const p = lockFileFor(filePath, opts);
  const info = {
    path: path.resolve(filePath),
    agentId, nodeId,
    startedAt: new Date().toISOString(),
    ttlMs: staleMs,
    reason,
  };

  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      writeLockfileExcl(p, info);
      return { ok: true, info };
    } catch (e) {
      if (e.code !== 'EEXIST') throw e;
      const existing = readLockfile(p);
      if (!existing || isStale(existing, staleMs)) {
        try { fs.unlinkSync(p); } catch {}
        continue;
      }
      if (existing.agentId === agentId) {
        return { ok: true, info: existing, refreshed: false };
      }
      return {
        ok: false,
        heldBy: existing.agentId,
        since:  existing.startedAt,
        reason: existing.reason,
        nodeId: existing.nodeId,
        ageMs:  Date.now() - new Date(existing.startedAt).getTime(),
      };
    }
  }
  const existing = readLockfile(p) || {};
  return {
    ok: false,
    heldBy: existing.agentId || 'unknown',
    since:  existing.startedAt || new Date().toISOString(),
    reason: existing.reason || '',
    nodeId: existing.nodeId || WHOLE_FILE,
    ageMs:  0,
  };
}

function releaseLock(filePath, opts = {}) {
  const p = lockFileFor(filePath, opts);
  const existing = readLockfile(p);
  if (!existing) return { ok: true, wasHeld: false };
  if (opts.agentId && existing.agentId !== opts.agentId) {
    return { ok: false, error: `not owner (held by ${existing.agentId})` };
  }
  try { fs.unlinkSync(p); } catch {}
  return { ok: true, wasHeld: true };
}

function listLocks(dir) {
  const lockDir = path.join(path.resolve(dir), LOCKDIR_NAME);
  if (!fs.existsSync(lockDir)) return [];
  const out = [];
  for (const ent of fs.readdirSync(lockDir)) {
    if (!ent.endsWith('.json')) continue;
    const obj = readLockfile(path.join(lockDir, ent));
    if (!obj || typeof obj.path !== 'string') continue;
    out.push({
      target:    obj.path,
      agentId:   obj.agentId,
      nodeId:    obj.nodeId || WHOLE_FILE,
      startedAt: obj.startedAt,
      reason:    obj.reason || '',
    });
  }
  return out;
}

async function withLock(filePath, opts, fn) {
  const lock = acquireLock(filePath, opts);
  if (!lock.ok) {
    const err = new Error(`Lock held by ${lock.heldBy} since ${lock.since} (reason: ${lock.reason || '-'})`);
    err.code = 'ELOCKED';
    err.details = lock;
    throw err;
  }
  try { return await fn(); }
  finally {
    releaseLock(filePath, { agentId: opts && opts.agentId, root: opts && opts.root });
  }
}

module.exports = {
  acquireLock, releaseLock, withLock, listLocks,
  WHOLE_FILE,
  _internal: { lockFileFor, lockDirFor, readLockfile, isStale, findProjectRoot },
};

if (require.main === module) {
  const [cmd, arg] = process.argv.slice(2);
  if (cmd === 'list') {
    const root = arg || path.join(__dirname, '..', '..');
    const all = listLocks(root);
    if (!all.length) console.log('(no active locks)');
    else for (const l of all) console.log(`${l.target}  agent=${l.agentId}  node=${l.nodeId}  since=${l.startedAt}  reason=${l.reason}`);
  } else if (cmd === 'release' && arg) {
    console.log(releaseLock(arg));
  } else if (cmd === 'show' && arg) {
    console.log(readLockfile(lockFileFor(arg)));
  } else {
    console.log('Usage:  agent-lock.cjs list | release <file> | show <file>');
  }
}
