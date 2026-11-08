# 运行参数即时生效：Task 14 验证记录

日期：2026-11-08
分支：`runtime-settings-live-apply`
性质：本文件记录**实际执行过的命令与结果**，不是计划。未执行的项明确标注。

## 1. 跨进程写入（真实双进程）

文件：`yi-agent-rs/crates/yi-agent-runtime/tests/live_settings_process.rs`

命令与结果：

```bash
cargo test -p yi-agent-runtime --test live_settings_process
# test result: ok. 8 passed; 0 failed; 0 ignored
```

覆盖的判据：

| 测试 | 判据 |
|---|---|
| `two_processes_writing_different_keys_keep_both` | 两个独立进程并发写不同键，两个键都在：证明 `flock` 真的跨进程排他 |
| `same_key_last_committed_wins_across_processes` | 同键按提交先后定胜负（不用 sleep 排竞态：收到 COMMITTED 才发下一次写） |
| `the_first_process_observes_a_remote_commit` | 另一个进程写盘后，持有实例的一方 `refresh` 能看到新值与新 revision |
| `a_held_lock_times_out_instead_of_corrupting_the_file` | 父进程持锁时子进程提交以 `LockTimeout` 有界失败，文件不被写坏 |
| `the_lock_file_is_never_unlinked_so_inodes_stay_stable` | 锁文件提交后仍存在（unlink 会让两个进程各拿一把不同的锁） |
| `a_child_that_dies_mid_commit_releases_the_lock` | 持锁进程被 kill 后内核释放锁，下一个进程立刻可提交 |
| `consumer_manifest_covers_all_fields` | 消费清单的键集合与 `FIELDS` 精确相等 |

**子进程协议不用 sleep**：父测试用 `current_exe --exact settings_writer_child` 拉起
同一测试二进制的子进程，通过 `YI_SETTINGS_CHILD READY` / `COMMITTED` / `TIMEOUT`
三行专用前缀握手。读侧走独立线程 + 20 秒 deadline，避免"断言失败导致读阻塞、
整个测试挂死"（挂死的测试既不红也不绿，比失败更坏）。

无 `YI_TEST_SETTINGS_CHILD_PATH` 时子进程测试立即返回，故普通全量运行里它是一条
通过的空测试，不碰任何文件。

### 变异验证（RED 证据）

把 `acquire_writer_lock` 里的 `flock` 改成无条件 break（即"不做跨进程排他"）：

```
test two_processes_writing_different_keys_keep_both ... FAILED
test a_held_lock_times_out_instead_of_corrupting_the_file ... FAILED
等 `YI_SETTINGS_CHILD TIMEOUT` 超时（20s）：子进程没有按协议推进
```

恢复后 8 条全绿。说明这两条判据不是空转。

## 2. CLI 端到端（真实子进程 + 假 provider）

文件：`yi-agent-rs/crates/yi-agent/tests/runtime_settings_live_e2e.rs`

```bash
cargo test -p yi-agent --test runtime_settings_live_e2e
# test result: ok. 7 passed; 0 failed; 0 ignored
```

关键一条 `a_save_from_another_process_reaches_the_next_real_request`：

1. 在临时 `YI_AGENT_HOME` 下写 `settings.json` 的 `agent.system_prompt = PROMPT-ALPHA`；
2. 起真实 `yi-agent run` 子进程，断言假 provider 收到的第一个请求的 `system`
   含 `PROMPT-ALPHA`；
3. 由**测试进程**（不是子进程）提交 `PROMPT-BETA`；
4. 再起一次 `run`，断言第二个请求含 `PROMPT-BETA` 且**不含** `PROMPT-ALPHA`。

断言落在**假 provider 实际收到的请求体**上，不是 `AgentConfig`：配置正确而循环
没去读时，只有看请求才抓得到。

其余判据：`a_corrupt_file_does_not_silently_look_like_defaults`（坏 JSON 必须
`Degraded` 且带 code）、`the_child_process_never_reads_the_real_host_settings`
（隔离自检：settings 与锁文件都在临时实例根内）。

### 变异验证（RED 证据）

把 `binding.rs` 的 `user_prompt: value("agent.system_prompt")...` 改成 `None`：

