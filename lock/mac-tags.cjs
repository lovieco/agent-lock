// macOS Finder tag helper used by file-lock.cjs so locked files get a red
// "Locked" dot in Finder and respond to `mdfind "tag:Locked"` Spotlight
// searches. Non-fatal — every call is best-effort and must never block a
// lock acquisition (the file-lock module wraps calls in try/catch).
//
// Tags live in the extended attribute `com.apple.metadata:_kMDItemUserTags`
// as a binary plist containing an NSArray of strings. Each entry is either
// the tag name alone (no color) or `"<name>\n<colorIndex>"` (colored).
//
// We use /usr/bin/xattr to read/write the xattr and /usr/bin/plutil to
// convert between binary plist and XML. Both are built-in macOS tools —
// no Homebrew dependency.
//
// Color indices (Finder convention):
//   0 none (grey dot, no fill)
//   1 grey    2 green   3 purple   4 blue
//   5 yellow  6 red     7 orange
//
// All functions are no-ops on non-Darwin platforms.

const { execFileSync } = require('child_process');

const TAG_XATTR = 'com.apple.metadata:_kMDItemUserTags';

// Standard tag used by the file-lock module. Callers can add tags with
// other names, but addLockTag/removeLockTag below are convenience wrappers.
const LOCK_TAG_NAME = 'Locked';
const LOCK_TAG_COLOR = 6; // red

function isMac() {
  return process.platform === 'darwin';
}

// ---------- XML helpers ----------

function encodeXml(s) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}
function decodeXml(s) {
  return s.replace(/&amp;/g, '&').replace(/&lt;/g, '<').replace(/&gt;/g, '>');
}

// ---------- Read ----------

/**
 * Read existing user tags on a file.
 * Returns [] on non-macOS, when no tags are set, or on any parse/error path.
 * Shape: [{ name: string, color: number }, ...]
 */
function readTags(filePath) {
  if (!isMac()) return [];
  let hex;
  try {
    hex = execFileSync('xattr', ['-px', TAG_XATTR, filePath], {
      stdio: ['ignore', 'pipe', 'ignore'],
      encoding: 'utf-8',
    }).replace(/\s+/g, '');
  } catch {
    return []; // xattr absent, file missing, or no tag attr — treat as "no tags"
  }
  if (!hex) return [];

  const bin = Buffer.from(hex, 'hex');
  let xml;
  try {
    xml = execFileSync('plutil', ['-convert', 'xml1', '-o', '-', '-'], {
      input: bin,
      stdio: ['pipe', 'pipe', 'ignore'],
      encoding: 'utf-8',
    });
  } catch {
    return [];
  }

  // XML plist preserves literal newlines inside <string>...</string> content.
  // So "Locked\n6" encodes as <string>Locked
  // 6</string> and we split on \n.
  const out = [];
  const re = /<string>([\s\S]*?)<\/string>/g;
  let m;
  while ((m = re.exec(xml))) {
    const raw = decodeXml(m[1]);
    const parts = raw.split('\n');
    const name = parts[0];
    const color = parts[1] != null ? parseInt(parts[1], 10) : 0;
    if (name) out.push({ name, color: Number.isFinite(color) ? color : 0 });
  }
  return out;
}

// ---------- Write ----------

/**
 * Overwrite the tag xattr with the given tag list.
 * Empty list → delete the xattr entirely (preserves Finder's "no tags" state
 * rather than leaving a 0-entry plist that some tools render as empty-but-present).
 */
function writeTags(filePath, tags) {
  if (!isMac()) return;

  if (!tags || tags.length === 0) {
    try {
      execFileSync('xattr', ['-d', TAG_XATTR, filePath], { stdio: 'ignore' });
    } catch {
      // xattr returns non-zero if the attribute didn't exist — harmless.
    }
    return;
  }

  const entries = tags.map((t) => {
    const s = t.color != null && t.color !== 0
      ? `${t.name}\n${t.color}`
      : t.name;
    return `<string>${encodeXml(s)}</string>`;
  }).join('');

  const xml = '<?xml version="1.0" encoding="UTF-8"?>\n'
    + '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
    + `<plist version="1.0"><array>${entries}</array></plist>`;

  const bin = execFileSync('plutil', ['-convert', 'binary1', '-o', '-', '-'], {
    input: xml,
    stdio: ['pipe', 'pipe', 'ignore'],
  });
  const hex = bin.toString('hex');
  execFileSync('xattr', ['-wx', TAG_XATTR, hex, filePath], { stdio: 'ignore' });
}

// ---------- Add / Remove (idempotent, preserving other tags) ----------

/**
 * Add a tag (idempotent — no-op if the same-name tag already exists).
 * Preserves any other user-added tags on the file.
 */
function addTag(filePath, name, color = 0) {
  if (!isMac()) return;
  const existing = readTags(filePath);
  if (existing.some((t) => t.name === name)) return;
  writeTags(filePath, [...existing, { name, color }]);
}

/**
 * Remove a tag by name. Preserves other tags. No-op if not present.
 */
function removeTag(filePath, name) {
  if (!isMac()) return;
  const existing = readTags(filePath);
  const next = existing.filter((t) => t.name !== name);
  if (next.length === existing.length) return;
  writeTags(filePath, next);
}

// ---------- Convenience wrappers for file-lock ----------

function addLockTag(filePath) {
  return addTag(filePath, LOCK_TAG_NAME, LOCK_TAG_COLOR);
}
function removeLockTag(filePath) {
  return removeTag(filePath, LOCK_TAG_NAME);
}

module.exports = {
  readTags,
  writeTags,
  addTag,
  removeTag,
  addLockTag,
  removeLockTag,
  isMac,
  LOCK_TAG_NAME,
  LOCK_TAG_COLOR,
};
