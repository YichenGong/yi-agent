#!/usr/bin/env bash
# 发布门禁：确认待发布的 tag 落在基线分支（默认 main）可达的提交上。
#
# 用法: check-tag-on-main.sh <tag> [base-branch] [repo-dir]
#
# 为什么需要它：`release.yml` 只由 tag 触发，且只跑打包、不跑测试。所以
# 一个打在未合并功能分支上的 tag 会被**直接发布出去**——线上版本从此脱离
# 主干，主干上后续的修复永远进不了这个版本。这里在打包前把它拦下来。
#
# 注意 `git tag --contains` 之类的方向容易搞反：我们要问的是「tag 指向的
# 提交是否是 base 的祖先」，即 `git merge-base --is-ancestor <tag^{}> <base>`。
set -euo pipefail

if [ "$#" -lt 1 ] || [ "$#" -gt 3 ]; then
  printf 'usage: %s <tag> [base-branch] [repo-dir]\n' "$0" >&2
  exit 2
fi

tag="$1"
base="${2:-main}"
repo="${3:-.}"

fail() { printf 'release gate FAILED: %s\n' "$*" >&2; exit 1; }

[ -n "$tag" ] || fail "empty tag name"

# tag 必须存在，且能解析到具体提交（annotated tag 要先 peel）。
git -C "$repo" rev-parse --verify --quiet "refs/tags/$tag^{commit}" >/dev/null \
  || fail "tag '$tag' does not exist (or does not point at a commit)"

# 基线分支必须存在且能解析。
git -C "$repo" rev-parse --verify --quiet "$base^{commit}" >/dev/null \
  || fail "base branch '$base' does not exist"

if ! git -C "$repo" merge-base --is-ancestor "refs/tags/$tag^{commit}" "$base^{commit}"; then
  commit="$(git -C "$repo" rev-parse --short "refs/tags/$tag^{commit}")"
  fail "tag '$tag' points at $commit, which is not reachable from '$base'. Tag a commit on '$base' (merge the work first, then tag)."
fi

printf 'release gate ok: %s is on %s\n' "$tag" "$base"
