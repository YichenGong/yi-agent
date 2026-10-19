#!/usr/bin/env bash
# 为构建准备一套可用的 Rust 环境，并**校验**它真的可用。
#
# 为什么需要这个脚本（都是在这台 Mac mini runner 上实测出来的）：
#
#   1. PATH 顺序会让 Homebrew 的 Rust 抢占 rustup 的 Rust。
#      /opt/homebrew/bin 里有 Homebrew 装的 rust 1.98.1
#      （`/opt/homebrew/bin/cargo -> ../Cellar/rust/1.98.1/bin/cargo`），
#      而 release workflow 的 PATH 把 /opt/homebrew/bin 排在
#      $HOME/.cargo/bin 前面。于是 `cargo` 解析到 Homebrew 那份，它不认
#      rustup 装在 ~/.rustup/toolchains 下的跨平台 std，交叉编译立刻报
#      `error[E0463]: can't find crate for 'core'` —— 实测 0.5~3 秒即失败，
#      与 GitHub Actions 上 v0.1.4~v0.1.7 每次的耗时完全吻合。
#      把 $HOME/.cargo/bin 提到最前，同一 target 1m05s 构建成功。
#
#   2. 文件描述符上限只有 256。
#      runner 侧 launchd 限制是 `maxfiles 256 unlimited`；14 核并行链接
#      x86_64-unknown-linux-gnu 时撞上
#      `unable to search for static library ...: ProcessFdQuotaExceeded`。
#      抬到 65536 后同一目标 1m06s 成功。
#
# 用法：
#   source yi-agent-rs/scripts/setup-build-env.sh           # 当前 shell 生效
#   bash  yi-agent-rs/scripts/setup-build-env.sh --print    # 打印 export 语句
#   bash  yi-agent-rs/scripts/setup-build-env.sh --check    # 只校验，不改环境
#
# 非 0 退出表示环境不可用；调用方必须让构建失败，而不是带着错环境继续。
set -uo pipefail

MODE="apply"
case "${1:-}" in
  --print) MODE="print" ;;
  --check) MODE="check" ;;
  "") ;;
  *) echo "setup-build-env: unknown option '$1' (expected --print or --check)" >&2; exit 2 ;;
esac

: "${HOME:?HOME is not set}"

# 默认校验三平台；只构建单一目标时可设 SETUP_BUILD_ENV_TARGETS 覆盖，
# 否则只做 darwin 构建也会被要求装 zig。
if [ -n "${SETUP_BUILD_ENV_TARGETS:-}" ]; then
  # shellcheck disable=SC2206
  TARGETS=(${SETUP_BUILD_ENV_TARGETS})
else
  TARGETS=(x86_64-apple-darwin aarch64-apple-darwin x86_64-unknown-linux-gnu)
fi

say() { echo "setup-build-env: $*" >&2; }
die() { say "$*"; exit 1; }

# ---- 1. PATH：rustup 的 shim 必须排在 Homebrew 之前 ----
# 重排而不是覆盖：保留用户 PATH 里其它目录，只把两个 rustup 目录提到前面。
reorder_path() { # $1 = 原 PATH, 输出重排后的 PATH
  local rustup_bin="$HOME/.cargo/bin" local_bin="$HOME/.local/bin"
  local out="" p
  local -a parts
  IFS=':' read -r -a parts <<< "$1"
  for p in "${parts[@]}"; do
    [ -z "$p" ] && continue
    [ "$p" = "$rustup_bin" ] && continue
    [ "$p" = "$local_bin" ] && continue
    out="${out:+$out:}$p"
  done
  printf '%s' "$rustup_bin:$local_bin${out:+:$out}"
}

# --check 校验的是「当前 PATH 是否可用」，不改环境；
# --apply / --print 才会重排 PATH。
if [ "$MODE" = "check" ]; then
  NEW_PATH="${PATH:-/usr/bin:/bin}"
else
  NEW_PATH="$(reorder_path "${PATH:-/usr/bin:/bin}")"
fi

# ---- 2. 文件描述符：并行链接需要远大于 256 ----
raise_fds() {
  local cur
  cur="$(ulimit -n 2>/dev/null || echo 0)"
  [ "$cur" -ge 4096 ] 2>/dev/null && return 0
  ulimit -n 65536 2>/dev/null && return 0
  local hard
  hard="$(ulimit -Hn 2>/dev/null || echo "")"
  if [ -n "$hard" ] && [ "$hard" != "unlimited" ] && [ "$hard" -ge 4096 ] 2>/dev/null; then
    ulimit -n "$hard" 2>/dev/null && return 0
  fi
  return 0  # 抬不动不致命，但不能假装抬了；由下面的告警体现
}

# ---- 3. 校验工具链 ----
# 判据不是「cargo 是不是 rustup 的 shim」——那是过强且错误的约束：
#   - rustup 默认布局：~/.cargo/bin/cargo -> rustup（shim）
#   - 也可直接软链到 toolchain：~/.local/bin/cargo -> ~/.rustup/toolchains/.../bin/cargo
# 两者都完全可用。真正决定成败的是：**当前 rustc 的 sysroot 里有没有
# 三个目标平台的 std**。Homebrew 的 rust 之所以必挂，正是因为它自己的
# sysroot 里没有 rustup 装的跨平台 std（于是报 E0463）。
normalize_target() { # 只接受本脚本支持的三平台
  case "$1" in
    aarch64-apple-darwin|arm64-apple-darwin) echo aarch64-apple-darwin ;;
    x86_64-apple-darwin) echo x86_64-apple-darwin ;;
    x86_64-unknown-linux-gnu) echo x86_64-unknown-linux-gnu ;;
    *) echo "" ;;
  esac
}

