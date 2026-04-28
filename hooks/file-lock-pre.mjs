#!/usr/bin/env node
// PreToolUse hook for Write|Edit|MultiEdit — acquires a whole-file lock
// in the central lockdir. Never modifies the target file.
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const require = createRequire(import.meta.url);
const here    = path.dirname(fileURLToPath(import.meta.url));
const lock    = require(path.resolve(here, '../lock/file-lock.cjs'));

if (process.env.CLAUDE_FILE_LOCK === '0') process.exit(0);

let raw = '';
for await (const chunk of process.stdin) raw += chunk;

let payload;
try { payload = JSON.parse(raw || '{}'); }
catch (e) {
  process.stderr.write(`[agent-lock] pre-hook: malformed JSON on stdin (${e.message})\n`);
  process.exit(0);
}

const filePath = payload?.tool_input?.file_path;
if (!filePath) process.exit(0);

const sid       = (payload.session_id || 'unknown').slice(0, 8);
const myAgentId = `claude-code-sess-${sid}`;
const toolName  = payload.tool_name || 'Edit';
const root      = process.env.CLAUDE_PROJECT_DIR;

const res = lock.acquireLock(filePath, {
  agentId: myAgentId,
  reason:  `Claude Code ${toolName}`,
  root,
});

if (!res.ok) {
  const ageS = res.ageMs ? Math.floor(res.ageMs / 1000) + 's ago' : 'just now';
  process.stderr.write(
    `[agent-lock] BLOCKED: ${filePath} is locked by agent "${res.heldBy}" ` +
    `since ${res.since} (${ageS}, reason: ${res.reason || '-'}). ` +
    `Wait, edit a different file, or set CLAUDE_FILE_LOCK=0 to override.\n`
  );
  process.exit(2);
}
process.exit(0);
