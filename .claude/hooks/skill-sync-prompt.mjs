#!/usr/bin/env node
// PostToolUse hook: after edits to skills/agent-lock/SKILL.md, prompt Claude
// to ask the user whether to copy it to ~/.claude/skills/agent-lock/SKILL.md
// (independent global copy, no auto-sync).

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';

let input;
try {
  input = JSON.parse(fs.readFileSync(0, 'utf8'));
} catch {
  process.exit(0);
}

const filePath = input?.tool_input?.file_path;
if (!filePath) process.exit(0);

const repoRoot = process.env.CLAUDE_PROJECT_DIR
  || path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const source = path.join(repoRoot, 'skills', 'agent-lock', 'SKILL.md');
const dest = path.join(os.homedir(), '.claude', 'skills', 'agent-lock', 'SKILL.md');

let resolvedFile;
try {
  resolvedFile = path.resolve(filePath);
} catch {
  process.exit(0);
}
if (resolvedFile !== source) process.exit(0);

let inSync = false;
try {
  inSync = fs.readFileSync(source, 'utf8') === fs.readFileSync(dest, 'utf8');
} catch { /* dest missing → still prompt */ }
if (inSync) process.exit(0);

const msg = [
  'skills/agent-lock/SKILL.md was just modified and differs from ~/.claude/skills/agent-lock/SKILL.md.',
  'Ask the user whether to copy it into ~/.claude/skills/agent-lock/SKILL.md (independent global copy, no auto-sync).',
  `If they confirm, run: mkdir -p "${path.dirname(dest)}" && cp "${source}" "${dest}"`,
].join(' ');

process.stdout.write(JSON.stringify({
  hookSpecificOutput: {
    hookEventName: 'PostToolUse',
    additionalContext: msg,
  },
}));
process.exit(0);
