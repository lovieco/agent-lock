// Heuristic semantic-scope detection for the JS/TS family and Markdown.
//
// Given the full text of a source file and an `old_string` from an
// Edit/MultiEdit payload, returns a stable `nodeId` identifying the
// enclosing declaration:
//
//   JS/TS:  fn:foo | cls:Foo | comp:Foo | iface:Foo | type:Foo | var:FOO |
//           cls:Foo.fn:bar   (method inside a class)
//   MD:     fm:frontmatter | h1:slug | h2:slug | … | h6:slug
//
// When the language is unsupported or the heuristic can't place the edit
// inside exactly one node, returns null — callers then fall back to
// whole-file locking, which is the safe choice.
//
// This is intentionally not a real parser. It handles the shapes that
// actually show up in hand-written source (top-level functions, classes,
// arrow-fn consts, named/default exports, single-line method signatures)
// and refuses to guess on the rest. A false-null costs parallelism, not
// correctness.

const path = require('path');

const CODE_EXTS = new Set(['.js', '.jsx', '.ts', '.tsx', '.cjs', '.mjs']);
const JSX_EXTS  = new Set(['.jsx', '.tsx']);
const MD_EXTS   = new Set(['.md']);
const SUPPORTED_EXTS = new Set([...CODE_EXTS, ...MD_EXTS]);

function isSupported(filePath) {
  return SUPPORTED_EXTS.has(path.extname(filePath).toLowerCase());
}

// ---------------------------------------------------------------------------
// JS/TS top-level classification

const PATTERNS = [
  { re: /^(?:export\s+(?:default\s+)?)?(?:async\s+)?function\s*\*?\s*(\w+)/, kind: 'fn' },
  { re: /^(?:export\s+(?:default\s+)?)?(?:abstract\s+)?class\s+(\w+)/,       kind: 'cls' },
  { re: /^(?:export\s+)?interface\s+(\w+)/,                                  kind: 'iface' },
  { re: /^(?:export\s+)?type\s+(\w+)/,                                       kind: 'type' },
  { re: /^(?:export\s+)?(?:const|let|var)\s+(\w+)/,                          kind: 'var' },
];

function classify(line, ext) {
  for (const { re, kind } of PATTERNS) {
    const m = line.match(re);
    if (m) {
      // In .jsx/.tsx, PascalCase top-level fn/var is treated as a React
      // component. Body inspection (does it return JSX?) would add parser
      // complexity; the PascalCase-in-JSX-file convention is strong enough
      // that agents get useful locking either way.
      let finalKind = kind;
      if ((kind === 'fn' || kind === 'var') && JSX_EXTS.has(ext) && /^[A-Z]/.test(m[1])) {
        finalKind = 'comp';
      }
      return { kind: finalKind, name: m[1] };
    }
  }
  return null;
}

// Strip strings, template literals, and comments before counting braces,
// so a `"{"` in a string doesn't unbalance us. Also track paren depth
// across the whole scan so braces inside parens — destructuring patterns
// `({ items })`, JSX `return (…)` bodies, type annotations `(x: {a:1})`
// — don't corrupt the body-level `{…}` count.
function bracesOnly(line, parenState) {
  let out = '';
  let i = 0, inStr = null;
  while (i < line.length) {
    const c = line[i];
    if (inStr) {
      if (c === '\\') { i += 2; continue; }
      if (c === inStr) inStr = null;
      i++; continue;
    }
    if (c === '"' || c === "'" || c === '`') { inStr = c; i++; continue; }
    if (c === '/' && line[i + 1] === '/') break;
    if (c === '/' && line[i + 1] === '*') {
      const end = line.indexOf('*/', i + 2);
      if (end < 0) break;
      i = end + 2;
      continue;
    }
    if (c === '(')      parenState.depth++;
    else if (c === ')') parenState.depth--;
    else if ((c === '{' || c === '}') && parenState.depth <= 0) out += c;
    i++;
  }
  return out;
}

function findBlockEnd(lines, startLine) {
  let depth = 0, seenOpen = false;
  const parenState = { depth: 0 };
  for (let i = startLine; i < lines.length; i++) {
    const braces = bracesOnly(lines[i], parenState);
    for (const c of braces) {
      if (c === '{') { depth++; seenOpen = true; }
      else if (c === '}') {
        depth--;
        if (seenOpen && depth === 0) return i;
      }
    }
    if (i === startLine && !seenOpen) {
      const t = lines[i].trimEnd();
      if (t.endsWith(';')) return i;
    }
  }
  return -1;
}

function listCodeNodes(text, ext) {
  const lines = text.split('\n');
  const out = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    // Top level = no leading whitespace.
    if (/^\s/.test(line)) { i++; continue; }
    const t = line.trim();
    if (!t || t.startsWith('//') || t.startsWith('/*') || t.startsWith('*') || t.startsWith('#!')) { i++; continue; }
    // Skip our own lock markers if they happen to sit at the top. In
    // practice marker lines always start with a comment-style opener so
    // they get filtered by the line above — this is defensive only.
    /* c8 ignore next */
    if (t.includes('livehub lock:')) { i++; continue; }
    const cls = classify(line, ext);
    if (!cls) { i++; continue; }
    const endLine = findBlockEnd(lines, i);
    if (endLine < i) { i++; continue; }
    out.push({
      id: `${cls.kind}:${cls.name}`,
      kind: cls.kind,
      name: cls.name,
      signature: t,
      startLine: i,
      endLine,
    });
    i = endLine + 1;
  }
  return out;
}

// ---------------------------------------------------------------------------
// Method-level descent inside a class body.
//
// Walks the class body tracking brace depth; at depth 1 (direct child of the
// class) tries to match a method signature. Only handles single-line
// signatures — multi-line method signatures fall back to the class lock.

