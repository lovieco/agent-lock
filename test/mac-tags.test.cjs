// Comprehensive test suite for lock/mac-tags.cjs.
//
// Goal: 100% line/branch/function coverage of the module.
//
// Strategy:
//   * One platform-agnostic describe block verifies the exported surface.
//   * Mac-only describe blocks (`{ skip: process.platform !== 'darwin' }`)
//     exercise the real xattr/plutil round trip against throwaway tmp files.
//   * Non-mac branches are covered by stubbing `process.platform` to 'linux'.

const { describe, it, before, after } = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const fs = require('node:fs');
const { spawnSync, execFileSync } = require('node:child_process');

const { makeTmpDir, writeFile } = require(path.resolve(__dirname, 'helpers', 'tmp.cjs'));
const tags = require(path.resolve(__dirname, '..', 'lock', 'mac-tags.cjs'));

const TAG_XATTR = 'com.apple.metadata:_kMDItemUserTags';

// ---------- helpers ----------

function listXattrs(filePath) {
  const r = spawnSync('/usr/bin/xattr', ['-l', filePath], { encoding: 'utf-8' });
  return (r.stdout || '') + (r.stderr || '');
}

function hasTagXattr(filePath) {
  return listXattrs(filePath).includes(TAG_XATTR);
}

/**
 * Seed a file's tag xattr directly via `xattr -wx` with an arbitrary hex blob.
 * Lets us exercise the "corrupted plist" branch of readTags.
 */
function seedXattrHex(filePath, hex) {
  execFileSync('/usr/bin/xattr', ['-wx', TAG_XATTR, hex, filePath]);
}

/**
 * Seed a file with a hand-crafted XML plist (we convert to binary1 ourselves).
 * Used for the "bare <string> entry with no color" branch.
 */
function seedXattrFromXml(filePath, xml) {
  const bin = execFileSync('/usr/bin/plutil', ['-convert', 'binary1', '-o', '-', '-'], {
    input: xml,
  });
  const hex = bin.toString('hex');
  seedXattrHex(filePath, hex);
}

// =============================================================================
// Platform-agnostic surface
// =============================================================================

describe('mac-tags: exported surface', () => {
  it('exports isMac() reflecting process.platform', () => {
    assert.equal(tags.isMac(), process.platform === 'darwin');
  });

  it('exports LOCK_TAG_NAME = "Locked"', () => {
    assert.equal(tags.LOCK_TAG_NAME, 'Locked');
  });

  it('exports LOCK_TAG_COLOR = 6', () => {
    assert.equal(tags.LOCK_TAG_COLOR, 6);
  });

  it('exports the expected functions', () => {
    for (const fn of ['readTags', 'writeTags', 'addTag', 'removeTag', 'addLockTag', 'removeLockTag', 'isMac']) {
      assert.equal(typeof tags[fn], 'function', `${fn} should be a function`);
    }
  });
});

// =============================================================================
// Mac-only integration tests
// =============================================================================

