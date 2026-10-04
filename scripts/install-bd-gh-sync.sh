#!/usr/bin/env bash
# install-bd-gh-sync.sh [VERSION] [DIR]: install the bd-gh-sync release binary
# into DIR (default ~/.local/bin), checking its SHA-256, and add DIR to
# $GITHUB_PATH when running in Actions. VERSION is a tag (v0.1.0) or "latest".
set -euo pipefail

version=${1:-latest}
dir=${2:-$HOME/.local/bin}
repo=${BD_GH_SYNC_REPO:-Takashiidobe/bd-gh-sync}

case $(uname -s) in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) echo "install-bd-gh-sync: unsupported OS $(uname -s)" >&2; exit 1 ;;
esac
case $(uname -m) in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) echo "install-bd-gh-sync: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
if [ "$os-$arch" = macos-x86_64 ]; then
  echo "install-bd-gh-sync: no Intel macOS build; use cargo install --git https://github.com/$repo" >&2
  exit 1
fi

case $version in
  latest) base=https://github.com/$repo/releases/latest/download ;;
  v*) base=https://github.com/$repo/releases/download/$version ;;
  *) base=https://github.com/$repo/releases/download/v$version ;;
esac
asset=bd-gh-sync-$os-$arch.tar.gz

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL --retry 3 -o "$tmp/$asset" "$base/$asset"
curl -fsSL --retry 3 -o "$tmp/$asset.sha256" "$base/$asset.sha256"
if command -v sha256sum >/dev/null; then
  (cd "$tmp" && sha256sum -c --quiet "$asset.sha256")
else
  (cd "$tmp" && shasum -a 256 -c --quiet "$asset.sha256")
fi
tar -xzf "$tmp/$asset" -C "$tmp" bd-gh-sync
mkdir -p "$dir"
install -m 0755 "$tmp/bd-gh-sync" "$dir/bd-gh-sync"
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$dir" >>"$GITHUB_PATH"
fi
"$dir/bd-gh-sync" --version