const METHOD_RE = new RegExp(
  '^\\s+' +
  // optional modifiers (can repeat: `public static async`)
  '(?:(?:async|static|public|private|protected|readonly|override|get|set|abstract)\\s+)*' +
  // optional generator `*`
  '\\*?\\s*' +
  // name (can be #private or constructor)
  '(#?\\w+)' +
  // optional generics `<T>` (non-nested)
  '\\s*(?:<[^>]*>)?' +
  // paren arg list — allow one level of nested parens
  '\\s*\\([^()]*(?:\\([^()]*\\)[^()]*)*\\)' +
  // optional return type annotation
  '(?:\\s*:\\s*[^={;]+)?' +
  // opens a body
  '\\s*\\{\\s*$'
);

function findMethodNodesInClass(lines, classNode) {
  const methods = [];
  const parenState = { depth: 0 };
  let depth = 0;
  // Seed: count the class-signature line so its opening `{` registers.
  for (const c of bracesOnly(lines[classNode.startLine], parenState)) {
    if (c === '{') depth++;
    else if (c === '}') depth--;
  }
  for (let i = classNode.startLine + 1; i <= classNode.endLine; i++) {
    // Depth === 1 means we're directly inside the class body — a candidate
    // for a method signature. Deeper depths are inside a method.
    if (depth === 1) {
      const m = lines[i].match(METHOD_RE);
      if (m) {
        const methodEnd = findBlockEnd(lines, i);
        if (methodEnd >= i) {
          const methodName = m[1].replace(/^#/, '');
          methods.push({
            id: `cls:${classNode.name}.fn:${methodName}`,
            kind: 'method',
            name: methodName,
            signature: lines[i].trim(),
            startLine: i,
            endLine: methodEnd,
          });
        }
      }
    }
    for (const c of bracesOnly(lines[i], parenState)) {
      if (c === '{') depth++;
      else if (c === '}') depth--;
    }
  }
  return methods;
}

// ---------------------------------------------------------------------------
// Markdown nodes: frontmatter + all headings (may be nested — h2 inside h1).

function slugify(s) {
  return s
    .toLowerCase()
    .replace(/[^a-z0-9\s-]/g, '')
    .trim()
    .replace(/\s+/g, '-');
}

function listMarkdownNodes(text) {
  const lines = text.split('\n');
  const nodes = [];
  let bodyStart = 0;

  // Frontmatter: `---` on line 0, content, closing `---`.
  if (lines[0] === '---') {
    for (let j = 1; j < lines.length; j++) {
      if (lines[j] === '---') {
        nodes.push({
          id: 'fm:frontmatter',
          kind: 'fm',
          name: 'frontmatter',
          signature: '---',
          startLine: 0,
          endLine: j,
        });
        bodyStart = j + 1;
        break;
      }
    }
  }

  // Collect every heading (levels 1-6) with its start line.
  const headings = [];
  for (let j = bodyStart; j < lines.length; j++) {
    const m = lines[j].match(/^(#{1,6})\s+(.+?)\s*$/);
    if (m) {
      const slug = slugify(m[2]);
      // All-punctuation heading → empty slug → skip, whole-file lock safer.
      if (!slug) continue;
      headings.push({ level: m[1].length, name: m[2], slug, startLine: j });
    }
  }

  // Compute each heading's endLine: line before the next same-or-higher
  // level heading, else EOF. Nodes from different levels may nest
  // (h2 under h1) — findEnclosingNode picks the smallest range that
  // covers the edit.
  for (let h = 0; h < headings.length; h++) {
    const cur = headings[h];
    let end = lines.length - 1;
    for (let k = h + 1; k < headings.length; k++) {
      if (headings[k].level <= cur.level) {
        end = headings[k].startLine - 1;
        break;
      }
    }
    nodes.push({
      id: `h${cur.level}:${cur.slug}`,
      kind: `h${cur.level}`,
      name: cur.name,
      signature: lines[cur.startLine],
      startLine: cur.startLine,
      endLine: end,
    });
  }

  return nodes;
}

// ---------------------------------------------------------------------------
// Public API

function listTopLevelNodes(text, ext) {
  const e = (ext || '').toLowerCase();
  if (CODE_EXTS.has(e)) return listCodeNodes(text, e);
  if (MD_EXTS.has(e))   return listMarkdownNodes(text);
  return [];
}

function findEnclosingNode(text, oldString, ext) {
  if (typeof text !== 'string' || typeof oldString !== 'string' || !oldString) return null;
  const e = (ext || '').toLowerCase();
  if (!SUPPORTED_EXTS.has(e)) return null;
  const idx = text.indexOf(oldString);
  if (idx < 0) return null;
  const startLine = text.slice(0, idx).split('\n').length - 1;
  const endLine = startLine + oldString.split('\n').length - 1;

  const nodes = listTopLevelNodes(text, e);
  let best = null;
  for (const n of nodes) {
    if (n.startLine <= startLine && n.endLine >= endLine) {
      if (!best || (n.endLine - n.startLine) < (best.endLine - best.startLine)) best = n;
    }
  }

  // Method-level descent: if the tightest enclosing node is a top-level
  // class, look for a method inside it that still covers the edit.
  if (best && best.kind === 'cls' && CODE_EXTS.has(e)) {
    const lines = text.split('\n');
    const methods = findMethodNodesInClass(lines, best);
    for (const m of methods) {
      if (m.startLine <= startLine && m.endLine >= endLine) {
        return m;
      }
    }
  }

  return best;
}

module.exports = { listTopLevelNodes, findEnclosingNode, isSupported, SUPPORTED_EXTS };