verify_env() { # 用给定 PATH 校验；返回非 0 表示不可用
  local path="$1" p t chk libdir sysroot rustc_ver
  local cargo_p rustc_p

  cargo_p="$(PATH="$path" command -v cargo 2>/dev/null || true)"
  rustc_p="$(PATH="$path" command -v rustc 2>/dev/null || true)"
  [ -n "$cargo_p" ] || { say "cargo not found in PATH"; return 1; }
  [ -n "$rustc_p" ] || { say "rustc not found in PATH"; return 1; }

  # cargo 会用 PATH 里的 rustc，所以以 rustc 的 sysroot 为准
  sysroot="$(PATH="$path" rustc --print sysroot 2>/dev/null || true)"
  [ -n "$sysroot" ] || { say "cannot determine rustc sysroot ($rustc_p)"; return 1; }
  rustc_ver="$(PATH="$path" rustc --version 2>/dev/null || echo unknown)"

  for t in "${TARGETS[@]}"; do
    chk="$(normalize_target "$t")"
    [ -n "$chk" ] || { say "unsupported target: $t"; return 1; }
    libdir="$sysroot/lib/rustlib/$chk/lib"
    if ! ls "$libdir"/libcore-*.rlib >/dev/null 2>&1; then
      case "$sysroot" in
        "$HOME/.rustup/toolchains/"*)
          say "missing std for $chk (looked in $libdir)"
          say "  active rustc: $rustc_ver"
          say "  fix: rustup target add $chk"
          ;;
        *)
          # sysroot 不在 rustup 的 toolchains 下 —— 用的是外部安装的 Rust
          # （最常见是 Homebrew 的 /opt/homebrew/bin/rustc），它不认 rustup
          # 装的跨平台 std，交叉编译会以 E0463 "can't find crate for core" 失败。
          say "rustc resolves to $rustc_p, whose sysroot is outside rustup:"
          say "  sysroot = $sysroot"
          say "  version = $rustc_ver"
          say "  Its sysroot has no std for $chk, so cross-compiling fails with"
          say '  E0463 "can not find crate for core".'
          say "  Homebrew's Rust is shadowing rustup's. Put \$HOME/.cargo/bin first,"
          say "  or run this script with --print and eval its output."
          ;;
      esac
      return 1
    fi
  done

  # 版本一致性：cargo 与 rustc 各自报的版本若差太多，说明两套工具链混用了。
  local cargo_ver
  cargo_ver="$(PATH="$path" cargo --version 2>/dev/null || echo unknown)"
  case "$cargo_ver" in
    *"$rustc_ver"*) ;;
    *)
      # 版本号格式不同（cargo 1.99.1 / rustc 1.99.1）时只比较数字部分
      local cn rn
      cn="$(printf '%s' "$cargo_ver" | sed -n 's/.*\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\).*/\1/p')"
      rn="$(printf '%s' "$rustc_ver" | sed -n 's/.*\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\).*/\1/p')"
      if [ -n "$cn" ] && [ -n "$rn" ] && [ "$cn" != "$rn" ]; then
        say "cargo ($cargo_ver) and rustc ($rustc_ver) come from different toolchains"
        say "  cargo = $cargo_p"
        say "  rustc = $rustc_p"
        return 1
      fi
      ;;
  esac

  # Linux 目标走 cargo-zigbuild，需要 zig
  if ! PATH="$path" command -v zig >/dev/null 2>&1; then
    say "zig not found (needed for x86_64-unknown-linux-gnu; brew install zig)"
    return 1
  fi
  if ! PATH="$path" command -v cargo-zigbuild >/dev/null 2>&1; then
    say "cargo-zigbuild not found (cargo install cargo-zigbuild --locked)"
    return 1
  fi
  return 0
}

case "$MODE" in
  check)
    verify_env "$NEW_PATH" || exit 1
    say "ok — cargo=$(PATH="$NEW_PATH" command -v cargo) ($(PATH="$NEW_PATH" cargo --version 2>/dev/null)), fd=$(ulimit -n)"
    ;;
  print)
    verify_env "$NEW_PATH" || exit 1
    printf 'export PATH="%s"\n' "$NEW_PATH"
    printf 'ulimit -n 65536 2>/dev/null || true\n'
    ;;
  apply)
    FAIL=1
    export PATH="$NEW_PATH"
    raise_fds
    verify_env "$PATH" && FAIL=0 || FAIL=1
    if [ "$FAIL" -ne 0 ]; then
      say "environment is not usable; refusing to continue"
      exit 1
    fi
    say "ok — cargo=$(command -v cargo) ($(cargo --version 2>/dev/null)), rustc=$(rustc --version 2>/dev/null), fd=$(ulimit -n)"
    ;;
esac
