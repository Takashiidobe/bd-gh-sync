#!/usr/bin/env bash
# install-bd.sh VERSION [DIR]: install the beads `bd` release binary into DIR
# (default ~/.local/bin) straight from the GitHub release, and add DIR to
# $GITHUB_PATH when running in Actions.
set -euo pipefail

version=${1:?usage: install-bd.sh VERSION [DIR]}
version=${version#v}
dir=${2:-$HOME/.local/bin}

case $(uname -s) in
  Linux) os=linux ;;
  Darwin) os=darwin ;;
  *) echo "install-bd: unsupported OS $(uname -s)" >&2; exit 1 ;;
esac
case $(uname -m) in
  x86_64|amd64) arch=amd64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo "install-bd: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

url=https://github.com/gastownhall/beads/releases/download/v$version/beads_${version}_${os}_${arch}.tar.gz
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL --retry 3 -o "$tmp/bd.tgz" "$url"
tar -xzf "$tmp/bd.tgz" -C "$tmp" bd
mkdir -p "$dir"
install -m 0755 "$tmp/bd" "$dir/bd"
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$dir" >>"$GITHUB_PATH"
fi
"$dir/bd" version
