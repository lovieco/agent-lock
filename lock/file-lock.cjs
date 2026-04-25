#!/usr/bin/env node
// File-lock coordination — INLINE-MARKER MODE with semantic scope.
//
// The lock is a list of comment lines at the top of the file (one per
// active lock). Each marker records { agentId, nodeId, startedAt, reason }.
// No sidecar files — `grep "livehub lock:" <file>` is the source of truth.
//
// Lock scopes:
//   nodeId = "*"            → whole file, dominates any other lock
//   nodeId = "fn:foo", etc. → one top-level declaration; other nodes in
//                             the same file remain free.
//
// Files whose extension has no comment syntax (JSON, binary) fall back to
// a sidecar lock file at `<filePath>.livehub-lock` storing the same marker
// payload as JSON. Scope is whole-file only (no semantic sub-locking for
// JSON/binary). Sidecar semantics mirror inline-marker behavior exactly:
// stale detection, cross-agent collision, and self-reacquire idempotency.

const fs = require('fs');
const path = require('path');
const macTags = require('./mac-tags.cjs');

const DEFAULT_STALE_MS = 10 * 60 * 1000;
const WHOLE_FILE = '*';

const COMMENT_STYLES = {
  '.ts':   { open: '// ',    close: '' },
  '.tsx':  { open: '// ',    close: '' },
  '.js':   { open: '// ',    close: '' },
  '.jsx':  { open: '// ',    close: '' },
  '.cjs':  { open: '// ',    close: '' },
  '.mjs':  { open: '// ',    close: '' },
  '.md':   { open: '<!-- ',  close: ' -->' },
  '.html': { open: '<!-- ',  close: ' -->' },
  '.py':   { open: '# ',     close: '' },
  '.sh':   { open: '# ',     close: '' },
  '.sql':  { open: '-- ',    close: '' },
  '.css':  { open: '/* ',    close: ' */' },
};

function styleFor(filePath) {
  return COMMENT_STYLES[path.extname(filePath).toLowerCase()] || null;
}

const SIDECAR_SUFFIX = '.livehub-lock';
function sidecarPath(filePath) { return filePath + SIDECAR_SUFFIX; }

function readSidecarMarker(filePath) {
  const p = sidecarPath(filePath);
  if (!fs.existsSync(p)) return null;
  try {
    const raw = fs.readFileSync(p, 'utf-8');
    const obj = JSON.parse(raw);
    if (!obj || !obj.agentId || !obj.startedAt) return null;
    return {
      agentId:   obj.agentId,
      nodeId:    obj.nodeId || WHOLE_FILE,
      startedAt: obj.startedAt,
      reason:    obj.reason || '',
    };
  } catch {
    // Corrupt/partial sidecar — treat as no lock; acquire will overwrite.
    return null;
  }
}

function writeSidecarMarker(filePath, info) {
  fs.writeFileSync(sidecarPath(filePath), JSON.stringify(info, null, 2) + '\n', 'utf-8');
}

function removeSidecar(filePath) {
  /* c8 ignore next 2 — race-only catch (sidecar already gone between readSidecarMarker and unlinkSync) */
  try { fs.unlinkSync(sidecarPath(filePath)); } catch {}
}

function finderTagsEnabled() {
  return macTags.isMac() && process.env.CLAUDE_FILE_LOCK_TAG !== '0';
}
/* c8 ignore next 2 — xattr catch branches silence rare OS faults */
function tryAddLockTag(p)    { if (finderTagsEnabled()) try { macTags.addLockTag(p); }    catch {} }
function tryRemoveLockTag(p) { if (finderTagsEnabled()) try { macTags.removeLockTag(p); } catch {} }

// One regex handles every comment style — they all embed the same
// `livehub lock:` key-value payload. Accepts both the short field names
// (A=, N=, @, R=) written by current releases AND the legacy long names
// (agent=, node=, started=, reason=) so markers written by older versions
// still parse until they age out of the tree.
const MARKER_LINE_RE = /livehub lock:\s*(?:A=|agent=)(\S+)\s+(?:N=|node=)(\S+)\s+(?:@|started=)(\S+)\s+(?:R=|reason=)(.*?)(?:\s*-->|\s*\*\/)?\s*$/;

function parseMarkerLine(line) {
  const m = line.match(MARKER_LINE_RE);
  if (!m) return null;
  return { agentId: m[1], nodeId: m[2], startedAt: m[3], reason: m[4].trim() };
}

