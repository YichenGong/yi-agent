#!/usr/bin/env bash
# 把 Tauri 产出的 .app 修复签名后放进 npm 的 darwin-arm64 子包。
#
# 与 release-dmg.sh 的分工：那个脚本产出 dmg 发到 GitLab（内网分发），
# 这个脚本产出 .app 塞进 npm 包（公网分发）。两者共用同一套版本注入
# 与 Tauri 构建，但产物形式不同——npm 场景下 .app 目录可直接复制，
# 不需要 dmg 那层挂载。
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"          # desktop/
ROOT="$(cd "$HERE/.." && pwd)"                    # repo root
NPM_PKG="$ROOT/yi-agent-rs/yi-agent-cli/platforms/darwin-arm64"

log() { printf '>>> %s\n' "$*"; }

# --- 1. 环境 -------------------------------------------------------------
for tool in node npm jq codesign; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "ERROR: '$tool' not found on PATH ($PATH)" >&2
    exit 1
  fi
done

if [ "$(uname -s)" != "Darwin" ]; then
  echo "ERROR: 桌面 App 只能在 macOS 上构建（当前 $(uname -s)）" >&2
  exit 1
fi
if [ "$(uname -m)" != "arm64" ]; then
  echo "ERROR: 只产出 arm64 .app，当前机器是 $(uname -m)" >&2
  exit 1
fi

# --- 2. 版本 -------------------------------------------------------------
VERSION="$(grep '^version' "$ROOT/yi-agent-rs/Cargo.toml" | head -1 | awk -F'"' '{print $2}')"
if [ -z "$VERSION" ]; then
  echo "ERROR: 读不到 yi-agent-rs/Cargo.toml 的 version" >&2
  exit 1
fi

if [ -n "${CI_COMMIT_TAG:-}" ]; then
  if [ "v$VERSION" != "$CI_COMMIT_TAG" ]; then
    echo "ERROR: tag ($CI_COMMIT_TAG) 与 Cargo.toml ($VERSION) 不一致" >&2
    exit 1
  fi
  log "版本校验通过: $VERSION"
fi

# --- 3. 构建 -------------------------------------------------------------
# 版本必须注入 tauri.conf.json，否则 App 的 CFBundleShortVersionString
# 会一直停在写死的值，postinstall 的版本比对就永远判定「需要替换」。
CONF="$HERE/src-tauri/tauri.conf.json"
log "注入版本 $VERSION 并收窄 targets 为 app"
jq --arg v "$VERSION" '.bundle.targets = ["app"] | .version = $v' "$CONF" > "$CONF.tmp"
mv "$CONF.tmp" "$CONF"
# 只在工作区改，不提交；CI 里用完即弃。
# 退出码必须自己写出来：CI 上第 13 步返回 0 却只跑到 `stage: tauri build`
# 就没有下文，而 `set -euo pipefail` 下没有任何路径能安静地退 0。到底是脚本
# 跑完了（日志被吞）还是被谁中途终止，只有这行能分清。
trap 'rc=$?; git -C "$ROOT" checkout -- "$CONF" 2>/dev/null || true; \
      log "script exiting rc=$rc"' EXIT

gh_note() { [ -n "${GITHUB_ACTIONS:-}" ] && echo "::notice title=npm-pack-desktop::$*" || true; }
gh_err()  { [ -n "${GITHUB_ACTIONS:-}" ] && echo "::error title=npm-pack-desktop::$*" || true; }

gh_note "stage: npm ci"
log "npm ci"
(cd "$HERE" && npm ci) || { gh_err "npm ci failed"; exit 1; }

gh_note "stage: build sidecar"
log "构建 sidecar（release 模式 CLI）"
(cd "$HERE" && npm run sidecar:release) || { gh_err "sidecar:release failed"; exit 1; }

gh_note "stage: tauri build"
log "tauri build（只出 .app）"

# tauri-build 会把 sidecar 复制到 `<target>/release/yi-agent`，复制前对已存在
# 的目标文件做 `fs::remove_file(...).unwrap()`（tauri-build-2.6.3/src/lib.rs:80）。
# 那个 unwrap 在删除失败时直接 panic，错误是
#   Os { code: 1, kind: PermissionDenied, message: "Operation not permitted" }
#
# self-hosted runner 是持久机器，`CARGO_TARGET_DIR` 跨 run 复用；一次被中断的
# 构建足够留下一个删不掉的残留（只读位 / immutable 标志 / 中途 kill）。之后
# 每次构建都在 33 秒左右的同一位置挂掉，而本地（缓存干净）永远复现不出来。
#
# 所以构建前先把它清掉：先给写权限再删，两者都失败才算真错误。
TAURI_TARGET_DIR_PRE="$(cargo metadata --manifest-path "$HERE/src-tauri/Cargo.toml" \
  --format-version 1 --no-deps 2>/dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$TAURI_TARGET_DIR_PRE" ] || TAURI_TARGET_DIR_PRE="$HERE/src-tauri/target"
