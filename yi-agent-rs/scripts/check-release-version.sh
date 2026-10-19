#!/usr/bin/env bash
# 发布门禁：确认产物自称的版本就是这次要发布的版本。
#
# 用法: check-release-version.sh <cargo-version> <reported-version>
#
# 之所以在流水线里再查一遍：构建期版本标识由 git 状态决定，浅克隆、缺 tag、
# 本地脏改动都会让二进制悄悄变成 `-dev` 或 `+nogit`。二进制不会说谎，查它。
set -euo pipefail

if [ "$#" -ne 2 ]; then
  printf 'usage: %s <cargo-version> <reported-version>\n' "$0" >&2
  exit 2
fi

want="$1"
got="$2"

fail() { printf 'release gate FAILED: %s\n' "$*" >&2; exit 1; }

[ -n "$want" ] || fail "empty expected version"
[ -n "$got" ] || fail "the binary reported an empty version"

case "$got" in
  *-dev*) fail "the binary is a dev build ($got), expected $want" ;;
  *+nogit*) fail "the binary was built without git ($got), expected $want" ;;
esac

[ "$want" = "$got" ] || fail "the binary reports $got, but this release is $want"

printf 'release gate ok: %s\n' "$got"