function formatMarkerLine(style, info) {
  const reason = (info.reason || '').replace(/\n/g, ' ');
  return `${style.open}livehub lock: A=${info.agentId} N=${info.nodeId} @${info.startedAt} R=${reason}${style.close}`;
}

function splitShebang(text) {
  const trailingNL = text.endsWith('\n');
  const body = trailingNL ? text.slice(0, -1) : text;
  if (body.startsWith('#!')) {
    const nl = body.indexOf('\n');
    if (nl >= 0) return { prefix: body.slice(0, nl + 1), rest: body.slice(nl + 1), trailingNL };
    // Shebang with no following newline — entire body IS the shebang. Force
    // a newline after it so marker lines can sit on line 2.
    return { prefix: body + '\n', rest: '', trailingNL };
  }
  return { prefix: '', rest: body, trailingNL };
}

function readState(filePath) {
  if (!fs.existsSync(filePath)) {
    return { markers: [], prefix: '', body: '', trailingNL: false, missing: true };
  }
  const text = fs.readFileSync(filePath, 'utf-8');
  const { prefix, rest, trailingNL } = splitShebang(text);
  const lines = rest.split('\n');
  const markers = [];
  let i = 0;
  while (i < lines.length) {
    const parsed = parseMarkerLine(lines[i]);
    if (!parsed) break;
    markers.push(parsed);
    i++;
  }
  return { markers, prefix, body: lines.slice(i).join('\n'), trailingNL, missing: false };
}

function writeState(filePath, state, style) {
  const headerLines = state.markers.map(m => formatMarkerLine(style, m));
  const header = headerLines.length ? headerLines.join('\n') + '\n' : '';
  const out = state.prefix + header + state.body + (state.trailingNL ? '\n' : '');
  fs.writeFileSync(filePath, out, 'utf-8');
}

function isStale(marker, staleMs) {
  const age = Date.now() - new Date(marker.startedAt).getTime();
  return !isFinite(age) || age >= staleMs;
}

// Serialize read-modify-write on a file across processes via an O_EXCL
// sidecar. The sidecar exists only for the microseconds it takes to
// read-check-write; the DURABLE lock state is still the marker lines
// inside the target file.
const CRIT_WAIT_MS = 2000;
function withCriticalSection(filePath, fn) {
  const critPath = filePath + '.lh-crit';
  const start = Date.now();
  let fd;
  while (true) {
    try {
      fd = fs.openSync(critPath, 'wx');
      break;
    } catch (e) {
      /* c8 ignore next — non-EEXIST errors from openSync are rare OS-level faults */
      if (e.code !== 'EEXIST') throw e;
      /* c8 ignore next 4 — stale-sidecar recovery fires after 2s of EEXIST */
      if (Date.now() - start > CRIT_WAIT_MS) {
        try { fs.unlinkSync(critPath); } catch {}
      }
      // Brief yield. A syscall loop resolves contention in microseconds.
    }
  }
  try { return fn(); }
  finally {
    /* c8 ignore next 2 — fd/unlink catch branches silence rare OS faults */
    try { fs.closeSync(fd); } catch {}
    try { fs.unlinkSync(critPath); } catch {}
  }
}

function acquireLock(filePath, opts = {}) {
  const style = styleFor(filePath);
  const agentId = opts.agentId || 'anonymous-' + process.pid;
  const nodeId  = opts.nodeId  || WHOLE_FILE;
  const reason  = opts.reason  || '';
  const staleMs = opts.staleMs ?? DEFAULT_STALE_MS;

  if (!style) {
    // Unsupported extension — sidecar lock. Target file doesn't need to
    // exist (we're coordinating a Write that will create it).
    return withCriticalSection(filePath, () => _acquireSidecarUnlocked(filePath, agentId, nodeId, reason, staleMs));
  }
  if (!fs.existsSync(filePath)) return { ok: true, supported: false, reason: 'file does not exist yet' };

  return withCriticalSection(filePath, () => _acquireUnlocked(filePath, style, agentId, nodeId, reason, staleMs));
}