# 目标路径按 tauri-build 自己的算法推导，不要凭直觉：
#   out_dir = <target>/release/build/desktop/<hash>/out
#   target_dir = out_dir.parent().parent().parent() = <target>/release/build
#   dest = target_dir / "yi-agent"
# 也就是 `<target>/release/build/yi-agent`。曾经的实现在 `<target>/release/`
# 下找，那里根本没有该文件，于是清理形同虚设、panic 照旧（v0.1.15 的第五次
# 失败就是 59 秒时仍在同一行 panic）。
#
# 两个候选都清：<target>/release/build 是上面算出来的真正目标；
# <target>/release 是早先版本的布局，留着只会让排查时看到两个同名文件。
# 候选路径：<target>/release/build 是 tauri-build 真正的 dest；
# <target>/release 是更早版本的布局，两个都清。
clear_stale_external_bin() {
  local stale="$1"
  local parent
  parent="$(dirname "$stale")"
  [ -e "$stale" ] || return 0
  log "清理陈旧的 externalBin 目标: $stale"
  ls -lO "$stale" >&2 || true
  # 只给文件解标志是不够的：真正卡住删除的是**父目录**。实测这台 runner 上
  # `release/build` 目录本身是 `dr-xr-xr-x` + `uchg`（一次中断的构建留下的），
  # 目录不可写 + immutable 时，里面文件的删除一律 EPERM，而文件自己看起来
  # 完全正常（`-rw-r--r--`）。先解父目录，再解文件。
  chflags nouchg "$parent" 2>/dev/null || true
  chmod u+w "$parent" 2>/dev/null || true
  # chmod 管只读位，chflags 管 immutable（uchg）与 system-immutable（schg）。
  # 都试一遍，失败不致命，删不掉时由下面的校验兜住。
  chmod -Rf u+w "$stale" 2>/dev/null || true
  chflags -Rf nouchg noschg "$stale" 2>/dev/null || true
  rm -rf "$stale" 2>/dev/null || true
  if [ -e "$stale" ]; then
    # 返回非零而不是 exit：调用方会用「换一个全新 target 目录」兜底，
    # 直接退出就没有兜底机会了。消息全部用 ASCII——这一支曾在 CI 上只打印
    # 出 bash 的 "line 110: stale"，看不到具体是哪个文件。
    echo "ERROR: cannot remove stale externalBin target: $stale" >&2
    ls -lO "$stale" >&2 || true
    gh_err "cannot remove stale externalBin target: $stale"
    return 1
  fi
  return 0
}

clear_stale_external_bin "$TAURI_TARGET_DIR_PRE/release/build/yi-agent" || true
clear_stale_external_bin "$TAURI_TARGET_DIR_PRE/release/yi-agent" || true

# 兜底：上面清不掉时，不要试图修那台机器的文件权限（我们连它是谁建的都
# 不知道，也可能带 system-immutable 需要 root），而是给这次构建换一个全新
# 的 target 目录。代价是重编一次依赖，但比「构建失败却报成功」便宜得多。
# 环境变量统一在这里 export，后面 `cargo metadata` 解析 APP_SRC 才能跟
# 构建产物走到同一个目录。
# 只认「文件」，不认目录：`release/build` 下还有 tauri 自己的
# `yi-agent-<hash>/` 构建缓存目录（正常产物，不能算陈旧）。早先这里用
# `-name 'yi-agent*'` 把那些目录也算进去，导致每次构建都误走兜底、丢掉
# 全部增量缓存。
if [ -e "$TAURI_TARGET_DIR_PRE/release/build/yi-agent" ] && \
   [ ! -d "$TAURI_TARGET_DIR_PRE/release/build/yi-agent" ]; then
  FRESH_TARGET="${CARGO_TARGET_DIR:-$TAURI_TARGET_DIR_PRE}/desktop-fresh-$RANDOM"
  log "陈旧目标无法删除，改用全新 target 目录: $FRESH_TARGET"
  mkdir -p "$FRESH_TARGET"
  export CARGO_TARGET_DIR="$FRESH_TARGET"
  gh_note "stage: fresh target dir ($FRESH_TARGET)"
fi

