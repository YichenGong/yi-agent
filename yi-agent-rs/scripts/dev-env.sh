#!/usr/bin/env bash
# 临时开发实例：一个 worktree 一个实例，随用随删。
#
# 用法：
#   dev-env.sh id                    打印当前 worktree 的实例 id
#   dev-env.sh up [--sync-skills]    进入实例子 shell
#   dev-env.sh run <cmd...>          在实例环境中执行一次命令
#   dev-env.sh down [--purge]        停止实例进程；--purge 连同目录一起删除
#
# 实例根目录：$YI_AGENT_INSTANCES_ROOT，默认 ~/.yi-agent-instances
set -euo pipefail

instances_root() {
  printf '%s\n' "${YI_AGENT_INSTANCES_ROOT:-$HOME/.yi-agent-instances}"
}

worktree_root() {
  git rev-parse --show-toplevel 2>/dev/null || pwd
}

instance_id() {
  local root branch slug hash
  root="$(worktree_root)"
  branch="$(git -C "$root" rev-parse --abbrev-ref HEAD 2>/dev/null || printf 'detached')"
  slug="$(basename "$root" | tr -c '[:alnum:]-' '-' | sed 's/--*/-/g; s/^-//; s/-$//' | cut -c1-32)"
  hash="$(printf '%s' "$root" | shasum -a 256 | awk '{print substr($1,1,6)}')"
  printf '%s-%s-%s\n' "$slug" "$(printf '%s' "$branch" | tr '/' '-' | cut -c1-24)" "$hash"
}

# 注意：`paths::instance_home()` 会把不以 `.yi-agent` 结尾的值自动补上后缀。
# 这里显式输出带后缀的路径，让脚本自己的断言与实际解析结果一致。
instance_home() {
  printf '%s/%s/.yi-agent\n' "$(instances_root)" "$(instance_id)"
}

ensure_instance() {
  local home
  home="$(instance_home)"
  mkdir -p "$home"
  if [ ! -f "$home/.env" ] && [ -f "$HOME/.yi-agent/.env" ]; then
    cp "$HOME/.yi-agent/.env" "$home/.env"
    printf 'dev-env: 已从 ~/.yi-agent/.env 复制到 %s/.env\n' "$home" >&2
    printf 'dev-env: 这是**真实凭据的副本**；实验跑量会消耗同一份额度，需要隔离请改用它。\n' >&2
  fi
  printf '%s\n' "$home"
}

export_env() {
  local home="$1"
  export YI_AGENT_HOME="$home"
  export YI_AGENT_RUNTIME_DIR="$home/runtime"
  export YI_AGENT_DISABLE_WATCHMAN=1
  export YI_AGENT_INSTANCE_ID="$(instance_id)"
}

sync_skills() {
  local home="$1" src
  src="$(worktree_root)/.yi-agent/skills"
  [ -d "$src" ] || return 0
  mkdir -p "$home/skills"
  cp -R "$src"/. "$home/skills"/ 2>/dev/null || true
  printf 'dev-env: 已同步项目 skill 到 %s/skills\n' "$home" >&2
}

cmd="${1:-}"
if [ -n "$cmd" ]; then shift; fi
case "$cmd" in
  id)
    instance_id
    ;;
  up)
    home="$(ensure_instance)"
    export_env "$home"
    if [ "${1:-}" = "--sync-skills" ]; then sync_skills "$home"; fi
    printf 'dev-env: 实例 %s\n' "$home" >&2
    printf 'dev-env: YI_AGENT_HOME=%s\n' "$YI_AGENT_HOME" >&2
    printf 'dev-env: 退出子 shell 后实例目录保留；销毁请跑 dev-env.sh down --purge\n' >&2
    exec "${SHELL:-/bin/bash}" -i
    ;;
  run)
    home="$(ensure_instance)"
    export_env "$home"
    exec "$@"
    ;;
  down)
    home="$(instance_home)"
    if [ -d "$home" ]; then
      # 先停前台值守（若曾拉起）
      if [ -f "$home/watch.pid" ]; then
        kill "$(cat "$home/watch.pid")" 2>/dev/null || true
        rm -f "$home/watch.pid"
      fi
      # 再停实例内 daemon；socket 属于该实例，不会误伤稳定版
      YI_AGENT_HOME="$home" YI_AGENT_RUNTIME_DIR="$home/runtime" \
        "${HOME}/.local/bin/yi-agent-dev" daemon stop >/dev/null 2>&1 || true
    fi
    if [ "${1:-}" = "--purge" ]; then
      rm -rf "$home"
      printf 'dev-env: 已删除 %s\n' "$home" >&2
    fi
    ;;
  *)
    sed -n '2,11p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
    exit 2
    ;;
esac
