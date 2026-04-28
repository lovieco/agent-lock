#!/usr/bin/env node
// PostToolUse hook for Write|Edit|MultiEdit — releases this session's lock.
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
  process.stderr.write(`[agent-lock] post-hook: malformed JSON on stdin (${e.message})\n`);
  process.exit(0);
}

const filePath = payload?.tool_input?.file_path;
if (!filePath) process.exit(0);

const sid  = (payload.session_id || 'unknown').slice(0, 8);
const root = process.env.CLAUDE_PROJECT_DIR;
lock.releaseLock(filePath, { agentId: `claude-code-sess-${sid}`, root });
process.exit(0);
