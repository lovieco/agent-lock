'use strict';

// Edge-case coverage for the semantic parser (lock/semantic.cjs) and for the
// nodeId that ends up on the lock marker when acquireLock is called against
// unusual JS/TS shapes. These tests pin down ACTUAL heuristic behavior —
// including known limitations — so future refactors surface regressions.
//
// Disable macOS Finder xattr tagging so these tests are platform-neutral.
process.env.CLAUDE_FILE_LOCK_TAG = '0';

const path = require('node:path');
const { describe, it } = require('node:test');
const assert = require('node:assert/strict');

const semantic = require(path.resolve(__dirname, '..', 'lock', 'semantic.cjs'));
const lock = require(path.resolve(__dirname, '..', 'lock', 'file-lock.cjs'));
const { makeTmpDir, writeFile, readFile } = require('./helpers/tmp.cjs');

// ---------------------------------------------------------------------------

describe('listTopLevelNodes — TypeScript and generics', () => {
  it('detects a generic function with TS annotations on a single line', () => {
    const src = 'function foo<T extends string>(x: T): T { return x; }';
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:foo');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 0);
  });

  it('detects a TS generic class `class Box<T>`', () => {
    const src = [
      'class Box<T> {',
      '  value: T;',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'cls:Box');
    assert.equal(nodes[0].endLine, 2);
  });

  it('detects `export class Foo extends Bar implements Baz {}`', () => {
    const src = 'export class Foo extends Bar implements Baz {}';
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'cls:Foo');
  });

  it('skips decorator lines but classifies the class below', () => {
    // `@injectable()` is not one of the classify patterns, so it's skipped;
    // `class Foo {}` lands on line 1 and is classified there.
    const src = [
      '@injectable()',
      'class Foo {}',
      '',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'cls:Foo');
    assert.equal(nodes[0].startLine, 1);
    assert.equal(nodes[0].endLine, 1);
  });

  it('classifies `interface User { ... }` as iface:User', () => {
    const src = 'interface User { name: string; }';
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'iface:User');
    assert.equal(nodes[0].kind, 'iface');
  });

  it('classifies `export interface Foo` as iface:Foo', () => {
    const src = 'export interface Foo { x: number; }';
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'iface:Foo');
  });

  it('classifies `type Handler = ...` as type:Handler', () => {
    const src = 'type Handler = (x: number) => string;';
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'type:Handler');
    assert.equal(nodes[0].kind, 'type');
  });

  it('classifies a multi-line `type X = { ... }` block', () => {
    const src = [
      'type Config = {',
      '  port: number;',
      '  host: string;',
      '};',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'type:Config');
    assert.equal(nodes[0].endLine, 3);
  });

  it('does not classify `enum Color { ... }`', () => {
    const src = 'enum Color { Red, Green, Blue }';
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 0);
  });

  it('does not classify `namespace Utils { ... }` nor its indented contents', () => {
    // `namespace` isn't in PATTERNS (line 0 is skipped). The inner function
    // is indented so the top-level gate skips it too.
    const src = [
      'namespace Utils {',
      '  export function x() {}',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 0);
  });

  it('does not classify `export * from "..."` or `import`', () => {
    const src = [
      "export * from './foo';",
      "import { x } from 'y';",
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 0);
  });
});

// ---------------------------------------------------------------------------