function _acquireSidecarUnlocked(filePath, agentId, nodeId, reason, staleMs) {
  // Sidecar scope is whole-file only. Any caller-supplied nodeId is coerced.
  const existing = readSidecarMarker(filePath);

  if (existing) {
    const stale = isStale(existing, staleMs);
    if (!stale && existing.agentId !== agentId) {
      return {
        ok: false,
        heldBy:  existing.agentId,
        since:   existing.startedAt,
        reason:  existing.reason,
        nodeId:  WHOLE_FILE,
        ageMs:   Date.now() - new Date(existing.startedAt).getTime(),
      };
    }
    // Self-reacquire: if same agent holds a fresh marker with the same
    // reason, keep it as-is. Mirrors the inline path's idempotency.
    if (!stale && existing.agentId === agentId && existing.reason === reason) {
      const REFRESH_THRESHOLD_MS = Math.floor(staleMs / 2);
      const age = Date.now() - new Date(existing.startedAt).getTime();
      if (isFinite(age) && age < REFRESH_THRESHOLD_MS) {
        tryAddLockTag(filePath);
        return { ok: true, info: existing, refreshed: false };
      }
    }
    // Stale OR same-agent-different-reason OR refresh-threshold-passed →
    // fall through and overwrite.
  }

  const info = { agentId, nodeId: WHOLE_FILE, startedAt: new Date().toISOString(), reason };
  writeSidecarMarker(filePath, info);
  tryAddLockTag(filePath);
  return { ok: true, info };
}

function _acquireUnlocked(filePath, style, agentId, nodeId, reason, staleMs) {
  const state = readState(filePath);
  state.markers = state.markers.filter(m => !isStale(m, staleMs));

  // Collision rules:
  //   * (us) ↔ anything held by someone else → blocked
  //   us ↔ * (them) → blocked
  //   us ↔ same nodeId held by someone else → blocked
  const blocker = state.markers.find(m => {
    if (m.agentId === agentId) return false;
    if (nodeId === WHOLE_FILE) return true;
    if (m.nodeId === WHOLE_FILE) return true;
    return m.nodeId === nodeId;
  });
  if (blocker) {
    return {
      ok: false,
      heldBy:  blocker.agentId,
      since:   blocker.startedAt,
      reason:  blocker.reason,
      nodeId:  blocker.nodeId,
      ageMs:   Date.now() - new Date(blocker.startedAt).getTime(),
    };
  }

  // Self re-acquire: if the same agent already holds the same node with the
  // same reason AND the marker is still fresh, keep it as-is. Rewriting the
  // startedAt on every edit causes Read→Edit hash-mismatch races for agents
  // editing the file. Refresh only when the marker is older than half the
  // stale window (so it never actually ages out) or the reason changed.
  const REFRESH_THRESHOLD_MS = Math.floor(staleMs / 2);
  const mine = state.markers.find(m => m.agentId === agentId && m.nodeId === nodeId);
  if (mine && mine.reason === reason) {
    const age = Date.now() - new Date(mine.startedAt).getTime();
    if (isFinite(age) && age < REFRESH_THRESHOLD_MS) {
      tryAddLockTag(filePath);
      return { ok: true, info: mine, refreshed: false };
    }
  }
  state.markers = state.markers.filter(m => !(m.agentId === agentId && m.nodeId === nodeId));

  const info = { agentId, nodeId, startedAt: new Date().toISOString(), reason };
  state.markers.push(info);
  writeState(filePath, state, style);
  tryAddLockTag(filePath);
  return { ok: true, info };
}

function releaseLock(filePath, opts = {}) {
  const style = styleFor(filePath);
  if (!style) {
    return withCriticalSection(filePath, () => _releaseSidecarUnlocked(filePath, opts));
  }
  if (!fs.existsSync(filePath)) return { ok: true, wasHeld: false };
  return withCriticalSection(filePath, () => _releaseUnlocked(filePath, style, opts));
}

function _releaseSidecarUnlocked(filePath, opts) {
  const existing = readSidecarMarker(filePath);
  if (!existing) {
    tryRemoveLockTag(filePath);
    return { ok: true, wasHeld: false };
  }
  if (opts.agentId && existing.agentId !== opts.agentId) {
    return { ok: false, error: `not owner (held by ${existing.agentId})` };
  }
  removeSidecar(filePath);
  tryRemoveLockTag(filePath);
  return { ok: true, wasHeld: true };
}

