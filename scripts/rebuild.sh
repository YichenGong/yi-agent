#!/usr/bin/env bash
#
# 本机开发重编 —— 把改完的代码编成能直接用的东西。
#
# 覆盖三个构建单元（都不走 CI 的发布流程）：
#   A. yi-agent-rs/                 Rust workspace → yi-agent CLI
#   B. desktop/                     Tauri 桌面 App（sidecar 拷 A 的产物）
#   C. plugins/superpowers-kanban/  独立 workspace → 看板插件二进制
#
# 不含：dmg/npm 发布、三平台交叉编译、中继 VPS、iOS 构建。iOS 需要 Xcode +
# 真机/模拟器组件与 Apple 签名，与本机 dev 迭代不同路；构建方式见
# docs/relay-deploy.md §4.2（`cd desktop && npm run ios:build`）。
#
# 用法：
#   ./scripts/rebuild.sh             # A → B → C 全编（A 先于 B），默认装开发版
#   ./scripts/rebuild.sh cli         # 只编 A，装到 ~/.local/bin/yi-agent-dev
#   ./scripts/rebuild.sh desktop     # 只编 B（需 A 的产物已存在）
#   ./scripts/rebuild.sh plugin      # 只编 C
#   ./scripts/rebuild.sh cli --profile stable
#                                    # 编稳定版，覆盖 ~/.cargo/bin/yi-agent
#   ./scripts/rebuild.sh cli --force # 跳过"无并发 cargo"检查（自担风险）
#   ./scripts/rebuild.sh stop        # 只停进程不编译（App / daemon / 值守 / 看板）
#   ./scripts/rebuild.sh --help
#
# A 编完会把 /opt/homebrew/bin/yi-agent 这个软链指到本次装好的二进制，因为
# 登录 shell 的 PATH 里 /opt/homebrew/bin 排在 ~/.local/bin 与 ~/.cargo/bin
# 之前：不接管它，「装好了」和「终端里敲 yi-agent 用的是它」就是两件事。
# 两个 profile 的区别只在二进制落点——dev 写 ~/.local/bin/yi-agent-dev（不
# 覆盖稳定版二进制），stable 写 ~/.cargo/bin/yi-agent；软链若已是真实文件
# 则拒绝覆盖（见 docs/superpowers/specs/2026-11-01-rebuild-install-global-design.md）。
#
# 任何构建子命令都会**先停掉所有依附进程**再动手：App、app-server、
# daemon serve、launchd 值守、被托管的 superpowers-kanban，外加终端里的
# yi-agent。重建会结束当前正在进行的会话——这是刻意的，否则旧 bundle 里的
# 二进制会一直服务下去，重建等于没做（见 docs/superpowers/specs/
# 2026-11-01-rebuild-stop-dependents-design.md）。
#
set -euo pipefail

# --- 0. 定位仓库根（脚本自身位置反推） -----------------------------------
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# --- PATH 自愈 -----------------------------------------------------------
# 非交互 shell 的 PATH 只有 /usr/bin:/bin:/usr/sbin:/sbin，cargo/node/zig
# 全都找不到。这里显式补上真实位置（与 desktop/scripts/release-dmg.sh 同款做法）。
export PATH="/opt/homebrew/bin:/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH"

# --- 常量 ----------------------------------------------------------------
RS_DIR="$ROOT/yi-agent-rs"
DESKTOP_DIR="$ROOT/desktop"
PLUGIN_DIR="$ROOT/plugins/superpowers-kanban"
APP_DEST="/Applications/yi-agent.app"
PLUGIN_BIN_DEST="/opt/homebrew/bin/superpowers-kanban"
SKILL_DEST="$HOME/.yi-agent/skills/superpowers-kanban/SKILL.md"

# 三个安装落点。默认值就是生产位置；测试用环境变量把它们指到临时目录，
# 免得为了验安装逻辑真编一次 Rust，也免得测试碰到 /opt/homebrew/bin。
LOCAL_BIN_DIR="${REBUILD_LOCAL_BIN_DIR:-$HOME/.local/bin}"
CARGO_BIN_DIR="${REBUILD_CARGO_BIN_DIR:-$HOME/.cargo/bin}"
GLOBAL_BIN="${REBUILD_GLOBAL_BIN:-/opt/homebrew/bin/yi-agent}"