# 完整输出落盘，失败时把尾部转成 annotation。原因：这个仓库是私有的，CI
# 日志要 admin 权限才能读（匿名 API 对 logs 与 job logs 都回 403），唯一能
# 匿名读到的是 check-run annotations。而 `tauri build` 的失败原因藏在几十行
# 输出里（cargo 报错、build script panic、bundle 失败），只回一句
# "tauri build failed" 等于什么都没说。
log "清理完成，准备 tauri build (target=$TAURI_TARGET_DIR_PRE)"
TAURI_LOG="$(mktemp -t yi-agent-tauri-build)"
log "TAURI_LOG=$TAURI_LOG"
# 这里**不能**写 `if ! cmd 2>&1 | tee log; then`：`!` 作用于整条管道，而
# 管道的退出码是最后一条命令（tee）的，恒为 0。于是 `!` 把成功翻成失败、
# `then` 分支永不进入，tauri build 的失败被彻底吞掉，脚本带着 rc=0 往下
# 走——这正是 v0.1.15 连续多次「第 13 步成功但产物不在」的成因：构建其实
# 早就失败了，只是没人看它的退出码。
# 这里**不用管道**。`cmd | tee log` 的退出码是 tee 的（恒为 0），必须靠
# PIPESTATUS 补救；而 PIPESTATUS 只反映紧邻的上一条管道，任何插入的命令都会
# 把它清空，在 `set -u` 下变成
#   package-npm-app.sh: line NNN: TAURI_RC: unbound variable
# ——v0.1.15 的第十二次正是栽在这：构建明明成功（日志里已有
# "Finished 1 bundle at: .../yi-agent.app"），脚本却在读退出码那行崩掉，
# 产物因此没被复制，第 14 步只能看到一个空目录。
#
# 改成重定向到文件再取 `$?`：退出码来自那个子 shell，没有管道、没有
# PIPESTATUS、也没有 set -u 的坑。要实时输出的部分（失败时）后面再 cat。
set +e
(cd "$HERE" && npm run tauri build -- --bundles app) >"$TAURI_LOG" 2>&1
TAURI_RC=$?
set -e
cat "$TAURI_LOG"
log "tauri build 退出码 $TAURI_RC（日志 $(wc -l < "$TAURI_LOG") 行）"
if [ "$TAURI_RC" -ne 0 ]; then
  # 也把尾部原样打到 stdout，便于本地复现时阅读。
  echo "---- tauri build: last 30 lines ----"
  tail -30 "$TAURI_LOG"
  echo "-------------------------------------"
  # annotation 单条有长度上限，压缩连续空白并截断，保留最能定位的尾部。
  detail="$(tail -30 "$TAURI_LOG" | tr '\n' '|' | tr -s ' ' | cut -c1-1800)"
  gh_err "tauri build failed (rc=$TAURI_RC): $detail"
  rm -f "$TAURI_LOG"
  exit 1
fi
log "tauri build 结束（成功），日志行数 $(wc -l < "$TAURI_LOG")"
rm -f "$TAURI_LOG"

# target 目录不能假定是 src-tauri/target：CI 把 CARGO_TARGET_DIR 指到
# $HOME/cargo-target/github，tauri 的产物会跟着走。cargo metadata 是唯一
# 真源（同时认 CARGO_TARGET_DIR 与 .cargo/config 的 build.target-dir），
# 写死路径会让「编译成功但找不到 .app」这种最难查的失败复活。
TAURI_TARGET_DIR="$(cargo metadata --manifest-path "$HERE/src-tauri/Cargo.toml" \
  --format-version 1 --no-deps 2>/dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$TAURI_TARGET_DIR" ] || TAURI_TARGET_DIR="$HERE/src-tauri/target"
APP_SRC="$TAURI_TARGET_DIR/release/bundle/macos/yi-agent.app"
if [ ! -d "$APP_SRC" ]; then
  echo "::error title=npm-pack-desktop::no .app at $APP_SRC" || true
  echo "ERROR: 未找到 $APP_SRC" >&2
  ls -l "$TAURI_TARGET_DIR/release/bundle/macos/" >&2 || true
  exit 1
fi

# --- 4. 签名与运行时清单 -------------------------------------------------
# 顺序固定，由 finalize-runtime-bundle.sh 保证：先签 sidecar（字节定稿）→
# 写 runtime-sidecar.json（含最终 digest）→ 签外层 app → verify 并复核
# digest。**不能再 --deep**：它会把已签的 sidecar 重签一遍，使刚写下的
# digest 与实际文件不符，而清单正是桌面壳用来证明「要跑的 daemon 就是
# 我发的那个」的依据。
gh_note "stage: finalize bundle"
log "finalize 运行时清单并重签"
bash "$HERE/scripts/finalize-runtime-bundle.sh" "$APP_SRC" || { gh_err "finalize-runtime-bundle failed"; exit 1; }

gh_note "stage: verify signature"
log "校验签名"
if ! codesign --verify --strict "$APP_SRC"; then
  echo "ERROR: 重签后签名仍无效，拒绝打包坏产物" >&2
  exit 1
fi

EXPECTED="$VERSION"
ACTUAL="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
  "$APP_SRC/Contents/Info.plist" 2>/dev/null || true)"
if [ "$ACTUAL" != "$EXPECTED" ]; then
  echo "ERROR: App 版本 $ACTUAL 与预期 $EXPECTED 不一致" >&2
  exit 1
fi

# --- 5. 就位 -------------------------------------------------------------
log "复制到 npm 子包"
rm -rf "$NPM_PKG/yi-agent.app"
mkdir -p "$NPM_PKG"
cp -R "$APP_SRC" "$NPM_PKG/yi-agent.app"

du -sh "$NPM_PKG/yi-agent.app"
log "完成: $NPM_PKG/yi-agent.app ($VERSION)"
