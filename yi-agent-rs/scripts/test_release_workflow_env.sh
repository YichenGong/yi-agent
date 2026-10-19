#!/usr/bin/env bash
# 自测 .github/workflows/release.yml 的环境不变量。
#
# 背景：v0.1.4 起每次 tag 触发的 GitHub Release 都在
# `Build & package all platforms` 这一步 **1~3 秒** 内失败（云端可查到的
# run 39238815577 / 38434825794 / 38430215902 / 38424746408 / 38064651580
# 全部是同一个 step 失败，且后续 Upload/npm/Homebrew 全部 skipped）。
# v0.1.3 用同样的 justfile 是成功的，差别在 runner 侧的环境：
#
#   1. workflow 把 PATH 与 CARGO_TARGET_DIR 硬编码成
#      `/Users/agenticlabs/...`。cargo 在创建 target 目录失败时会在
#      **0.2 秒**内直接退出（实测：`error: failed to create directory
#      /Users/agenticlabs/cargo-target: Permission denied`），与 CI 观测到的
#      1~3 秒耗时就完全吻合。只要 runner 的登录用户不是 agenticlabs，
#      整条 release 就必然在第一步构建时秒挂。仓库历史里 GitLab 侧的
#      提交直接写过「runner 用户可能是 agenticlabs 或 gongyichen」，
#      所以这个假设有实际依据，不是猜测。
#   2. workflow 只 `rustup default stable`，从不 `rustup update stable`。
#      `default` 只切换别名，不会升级已安装的工具链；而 GitLab 的 ci-push
#      是带 `rustup update stable` 的。0.1.4 引入的依赖把构建图的最高 MSRV
#      抬到了 1.88（time/zip/rmcp/lopdf），0.1.7 又抬到 1.89（aes 0.9.3）。
#
#   3. （真机复现后补充）PATH 顺序让 Homebrew 的 rust 抢占了 rustup 的 rust。
#      runner 上 /opt/homebrew/bin/cargo 指向 Cellar/rust/1.98.1，而
#      GITHUB_PATH 是「后写的在前」：workflow 先写 $HOME/.cargo/bin、
#      再写 /opt/homebrew/bin，结果 brew 排在前面。brew 的 cargo 看不见
#      rustup 装在 ~/.rustup/toolchains 下的跨平台 std，交叉编译立刻报
#      `error[E0463]: can't find crate for 'core'`——实测 0.5~3 秒失败，
#      与 CI 观测耗时完全吻合。同一台机器把 $HOME/.cargo/bin 提到最前，
#      同一 target 1m05s 构建成功。
#   4. （真机复现后补充）launchd 的 maxfiles 是 256。14 核并行链接
#      x86_64-unknown-linux-gnu 时撞上
#      `ProcessFdQuotaExceeded`。抬到 65536 后 1m06s 成功。
#
# 这个脚本是静态门禁：把这几条不变量钉死，避免再次「本地能编、CI 秒挂」。
set -uo pipefail

WORKFLOW="$(cd "$(dirname "$0")/../.." && pwd)/.github/workflows/release.yml"

pass=0
failn=0
ok() { echo "ok   - $1"; pass=$((pass + 1)); }
bad() { echo "FAIL - $1"; failn=$((failn + 1)); }

if [ ! -f "$WORKFLOW" ]; then
  echo "FAIL - workflow not found: $WORKFLOW"
  exit 1
fi

# 只保留非注释行。注释里解释历史写法时会出现 /Users/... 之类的字样，
# 那不是配置，不该判红。
config="$(grep -vE '^[[:space:]]*#' "$WORKFLOW")"

# 1) 配置里不得出现写死的家目录（/Users/<someone> 或 /home/<someone>）。
#    这类路径只在作者本机成立，runner 换用户就直接不可写。
if printf '%s\n' "$config" | grep -qE '/Users/[A-Za-z0-9._-]+|/home/[A-Za-z0-9._-]+'; then
  bad "workflow must not hardcode a user home directory"
  printf '%s\n' "$config" | grep -nE '/Users/[A-Za-z0-9._-]+|/home/[A-Za-z0-9._-]+' | sed 's/^/       /'
else
  ok "workflow has no hardcoded user home directory"
fi

# 2) CARGO_TARGET_DIR 必须由 $HOME 推导。写死绝对路径会让 runner 换用户后
#    无法创建 target 目录，cargo 会立刻失败。
if printf '%s\n' "$config" | grep -qE 'CARGO_TARGET_DIR[=:][[:space:]]*"?\$?\{?HOME\}?/'; then
  ok "CARGO_TARGET_DIR is derived from \$HOME"
