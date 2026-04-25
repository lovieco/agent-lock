'use strict';

// Semantic coverage for Markdown (.md) files.
//
// Markdown nodes differ from JS/TS in two ways:
//   1. Nodes can NEST (h2 under h1), whereas code top-level nodes don't.
//   2. Frontmatter is its own kind (fm:), identified by a leading `---…---`.
//
// findEnclosingNode picks the smallest range covering the edit, so a
// body-only edit resolves to the tightest heading; a cross-section edit
// resolves to the common parent.

process.env.CLAUDE_FILE_LOCK_TAG = '0';

const path = require('node:path');
const { describe, it } = require('node:test');
const assert = require('node:assert/strict');

const semantic = require(path.resolve(__dirname, '..', 'lock', 'semantic.cjs'));
const lock = require(path.resolve(__dirname, '..', 'lock', 'file-lock.cjs'));
const { makeTmpDir, writeFile, readFile } = require('./helpers/tmp.cjs');

// ---------------------------------------------------------------------------

describe('isSupported — markdown', () => {
  it('returns true for .md', () => {
    assert.equal(semantic.isSupported('foo.md'), true);
  });

  it('SUPPORTED_EXTS includes .md', () => {
    assert.ok(semantic.SUPPORTED_EXTS.has('.md'));
  });
});

describe('listTopLevelNodes — markdown frontmatter', () => {
  it('returns [] for an empty markdown file', () => {
    assert.deepEqual(semantic.listTopLevelNodes('', '.md'), []);
  });

  it('detects a YAML frontmatter block as fm:frontmatter', () => {
    const src = [
      '---',
      'name: agent',
      'type: foo',
      '---',
      '',
      'body',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.md');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'fm:frontmatter');
    assert.equal(nodes[0].kind, 'fm');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 3);
  });

  it('returns [] when frontmatter is unterminated', () => {
    const src = [
      '---',
      'no closing fence',
      '',
    ].join('\n');
    assert.deepEqual(semantic.listTopLevelNodes(src, '.md'), []);
  });

  it('skips frontmatter when line 0 is not `---`', () => {
    const src = [
      '# Heading',
      '---',
      'body',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.md');
    // No fm node; just the heading.
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'h1:heading');
  });
});

