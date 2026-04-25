#!/usr/bin/env node
// PreToolUse hook for Write|Edit|MultiEdit — acquires a lock scoped to the
// actual top-level node being edited (function, class, const). Falls back
// to whole-file locking when the language is unsupported, the file is
// missing, the edit is a full Write, or the heuristic can't place the edit
// inside exactly one top-level node.
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const require   = createRequire(import.meta.url);
const here      = path.dirname(fileURLToPath(import.meta.url));
const lock      = require(path.resolve(here, '../lock/file-lock.cjs'));
const semantic  = require(path.resolve(here, '../lock/semantic.cjs'));
const fs        = require('node:fs');

if (process.env.CLAUDE_FILE_LOCK === '0') process.exit(0);

let raw = '';
for await (const chunk of process.stdin) raw += chunk;

let payload;
try { payload = JSON.parse(raw || '{}'); } catch { process.exit(0); }

const filePath = payload?.tool_input?.file_path;
if (!filePath) process.exit(0);

const sid        = (payload.session_id || 'unknown').slice(0, 8);
const myAgentId  = `claude-code-sess-${sid}`;
const toolName   = payload.tool_name || 'Edit';
/* c8 ignore next */
const input      = payload.tool_input || {};
const ext        = path.extname(filePath).toLowerCase();

function nodeIdForEdit(text, oldString) {
  if (!oldString) return '*';
  const node = semantic.findEnclosingNode(text, oldString, ext);
  return node ? node.id : '*';
}

function targetNodes() {
  if (toolName === 'Write') return ['*'];
  if (!fs.existsSync(filePath)) return ['*'];
  const text = fs.readFileSync(filePath, 'utf-8');
  if (toolName === 'MultiEdit' && Array.isArray(input.edits)) {
    const ids = input.edits.map(e => nodeIdForEdit(text, e.old_string));
    const uniq = [...new Set(ids)];
    return uniq.includes('*') ? ['*'] : uniq;
  }
  return [nodeIdForEdit(text, input.old_string)];
}

const nodes = targetNodes();
const acquired = [];
for (const nodeId of nodes) {
  const res = lock.acquireLock(filePath, {
    agentId: myAgentId,
    nodeId,
    reason: `Claude Code ${toolName}`,
  });
  if (!res.ok) {
    for (const a of acquired) lock.releaseLock(filePath, { agentId: myAgentId, nodeId: a });
    /* c8 ignore next — ageMs is always > 0 for real collisions */
    const ageS  = res.ageMs ? Math.floor(res.ageMs / 1000) + 's ago' : 'unknown';
    const scope = res.nodeId === '*' ? 'whole file' : `node ${res.nodeId}`;
    process.stderr.write(
      `[livehub] BLOCKED: ${filePath} (${scope}) is locked by agent "${res.heldBy}" ` +
      `since ${res.since} (${ageS}, reason: ${res.reason || '-'}). ` +
      `Wait, work on a different node, or set CLAUDE_FILE_LOCK=0 to override.\n`
    );
    process.exit(2);
  }
  acquired.push(nodeId);
}
process.exit(0);