# 入口段（原样保留的开关）也在这里初始化：安装函数要读 PROFILE，
# 而 PROFILE 在 source 场景下不会被入口段赋默认值。
FORCE=0
# 默认装开发版：`~/.local/bin/yi-agent-dev`，不覆盖稳定版二进制。
# 稳定版只应来自发布渠道（npm / Homebrew）；本机覆盖它必须显式
# --profile stable，否则一次随手重编就会把「稳定版」定义污染成某个下午的 main。
PROFILE="${PROFILE:-dev}"
CMD="all"

# 版本对齐 yi-agent-rs 的 workspace 版本（App bundle 与 CLI 都用它）
VERSION="$(grep '^version' "$RS_DIR/Cargo.toml" | head -1 | awk -F'"' '{print $2}')"

log()  { printf '\n=== %s ===\n' "$*"; }
warn() { printf 'WARN: %s\n' "$*" >&2; }
die()  { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

usage() {
  # 截取 shebang 之后连续的注释行，遇到第一行非注释即停。
  # 不用固定行号：头部注释一旦增删，写死的区间会让 --help 静默截断或吞进代码。
  awk 'NR==1{next} /^#/{sub(/^# ?/,""); print; next} {exit}' "${BASH_SOURCE[0]}"
}

# --- 前置守卫 -----------------------------------------------------------
preflight() {
  log "前置检查"

  for tool in cargo node npm; do
    command -v "$tool" >/dev/null 2>&1 || die "'$tool' 不在 PATH 上（PATH=${PATH}）"
  done
  echo "工具: cargo=$(cargo --version | awk '{print $2}') node=$(node -v) npm=$(npm -v)"

  # 并发跑 cargo 会 exit 137（OOM），且被中断的编译会留僵尸进程持有 target 锁。
  # 这里的检查无法区分 cargo 的 target 目录，故偏保守——不同 worktree / 独立
  # workspace 的 cargo 其实互不锁定，但宁可按最坏情况拦下，用 --force 显式放行。
  if pgrep -qf 'cargo (build|test|check|clippy|install)' 2>/dev/null; then
    if [ "$FORCE" = 1 ]; then
      warn "--force：跳过并发检查。当前在跑的 cargo 进程："
      pgrep -fl cargo 2>/dev/null | sed 's/^/  /' >&2
    else
      warn "检测到正在运行的 cargo 进程；并发编译可能导致 exit 137："
      pgrep -fl cargo 2>/dev/null | sed 's/^/  /' >&2
      die "请等它结束，或加 --force 自行承担风险（不同 target/workspace 通常安全）。"
    fi
  fi

  [ -n "$VERSION" ] || die "读不到 yi-agent-rs/Cargo.toml 的版本号"
  echo "版本: $VERSION"
  echo "仓库根: $ROOT"
}

# --- 停掉所有依附进程 ---------------------------------------------------
# 重建必须先把旧二进制从进程里赶走，否则 /Applications 里的 bundle 就算换成
# 新的也没人用：daemon serve 已 setsid 脱离、resident watch 由 launchd
# KeepAlive 托管、superpowers-kanban 又挂在 daemon 名下。顺序与理由见
# docs/superpowers/specs/2026-11-01-rebuild-stop-dependents-design.md §2.1。

WATCHMAN_LABEL="ai.yi-agent.resident-daemon-watchman"
LEGACY_WATCHMAN_LABEL="ai.yi-agent.board-watchman"

# 判定一行 `ps` 输出是否属于要停的进程。
#
# 匹配 argv[0] 的 basename（而不是整行包含 `yi-agent`）：整行匹配会误杀
# `vi yi-agent/docs/x.md`、`grep yi-agent`、`tail -f .../yi-agent.log`。
# 第二条兜底覆盖 bundle 内的 Helper：desktop 的 basename 不叫 yi-agent，
# 但路径在 bundle 里。
# 不能改用 `pgrep -f`：macOS 的 pgrep -f 只匹配 comm 不匹配 argv，
# 对 `/Applications/.../desktop` 恒不命中（本文件 build_desktop 的旧注释
# 亦记过这个坑）。
is_target_command() {
  local command="$1" exe base
  case "$command" in
    *"/Applications/yi-agent.app/Contents/"*) return 0 ;;
  esac
  exe="${command%% *}"
  base="${exe##*/}"
  case "$base" in
    yi-agent|yi-agent-dev|superpowers-kanban) return 0 ;;
  esac
  return 1
}

