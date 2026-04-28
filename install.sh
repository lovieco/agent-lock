#!/usr/bin/env bash
# agent-lock installer — clone, wire up, symlink onto PATH.
#
# One-liner:
#   curl -fsSL https://raw.githubusercontent.com/lovieco/agent-lock/main/install.sh | bash
#
# Or with options:
#   curl -fsSL .../install.sh | bash -s -- --ref v0.1.0 --bin ~/.local/bin
#
# Env / flags:
#   --home <dir>    where to clone the repo (default: ~/.agent-lock-core)
#   --repo <url>    source git URL         (default: https://github.com/lovieco/agent-lock.git)
#   --ref  <ref>    branch / tag / commit  (default: main)
#   --bin  <dir>    where to symlink the CLI (default: auto-detect from PATH)
#   --no-symlink    skip the PATH symlink step
#   --no-deps       skip `npm install` (tests won't run but the CLI still works)
#
# What it does (in order):
#   1. Verifies Node >= 16 and git are installed.
#   2. Clones (or updates) the agent-lock repo into --home.
#   3. Runs `npm install` for the test suite (unless --no-deps).
#   4. Symlinks <home>/bin/agent-lock into a writable dir on your PATH.
#   5. Prints next-step commands.
#
# The install script does NOT touch any of your projects. It just makes the
# `agent-lock` CLI available; per-project wiring happens when you run
# `agent-lock install /path/to/project` later.

set -euo pipefail

LIVEHUB_HOME="${LIVEHUB_HOME:-$HOME/.agent-lock-core}"
LIVEHUB_REPO="${LIVEHUB_REPO:-https://github.com/lovieco/agent-lock.git}"
LIVEHUB_REF="${LIVEHUB_REF:-main}"
LIVEHUB_BIN="${LIVEHUB_BIN:-}"
DO_SYMLINK=1
DO_DEPS=1

# --- args -----------------------------------------------------------------
while [ $# -gt 0 ]; do
  case "$1" in
    --home) LIVEHUB_HOME="$2"; shift 2 ;;
    --repo) LIVEHUB_REPO="$2"; shift 2 ;;
    --ref)  LIVEHUB_REF="$2";  shift 2 ;;
    --bin)  LIVEHUB_BIN="$2";  shift 2 ;;
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
command -v node >/dev/null 2>&1 || die "node is required (https://nodejs.org)"
command -v git  >/dev/null 2>&1 || die "git is required"
node_major=$(node -p 'parseInt(process.versions.node.split(".")[0],10)')
[ "$node_major" -ge 16 ] || die "node >= 16 required (have $node_major)"

# --- clone / update -------------------------------------------------------
if [ -d "$LIVEHUB_HOME/.git" ]; then
  say "updating existing install at $LIVEHUB_HOME"
  git -C "$LIVEHUB_HOME" fetch --quiet origin
  git -C "$LIVEHUB_HOME" checkout --quiet "$LIVEHUB_REF"
  git -C "$LIVEHUB_HOME" reset --hard --quiet "origin/$LIVEHUB_REF" 2>/dev/null || true
else
  say "cloning $LIVEHUB_REPO → $LIVEHUB_HOME ($LIVEHUB_REF)"
  mkdir -p "$(dirname "$LIVEHUB_HOME")"
  if ! git clone --quiet --depth 1 --branch "$LIVEHUB_REF" "$LIVEHUB_REPO" "$LIVEHUB_HOME" 2>/dev/null; then
    git clone --quiet "$LIVEHUB_REPO" "$LIVEHUB_HOME"
    git -C "$LIVEHUB_HOME" checkout --quiet "$LIVEHUB_REF"
  fi
fi

[ -x "$LIVEHUB_HOME/bin/agent-lock" ] || die "bin/agent-lock missing — unexpected repo layout"

# --- dev deps (optional) --------------------------------------------------
if [ "$DO_DEPS" = 1 ] && [ -f "$LIVEHUB_HOME/package.json" ] && [ ! -d "$LIVEHUB_HOME/node_modules" ]; then
  if command -v npm >/dev/null 2>&1; then
    say "installing dev dependencies (for npm test / coverage)"
    (cd "$LIVEHUB_HOME" && npm install --no-audit --no-fund --silent) \
      || warn "npm install failed — tests won't run, but the CLI still works"
  else
    warn "npm not found — skipping dev deps"
  fi
fi

# --- symlink --------------------------------------------------------------
pick_bin_dir() {
  if [ -n "$LIVEHUB_BIN" ]; then printf '%s' "$LIVEHUB_BIN"; return; fi
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
    warn "no writable bin dir on PATH found — add $LIVEHUB_HOME/bin to PATH manually"
  else
    symlink_path="$bin_dir/agent-lock"
    if [ -w "$bin_dir" ]; then
      ln -sfn "$LIVEHUB_HOME/bin/agent-lock" "$symlink_path"
    else
      say "need sudo to symlink into $bin_dir"
      sudo ln -sfn "$LIVEHUB_HOME/bin/agent-lock" "$symlink_path"
    fi
    say "symlinked $symlink_path → bin/agent-lock"
  fi
fi

# --- done -----------------------------------------------------------------
installed_ver=$(git -C "$LIVEHUB_HOME" describe --tags --always 2>/dev/null || printf '%s' "$LIVEHUB_REF")
say "installed $installed_ver in $LIVEHUB_HOME"

cat <<EOF

  Next steps:

    agent-lock help                                  show usage
    agent-lock install /path/to/your/project         wire a project up
    cd /path/to/your/project && agent-lock test      end-to-end smoke test

  Update later:
    curl -fsSL $LIVEHUB_REPO/raw/main/install.sh | bash

  Uninstall:
    rm -rf "$LIVEHUB_HOME"$( [ -n "$symlink_path" ] && printf ' && rm -f "%s"' "$symlink_path" )

EOF