describe('listTopLevelNodes — markdown headings', () => {
  it('detects a single h1', () => {
    const src = [
      '# Title',
      'body line',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.md');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'h1:title');
    assert.equal(nodes[0].startLine, 0);
    assert.equal(nodes[0].endLine, 1);
  });

  it('detects multiple headings at various levels as separate nodes', () => {
    const src = [
      '# Top',
      'intro',
      '## Sub A',
      'sub a body',
      '## Sub B',
      'sub b body',
      '### Deep',
      'deep body',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.md');
    const ids = nodes.map(n => n.id);
    assert.deepEqual(ids, ['h1:top', 'h2:sub-a', 'h2:sub-b', 'h3:deep']);
  });

  it('computes each heading endLine as the line before the next same-or-higher level', () => {
    const src = [
      '# A',          // 0
      'a body',        // 1
      '## A1',        // 2
      'a1 body',       // 3
      '## A2',        // 4
      'a2 body',       // 5
      '# B',          // 6
      'b body',        // 7
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.md');
    // h1:A ends at line 5 (just before h1:B). h2:A1 ends at 3 (before h2:A2).
    // h2:A2 ends at 5 (before h1:B at same-or-higher level).
    const byId = Object.fromEntries(nodes.map(n => [n.id, n]));
    assert.equal(byId['h1:a'].endLine, 5);
    assert.equal(byId['h2:a1'].endLine, 3);
    assert.equal(byId['h2:a2'].endLine, 5);
    assert.equal(byId['h1:b'].endLine, 7);
  });

  it('skips all-punctuation headings (empty slug)', () => {
    const src = [
      '# !!!',
      '# Real One',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.md');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'h1:real-one');
  });

  it('slugifies heading text: lowercase, strips punctuation, collapses spaces', () => {
    const src = '## Behavioral Rules (Always Enforced)';
    const nodes = semantic.listTopLevelNodes(src, '.md');
    assert.equal(nodes.length, 1);
    assert.equal(nodes[0].id, 'h2:behavioral-rules-always-enforced');
  });

  it('combines frontmatter + headings in one pass', () => {
    const src = [
      '---',
      'name: x',
      '---',
      '# Hello',
      'body',
    ].join('\n');
    const nodes = semantic.listTopLevelNodes(src, '.md');
    assert.equal(nodes.length, 2);
    assert.equal(nodes[0].id, 'fm:frontmatter');
    assert.equal(nodes[1].id, 'h1:hello');
    assert.equal(nodes[1].startLine, 3);
  });
});

describe('findEnclosingNode — markdown', () => {
  it('returns the deepest heading that contains the edit', () => {
    const src = [
      '# A',
      '## A1',
      'target inside a1',
      '## A2',
      'other',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'target inside a1', '.md');
    assert.ok(node);
    assert.equal(node.id, 'h2:a1');
  });

  it('returns the parent when the edit spans sibling headings', () => {
    const src = [
      '# A',
      '## A1',
      'stuff',
      '## A2',
      'more',
    ].join('\n');
    // oldString spans end of A1 into A2 — only the common parent h1:a covers.
    const oldString = 'stuff\n## A2';
    const node = semantic.findEnclosingNode(src, oldString, '.md');
    assert.ok(node);
    assert.equal(node.id, 'h1:a');
  });

  it('returns the frontmatter for an edit inside ---…---', () => {
    const src = [
      '---',
      'name: agent',
      'type: worker',
      '---',
      '# Body',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'type: worker', '.md');
    assert.ok(node);
    assert.equal(node.id, 'fm:frontmatter');
  });

  it('returns null when the edit is outside any heading or frontmatter', () => {
    const src = [
      'plain text, no structure',
      'another line',
    ].join('\n');
    const node = semantic.findEnclosingNode(src, 'another line', '.md');
    assert.equal(node, null);
  });
});

describe('integration — acquireLock on .md with heading scope', () => {
  function markerNodeOf(filePath) {
    const txt = readFile(filePath);
    const m = txt.match(/livehub lock:\s+A=\S+\s+N=(\S+)/);
    return m ? m[1] : null;
  }

  it('records h2:<slug> on the HTML-comment marker', () => {
    const dir = makeTmpDir();
    const src = [
      '# Top',
      '## Section One',
      'section body',
      '## Section Two',
      'other body',
    ].join('\n') + '\n';
    const p = writeFile(dir, 'doc.md', src);

    const node = semantic.findEnclosingNode(src, 'section body', '.md');
    assert.ok(node);
    assert.equal(node.id, 'h2:section-one');

    const res = lock.acquireLock(p, {
      agentId: 'md-agent',
      nodeId: node.id,
      reason: 'edit section',
    });
    assert.equal(res.ok, true);
    assert.equal(markerNodeOf(p), 'h2:section-one');
  });

  it('two agents in different h2 sections can both acquire', () => {
    const dir = makeTmpDir();
    const src = [
      '# Top',
      '## Alpha',
      'alpha body',
      '## Beta',
      'beta body',
    ].join('\n') + '\n';
    const p = writeFile(dir, 'parallel.md', src);

    const aNode = semantic.findEnclosingNode(src, 'alpha body', '.md');
    const bNode = semantic.findEnclosingNode(src, 'beta body', '.md');

    const resA = lock.acquireLock(p, { agentId: 'A', nodeId: aNode.id, reason: 'edit alpha' });
    const resB = lock.acquireLock(p, { agentId: 'B', nodeId: bNode.id, reason: 'edit beta' });
    assert.equal(resA.ok, true);
    assert.equal(resB.ok, true, 'different h2 sections must not collide');
  });
});