# 收集应停止的 pid（不含 protected 名单），一行一个。
#
# $1 是一个以空格分隔的「不要停」pid 列表。
target_pids() {
  local protected=" $1 " line pid command
  ps -axo pid=,ppid=,command= 2>/dev/null | while IFS= read -r line; do
    # 前导空格 + pid + ppid，之后是 command（command 本身可含空格）
    pid="$(printf '%s' "$line" | awk '{print $1}')"
    [ -n "$pid" ] || continue
    case "$protected" in *" $pid "*) continue ;; esac
    command="$(printf '%s' "$line" | awk '{$1=""; $2=""; sub(/^ +/, ""); print}')"
    [ -n "$command" ] || continue
    is_target_command "$command" || continue
    printf '%s %s\n' "$pid" "$command"
  done
}

# 本次脚本自身的祖先进程链（pid 空格分隔）。
#
# 从 App 里发起的重建（agent 进程就跑在 app-server 里）如果无条件 kill，
# 会把发起脚本的祖先一起杀掉，脚本自己在 rm -rf 与 cp 之间被打断，留下
# 残缺的 bundle。故这一串 pid 一律跳过（spec §2.4）。
ancestor_pids() {
  local pid=$$ line
  while [ "${pid:-1}" -gt 1 ] 2>/dev/null; do
    printf '%s ' "$pid"
    line="$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')"
    [ -n "$line" ] || break
    pid="$line"
  done
  printf '%s ' "$pid"
}

# 等一批 pid 消失。$1 秒上限，$2... 要等的 pid。
wait_gone() {
  local seconds="$1"; shift
  local deadline=$(( $(date +%s) + seconds ))
  while [ "$(date +%s)" -le "$deadline" ]; do
    local alive=0 pid
    for pid in "$@"; do
      if kill -0 "$pid" 2>/dev/null; then alive=1; fi
    done
    if [ "$alive" = 0 ]; then return 0; fi
    sleep 0.2
  done
  return 1
}

