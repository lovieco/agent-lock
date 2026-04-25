#!/usr/bin/env node
// PostToolUse hook for Write|Edit|MultiEdit — releases every lock this
// session holds on the target file (regardless of which nodes).
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
/* c8 ignore next */
try { payload = JSON.parse(raw || '{}'); } catch { process.exit(0); }

const filePath = payload?.tool_input?.file_path;
if (!filePath) process.exit(0);

const sid = (payload.session_id || 'unknown').slice(0, 8);
lock.releaseLock(filePath, { agentId: `claude-code-sess-${sid}` });
process.exit(0);