describe('listTopLevelNodes — formatting edges', () => {
  it('still detects both functions when separated by blank lines', () => {
    const src = [
      'function foo() {',
      '  return 1;',
      '}',
      '',
      '',
      'function bar() {',
      '  return 2;',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 2);
    assert.equal(nodes[0].id, 'fn:foo');
    assert.equal(nodes[1].id, 'fn:bar');
    assert.equal(nodes[1].startLine, 5);
  });

  it('does not detect a function whose first line starts with a BOM', () => {
    // DOCUMENTED LIMITATION: the classify regex is `^function...`, and the
    // top-level gate rejects lines starting with whitespace. A BOM (U+FEFF)
    // is treated as whitespace by `/^\s/` in modern V8, so the line is
    // skipped entirely. Pinning the current behavior here.
    const src = '\uFEFFfunction foo() {}';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 0);
  });

  it('does not treat a tab-indented `function inner()` as top-level', () => {
    const src = [
      'function outer() {',
      '\tfunction inner() { return 1; }',
      '  return 1;',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:outer');
  });

  it('classifies a multi-line arrow component with JSX as comp:MyComp (.tsx)', () => {
    const src = [
      'const MyComp = ({ items }) => {',
      '  return (',
      '    <div>',
      '      {items.map((i) => <span key={i}>{i}</span>)}',
      '    </div>',
      '  );',
      '};',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.tsx');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'comp:MyComp');
    assert.equal(nodes[0].kind, 'comp');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 6);
  });

  it('keeps lowercase arrow fn as var (not comp) in .tsx', () => {
    const src = 'const helper = (x) => x + 1;';
    const nodes = semantic.listTopLevelNodes(src, '.tsx');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'var:helper');
  });

  it('classifies PascalCase function in .jsx as comp:', () => {
    const src = 'function MyPage() { return 1; }';
    const nodes = semantic.listTopLevelNodes(src, '.jsx');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'comp:MyPage');
  });

  it('leaves PascalCase function as fn: in .ts (no JSX)', () => {
    const src = 'function MyHelper() { return 1; }';
    const nodes = semantic.listTopLevelNodes(src, '.ts');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:MyHelper');
  });

  it('handles a const-assigned object literal with nested braces and strings', () => {
    const src = [
      'const config = {',
      '  paths: { in: "/tmp", out: "{out}" },',
      '  fn: function() { return "}"; },',
      '};',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'var:config');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 3);
  });

  it('classifies a really long single-line arrow as var:f (startLine === endLine)', () => {
    const src = 'const f = (a,b,c) => a+b+c;';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'var:f');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 0);
  });
});

// ---------------------------------------------------------------------------

describe('listTopLevelNodes — strings and template literals', () => {
  it('classifies a single-line template literal with ${...} as var:msg', () => {
    // DOCUMENTED LIMITATION: `bracesOnly` enters "string" mode on backtick
    // and treats the entire backtick-delimited region as opaque — so the
    // `${name}` interpolation's braces are stripped along with the rest.
    // For this single-line case that's fine: no open `{`, ends with `;`,
    // so findBlockEnd returns startLine via the `ends-with-;` escape hatch.
    const src = 'const msg = `hello ${name} world`;';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'var:msg');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 0);
  });

  it('classifies only the first of two consecutive `const`s on one line', () => {
    // DOCUMENTED LIMITATION: the parser walks line-by-line. Both decls live
    // on line 0; after classifying `a` and computing endLine=0, the parser
    // jumps to line 1 and never sees the second decl.
    const src = 'const a=1; const b=2;';
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'var:a');
  });

  it('does NOT handle regex literals in a function body — documented limitation', () => {
    // DOCUMENTED LIMITATION: distinguishing `/` as division vs regex-start
    // requires context-aware tokenisation, which this heuristic skips. The
    // `}` inside `/[}{]/` is counted as a closing brace, so the parser
    // thinks the function ends on the regex line rather than the real
    // closing brace. `fn:foo` is still detected — just with a wrong
    // endLine. If someone upgrades the parser (e.g. to @babel/parser),
    // update the endLine expectation below.
    const src = [
      'function foo() {',
      '  const re = /[}{]/g;',
      '  return re;',
      '}',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fn:foo');
    // Actual behaviour: endLine is the regex line, not the real `}` line.
    assert.equal(nodes[0].endLine, 1);
  });

  it('multi-line template literal containing a function-looking line', () => {
    // DOCUMENTED LIMITATION: `bracesOnly` resets its string-state on every
    // line, so it cannot know that line 1 is inside an unterminated
    // template literal started on line 0. The `function fake() {}` on
    // line 1 has balanced `{}` and is treated as the closing brace of the
    // `const tpl` declaration — so endLine lands on line 1, not on the
    // closing `` `; ``. We pin this behavior so a future multi-line-string
    // improvement surfaces as a test change.
    const src = [
      'const tpl = `',
      'function fake() {}',
      '`;',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.js');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'var:tpl');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 1);
  });
});

