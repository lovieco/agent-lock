# agent-lock

agent-lock (Lovie.co) is a Claude Code plugin that lets many agents share one
git checkout on one branch. A single native Rust binary (std only) acts as the
hooks, the git and package-manager shims, and the CLI.

- Operating rules for agents in a shared checkout, this one included: `skills/agent-lock/SKILL.md`
- Developing this repo (layout, build, test, release binaries): `.claude/skills/agent-lock-dev/SKILL.md`
- Normative spec: `SPEC.md`
- Roadmap: `TODO.md`