else
  bad "CARGO_TARGET_DIR must be derived from \$HOME (e.g. \$HOME/cargo-target/github)"
  printf '%s\n' "$config" | grep -n 'CARGO_TARGET_DIR' | sed 's/^/       /'
fi

# 3) PATH 必须包含 $HOME 下的工具目录（写进 GITHUB_PATH 也算），
#    否则自安装的 cargo / just 找不到。
if printf '%s\n' "$config" | grep -qE '\$HOME/\.cargo/bin|\$\{HOME\}/\.cargo/bin' &&
   printf '%s\n' "$config" | grep -qE '\$HOME/\.local/bin|\$\{HOME\}/\.local/bin'; then
  ok "PATH includes \$HOME/.cargo/bin and \$HOME/.local/bin"
else
  bad "PATH must include \$HOME/.cargo/bin and \$HOME/.local/bin"
fi

# 4) 工具链必须升级而不仅是切换别名。`rustup default stable` 不会拉新版本，
#    依赖 MSRV 一旦超过 runner 上已装的 stable，构造期的解析就会失败。
if printf '%s\n' "$config" | grep -qE '^[[:space:]]*rustup update stable'; then
  ok "workflow runs 'rustup update stable'"
else
  bad "workflow must run 'rustup update stable' (default alone never upgrades)"
fi

# 5) GITHUB_PATH 的写入顺序：/opt/homebrew/bin 必须先于 $HOME/.cargo/bin。
#    GITHUB_PATH 是后写优先，所以「先 brew 后 cargo」最终让 rustup 胜出。
#    顺序反了会让 Homebrew 的 rust 抢到 cargo，交叉编译以 E0463 秒挂。
brew_line="$(printf '%s\n' "$config" | grep -n 'GITHUB_PATH' | grep -F '/opt/homebrew/bin' | head -1 | cut -d: -f1)"
cargo_line="$(printf '%s\n' "$config" | grep -n 'GITHUB_PATH' | grep -F 'HOME/.cargo/bin' | head -1 | cut -d: -f1)"
if [ -n "$brew_line" ] && [ -n "$cargo_line" ] && [ "$brew_line" -lt "$cargo_line" ]; then
  ok "GITHUB_PATH adds /opt/homebrew/bin before \$HOME/.cargo/bin (rustup wins)"
else
  bad "GITHUB_PATH must add /opt/homebrew/bin BEFORE \$HOME/.cargo/bin (lines: brew=${brew_line:-none} cargo=${cargo_line:-none})"
fi

# 6) 构建步骤必须显式装好构建环境（重排 PATH、抬高 fd 上限）。
if printf '%s\n' "$config" | grep -qF 'source scripts/setup-build-env.sh'; then
  ok "build step sources scripts/setup-build-env.sh"
else
  bad "build step must source scripts/setup-build-env.sh before building"
fi

# 7) 环境脚本本身必须存在，并且真的处理了 fd 上限。
ENV_SCRIPT="$(cd "$(dirname "$0")" && pwd)/setup-build-env.sh"
if [ -f "$ENV_SCRIPT" ]; then
  ok "scripts/setup-build-env.sh exists"
  if grep -q 'ulimit -n 65536' "$ENV_SCRIPT"; then
    ok "setup-build-env.sh raises the file descriptor limit"
  else
    bad "setup-build-env.sh must raise the fd limit (launchd defaults to 256)"
  fi
  if grep -q 'shadowing rustup' "$ENV_SCRIPT"; then
    ok "setup-build-env.sh rejects Homebrew's Rust shadowing rustup"
  else
    bad "setup-build-env.sh must reject Homebrew's cargo shadowing rustup"
  fi
else
  bad "scripts/setup-build-env.sh is missing"
fi

# 8) 目标平台必须装齐三平台，否则 package-all 里会缺产物。
for target in x86_64-apple-darwin aarch64-apple-darwin x86_64-unknown-linux-gnu; do
  if printf '%s\n' "$config" | grep -qF "$target"; then
    ok "workflow covers target $target"
  else
    bad "workflow is missing target $target"
  fi
done

echo
if [ "$failn" -eq 0 ]; then
  echo "$pass passed"
  exit 0
fi
echo "$pass passed, $failn failed"
exit 1