```
test a_save_from_another_process_reaches_the_next_real_request ... FAILED
首个请求必须带上文件里的提示，实际收到 [...]  -> 只有内置系统提示
```

### 一个真实缺陷的记录

变异后失败信息显示 system 里是完整的内置提示词（含本机 skill 路径）。这是**预期
行为**：`agent.system_prompt` 是"用户段"，追加在内置提示之后，不是替换它。

## 3. 全字段消费覆盖

脚本：`yi-agent-rs/scripts/check-live-settings-coverage.py`
清单：`yi-agent-rs/scripts/runtime-settings-consumers.json`

```bash
python3 scripts/check-live-settings-coverage.py --list-only
# OK: 16 个字段全部有可执行的消费测试（仅校验存在性）

python3 scripts/check-live-settings-coverage.py
# OK: 16 个字段全部有可执行的消费测试（已逐项执行）
```

脚本对每个字段真实执行那个测试，并要求：退出码 0、`N passed` 且 N ≥ 1、
`0 ignored`。**只看退出码不够**——名字匹配不上时 cargo 也返回 0。

### 它在开发过程中抓到的真实问题

第一版跑出：

```
FAIL: 测试没有真正运行（0 个用例通过）：
  cargo test -p yi-agent-app-server --lib -- --exact power_guard::tests::disabling_the_guard_releases_the_real_assertion
  test ... ignored
```

该测试是 `#[ignore]` 的真机测试（要读系统 `pmset` 断言表）。换成同字段的
`sleep_hardening_disable_wakes_the_worker_and_skips_the_grace`（真实运行的单元
测试）后通过。这正是脚本要求的"每项必须是真实运行、非 ignored"。

### 变异验证（RED 证据）

| 变异 | 脚本输出 |
|---|---|
| 从清单删掉 `compact.ratio` | `FAIL: 以下字段在 FIELDS 里但没有消费测试映射：['compact.ratio']` |
| 把 `agent.max_turns` 的测试名改成不存在的名字 | `FAIL: agent.max_turns 映射的测试不存在：yi-agent-core/test live_compact_settings -> this_test_does_not_exist` |
| 往 `FIELDS` 里加一个 `tui.extra_probe` | `FAIL: 以下字段在 FIELDS 里但没有消费测试映射：['tui.extra_probe']` |

三条都变红，恢复后全绿。

## 4. 本任务修复的两条测试缺陷（都是本次新引入的测试）

修复它们的过程本身就是证据：两条都是**偶发失败**，且都在单独跑时永远绿。

### 4.1 `source_failure_returns_queued_interjections`（core）

现象：全量跑偶尔红，单独跑 6 次全绿。

根因：`run()` 之后才 `interject`。若循环已经到达第一个安全点并因 source 失败而
`flush_unconsumed`，插话还没进 inbox，那一帧就没机会被返回。

修法：让第一次 `snapshot()` 用两个 `Barrier` 做会合点——先发 `entered`，等测试
把插话投进 inbox，再由测试 `release`。这样"插话先于失败"是**因果**而非运气。

期间踩到一个自己写的 bug：`AtomicBool::swap` 返回**旧值**，我把条件写反了
（`!swap(...)`），导致第一次调用反而不等待，测试永久挂死。该 bug 与本注记一起
留在代码注释里。

验证：连续跑 15 次全量，15/15 绿。

### 4.2 `an_external_edit_reaches_clients_through_the_observer`（app-server）

现象：约每 6 次红一次，且红时是 30 秒超时（等基线帧）。

根因有两层：

1. 测试假设"必然先收到一条基线帧"。泵的握手是"取基线 → 订阅 → 补齐"，它可能把
   基线本身吸收成起点，那条帧于是永远不会到来。
2. 更隐蔽的一层：泵握手时的 `refresh()` 会读盘。若测试已经写了新值（6），泵就把
   **6 当成基线**吸收了，从那以后没有任何"变化"可播。

修法：

- 不等基线帧，改用 `wait_for_published_max_turns` 轮询**已发布快照**确认基线到位，
  再写新值。这是必须先建立的因果：泵把"启动时文件内容"当基线是正确行为（否则
  每个客户端一连上就收到一条无意义的失效帧）。
