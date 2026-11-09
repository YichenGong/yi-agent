#!/usr/bin/env bash
# 跨端 daemon socket 契约门禁：Rust-only，不依赖 Node。
#
# 为什么需要它：插件的地址解析是宿主 `scope.rs` 的独立复刻（插件不能依赖任何
# `yi-agent-*` crate）。两侧规则一旦漂移，插件就会探测一个没人监听的地址，
# 三次失联后退出——这正是「更新后看板用不了」的现场表现。所以必执行的门禁
# 必须真的比较两侧**真实函数**的输出，而不是各跑各的单测。
#
# 这个脚本只覆盖 Rust 侧：宿主 example + 两个 fixture 测试 + ignored 的跨进程
# 对照，再整包回归 IPC 与 runner。Node 侧的契约检查在 `plugin-contracts-check`
# 里另外跑。
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd -P)
cd "$ROOT"
command -v python3 >/dev/null

# 交叉编译出来的 example 不能在本机执行：那会「成功构建、跑错架构」。
if [[ -n ${CARGO_BUILD_TARGET:-} ]]; then
  echo 'cross-target execution is not supported' >&2
  exit 1
fi

# 这些变量会被被测代码读取，继承下来会让用例互相污染。
unset RUST_TEST_NOCAPTURE KANBAN_CONTRACT_RUNTIME_HEX KANBAN_CONTRACT_DELETE_CWD
export CARGO_TERM_COLOR=never

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# 从 cargo metadata 取宿主 workspace 的 target 目录。build 与 metadata 都从
# 仓库根执行：两个 workspace 对**相对** CARGO_TARGET_DIR 的解释不同，混着用
# 会让「构建的产物」和「校验的路径」指向两处。
HOST_TARGET=$(cargo metadata --manifest-path yi-agent-rs/Cargo.toml --no-deps --format-version 1 |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')

cargo build --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-lifecycle \
  --example daemon_socket_contract_probe --message-format=json >"$TMP/build.json"

# 只认本次构建产出的那一个 example，且路径必须落在预期的 debug/examples 下。
# 从磁盘上「碰巧找到一个旧 example」就继续，会让门禁对着陈旧二进制放行。
PROBE=$(python3 - "$TMP/build.json" "$HOST_TARGET" <<'PY'
import json, os, sys
rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
paths = [r['executable'] for r in rows
         if r.get('reason') == 'compiler-artifact'
         and r.get('target', {}).get('name') == 'daemon_socket_contract_probe'
         and 'example' in r.get('target', {}).get('kind', [])
         and r.get('executable')]
assert len(paths) == 1, 'missing or ambiguous host probe artifact'
expected = os.path.join(sys.argv[2], 'debug', 'examples', 'daemon_socket_contract_probe')
assert os.path.realpath(paths[0]) == os.path.realpath(expected), 'cross-target or stale artifact path'
assert os.path.isfile(paths[0]) and os.access(paths[0], os.X_OK), 'host probe is missing'
print(os.path.abspath(paths[0]))
PY
)
export KANBAN_HOST_SOCKET_PROBE="$PROBE"

# 单测试执行：必须是「恰好 1 个 test、真的 ok」，0 test 与 ignored 跳过都不算通过。
#
# 注意 `flags` 不能改成空数组再 `"${flags[@]}"` 展开：Bash 3.2（macOS 自带）
# 在 `set -u` 下会报 unbound variable，而且某些路径上还会错误地 exit 0。
run_one() {
  local name=$1 mode=$2
  shift 2
  local flags=(--exact)
  if [[ $mode == ignored ]]; then flags+=(--ignored); fi
  local log="$TMP/one.log"
  if ! cargo test "$@" "$name" -- "${flags[@]}" --format pretty --color never >"$log" 2>&1; then
    cat "$log" >&2; return 1
  fi
  cat "$log"
  grep -Fqx 'running 1 test' "$log"
  grep -Fqx "test $name ... ok" "$log"
  grep -Eq '^test result: ok\. 1 passed; 0 failed; 0 ignored;' "$log"
}

run_one root_vector_and_fixture_schema_are_pinned normal \
  --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-lifecycle --test daemon_socket_contract
run_one root_vector_and_fixture_schema_are_pinned normal \
  --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-ipc --test daemon_socket_contract
run_one cross_process_daemon_socket_contract ignored \
  --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-ipc --test daemon_socket_contract

if ! cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml \
  -p superpowers-kanban-ipc -- --format pretty --color never >"$TMP/ipc.log" 2>&1; then
  cat "$TMP/ipc.log" >&2; exit 1
fi
cat "$TMP/ipc.log"
for name in status_io::tests::nul_is_rejected_before_connect \
  status_io::tests::connect_backlog_obeys_deadline \
  status_io::tests::connect_retries_keep_original_deadline \
  status_io::tests::poll_eintr_keeps_deadline \
  status_io::tests::failed_attempts_close_descriptors \
  status_io::tests::short_writes_and_interrupted_io_complete_one_frame \
  status_io::tests::write_stall_times_out_at_write_stage \
  status_io::tests::broken_peer_does_not_raise_sigpipe \
  silent_peer_times_out_during_read drip_feed_cannot_renew_budget \
  hup_with_complete_frame_is_not_truncated \
  valid_status_and_business_error_are_replies truncated_frame_is_rejected \
  malformed_frame_is_rejected oversized_stream_is_bounded \
  protocol_and_request_id_mismatches_are_rejected frame_limit_excludes_newline; do
  grep -Fqx "test $name ... ok" "$TMP/ipc.log"
done

if ! cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml \
  -p superpowers-kanban-runner --test daemon_liveness \
  -- --test-threads=1 --format pretty --color never >"$TMP/runner.log" 2>&1; then
  cat "$TMP/runner.log" >&2; exit 1
fi
cat "$TMP/runner.log"
for name in new_socket_keeps_same_pid_and_old_sockets_are_untouched \
  three_misses_exit_and_release_lock disabled_switch_still_probes \
  success_resets_the_streak silent_status_peer_exits_under_empty_loop_budget \
  missing_instance_home_fails_before_state_side_effects \
  long_instance_home_fails_before_state_side_effects bound_card_state_is_not_changed_by_probing; do
  grep -Fqx "test $name ... ok" "$TMP/runner.log"
done

if ! cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml \
  -p superpowers-kanban-runner --lib -- --format pretty --color never >"$TMP/runner-lib.log" 2>&1; then
  cat "$TMP/runner-lib.log" >&2; exit 1
fi
cat "$TMP/runner-lib.log"
for name in client::tests::probe_preserves_error_and_accepts_business_error \
  single_instance::tests::misses_accessor_tracks_only_consecutive_failures; do
  grep -Fqx "test $name ... ok" "$TMP/runner-lib.log"
done