# 测试残留的 helper 子进程：单独一类，不进主匹配。
#
# 这些是本仓库测试二进制的 `--exact <helper> --ignored` 子进程（见
# `crates/yi-agent-supervisors/tests/support/process_fixture.rs`）。它们与
# 生产的 `yi-agent` 无关，所以**不能**塞进 `is_target_command`：那条规则按
# basename 精确匹配是为了避免误杀，放宽成「路径含 deps/」会命中任何在
# `target/` 下跑的东西。
#
# 为什么要单独清扫：managed 子进程在 `pre_exec` 里 `setsid()`，自成会话与
# 进程组。测试二进制退出（或被 Ctrl-C）后它们收不到任何信号，直接挂到
# launchd 名下继续跑。`ProcessRegistry` 的 `Drop` 已经兜底回收，但历史遗留
# 与被打断的编译产物仍可能留在表里。
#
# 规则刻意收窄成三条同时成立：路径在 `target/{debug,release}/deps/`、
# 带 `--exact`、带 `--ignored`——正常构建产物与正在跑的 `cargo test` 主进程
# 都不匹配。
test_helper_pids() {
  ps -axo pid=,command= 2>/dev/null | while IFS= read -r line; do
    case "$line" in
      # 测试二进制的 `--exact <helper> --ignored` 子进程
      # （`crates/yi-agent-supervisors/tests/support/process_fixture.rs`）。
      #
      # 只认 deps/ 下的这一形态。`internal-runtime guardian` 不在此列：它的
      # basename 就是 `yi-agent`，主流程的 `is_target_command` 已经按 basename
      # 全量覆盖，在这里重复一遍只会让两处规则各自漂移。
      */target/debug/deps/*|*/target/release/deps/*) ;;
      *) continue ;;
    esac
    case "$line" in
      *--exact*) ;;
      *) continue ;;
    esac
    case "$line" in
      *--ignored*) ;;
      *) continue ;;
    esac
    printf '%s\n' "${line%% *}"
  done
}

# 停止测试二进制留下的 helper。返回已停止的个数（供日志用）。
stop_test_helpers() {
  local pids count=0
  pids="$(test_helper_pids | tr '\n' ' ')"
  [ -n "${pids// /}" ] || return 0
  for pid in $pids; do
    # 先 CONT：被 SIGSTOP 暂停的进程收到 SIGTERM 只会挂起信号，永不退出。
    kill -CONT "$pid" 2>/dev/null || true
    if kill -TERM "$pid" 2>/dev/null; then
      count=$((count + 1))
    fi
  done
  # 给它们一点时间走 SIGTERM；仍在的交给 stop_dependents 之外的收尾。
  wait_gone 3 $pids || true
  for pid in $pids; do
    kill -KILL "$pid" 2>/dev/null || true
  done
  if [ "$count" -gt 0 ]; then
    echo "测试残留 -> 已停止 $count 个 helper"
  fi
}

# stop_dependents <子命令名>：把依附旧二进制的进程停干净。
#
# <子命令名> 用于事件日志与汇总文案；`stop` 只停不编。
stop_dependents() {
  local cmd="${1:-all}"
  log "停止依附旧二进制的进程"

  stop_test_helpers

  # ① 先 bootout 值守。KeepAlive=true：先 kill 会在 10s 内复活一个新的
  #    resident watch，而它的第一个动作就是把刚杀掉的 daemon 拉回来。
  #    只 bootout、不删 plist：下次登录 launchd 会自动重新加载，值守自回来
  #    （spec §2.2）。
  local domain="gui/$(id -u)" label
  for label in "$WATCHMAN_LABEL" "$LEGACY_WATCHMAN_LABEL"; do
    if launchctl print "$domain/$label" >/dev/null 2>&1; then
      if launchctl bootout "$domain/$label" >/dev/null 2>&1; then
        echo "值守 -> 已 bootout $label"
      else
        warn "bootout $label 失败；继续尝试停止进程"
      fi
    fi
  done

  local protected
  protected="$(ancestor_pids)"
  local entries
  entries="$(target_pids "$protected")"

  # 祖先里命中的单独告警：它们会留着旧二进制。
  local skipped=""
  if [ -n "$protected" ]; then
    local pid
    for pid in $protected; do
      local command
      command="$(ps -o command= -p "$pid" 2>/dev/null || true)"
      [ -n "$command" ] || continue
      is_target_command "$command" || continue
      skipped="$skipped$pid $command"$'\n'
    done
  fi

  if [ -z "$entries" ]; then
    echo "没有需要停止的进程。"
  else
    local pids=""
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      pids="$pids ${line%% *}"
      echo "  停止 -> $(printf '%s' "$line" | cut -c1-140)"
    done <<< "$entries"

    # ② SIGTERM：daemon 走正常退出路径，Supervisor::stop_all 会先给看板写
    #    stopped 事件再回收子进程；直接 SIGKILL 会留下没有回收者的孤儿。
    local pid
    for pid in $pids; do
      kill -TERM "$pid" 2>/dev/null || true
    done

    # ③ 最多等 5s；④ 仍活着的 SIGKILL 兜底；⑤ 再等 2s 确认。
    # shellcheck disable=SC2086
    if ! wait_gone 5 $pids; then
      warn "5s 内仍有进程存活，SIGKILL 兜底"
      for pid in $pids; do
        if kill -0 "$pid" 2>/dev/null; then
          kill -KILL "$pid" 2>/dev/null || true
        fi
      done
      # shellcheck disable=SC2086
      wait_gone 2 $pids || die "仍有进程停不下来，中止（不会去动 /Applications）"
    fi
  fi

  if [ -n "$skipped" ]; then
    warn "以下是本次重建的祖先进程，已跳过（它们仍持有旧二进制）："
    printf '%s' "$skipped" | while IFS= read -r line; do
      [ -n "$line" ] && printf '  %s\n' "$(printf '%s' "$line" | cut -c1-140)" >&2
    done
    warn "它们是本次重建的祖先进程，已跳过；其余流程不受影响。"

    # desktop 重建必须替换 /Applications，而祖先正占用它：此时无论编多久，
    # 最后都装不进去。提前失败，别让人白等一次完整编译。
    # 放在函数末尾，这样它是 stop_dependents 的最终结果。
    case "$cmd" in
      desktop|all)
        die "本次重建从 App 内发起，而 App 自己占着 ${APP_DEST}。
      bundle 无法替换，重建不可能生效。请到终端里运行：
        cd $ROOT && ./scripts/rebuild.sh $cmd"
        ;;
    esac
  fi
}

# bundle 里是否还有进程在跑。用 ps 而非 `pgrep -f`（后者不匹配 argv）。
app_is_running() {
  ps -axo command= 2>/dev/null | grep -v grep | grep -qF "yi-agent.app"
}

# --- A. CLI ---------------------------------------------------------------

# 把全局路径接到本次装好的二进制上。
#
# 为什么必须做：登录 shell 的 PATH 里 /opt/homebrew/bin 排在 ~/.local/bin 与
# ~/.cargo/bin 之前（实测 `which -a yi-agent` 首选它），所以「装到
# ~/.local/bin」改变不了终端里跑哪个。不接管这个软链，rebuild 跑完敲
# `yi-agent` 命中的仍是上一次装的东西。
#
# $1 目标二进制（绝对路径）。
link_global() {
  local target="$1"
  local dir
  dir="$(dirname "$GLOBAL_BIN")"

  if [ -d "$GLOBAL_BIN" ] && [ ! -L "$GLOBAL_BIN" ]; then
    die "$GLOBAL_BIN 是个目录，不接管"
  fi

  # 真实文件（Homebrew 装的 / 手工 cp 的）不覆盖：删掉会让 brew 的账本与
  # 磁盘不一致，后续 upgrade/uninstall 行为不可预期。也不能只 warn 后
  # 成功退出——「全局路径指向刚编的产物」正是本改动的全部意义。
  if [ -e "$GLOBAL_BIN" ] && [ ! -L "$GLOBAL_BIN" ]; then
    die "$GLOBAL_BIN 是真实文件，不是软链；不覆盖。
     路径：$(ls -l "$GLOBAL_BIN" 2>/dev/null | sed 's/  */ /g')
     先把它移走再重跑（若是 Homebrew 装的：brew uninstall yi-agent），
     或设 REBUILD_GLOBAL_BIN 指到别处。"
  fi

  # 父目录不存在：不造也不拦。该目录归 Homebrew 管，在没有 brew 的机器上
  # 凭空建 /opt/homebrew/bin 只会让人误以为 brew 装好了；而这里也没有本该
  # 被接管的对象，谈不上分叉。warn 后继续，让 CLI / App / 插件照常编完。
  if [ ! -d "$dir" ]; then
    warn "全局路径的父目录不存在：$dir（不代为创建），跳过全局软链。
     没有 Homebrew 时请设 REBUILD_GLOBAL_BIN 指到 PATH 上的别处。"
    return 0
  fi

  # 目录在、但写不进去（/opt/homebrew 属 admin:admin 775）：此时路径是真实
  # 存在的，用户会以为它已被更新——必须失败。
  if [ ! -w "$dir" ]; then
    die "全局路径的父目录不可写：$dir（需要 sudo 或调整属主）。"
  fi

  ln -sfn "$target" "$GLOBAL_BIN"
  echo "     -> $GLOBAL_BIN (软链 -> $target)"
}