function _releaseUnlocked(filePath, style, opts) {
  const state = readState(filePath);
  if (!state.markers.length) {
    writeState(filePath, state, style); // idempotent normalise
    tryRemoveLockTag(filePath);
    return { ok: true, wasHeld: false };
  }

  // Owner-mismatch check: if caller pins agentId and isn't on the relevant
  // marker, refuse. Scoped by nodeId when specified, else scoped to the
  // whole file (must own at least one marker to touch anything).
  if (opts.agentId) {
    if (opts.nodeId) {
      const onNode = state.markers.find(m => m.nodeId === opts.nodeId);
      if (onNode && onNode.agentId !== opts.agentId) {
        return { ok: false, error: `not owner (held by ${onNode.agentId})` };
      }
    } else {
      const mine = state.markers.some(m => m.agentId === opts.agentId);
      if (!mine) {
        return { ok: false, error: `not owner (held by ${state.markers[0].agentId})` };
      }
    }
  }

  const before = state.markers.length;
  state.markers = state.markers.filter(m => {
    if (opts.agentId && m.agentId !== opts.agentId) return true;
    if (opts.nodeId  && m.nodeId  !== opts.nodeId)  return true;
    return false;
  });
  const wasHeld = state.markers.length < before;
  writeState(filePath, state, style);
  if (state.markers.length === 0) tryRemoveLockTag(filePath);
  return { ok: true, wasHeld };
}

async function withLock(filePath, opts, fn) {
  const lock = acquireLock(filePath, opts);
  if (!lock.ok) {
    /* c8 ignore next — heldBy/since are always populated on real collisions */
    const err = new Error(`Lock held by ${lock.heldBy || 'unknown'} since ${lock.since || 'unknown'} (reason: ${lock.reason || '-'})`);
    err.code = 'ELOCKED';
    err.details = lock;
    throw err;
  }
  try { return await fn(); }
  finally {
    releaseLock(filePath, {
      agentId: opts && opts.agentId,
      nodeId:  opts && opts.nodeId,
    });
  }
}

function listLocks(dir) {
  const out = [];
  const walk = (d) => {
    let entries;
    try { entries = fs.readdirSync(d, { withFileTypes: true }); }
    catch { return; }
    for (const ent of entries) {
      const full = path.join(d, ent.name);
      if (ent.isDirectory()) {
        if (['node_modules', '.git', '.next'].includes(ent.name)) continue;
        if (ent.name === 'worktrees' && path.basename(d) === '.claude') continue;
        walk(full);
      } else if (ent.name.endsWith(SIDECAR_SUFFIX)) {
        // Sidecar entry — target is the real file minus the suffix. Only
        // surface it if styleFor(target) is null; otherwise skip to avoid
        // double-counting (inline path wins for supported extensions).
        const target = full.slice(0, -SIDECAR_SUFFIX.length);
        if (styleFor(target)) continue;
        const m = readSidecarMarker(target);
        if (m) out.push({ target, ...m });
      } else if (styleFor(full)) {
        const { markers } = readState(full);
        for (const m of markers) out.push({ target: full, ...m });
      }
    }
  };
  walk(dir);
  return out;
}

module.exports = {
  acquireLock, releaseLock, withLock, listLocks,
  WHOLE_FILE,
  _internal: (() => {
    // Unified readMarker — uses inline markers for supported extensions,
    // sidecar for everything else. Same shape either way.
    const readMarker = (filePath) => {
      if (styleFor(filePath)) {
        const { markers } = readState(filePath);
        return markers[0] || null;
      }
      return readSidecarMarker(filePath);
    };
    const readMarkers = (filePath) => {
      if (styleFor(filePath)) return readState(filePath).markers;
      const m = readSidecarMarker(filePath);
      return m ? [m] : [];
    };
    // lockPath: null for supported extensions (inline marker is authoritative),
    // sidecar path for unsupported ones. Note: the sidecar is the marker
    // store, not the `.lh-crit` critical-section file.
    const lockPath = (filePath) => styleFor(filePath) ? null : sidecarPath(filePath);
    return {
      readState,
      parseMarkerLine,
      formatMarkerLine,
      readMarker,                                   // first marker or null
      readMarkers,
      readSidecar: readMarker,                      // back-compat alias
      lockPath,
    };
  })(),
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
    const { markers } = readState(arg);
    console.log(markers.length ? markers : null);
  } else {
    console.log('Usage:  file-lock.cjs list | release <file> | show <file>');
  }
}
