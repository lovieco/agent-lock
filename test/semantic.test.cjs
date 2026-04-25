'use strict';

const path = require('node:path');
const { describe, it } = require('node:test');
const assert = require('node:assert/strict');

const semantic = require(path.resolve(__dirname, '..', 'lock', 'semantic.cjs'));

describe('isSupported', () => {
  it('returns true for .js', () => {
    assert.equal(semantic.isSupported('foo.js'), true);
  });

  it('returns true for .jsx', () => {
    assert.equal(semantic.isSupported('foo.jsx'), true);
  });

  it('returns true for .ts', () => {
    assert.equal(semantic.isSupported('foo.ts'), true);
  });

  it('returns true for .tsx', () => {
    assert.equal(semantic.isSupported('foo.tsx'), true);
  });

  it('returns true for .cjs', () => {
    assert.equal(semantic.isSupported('foo.cjs'), true);
  });

  it('returns true for .mjs', () => {
    assert.equal(semantic.isSupported('foo.mjs'), true);
  });

  it('returns false for .py', () => {
    assert.equal(semantic.isSupported('foo.py'), false);
  });

  it('returns false for .json', () => {
    assert.equal(semantic.isSupported('foo.json'), false);
  });

  it('returns false for extensionless files', () => {
    assert.equal(semantic.isSupported('Makefile'), false);
  });

  it('exposes SUPPORTED_EXTS as a Set containing each ext', () => {
    assert.ok(semantic.SUPPORTED_EXTS instanceof Set);
    for (const ext of ['.js', '.jsx', '.ts', '.tsx', '.cjs', '.mjs']) {
      assert.ok(semantic.SUPPORTED_EXTS.has(ext), `expected ${ext} to be supported`);
    }
  });
});

describe('listTopLevelNodes — basic', () => {
  it('returns [] for empty string', () => {
    assert.deepEqual(semantic.listTopLevelNodes('', '.js'), []);
  });

  it('returns [] for unsupported extension (.py)', () => {
    assert.deepEqual(semantic.listTopLevelNodes('function foo() {}', '.py'), []);
  });

  it('returns [] for unsupported extension (.json)', () => {
    assert.deepEqual(semantic.listTopLevelNodes('function foo() {}', '.json'), []);
  });

  it('returns [] when ext is missing', () => {
    assert.deepEqual(semantic.listTopLevelNodes('function foo() {}'), []);
  });

  it('detects a single single-line top-level function', () => {
    const src = 'function foo() { return 1; }';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:foo');
    assert.equal(nodes[0].kind, 'fn');
    assert.equal(nodes[0].name, 'foo');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 0);
  });

  it('detects multiple top-level functions on separate lines', () => {
    const src = [
      'function foo() { return 1; }',
      'function bar() { return 2; }',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 2);
    assert.equal(nodes[0].id, 'fn:foo');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 0);
    assert.equal(nodes[1].id, 'fn:bar');
    assert.equal(nodes[1].startLine, 1);
    assert.equal(nodes[1].endLine, 1);
  });

  it('classifies async function as fn', () => {
    const src = 'async function foo() { return 1; }';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:foo');
  });

  it('classifies generator function* as fn', () => {
    const src = 'function* gen() { yield 1; }';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:gen');
  });

  it('detects a multi-line top-level class', () => {
    const src = [
      'class Bar {',
      '  hello() {',
      '    return 1;',
      '  }',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'cls:Bar');
    assert.equal(nodes[0].kind, 'cls');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 4);
  });

  it('classifies abstract class as cls', () => {
    const src = [
      'abstract class Baz {',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'cls:Baz');
  });
});

describe('listTopLevelNodes — exports', () => {
  it('strips `export` from function declaration', () => {
    const src = 'export function foo() {}';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:foo');
  });

  it('strips `export async` from function declaration', () => {
    const src = 'export async function foo() {}';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:foo');
  });

  it('classifies `export default function named()` as fn:named', () => {
    const src = 'export default function named() {}';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:named');
  });

  it('skips anonymous `export default function() {}`', () => {
    const src = 'export default function() {}';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 0);
  });

  it('classifies `export default class Named` as cls:Named', () => {
    const src = [
      'export default class Named {',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'cls:Named');
  });

  it('classifies `export const FOO = 1` as var:FOO', () => {
    const src = 'export const FOO = 1;';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'var:FOO');
  });
});

describe('listTopLevelNodes — vars', () => {
  it('detects `const x = 1;` with startLine === endLine', () => {
    const src = 'const x = 1;';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'var:x');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 0);
  });

  it('detects single-line arrow function assigned to let', () => {
    const src = 'let y = () => { return 42; };';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'var:y');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 0);
  });

  it('detects multi-line arrow function assigned to const', () => {
    const src = [
      'const handler = (req, res) => {',
      "  return res.send('ok');",
      '};',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'var:handler');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 2);
  });
});