# 安装 + 接软链。与编译分开，测试可以直接喂一个假二进制进来。
# $1 已编好的二进制（绝对路径）。
install_cli_bin() {
  local bin="$1"
  [ -x "$bin" ] || die "未产出 $bin"

  if [ "$PROFILE" = dev ]; then
    # 开发构建只写独立文件名：稳定版二进制（~/.cargo/bin/yi-agent）一律不碰。
    mkdir -p "$LOCAL_BIN_DIR"
    install -m 0755 "$bin" "$LOCAL_BIN_DIR/yi-agent-dev"
    echo "CLI  -> $bin"
    echo "     -> $LOCAL_BIN_DIR/yi-agent-dev (开发版，不覆盖稳定版)"
    link_global "$LOCAL_BIN_DIR/yi-agent-dev"
    echo "版本确认: $("$GLOBAL_BIN" --version 2>&1 || echo '(执行失败)')"
    return 0
  fi

  # 装到 ~/.cargo/bin（对齐 .bashrc 的 installYiAgent 约定），全局软链再
  # 指过来让终端可直接调用。
  mkdir -p "$CARGO_BIN_DIR"
  install -m 0755 "$bin" "$CARGO_BIN_DIR/yi-agent"
  echo "CLI  -> $bin"
  echo "     -> $CARGO_BIN_DIR/yi-agent (已同步)"
  link_global "$CARGO_BIN_DIR/yi-agent"
  echo "版本确认: $("$GLOBAL_BIN" --version 2>&1 || echo '(执行失败)')"
}