describe('mac-tags: macOS integration', { skip: process.platform !== 'darwin' }, () => {
  let tmp;

  before(() => {
    tmp = makeTmpDir('mac-tags-');
  });

  it('readTags returns [] on a file with no tags', () => {
    const f = writeFile(tmp, 'no-tags.txt', 'hello');
    assert.deepEqual(tags.readTags(f), []);
  });

  it('readTags returns [] on a nonexistent file (errors swallowed)', () => {
    const f = path.join(tmp, 'does-not-exist.txt');
    assert.deepEqual(tags.readTags(f), []);
  });

  it('addTag adds an uncolored tag and readTags returns color 0', () => {
    const f = writeFile(tmp, 'home-tag.txt', 'x');
    tags.addTag(f, 'Home');
    assert.deepEqual(tags.readTags(f), [{ name: 'Home', color: 0 }]);
  });

  it('addTag is idempotent (no duplicate entries)', () => {
    const f = writeFile(tmp, 'dup.txt', 'x');
    tags.addTag(f, 'Home');
    tags.addTag(f, 'Home');
    tags.addTag(f, 'Home');
    assert.deepEqual(tags.readTags(f), [{ name: 'Home', color: 0 }]);
  });

  it('addTag with an explicit color round-trips correctly', () => {
    const f = writeFile(tmp, 'colored.txt', 'x');
    tags.addTag(f, 'Important', 6);
    assert.deepEqual(tags.readTags(f), [{ name: 'Important', color: 6 }]);
  });

  it('addTag preserves existing tags', () => {
    const f = writeFile(tmp, 'preserve-add.txt', 'x');
    tags.addTag(f, 'First', 2);
    tags.addTag(f, 'Second', 4);
    const got = tags.readTags(f);
    assert.deepEqual(got, [
      { name: 'First', color: 2 },
      { name: 'Second', color: 4 },
    ]);
  });

  it('readTags preserves the order of multiple tags', () => {
    const f = writeFile(tmp, 'order.txt', 'x');
    tags.writeTags(f, [
      { name: 'Alpha', color: 1 },
      { name: 'Bravo', color: 2 },
      { name: 'Charlie', color: 3 },
    ]);
    assert.deepEqual(tags.readTags(f), [
      { name: 'Alpha', color: 1 },
      { name: 'Bravo', color: 2 },
      { name: 'Charlie', color: 3 },
    ]);
  });

  it('removeTag removes only the named tag and preserves others', () => {
    const f = writeFile(tmp, 'remove.txt', 'x');
    tags.writeTags(f, [
      { name: 'Keep1', color: 1 },
      { name: 'Drop',  color: 6 },
      { name: 'Keep2', color: 0 },
    ]);
    tags.removeTag(f, 'Drop');
    assert.deepEqual(tags.readTags(f), [
      { name: 'Keep1', color: 1 },
      { name: 'Keep2', color: 0 },
    ]);
  });

  it('removeTag is a no-op when the tag is not present', () => {
    const f = writeFile(tmp, 'remove-missing.txt', 'x');
    tags.addTag(f, 'Here', 2);
    tags.removeTag(f, 'Absent'); // no throw, no change
    assert.deepEqual(tags.readTags(f), [{ name: 'Here', color: 2 }]);
  });

  it('removeTag on a file with no tags at all is a no-op', () => {
    const f = writeFile(tmp, 'remove-clean.txt', 'x');
    tags.removeTag(f, 'Anything');
    assert.deepEqual(tags.readTags(f), []);
  });

  it('writeTags([]) deletes the xattr entirely', () => {
    const f = writeFile(tmp, 'clear.txt', 'x');
    tags.addTag(f, 'Temp', 3);
    assert.ok(hasTagXattr(f), 'xattr should be present after addTag');
    tags.writeTags(f, []);
    assert.deepEqual(tags.readTags(f), []);
    assert.equal(hasTagXattr(f), false, 'xattr should be absent after writeTags([])');
  });

  it('writeTags(null) also deletes the xattr', () => {
    const f = writeFile(tmp, 'clear-null.txt', 'x');
    tags.addTag(f, 'Temp', 3);
    tags.writeTags(f, null);
    assert.equal(hasTagXattr(f), false);
    assert.deepEqual(tags.readTags(f), []);
  });

  it('writeTags([]) on a file that has no xattr is a silent no-op', () => {
    const f = writeFile(tmp, 'clear-noop.txt', 'x');
    // Should not throw even though xattr -d will fail under the hood.
    tags.writeTags(f, []);
    assert.equal(hasTagXattr(f), false);
  });

  it('writeTags round-trips tags with mixed colors (including uncolored)', () => {
    const f = writeFile(tmp, 'mixed.txt', 'x');
    tags.writeTags(f, [
      { name: 'Uncolored' },                 // color undefined
      { name: 'ExplicitZero', color: 0 },    // color === 0 is treated as uncolored
      { name: 'Red', color: 6 },
    ]);
    assert.deepEqual(tags.readTags(f), [
      { name: 'Uncolored', color: 0 },
      { name: 'ExplicitZero', color: 0 },
      { name: 'Red', color: 6 },
    ]);
  });

  it('writeTags round-trips names containing XML special chars &, <, >', () => {
    const f = writeFile(tmp, 'xml-escape.txt', 'x');
    const input = [
      { name: 'A & B', color: 2 },
      { name: '<angle>', color: 3 },
      { name: 'x > y & z', color: 0 },
    ];
    tags.writeTags(f, input);
    assert.deepEqual(tags.readTags(f), [
      { name: 'A & B', color: 2 },
      { name: '<angle>', color: 3 },
      { name: 'x > y & z', color: 0 },
    ]);
  });

  it('addLockTag adds { name: "Locked", color: 6 }', () => {
    const f = writeFile(tmp, 'lock.txt', 'x');
    tags.addLockTag(f);
    assert.deepEqual(tags.readTags(f), [{ name: 'Locked', color: 6 }]);
  });

  it('removeLockTag removes the Locked tag (and leaves others alone)', () => {
    const f = writeFile(tmp, 'unlock.txt', 'x');
    tags.addTag(f, 'Other', 2);
    tags.addLockTag(f);
    assert.equal(tags.readTags(f).length, 2);
    tags.removeLockTag(f);
    assert.deepEqual(tags.readTags(f), [{ name: 'Other', color: 2 }]);
  });

  it('readTags returns [] when the xattr is a corrupted plist (plutil error swallowed)', () => {
    const f = writeFile(tmp, 'corrupt.txt', 'x');
    // Random bytes that plutil will refuse to parse as any plist format.
    seedXattrHex(f, 'DEADBEEFCAFEBABEBAADF00DABAD1DEA');
    assert.deepEqual(tags.readTags(f), []);
  });

  it('readTags handles a bare <string>Simple</string> entry (no color component)', () => {
    const f = writeFile(tmp, 'bare.txt', 'x');
    const xml =
      '<?xml version="1.0" encoding="UTF-8"?>\n' +
      '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n' +
      '<plist version="1.0"><array><string>Simple</string></array></plist>';
    seedXattrFromXml(f, xml);
    assert.deepEqual(tags.readTags(f), [{ name: 'Simple', color: 0 }]);
  });

  it('readTags treats a non-numeric color component as 0 (Number.isFinite branch)', () => {
    const f = writeFile(tmp, 'nan-color.txt', 'x');
    // "Weird\nabc" — name "Weird", color parseInt("abc") = NaN → fallback to 0.
    const xml =
      '<?xml version="1.0" encoding="UTF-8"?>\n' +
      '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n' +
      '<plist version="1.0"><array><string>Weird\nabc</string></array></plist>';
    seedXattrFromXml(f, xml);
    assert.deepEqual(tags.readTags(f), [{ name: 'Weird', color: 0 }]);
  });

  it('readTags skips entries with an empty name', () => {
    const f = writeFile(tmp, 'empty-name.txt', 'x');
    const xml =
      '<?xml version="1.0" encoding="UTF-8"?>\n' +
      '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n' +
      '<plist version="1.0"><array><string></string><string>Real</string></array></plist>';
    seedXattrFromXml(f, xml);
    assert.deepEqual(tags.readTags(f), [{ name: 'Real', color: 0 }]);
  });
});

