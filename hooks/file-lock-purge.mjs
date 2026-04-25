#!/usr/bin/env node
// SessionEnd hook — self-heal.
//
// A normal Edit/Write flow releases its own lock via file-lock-post.mjs.
// But a session that crashes, times out, or is force-terminated leaves
// markers behind. This hook runs on SessionEnd and:
//
//   1. Releases every marker held by THIS session (matched by agentId
//      prefix derived from session_id on stdin).
//   2. Strips any marker older than STALE_MS regardless of owner —
//      catches markers left by prior crashed sessions.
//
// Runs non-destructively: if it can't read session_id or the tree, it
// falls back to the stale-only sweep that the previous version did.

import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const require = createRequire(import.meta.url);
const here    = path.dirname(fileURLToPath(import.meta.url));
const lock    = require(path.resolve(here, '../lock/file-lock.cjs'));

const STALE_MS = 10 * 60 * 1000;
const root     = process.env.CLAUDE_PROJECT_DIR || path.resolve(here, '../..');

let raw = '';
for await (const chunk of process.stdin) raw += chunk;

let payload = {};
try { payload = JSON.parse(raw || '{}'); } catch { /* invalid JSON → no session_id, fall through to stale-only */ }

const sid = typeof payload.session_id === 'string' ? payload.session_id.slice(0, 8) : '';
const myAgentId = sid ? `claude-code-sess-${sid}` : null;

const all = lock.listLocks(root);
let purgedOwned = 0;
let purgedStale = 0;

for (const l of all) {
  if (myAgentId && l.agentId === myAgentId) {
    lock.releaseLock(l.target, { agentId: myAgentId, nodeId: l.nodeId });
    purgedOwned++;
    continue;
  }
  const age = Date.now() - new Date(l.startedAt).getTime();
  if (age > STALE_MS) {
    lock.releaseLock(l.target, { nodeId: l.nodeId });
    purgedStale++;
  }
}

if (purgedOwned) process.stderr.write(`[livehub] purged ${purgedOwned} own lock(s) from this session\n`);
if (purgedStale) process.stderr.write(`[livehub] purged ${purgedStale} stale lock(s)\n`);
process.exit(0);