build_cli() {
  log "A. 编译 yi-agent CLI (release)"
  ( cd "$RS_DIR" && cargo build --release -p yi-agent )
  install_cli_bin "$RS_DIR/target/release/yi-agent"
}

# --- B. 桌面 App ----------------------------------------------------------
build_desktop() {
  log "B. 编译桌面 App (release, 版本 $VERSION)"

  local sidecar_src="$RS_DIR/target/release/yi-agent"
  [ -x "$sidecar_src" ] || die "缺 sidecar 源 ${sidecar_src}；先跑 './scripts/rebuild.sh cli'"

  ( cd "$DESKTOP_DIR" && npm run sidecar:release )

  # --config 注入版本与收窄 bundle.targets，不改任何仓库文件
  # （tauri.conf.json 里版本写死 0.1.0，targets 默认 "all" 会多产 updater 物）。
  # beforeBuildCommand = "npm run build"，前端构建由 tauri 自动触发。
  #
  # 只出 app：dmg 必须在 finalize 之后从成品 .app 生成，否则打进去的是
  # 清单缺失或 digest 对不上的旧快照。
  ( cd "$DESKTOP_DIR" && npm run tauri -- build \
      --config "{\"version\":\"$VERSION\",\"bundle\":{\"targets\":[\"app\"]}}" )

  local app_src="$DESKTOP_DIR/src-tauri/target/release/bundle/macos/yi-agent.app"
  [ -d "$app_src" ] || die "未产出 $app_src"

  # finalize：先签 sidecar → 写 runtime-sidecar.json（含最终 digest）→ 签外层
  # app → verify 并复核 digest。桌面壳靠这份清单证明「要跑的 daemon 就是我
  # 发的那个」，顺序错了清单就是废纸。
  bash "$DESKTOP_DIR/scripts/finalize-runtime-bundle.sh" "$app_src"

  local dmg_src="$DESKTOP_DIR/src-tauri/target/release/bundle/dmg"
  mkdir -p "$dmg_src"
  hdiutil create -volname "yi-agent" -srcfolder "$app_src" \
    -ov -format UDZO "$dmg_src/yi-agent_${VERSION}_aarch64.dmg" >/dev/null
  echo "App  -> $app_src"
  ls -1 "$dmg_src" 2>/dev/null | sed 's/^/dmg  -> /' || true

  # 替换 /Applications。
  #
  # 到这里依附进程应该已被 stop_dependents 停干净。仍有人在跑只有一种可能：
  # 它是本次重建的祖先进程（spec §2.4，从 App 里发起重建时就是 app-server）。
  # 此时不能 rm -rf——那会在运行中的进程脚下抽掉 bundle，且上面的
  # build/daemon 已无意义。宁可失败，也不要产出残缺的 App。
  if app_is_running; then
    warn "仍有进程在使用 ${APP_DEST}，且它们是本次重建的祖先进程。"
    warn "不会删除正在被使用的 bundle。请手动退出 App 后重跑:"
    warn "  ./scripts/rebuild.sh desktop"
    die "跳过安装，避免留下残缺的 bundle"
  fi
  rm -rf "$APP_DEST"
  cp -R "$app_src" "$APP_DEST"
  echo "已安装 -> $APP_DEST"
}