- 记下基线 revision，在等帧的循环里排除它。理由：文件原本不存在时"无 → 5"本身
  就是一次变化，会推一条 `changed_keys` 相同的帧进队列；只按 `changed_keys` 匹配
  会读到那条旧帧。

验证：连续跑 10 次，10/10 绿；把观察器接线改成不观察（变异）后，本条与
`a_health_regression_is_broadcast_even_with_no_changed_keys` 在 3.5 秒内变红
（不是 30 秒超时），说明修复后的判据是紧的。

## 5. 本轮回归命令汇总

```bash
cargo test -p yi-agent-runtime --test live_settings_process      # 8 passed
cargo test -p yi-agent-core --test live_agent_settings           # 11 passed（连续 15 次）
cargo test -p yi-agent-core --test live_compact_settings         # 13 passed
cargo test -p yi-agent-store --test live_admission               # 8 passed
cargo test -p yi-agent-app-server --lib runtime_settings         # 14 passed（连续 10 次）
cargo test -p yi-agent-subagent --lib                            # 105 passed
cargo test -p yi-agent --test runtime_settings_live_e2e          # 7 passed
python3 scripts/check-live-settings-coverage.py                  # 16 字段逐项执行
cargo fmt --all -- --check                                       # 干净
cd yi-agent-rs && just ci                                        # EXIT=0 / CI passed
```

前端：

```bash
cd desktop && npx tsc --noEmit          # 无错
npx vitest run                          # 2327 passed（167 files）
npm run build                           # EXIT=0
```

## 5.1 自查阶段发现并修复的前端缺陷（4 处）

提交前按 spec §8.2 逐条对照实现，发现四处真实缺陷。每处都先写会红的测试，
再修，最后做反向变异（把修复回退必须变红）确认测试非空转。

| # | 缺陷 | 后果 | 测试 |
|---|---|---|---|
| 1 | 每次后台刷新都 `setLoading(true)`，整块表单被换回「读取中…」 | 输入框卸载重挂：正在打字时别的窗口保存一次，光标/选区丢失，滚动位置跳回顶部。实测 `sameNode=false`、`focusKept=false` | `refreshes in place without unmounting the form or stealing focus` |
| 2 | 提交时的代数快照写在 `setDraftState` 的 updater 里 | updater 何时执行由 React 决定；取到空对象时每个提交键都被判成「代数未变」而清掉，保存期间的新输入消失 | `clears the submitted draft even when the follow-up read fails`（变异成空快照即变红） |
| 3 | `subscribe` 的 effect 把 `saving` 放进依赖 | 保存开始/结束各退订重订一次，两次之间到达的通知永久丢失——而「保存期间别人也保存了」正是最需要补读的时刻。实测日志 `SUB,UNSUB,SUB` | `keeps its subscription while a save is in flight` |
| 4 | 保存成功后无条件写成功文案 | 紧随的权威读失败时，错误被成功提示覆盖，用户看到绿灯却不知道界面值是旧的（违反 spec §8.2） | `does not let the success toast hide a failed follow-up read` |

缺陷 3 的测试写法值得记录：最初写的是「保存期间插一条通知，等保存结束应该补读」，
**但这个测试抓不住缺陷**——重订发生在同一次渲染提交内，测试无法在那个极短窗口
插入通知。改成直接断言订阅次数（保存前/保存中/保存后都应恰好一次订阅）后才
真正变红。这是"测试看似合理但空转"的实例。

## 6. 未执行项（如实记录）

- **手机端保存的人工验收未执行**：本机无配对设备。代码路径（`settings/write`
  经 control 作用域 RPC）已有自动化覆盖，但"真机点保存"这一步没有做过。
- **桌面端人工点击验收未执行**：前端有 2323 条自动化测试（含 24 条运行参数面板、
  2 条 App 层 `settings/changed` 接线），但没有人手点过真实 Tauri 窗口。
- **旧 daemon 二进制**：只用"无 capability 的旧响应 fixture"验证，没有拿真的旧
  二进制对跑。

因此本次不宣称"全平台人工验收完成"。自动化判据如下节。
