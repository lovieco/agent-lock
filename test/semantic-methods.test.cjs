'use strict';

// Semantic coverage for method-level descent inside classes. A top-level
// class is detected as `cls:Foo`; findEnclosingNode then dives into the
// class body looking for a single-line method signature that still covers
// the edit, and if found returns `cls:Foo.fn:method`.

process.env.CLAUDE_FILE_LOCK_TAG = '0';

const path = require('node:path');
const { describe, it } = require('node:test');
const assert = require('node:assert/strict');

const semantic = require(path.resolve(__dirname, '..', 'lock', 'semantic.cjs'));

describe('findEnclosingNode — method descent', () => {
  it('returns the first method when a class has multiple methods', () => {
    const src = [
      'class Svc {',
      '  alpha() {',
      '    return 1;',
      '  }',
      '  beta() {',
      '    return 2;',
      '  }',
      '}',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'return 1;', '.ts');
    assert.ok(node);
    assert.equal(node.id, 'cls:Svc.fn:alpha');
  });

  it('returns the SECOND method for an edit in its body', () => {
    const src = [
      'class Svc {',
      '  alpha() {',
      '    return 1;',
      '  }',
      '  beta() {',
      '    return 222;',
      '  }',
      '}',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'return 222;', '.ts');
    assert.ok(node);
    assert.equal(node.id, 'cls:Svc.fn:beta');
  });

  it('handles `async` and return-type annotations', () => {
    const src = [
      'class Svc {',
      '  async load(id: string): Promise<string> {',
      '    return id;',
      '  }',
      '}',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'return id;', '.ts');
    assert.ok(node);
    assert.equal(node.id, 'cls:Svc.fn:load');
  });

  it('handles `static` and `get/set` modifiers', () => {
    const src = [
      'class Svc {',
      '  static create() {',
      '    return new Svc();',
      '  }',
      '  get size() {',
      '    return 0;',
      '  }',
      '}',
    ].join('\n');
    const a = semantic.findEnclosingNode(src, 'return new Svc();', '.ts');
    const b = semantic.findEnclosingNode(src, 'return 0;', '.ts');
    assert.equal(a.id, 'cls:Svc.fn:create');
    assert.equal(b.id, 'cls:Svc.fn:size');
  });

  it('strips the `#` prefix from a private method', () => {
    const src = [
      'class Svc {',
      '  #secret() {',
      '    return 42;',
      '  }',
      '}',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'return 42;', '.ts');
    assert.ok(node);
    assert.equal(node.id, 'cls:Svc.fn:secret');
  });

  it('ignores a nested-function line inside a method body (depth > 1)', () => {
    // The nested `helper() {` appears at class-depth 2. METHOD_RE would match
    // the shape, but the depth guard prevents reclassifying it as a method.
    // Correct scope for an edit inside the nested fn is the OUTER method.
    const src = [
      'class Svc {',
      '  outer() {',
      '    function helper() {',
      '      return 9;',
      '    }',
      '    return helper();',
      '  }',
      '}',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'return 9;', '.ts');
    assert.ok(node);
    assert.equal(node.id, 'cls:Svc.fn:outer');
  });

  it('falls back to the class for a property declaration (no method sig match)', () => {
    const src = [
      'class Box {',
      '  value: number = 1;',
      '  bump() { return this.value; }',
      '}',
    ].join('\n');
    // Edit inside the property line — no method regex matches, falls back
    // to the enclosing class.
    const node = semantic.findEnclosingNode(src, 'value: number = 1;', '.ts');
    assert.ok(node);
    assert.equal(node.id, 'cls:Box');
  });

  it('returns the class for a single-line class body (exercises seed `}` branch)', () => {
    // Single-line class: seed line contains both `{` and `}` — depth rises
    // then falls in the class-seed loop, hitting both branches. No methods
    // are enumerable because the class body has no inner lines.
    const src = 'class Empty {}';
    const node = semantic.findEnclosingNode(src, 'class Empty', '.ts');
    assert.ok(node);
    assert.equal(node.id, 'cls:Empty');
  });

  it('returns the class when the class body spans lines but no method matches', () => {
    // Property-only class body exercises the depth===1 / METHOD_RE-miss path.
    const src = [
      'class Config {',
      '  port: number = 3000;',
      '  host: string = "localhost";',
      '}',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'port: number = 3000;', '.ts');
    assert.ok(node);
    assert.equal(node.id, 'cls:Config');
  });

  it('descends into methods in .cjs as well as .ts', () => {
    const src = [
      'class Foo {',
      '  bar() {',
      '    return 1;',
      '  }',
      '}',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'return 1;', '.cjs');
    assert.ok(node);
    assert.equal(node.id, 'cls:Foo.fn:bar');
  });

  it('does not try method descent on markdown (defensive)', () => {
    // A markdown heading "# class" must not trip method descent logic.
    const src = [
      '# class Outer',
      'body',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'body', '.md');
    assert.ok(node);
    assert.equal(node.kind, 'h1');
  });
});
