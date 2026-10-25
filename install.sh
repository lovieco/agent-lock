#!/usr/bin/env bash
# agent-lock CLI installer — clone, then symlink `agent-lock` onto your PATH.
#
# One-liner:
#   curl -fsSL https://raw.githubusercontent.com/lovieco/agent-lock/main/install.sh | bash
#
# Or with options:
#   curl -fsSL .../install.sh | bash -s -- --ref <tag> --bin ~/.local/bin
#
# Env / flags:
#   --home <dir>    where to clone the repo (default: ~/.agent-lock-core)
#   --repo <url>    source git URL         (default: https://github.com/lovieco/agent-lock.git)
#   --ref  <ref>    branch / tag / commit  (default: main)
#   --bin  <dir>    where to symlink the CLI (default: auto-detect from PATH)
#   --no-symlink    skip the PATH symlink step
#   --no-deps       accepted for compatibility; v3 has no dependencies
#
# What it does (in order):
#   1. Verifies git is installed (agent-lock is a native binary: no Node, no runtime).
#   2. Clones (or updates) the agent-lock repo into --home.
#   3. Checks that a prebuilt binary exists for this machine in bin/native/.
#   4. Symlinks <home>/bin/agent-lock into a writable dir on your PATH.
#   5. Prints next-step commands.
#
# This is the CLI for your own shell (status, doctor, selftest). Agents get
# agent-lock from the Claude Code plugin (claude plugin install agent-lock --marketplace lovieco/agent-lock).
# Nothing here touches your projects.

set -euo pipefail

AGENT_LOCK_HOME="${AGENT_LOCK_HOME:-$HOME/.agent-lock-core}"
AGENT_LOCK_REPO="${AGENT_LOCK_REPO:-https://github.com/lovieco/agent-lock.git}"
AGENT_LOCK_REF="${AGENT_LOCK_REF:-main}"
AGENT_LOCK_BIN="${AGENT_LOCK_BIN:-}"
DO_SYMLINK=1
DO_DEPS=1  # unused since v3 (no dependencies)

# --- args -----------------------------------------------------------------
while [ $# -gt 0 ]; do
  case "$1" in
    --home) AGENT_LOCK_HOME="$2"; shift 2 ;;
    --repo) AGENT_LOCK_REPO="$2"; shift 2 ;;
    --ref)  AGENT_LOCK_REF="$2";  shift 2 ;;
    --bin)  AGENT_LOCK_BIN="$2";  shift 2 ;;
    --no-symlink) DO_SYMLINK=0; shift ;;
    --no-deps)    DO_DEPS=0;    shift ;;
    -h|--help)
      sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) printf 'unknown flag: %s\n' "$1" >&2; exit 1 ;;
  esac
done

# --- logging --------------------------------------------------------------
if [ -t 1 ]; then C="\033[1;36m"; Y="\033[1;33m"; R="\033[1;31m"; X="\033[0m"
else               C=""; Y=""; R=""; X=""; fi
say()  { printf "${C}[agent-lock]${X} %s\n" "$*"; }
warn() { printf "${Y}[agent-lock]${X} %s\n" "$*" >&2; }
die()  { printf "${R}[agent-lock]${X} %s\n" "$*" >&2; exit 1; }

# --- pre-flight -----------------------------------------------------------
command -v git >/dev/null 2>&1 || die "git is required"

# --- clone / update -------------------------------------------------------
if [ -d "$AGENT_LOCK_HOME/.git" ]; then
  say "updating existing install at $AGENT_LOCK_HOME"
  git -C "$AGENT_LOCK_HOME" fetch --quiet origin
  git -C "$AGENT_LOCK_HOME" checkout --quiet "$AGENT_LOCK_REF"
  git -C "$AGENT_LOCK_HOME" reset --hard --quiet "origin/$AGENT_LOCK_REF" 2>/dev/null || true
else
  say "cloning $AGENT_LOCK_REPO → $AGENT_LOCK_HOME ($AGENT_LOCK_REF)"
  mkdir -p "$(dirname "$AGENT_LOCK_HOME")"
  if ! git clone --quiet --depth 1 --branch "$AGENT_LOCK_REF" "$AGENT_LOCK_REPO" "$AGENT_LOCK_HOME" 2>/dev/null; then
    git clone --quiet "$AGENT_LOCK_REPO" "$AGENT_LOCK_HOME"
    git -C "$AGENT_LOCK_HOME" checkout --quiet "$AGENT_LOCK_REF"
  fi
fi

[ -x "$AGENT_LOCK_HOME/bin/agent-lock" ] || die "bin/agent-lock missing — unexpected repo layout"

# --- native binary --------------------------------------------------------
"$AGENT_LOCK_HOME/bin/agent-lock" version >/dev/null 2>&1 \
  && [ -n "$("$AGENT_LOCK_HOME/bin/agent-lock" version 2>/dev/null)" ] \
  || warn "no prebuilt binary for $(uname -sm) in bin/native/ yet — build one with rust/build.sh (needs cargo)"

# --- symlink --------------------------------------------------------------
pick_bin_dir() {
  if [ -n "$AGENT_LOCK_BIN" ]; then printf '%s' "$AGENT_LOCK_BIN"; return; fi
  for d in "$HOME/.local/bin" "$HOME/bin" /usr/local/bin; do
    [ -d "$d" ] || continue
    case ":$PATH:" in *":$d:"*) : ;; *) continue ;; esac
    if [ -w "$d" ]; then printf '%s' "$d"; return; fi
    if [ "$d" = "/usr/local/bin" ] && command -v sudo >/dev/null 2>&1; then
      printf '%s' "$d"; return
    fi
  done
}

symlink_path=""
if [ "$DO_SYMLINK" = 1 ]; then
  bin_dir=$(pick_bin_dir)
  if [ -z "$bin_dir" ]; then
    warn "no writable bin dir on PATH found — add $AGENT_LOCK_HOME/bin to PATH manually"
  else
    symlink_path="$bin_dir/agent-lock"
    if [ -w "$bin_dir" ]; then
      ln -sfn "$AGENT_LOCK_HOME/bin/agent-lock" "$symlink_path"
    else
      say "need sudo to symlink into $bin_dir"
      sudo ln -sfn "$AGENT_LOCK_HOME/bin/agent-lock" "$symlink_path"
    fi
    say "symlinked $symlink_path → bin/agent-lock"
  fi
fi

# --- done -----------------------------------------------------------------
installed_ver=$(git -C "$AGENT_LOCK_HOME" describe --tags --always 2>/dev/null || printf '%s' "$AGENT_LOCK_REF")
say "installed $installed_ver in $AGENT_LOCK_HOME"

cat <<EOF

  Next steps:

    The Claude Code plugin — this is what makes agents coordinate:
      claude plugin install agent-lock --marketplace lovieco/agent-lock

    In a repo, from your shell:
      agent-lock status       claims, sessions and locks in this checkout
      agent-lock doctor       check the checkout and your setup (--fix repairs)
      agent-lock install      enable the plugin for everyone on the project
                              (writes .claude/settings.json; commit it)
      agent-lock selftest     end-to-end smoke test in a throwaway repo
      agent-lock help         all commands

  Update later:
    curl -fsSL https://raw.githubusercontent.com/lovieco/agent-lock/main/install.sh | bash

  Uninstall:
    rm -rf "$AGENT_LOCK_HOME"$( [ -n "$symlink_path" ] && printf ' && rm -f "%s"' "$symlink_path" )

EOF