// ---------------------------------------------------------------------------

describe('findEnclosingNode — boundaries', () => {
  it('returns the function when oldString is exactly the signature line', () => {
    const src = [
      'function foo() {',
      '  return 1;',
      '}',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'function foo() {', '.js');
    assert.ok(node);
    assert.equal(node.id, 'fn:foo');
    assert.equal(node.startLine, 0);
  });

  it('returns the function when oldString is the entire body including braces', () => {
    const src = [
      'function foo() {',
      '  return 1;',
      '}',
    ].join('\n');
    const oldString = 'function foo() {\n  return 1;\n}';
    const node = semantic.findEnclosingNode(src, oldString, '.js');
    assert.ok(node);
    assert.equal(node.id, 'fn:foo');
  });

  it('returns null when oldString spans the last line of one fn and the first of the next', () => {
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

  it('uses the FIRST occurrence of oldString when it appears twice', () => {
    // DOCUMENTED BEHAVIOR: `text.indexOf(oldString)` — only the first hit is
    // considered. So a duplicate token inside fn `foo` is resolved to foo
    // even if another copy lives inside bar.
    const src = [
      'function foo() {',
      '  return "dup";',
      '}',
      'function bar() {',
      '  return "dup";',
      '}',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, '"dup"', '.js');
    assert.ok(node);
    assert.equal(node.id, 'fn:foo');
  });

  it('handles a lone newline as oldString (actual behavior, whatever it is)', () => {
    const src = [
      'function foo() {',
      '  return 1;',
      '}',
    ].join('\n');
    // The first `\n` is at index 16 (end of "function foo() {"), so the
    // match straddles line 0 -> line 1. That range is fully inside foo
    // (startLine 0, endLine 2), so the node resolves to fn:foo.
    const node = semantic.findEnclosingNode(src, '\n', '.js');
    assert.ok(node);
    assert.equal(node.id, 'fn:foo');
  });

  it('returns null for an empty file regardless of oldString', () => {
    assert.equal(semantic.findEnclosingNode('', 'anything', '.js'), null);
  });

  it('returns null for a file that is only comments', () => {
    const src = [
      '// just a comment',
      '/* and another */',
      '// nothing to see',
    ].join('\n');
    assert.deepEqual(semantic.listTopLevelNodes(src, '.js'), []);
    assert.equal(semantic.findEnclosingNode(src, 'comment', '.js'), null);
  });
});

// ---------------------------------------------------------------------------

describe('integration — acquireLock with tricky files', () => {
  function markerNodeOf(filePath) {
    const txt = readFile(filePath);
    const m = txt.match(/livehub lock:\s+A=\S+\s+N=(\S+)/);
    return m ? m[1] : null;
  }

  it('records N=comp:MyComp for an Edit inside a JSX arrow component (.tsx)', () => {
    const dir = makeTmpDir();
    const src = [
      'const MyComp = ({ items }) => {',
      '  return (',
      '    <div>',
      '      {items.map((i) => <span key={i}>{i}</span>)}',
      '    </div>',
      '  );',
      '};',
    ].join('\n') + '\n';
    const p = writeFile(dir, 'MyComp.tsx', src);

    const oldString = '<div>\n      {items.map((i)';
    const node = semantic.findEnclosingNode(src, oldString, '.tsx');
    assert.ok(node);
    assert.equal(node.id, 'comp:MyComp');

    const res = lock.acquireLock(p, {
      agentId: 'jsx-agent',
      nodeId: node.id,
      reason: 'edit jsx',
    });
    assert.equal(res.ok, true);
    assert.equal(markerNodeOf(p), 'comp:MyComp');
  });

  it('records N=cls:TheClass.fn:greet for an Edit inside a method body (.ts)', () => {
    const dir = makeTmpDir();
    const src = [
      'class TheClass {',
      '  greet(name: string): string {',
      '    return `hello ${name}`;',
      '  }',
      '}',
    ].join('\n') + '\n';
    const p = writeFile(dir, 'TheClass.ts', src);

    const oldString = 'return `hello ${name}`;';
    const node = semantic.findEnclosingNode(src, oldString, '.ts');
    assert.ok(node);
    assert.equal(node.id, 'cls:TheClass.fn:greet');
    assert.equal(node.kind, 'method');

    const res = lock.acquireLock(p, {
      agentId: 'cls-agent',
      nodeId: node.id,
      reason: 'edit method',
    });
    assert.equal(res.ok, true);
    assert.equal(markerNodeOf(p), 'cls:TheClass.fn:greet');
  });

  it('whole-file lock blocks a later node-scoped lock on the same file', () => {
    // One agent targets `enum Color` (not classified — falls back to *).
    // Another targets `function helper` (fn:helper). The whole-file `*`
    // lock, acquired first, dominates — the second acquire is blocked.
    const dir = makeTmpDir();
    const src = [
      'enum Color { Red, Green, Blue }',
      'function helper() { return 1; }',
    ].join('\n') + '\n';
    const p = writeFile(dir, 'mixed.ts', src);

    // Agent A: enum -> unrecognised -> fallback to WHOLE_FILE.
    const enumNode = semantic.findEnclosingNode(src, 'enum Color', '.ts');
    assert.equal(enumNode, null);
    const nodeIdA = enumNode ? enumNode.id : lock.WHOLE_FILE;

    const resA = lock.acquireLock(p, {
      agentId: 'agent-A',
      nodeId: nodeIdA,
      reason: 'edit enum',
    });
    assert.equal(resA.ok, true);
    assert.equal(markerNodeOf(p), lock.WHOLE_FILE);

    // Agent B: targets fn:helper; blocked because someone else holds `*`.
    const helperNode = semantic.findEnclosingNode(src, 'return 1;', '.ts');
    assert.ok(helperNode);
    assert.equal(helperNode.id, 'fn:helper');

    const resB = lock.acquireLock(p, {
      agentId: 'agent-B',
      nodeId: helperNode.id,
      reason: 'edit helper',
    });
    assert.equal(resB.ok, false);
    assert.equal(resB.heldBy, 'agent-A');
    assert.equal(resB.nodeId, lock.WHOLE_FILE);
  });

  it('a file of only enum/namespace lists [] and locks default to `*`', () => {
    // enum and namespace remain unclassified (deliberate — they're rare in
    // hand-written code relative to iface/type), so a file containing only
    // them falls back to whole-file locking.
    const dir = makeTmpDir();
    const src = [
      'enum Color { Red, Green, Blue }',
      'namespace Utils { export function x() {} }',
    ].join('\n') + '\n';
    const p = writeFile(dir, 'types.ts', src);

    assert.deepEqual(semantic.listTopLevelNodes(src, '.ts'), []);

    const node = semantic.findEnclosingNode(src, 'Red, Green', '.ts');
    assert.equal(node, null);

    const res = lock.acquireLock(p, {
      agentId: 'types-agent',
      nodeId: node ? node.id : undefined,
      reason: 'edit types',
    });
    assert.equal(res.ok, true);
    assert.equal(markerNodeOf(p), lock.WHOLE_FILE);
  });

  it('a file of iface/type is node-locked, not whole-file', () => {
    // With iface/type now recognised, two agents editing different
    // declarations in the same types file can run in parallel.
    const dir = makeTmpDir();
    const src = [
      'interface User { name: string; }',
      'type Handler = (x: number) => string;',
    ].join('\n') + '\n';
    const p = writeFile(dir, 'shapes.ts', src);

    const userNode = semantic.findEnclosingNode(src, 'name: string', '.ts');
    assert.ok(userNode);
    assert.equal(userNode.id, 'iface:User');

    const handlerNode = semantic.findEnclosingNode(src, '(x: number)', '.ts');
    assert.ok(handlerNode);
    assert.equal(handlerNode.id, 'type:Handler');

    const resA = lock.acquireLock(p, { agentId: 'A', nodeId: userNode.id,    reason: 'edit User' });
    const resB = lock.acquireLock(p, { agentId: 'B', nodeId: handlerNode.id, reason: 'edit Handler' });
    assert.equal(resA.ok, true);
    assert.equal(resB.ok, true, 'different iface/type nodes should NOT collide');
  });
});