// =============================================================================
// Non-mac branches — stub process.platform so every function takes the
// early-return path. Covers the `!isMac()` guard in each exported function.
// =============================================================================

describe('mac-tags: non-mac early-return branches', () => {
  const originalPlatform = process.platform;
  const originalDescriptor = Object.getOwnPropertyDescriptor(process, 'platform');

  before(() => {
    Object.defineProperty(process, 'platform', { value: 'linux', configurable: true });
  });

  after(() => {
    if (originalDescriptor) {
      Object.defineProperty(process, 'platform', originalDescriptor);
    } else {
      Object.defineProperty(process, 'platform', {
        value: originalPlatform,
        configurable: true,
        writable: false,
        enumerable: true,
      });
    }
  });

  it('isMac() returns false when platform is not darwin', () => {
    assert.equal(tags.isMac(), false);
  });

  it('readTags returns [] without touching the filesystem', () => {
    assert.deepEqual(tags.readTags('/definitely/not/a/real/path/foo.txt'), []);
  });

  it('writeTags is a silent no-op and returns undefined', () => {
    assert.equal(tags.writeTags('/definitely/not/a/real/path/foo.txt', [{ name: 'x' }]), undefined);
  });

  it('writeTags with [] is also a silent no-op on non-mac', () => {
    assert.equal(tags.writeTags('/definitely/not/a/real/path/foo.txt', []), undefined);
  });

  it('addTag is a silent no-op and returns undefined', () => {
    assert.equal(tags.addTag('/definitely/not/a/real/path/foo.txt', 'X', 2), undefined);
  });

  it('removeTag is a silent no-op and returns undefined', () => {
    assert.equal(tags.removeTag('/definitely/not/a/real/path/foo.txt', 'X'), undefined);
  });

  it('addLockTag is a silent no-op and returns undefined', () => {
    assert.equal(tags.addLockTag('/definitely/not/a/real/path/foo.txt'), undefined);
  });

  it('removeLockTag is a silent no-op and returns undefined', () => {
    assert.equal(tags.removeLockTag('/definitely/not/a/real/path/foo.txt'), undefined);
  });
});

// Reference unused imports so linters/IDEs don't warn. (fs is used nowhere
// above but is commonly useful for debugging test failures.)
void fs;
