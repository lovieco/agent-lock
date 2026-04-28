#!/usr/bin/env node
// SessionEnd hook — release every lockfile owned by this session, plus
// any stale lockfile regardless of owner.
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
try { payload = JSON.parse(raw || '{}'); }
catch (e) { process.stderr.write(`[agent-lock] purge-hook: malformed JSON on stdin (${e.message})\n`); }

const sid       = typeof payload.session_id === 'string' ? payload.session_id.slice(0, 8) : '';
const myAgentId = sid ? `claude-code-sess-${sid}` : null;

const all = lock.listLocks(root);
let purgedOwned = 0, purgedStale = 0;
for (const l of all) {
  if (myAgentId && l.agentId === myAgentId) {
    lock.releaseLock(l.target, { agentId: myAgentId, root });
    purgedOwned++;
    continue;
  }
  if (lock._internal.isStale({ startedAt: l.startedAt }, STALE_MS)) {
    lock.releaseLock(l.target, { root });
    purgedStale++;
  }
}
if (purgedOwned) process.stderr.write(`[agent-lock] purged ${purgedOwned} own lock(s) from this session\n`);
if (purgedStale) process.stderr.write(`[agent-lock] purged ${purgedStale} stale lock(s)\n`);
process.exit(0);
