#!/bin/sh
# Build agent-lock (release) and install it into the plugin's bin/native/ as
# agent-lock-<os>-<arch> (os: darwin|linux, arch: arm64|x86_64), plus a
# universal agent-lock-darwin when both macOS slices were built.
#
#   TARGETS="<rust triple> ..."  default: this machine, plus the other macOS
#                                arch when its standard library is installed
#   CARGO_CMD="cargo zigbuild"   the build command (default: cargo)
#   OUT=<dir>                    default: <repo>/bin/native
#   CARGO_PROFILE_RELEASE_STRIP=false   strip with /usr/bin/strip instead of rustc
#                                (always so for macOS binaries built on macOS)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${OUT:-$(dirname "$here")/bin/native}
if ! command -v cargo >/dev/null 2>&1; then
  for d in "$HOME/.cargo/bin" "$HOME"/.rustup/toolchains/stable-*/bin; do
    [ -x "$d/cargo" ] && PATH="$d:$PATH" && break
  done
fi
host=$(rustc -vV | sed -n 's/^host: //p')
case $host in *-apple-darwin) host_os=darwin ;; *) host_os=linux ;; esac
if [ -z "${TARGETS:-}" ]; then
  TARGETS=$host
  case $host in
    aarch64-apple-darwin) other=x86_64-apple-darwin ;;
    x86_64-apple-darwin) other=aarch64-apple-darwin ;;
    *) other= ;;
  esac
  if [ -n "$other" ] && [ -d "$(rustc --print sysroot)/lib/rustlib/$other/lib" ]; then
    TARGETS="$TARGETS $other"
  fi
fi

mkdir -p "$out"
darwin=
for t in $TARGETS; do
  case $t in
    aarch64-*) arch=arm64 ;;
    x86_64-*) arch=x86_64 ;;
    *) echo "build.sh: unsupported target $t" >&2; exit 1 ;;
  esac
  case $t in
    *-apple-darwin) os=darwin ;;
    *-linux-*) os=linux ;;
    *) echo "build.sh: unsupported target $t" >&2; exit 1 ;;
  esac
  # macOS: rustc strips with rust-objcopy, which some toolchains lack; Apple's strip always works
  strip=${CARGO_PROFILE_RELEASE_STRIP:-}
  [ "$os" = darwin ] && [ "$host_os" = darwin ] && [ -x /usr/bin/strip ] && strip=false
  (cd "$here" && CARGO_PROFILE_RELEASE_STRIP=${strip:-true} ${CARGO_CMD:-cargo} build --release --locked --target "$t")
  dst="$out/agent-lock-$os-$arch"
  cp "$here/target/$t/release/agent-lock" "$dst.tmp"
  if [ "$strip" = false ] && [ "$os" = "$host_os" ] && [ -x /usr/bin/strip ]; then
    /usr/bin/strip "$dst.tmp" 2>/dev/null || echo "build.sh: could not strip $t; kept its symbols" >&2
  fi
  if [ "$os" = darwin ] && command -v codesign >/dev/null 2>&1; then
    codesign -s - -f "$dst.tmp" 2>/dev/null || true # stripping drops the linker's ad-hoc signature
  fi
  chmod 755 "$dst.tmp"
  mv -f "$dst.tmp" "$dst" # a rename: hooks running the old binary keep their copy
  echo "built $dst ($(wc -c <"$dst" | tr -d ' ') bytes)"
  [ "$os" = darwin ] && darwin="$darwin $arch"
done

if [ "$darwin" = " arm64 x86_64" ] || [ "$darwin" = " x86_64 arm64" ]; then
  lipo -create -output "$out/agent-lock-darwin.tmp" "$out/agent-lock-darwin-arm64" "$out/agent-lock-darwin-x86_64"
  mv -f "$out/agent-lock-darwin.tmp" "$out/agent-lock-darwin"
  echo "built $out/agent-lock-darwin (universal)"
fi