# --- C. 看板插件 ----------------------------------------------------------
build_plugin() {
  log "C. 编译看板插件 (release)"
  ( cd "$PLUGIN_DIR" && cargo build --release )

  local bin="$PLUGIN_DIR/target/release/superpowers-kanban"
  [ -x "$bin" ] || die "未产出 $bin"

  # supervisor 清单里 command 硬编码了 /opt/homebrew/bin/superpowers-kanban。
  install -m 0755 "$bin" "$PLUGIN_BIN_DEST"
  echo "插件 -> $PLUGIN_BIN_DEST"

  mkdir -p "$(dirname "$SKILL_DEST")"
  cp "$PLUGIN_DIR/skills/superpowers-kanban/SKILL.md" "$SKILL_DEST"
  echo "skill -> ${SKILL_DEST}（下一个会话生效）"
}

# --- 汇总 ---------------------------------------------------------------
summary() {
  log "产物汇总"
  for f in \
    "$RS_DIR/target/release/yi-agent" \
    "$DESKTOP_DIR/src-tauri/target/release/bundle/macos/yi-agent.app" \
    "$PLUGIN_DIR/target/release/superpowers-kanban" \
    "$APP_DEST" \
    "$PLUGIN_BIN_DEST"
  do
    [ -e "$f" ] && printf '  %s  %s\n' "$(stat -f '%Sm' -t '%m-%d %H:%M' "$f")" "$f"
  done
  cat <<'EOF'

注意：已运行的进程不会自动换用新二进制，需重启才生效：
  - 桌面 App（app-server / daemon serve / boards watch 都由 App bundle 拉起）
  - 终端里的 yi-agent / TUI
  - superpowers-kanban（daemon 按 supervisor 清单重新拉起）
EOF
}

# --- 入口：解析子命令与开关 -----------------------------------------------
# FORCE / PROFILE / CMD 的默认值在常量段初始化（安装函数要读 PROFILE，
# 且 source 场景不经过这里）。
main() {
  while [ $# -gt 0 ]; do
    arg="$1"; shift
    case "$arg" in
      -h|--help) usage; exit 0 ;;
      -f|--force) FORCE=1 ;;
      dev|--profile=dev) PROFILE=dev ;;
      --profile)
        [ $# -ge 1 ] || die "--profile 需要一个值（stable|dev）"
        case "$1" in
          stable|dev) PROFILE="$1" ;;
          *) die "--profile 只接受 stable 或 dev，收到 '$1'" ;;
        esac
        shift ;;
      all|cli|desktop|plugin|stop) CMD="$arg" ;;
      *) die "未知参数 '$arg'（可用: all|cli|desktop|plugin|stop|dev|--profile stable|dev|--force|--help）" ;;
    esac
  done

  # 四个构建子命令都先停依附进程：plugin 单独重建也要让新二进制真正生效，
  # 而旧的 superpowers-kanban 正被旧 daemon 托管着（spec §2.5）。
  # `stop` 只停不编：用于随时手动清理，也是本改动最省时的验收入口。
  case "$CMD" in
    stop)      preflight; stop_dependents stop; summary ;;
    cli)       preflight; stop_dependents cli; build_cli; summary ;;
    desktop)   preflight; stop_dependents desktop; build_desktop; summary ;;
    plugin)    preflight; stop_dependents plugin; build_plugin; summary ;;
    all)       preflight; stop_dependents all; build_cli; build_desktop; build_plugin; summary ;;
  esac
}

# 被 source 时只加载函数、不跑构建——scripts/test_rebuild_global_link.sh 靠
# 这一点在临时目录里验安装逻辑，不必真编一次 Rust。
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