describe('listTopLevelNodes — strings/comments', () => {
  it('ignores line-comment lines that look like declarations', () => {
    const src = [
      '// function foo() {}',
      'function bar() {}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:bar');
  });

  it('ignores lines that begin with /*', () => {
    const src = [
      '/* function foo() {} */',
      'function bar() {}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:bar');
  });

  it('ignores lines that begin with * (JSDoc body)', () => {
    const src = [
      '* function foo() {}',
      'function bar() {}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:bar');
  });

  it('ignores shebang line', () => {
    const src = [
      '#!/usr/bin/env node',
      'function bar() {}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:bar');
    assert.equal(nodes[0].startLine, 1);
  });

  it('ignores livehub lock marker lines at top level', () => {
    const src = [
      '// livehub lock: abc123',
      'function bar() {}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:bar');
  });

  it('does not let a string brace `"}{"` unbalance end detection', () => {
    const src = [
      'function foo() {',
      '  const s = "}{";',
      '  return s;',
      '}',
      'function bar() {}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 2);
    assert.equal(nodes[0].id, 'fn:foo');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 3);
    assert.equal(nodes[1].id, 'fn:bar');
    assert.equal(nodes[1].startLine, 4);
  });

  it("handles single-quote strings like `'}'`", () => {
    const src = [
      'function foo() {',
      "  const x = '}';",
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].endLine, 2);
  });

  it('handles backtick template literals containing `{}`', () => {
    const src = [
      'function foo() {',
      '  const t = `a{b}c`;',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].endLine, 2);
  });

  it('ignores inline /* } */ block comments for brace counting', () => {
    const src = [
      'function foo() {',
      '  /* } */',
      '  return 1;',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].endLine, 3);
  });

  it('ignores `// }` line-comments inside function bodies', () => {
    const src = [
      'function foo() {',
      '  // }',
      '  return 1;',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].endLine, 3);
  });

  it('handles escaped quotes inside strings', () => {
    const src = [
      'function foo() {',
      '  const x = "\\"";',
      '  return x;',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:foo');
    assert.equal(nodes[0].endLine, 3);
  });
});

describe('listTopLevelNodes — edge cases', () => {
  it('ignores indented declarations (nested function)', () => {
    const src = [
      'function outer() {',
      '  function inner() { return 1; }',
      '  return inner();',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:outer');
  });

  it('drops declarations with unterminated `{`', () => {
    const src = [
      'function broken() {',
      '  const x = 1;',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 0);
  });
});

describe('findEnclosingNode', () => {
  it('returns null for unsupported extension', () => {
    const src = 'function foo() { return 1; }';
    assert.equal(semantic.findEnclosingNode(src, 'return 1', '.py'), null);
  });

  it('returns null when oldString is not in the file', () => {
    const src = 'function foo() { return 1; }';
    assert.equal(semantic.findEnclosingNode(src, 'nope', '.js'), null);
  });

  it('returns null for empty oldString', () => {
    const src = 'function foo() { return 1; }';
    assert.equal(semantic.findEnclosingNode(src, '', '.js'), null);
  });

  it('returns null for non-string oldString', () => {
    const src = 'function foo() { return 1; }';
    assert.equal(semantic.findEnclosingNode(src, undefined, '.js'), null);
  });

  it('returns the enclosing function for a single-line oldString inside its body', () => {
    const src = [
      'function foo() {',
      '  const x = 42;',
      '  return x;',
      '}',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'const x = 42;', '.js');
    assert.ok(node);
    assert.equal(node.id, 'fn:foo');
  });

  it('returns the enclosing function for a multi-line oldString inside its body', () => {
    const src = [
      'function foo() {',
      '  const x = 42;',
      '  return x;',
      '}',
    ].join('\n');
    const oldString = '  const x = 42;\n  return x;';
    const node = semantic.findEnclosingNode(src, oldString, '.js');
    assert.ok(node);
    assert.equal(node.id, 'fn:foo');
  });

  it('returns null when oldString spans two top-level functions', () => {
    const src = [
      'function foo() {',
      '  return 1;',
      '}',
      'function bar() {',
      '  return 2;',
      '}',
    ].join('\n');
    const oldString = '}\nfunction bar() {';
    const node = semantic.findEnclosingNode(src, oldString, '.js');
    assert.equal(node, null);
  });

  it('returns null when oldString is between top-level declarations (imports area)', () => {
    const src = [
      "const imp = require('x');",
      '',
      'function foo() {',
      '  return 1;',
      '}',
    ].join('\n');
    // A blank line between top-level nodes isn't enclosed by any node.
    const node = semantic.findEnclosingNode(src, '\n\nfunction', '.js');
    assert.equal(node, null);
  });

  it('returns the enclosing method (cls:Outer.fn:method) for an oldString inside a class method', () => {
    const src = [
      'class Outer {',
      '  method() {',
      '    return 42;',
      '  }',
      '}',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'return 42;', '.js');
    assert.ok(node);
    assert.equal(node.id, 'cls:Outer.fn:method');
    assert.equal(node.kind, 'method');
  });

  it('returns the outer class when the edit spans multiple methods', () => {
    const src = [
      'class Outer {',
      '  a() { return 1; }',
      '  b() { return 2; }',
      '}',
    ].join('\n');
    // oldString covers both method signatures → no single method contains it
    // → falls back to the enclosing class.
    const node = semantic.findEnclosingNode(src, 'a() { return 1; }\n  b() { return 2; }', '.js');
    assert.ok(node);
    assert.equal(node.id, 'cls:Outer');
  });

  it('picks the node whose range covers the oldString when multiple top-level decls exist', () => {
    const src = [
      'function foo() {',
      '  return 1;',
      '}',
      'function bar() {',
      '  return 222;',
      '}',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'return 222;', '.js');
    assert.ok(node);
    assert.equal(node.id, 'fn:bar');
  });
});
