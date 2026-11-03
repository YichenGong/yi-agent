# 插件清单自述：全阶段限时、输出排空与取消设计

日期：2026-11-03
基线提交：`20fc3133f04a3659038808cac694adfd1bc8f71e`
状态：待评审、待实现。本轮随机审查 `yi-agent-supervisors`，只交付修复 spec，不修改生产代码。

## 1. 问题与证据

### 1.1 已确认根因

`yi-agent-supervisors/src/refresh.rs:110–153` 的 `describe`：

1. 将 stdout/stderr 都设为 `Stdio::piped()`。
2. 循环 `try_wait()`，**期间不读取任何管道**。
3. 只有直接子进程退出后，才调用 `wait_with_output()`。
4. 15 秒截止时间只在 `Ok(None)` 分支检查，读取输出及错误清理不在截止时间控制之内。

这产生两个同源故障：

- **F1：管道背压误判超时。** 输出超过内核管道容量时，子进程卡在 write、无法退出；宿主等退出而不读取，于是本来有效的清单也被丢弃。stderr 写满同样可以阻断有效 stdout。
- **F2：直接子进程退出后失去超时。** 后代继承 stdout/stderr 写端时，直接子进程可正常退出，但 `wait_with_output()` 会等所有写端关闭。后代永不关管道时，这一读取没有上限。

生产影响有代码级证据：`yi-agent-runtime/src/supervise.rs:176–188` 在进入对账循环前同步刷新，F2 因而阻止该项目的插件起停与路由发布；`supervise.rs:35–39` 的 shutdown 设置 stop 后 join，而刷新不读取 stop，停止/Drop 也会等卡住的刷新。**这不是整个 app-server 或 daemon socket 必然无法启动的证明**，影响限定在这条监督线程及等待它的停机路径。

### 1.2 复现方法与已测结果

使用原始 `describe` 函数的完整文本和原始 15 秒常量，编译为只依赖标准库的独立程序。用临时 Python 可执行文件替代插件，每个场景单独进程组，外层 25 秒 backstop 清理整个组。不调用真实插件，不写真实实例配置。

第一轮观测：

| 场景 | 返回 | 原始函数耗时 |
|---|---|---|
| 66 字节正常清单 | `Ok(66)` | 0.448 秒 |
| 合法 JSON 清单，单个 args 值为 2 MiB | `Err(... did not answer within 15s)` | 15.014 秒 |
| 66 字节清单，直接子进程启动继承管道的 20 秒后代后退出 | `Ok(66)` | 21.678 秒 |

补充轮次（多 worktree 编译高负载）：256 KiB 合法清单 15.017 秒后被误判超时；先输出 2 MiB stderr 再输出小清单，15.009 秒后也被误判超时；继承管道场景触发外层 25 秒保护并被清理。本轮不把外层超时当作精确函数耗时。小清单控制组仍成功（函数 3.721 秒）。

F1/F2 为**原始函数 + 合成插件的实际复现**，不是线上事故观测。后代更长寿会延长 F2 等待，永久持有写端可导致无界等待，这是控制流推论，不冒称已经无限时长实测。

已有刷新测试主要覆盖新旧清单、命令不可运行、名称不匹配、坏 JSON 及字段差异（`refresh.rs:192–353`），没有验证两根管道背压、退出后持有写端或刷新取消。

### 1.3 同仓库正确模式

`yi-agent-tools/src/shell/bash.rs:168–222` 在启动时建立独立进程组，并在等待完成之前并发读取两根管道；超时/取消清理进程组。这是本设计的生命周期参考，不直接引入 tools 依赖：supervisors 当前为同步代码，本次不新增 Tokio 或改变依赖方向。

## 2. 目标、不变量与非目标

### 2.1 目标

- 从成功 spawn 到完整收集输出的**所有状态**都受同一个单调时钟截止时间约束；保留生产 15 秒预算。
- stdout/stderr 同时排空，不能因为先读一根而被另一根堵住。
- 直接子进程退出不是操作完成；成功必须满足：退出成功、两根管道均 EOF、stdout 未超限且 UTF-8 有效。
- 刷新停止请求能中断自述和整个批次，不启动后续插件。
- 超时、取消、输出超限、IO 失败均清理本次自述资源，保留磁盘原清单。
- 内存有硬上限，持续输出不得饿死截止时间/取消检查。

### 2.2 保持不变

- 只执行已安装清单的 `<command> manifest`，不按插件名猜测命令。
- 仅同名且可解析的新清单允许原子替换；失败不覆盖原文件。
- `Current` / `Refreshed` / `Skipped` 现有公开结果类型保持不变。
- 启动时只刷新一次，正常运行仍每 500ms 对账。
- 开关来源、socket 路径、清单 schema、安装/卸载协议均不变。

### 2.3 非目标

- 不修正常 `run` 子进程的日志、重启退避、孤儿接管或二进制升级检测。
- 不重构 `plugin/install` 自述入口，不声称该入口已经具有本设计的保障；未来可另审。
- 不改变原子写入的并发竞争、文件名/name 对齐和路径校验。
- 不做插件自述缓存、并发刷新队列或一次刷新全目录的总时间预算。未取消时 N 个坏插件仍可能串行消耗约 N×15 秒。
- 不保证停止自行 `setsid` / 改进程组的恶意后代。即使后代逃逸且仍持管道，宿主也必须按期限关闭自己的读端并返回。
- 不承诺 OS 内核长期不可中断状态下的绝对墙钟限时；spawn 和最终直接子进程回收仍受平台调度约束。不得以此为理由留下用户态阻塞读或无界 reader join。

## 3. 方案选择

1. **推荐：同步非阻塞双管道状态机 + 进程组 + 统一截止时间/取消。** 使用现有 `libc` 做 `fcntl`，通过单一循环兼顾排空、不回收的退出观察、截止时间及取消。无新增依赖、无 reader 线程泄漏。代价是要显式处理 EOF/WouldBlock/Interrupted 和公平性。
2. 两个阻塞 reader 线程 + 等待线程：解决 F1，但直接子进程退出后后代持写端仍会卡 reader join。若不 join 则泄漏线程/句柄；完善到可取消读取后复杂度不低于方案 1，不采用。
3. 放宽超时或改成 `Command::output()`：前者不能解决退出后的无界读取，后者没有超时，也不提供取消和输出上限，不采用。

## 4. 自述执行器

实现局限于 `yi-agent-supervisors/src/refresh.rs` 的私有执行器；不扩展为通用进程框架。

### 4.1 策略与输入

私有 `DescribePolicy` 统一持有：

| 字段 | 生产值 | 语义 |
|---|---|---|
| timeout | 15 秒 | 整个自述操作的单调时间预算 |
| poll_interval | 20 ms | 无进展时等待及取消检查粒度 |
| stdout_limit | 1 MiB（1,048,576 字节） | 恰好等于上限允许，下一字节触发失败；不得截断后尝试解析 |
| stderr_capture_limit | 64 KiB（65,536 字节） | 仅保存此前缀，后续字节继续排空并丢弃，不因 stderr 多而失败 |
| read_chunk | 8 KiB | 单次 read 的固定缓冲 |
| drain_budget_per_stream | 64 KiB/轮 | 每根流每轮最大读取量，防止持续写入饿死其他状态检查 |

测试注入短预算（如 300ms），不能改全局生产常量或靠环境变量控制。读取字节数按字节计；失败诊断只取 stderr 第一行，展示最多 512 个字符，超过则标明截断。stdout 内容不进日志，避免带入自述中的敏感参数。

### 4.2 启动与所有权

1. spawn 前检查取消；建立单一 deadline。
2. Unix `CommandExt::process_group(0)` 为这次 `manifest` 命令建立独立组，保存 pgid。它不是正常长驻 `run` 进程的组。
3. stdin 为 null，接管 stdout/stderr，分别设置 `O_NONBLOCK`（保留原 flags）。任何设置失败走清理，不进入阻塞读。
4. 私有资源 guard 独占 child、pgid 与读端，负责所有返回/展开路径的清理。不创建后台 reader 线程。

### 4.3 每轮状态机

每轮依次：

1. 检查取消与 deadline；已经到达 deadline 不允许因看到退出状态而改判成功。
2. 各排空至多 64 KiB：`read=0` 记录 EOF；`WouldBlock` 让出；`Interrupted` 先回到预算/取消检查；其余 IO 错误返回失败。
3. 保存 stdout 直至 1 MiB，超限即失败；保存 stderr 前缀，超限后继续读取并丢弃。两流互不等待。
4. 使用 Unix `waitid(P_PID, ..., WEXITED | WNOHANG | WNOWAIT)` 的私有平台封装观察退出，保存退出成功/失败信息，**此时不回收直接子进程**；没有退出事件时继续轮询。已经观察到退出后仍处理未 EOF 的流，并继续检查同一个 deadline。观察失败返回 IO 错误并清理，不回退到会回收 child 的 `try_wait`。
5. 只有 `status + stdout EOF + stderr EOF` 全齐才判完成。非零退出返回错误；零退出再校验 stdout UTF-8。上层保持原解析/同名校验流程。
6. 没有进展时最多 sleep `min(20ms, deadline-now)`；有进展可继续下一轮，但每轮必须重查截止时间、取消。

禁止在退出分支调用 `wait_with_output` / 阻塞 `read_to_end`，禁止先排空一根至 EOF 再读另一根。

### 4.4 清理与错误

**进程组身份必须在清理前保持占用。** 直接子进程活着或已退出但未回收时，其 PID 不会被复用；因此先用 `waitid(... WNOWAIT)` 观察、不用 `try_wait` 提前回收，清理完组后才 `child.wait()`。不得在已回收 leader 后凭缓存 pgid 继续发信号，也不得把 pgid 留到操作之外。

超时、取消、stdout 超限、读取/等待错误：对**此次隔离组**发 SIGKILL，关闭本地读端，最后回收直接子进程。`ESRCH` 是组已退出的正常情况；其他清理失败有可读诊断，但不覆盖主要失败原因。直接子进程已退出而后代还在时仍必须清理组，不仅调用 `child.kill()`。

成功路径同样收口此次自述组中残存的普通后代，避免 `manifest` 意外留下后台进程；不得发信号给正常监督插件或宿主所在组。guard 完成收口后解除，避免重复清理和 pgid 长期存储。

所有失败转为现有 `Skipped { name, reason }`，reason 明确区分 timeout、cancelled、stdout 超限、非 UTF-8、非零退出及 IO 失败。超限 reason 带上阈值；stderr 截断诊断明确带截断标记。

## 5. 刷新取消与 runtime 接线

保留现有 `refresh_from_plugins(dir)`、`refresh_and_log(dir)`，作为“永不取消”的兼容入口。新增带取消谓词的对应入口，建议命名 `refresh_from_plugins_until(dir, should_stop)` 与 `refresh_and_log_until(dir, should_stop)`；谓词必须传递到同一私有自述执行器，不复制执行逻辑。

- 每个命令前、执行器每轮、写回前检查取消。
- 批次开始即取消时返回空结果，不执行命令。
- 自述进行中取消：该项产生 cancelled Skipped 并保留原文件，剩余项不执行、不伪造结果。
- 已完成项保留其结果；取消后的写回不开始。检查之后已经进入一次原子写入时允许该单次写入完成，不声称取消可撤销已提交的 rename。
- `refresh_and_log_until` 对取消可用 debug 记录，不把正常停机误报插件故障；其他失败继续 warn。
- runtime `serve_inner` 使用该入口，谓词读取已有 stop AtomicBool。刷新返回后若 stop 已设，不开始新一轮对账。既有 cleanup 路径仍必须清空本项目路由。

停止时间契约：忽略 OS 不可中断状态时，停止请求在下一次轮询观察到，取消返回不再等待余下 15 秒或后代自行 EOF。自动化用 2 秒宽 backstop 验证，不把 20ms 设计粒度当 CI 精确性能断言。

## 6. 验收与回归测试

以下测试名称是**待实现的验收目标**，本轮未添加/未通过它们。

| 测试目标 | 输入与必须断言 |
|---|---|
| `describe_drains_large_stdout_without_a_false_timeout` | 合法 256 KiB args 清单（低于新上限），使用正常 15 秒预算、宽外层 backstop，必须成功并验证内容完整；不套用超时测试的 300ms 注入预算 |
| `describe_drains_stderr_after_its_capture_limit` | 先写 2 MiB stderr，再输出小清单；必须成功，不因 stderr 超 capture 上限阻断 |
| `describe_deadline_covers_inherited_pipes_after_exit` | 父输出有效小清单后退出，后代持写端并长期等待；短 deadline 后返回 timeout，不等后代自行退出 |
| `describe_times_out_while_output_keeps_arriving` | 子进程持续输出 stderr，deadline 仍生效，证明 drain 公平性 |
| `describe_rejects_stdout_over_limit_without_rewriting` | stdout 刚好上限可以收集，超一字节拒绝；原清单字节不变，不解析截断内容 |
| `describe_cleanup_stops_its_process_group` | 超时/取消时父及组内后代均停止；退出观察尚未回收 leader、组清理先于最终 wait；不以只杀直接子进程作为充分证据 |
| `describe_keeps_bounded_failure_diagnostics` | 非零退出、空 stderr、超长第一行、非 UTF-8 stdout、IO 注入失败均可区分；首行展示和 capture 硬限成立 |
| `cancelled_refresh_does_not_start_the_next_plugin` | 用启动 marker 同步：A 自述中取消，A 原文件不变，B 没有启动 marker；先前成功项不回滚 |
| `stopping_during_manifest_refresh_joins_promptly` | runtime 真实刷新入口，不用 `serve_without_manifest_refresh`；自述已启动后 stop，在 2 秒 backstop 内返回 |
| `a_slow_description_does_not_block_later_supervision` | A 短预算超时后 B 仍完成自述；runtime 进入真实对账循环，B 启动并可查询 |

测试规范：

- 临时目录、可区分 `manifest` 与正常 run 的假插件、独立进程组；不写全局 flags、不操作线上插件。
- 用 marker/条件轮询证明已进入目标阶段，再触发取消，不靠固定 sleep 猜时机。
- 后代自身必须有可回收 backstop；测试失败也清理，禁止让回归测试永远挂住。
- 内部短 deadline 与外部宽 backstop 分开；必要时用进程级驱动隔离卡死代码，不能在卡住的 Rust 线程上再无限 join。
- 先在旧逻辑上看到新增场景失败，再修复；变异验证：恢复“等退出后读”会使双流测试变红；退出后停止查 deadline 会使继承管道测试变红；去掉 stop 接线会使 runtime 停机测试变红；只杀直接 child 会使组清理测试变红。

实现后执行：

```sh
cd yi-agent-rs
cargo fmt --all
cargo test -p yi-agent-supervisors
cargo test -p yi-agent-runtime --lib supervise
cargo clippy -p yi-agent-supervisors -p yi-agent-runtime --all-targets -- -D warnings
just ci
```

依赖不变；使用 API 必须兼容当前 workspace `rust-version`，并由 `just ci` 的 msrv-check 验证。实施提交后更新 bug-list 为已修复、runtime 模块完成判据与索引计数。本轮只标未完成。

## 7. 兼容性与风险

- **新增 stdout 1 MiB 硬限是显式协议收紧。** 正常小清单不变，超大清单不再尝试无限收集；原清单保留、告警说明限值。2 MiB 的第一轮复现将被明确拒绝，不应要求它在新设计中成功；F1 的正向验收改用 256 KiB 有效清单及大 stderr。
- 非零退出也要排空/超时，不以错误状态为由进入阻塞收集。
- 逃逸后代可能继续存在，但持有的管道不能继续绑住宿主；这不等于完整沙箱或恶意插件隔离。
- 一次 SIGKILL 不能回收非直接后代的 wait 状态；本设计只 wait 自己的直接 child，后代由系统父进程回收。
- stop 谓词只在刷新线程调用，兼容入口不提供取消；生产 runtime 必须切换到取消入口，否则修了 F1/F2 仍留停机等待 15 秒的问题。

## 附录 A：原始函数复现脚本

在仓库根目录，将下列内容保存到临时文件运行。要求 Python 3 与 PATH 中的 rustc。程序从源码抽取未修改函数，临时目录自动删除；每个场景最终杀掉本次进程组。外层阈值只是复现保护，不是生产代码的修复。

```python
import os, signal, subprocess, tempfile, time
from pathlib import Path

root = Path.cwd()
source = (root / 'yi-agent-rs/crates/yi-agent-supervisors/src/refresh.rs').read_text()
fn = source[source.index('fn describe(command:'):source.index('\n/// 同目录 `tmp`')]
rust = ('use std::time::{Duration, Instant};\n'
        'const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(15);\n' + fn +
        '\nfn main() { let c = std::env::args().nth(1).unwrap(); '
        'let t = Instant::now(); println!("{:?} elapsed={:?}", '
        'describe(&c).map(|s| s.len()), t.elapsed()); }\n')
small = 'print(json.dumps({"name":"demo","command":"/bin/true","switch_key":"demo_on"}))\n'
cases = {
    'small': small,
    'large': 'print(json.dumps({"name":"demo","command":"/bin/true",'
             '"switch_key":"demo_on","args":["x"*(2*1024*1024)]}))\n',
    'medium': 'print(json.dumps({"name":"demo","command":"/bin/true",'
              '"switch_key":"demo_on","args":["x"*(256*1024)]}))\n',
    'noisy': 'sys.stderr.write("x"*(2*1024*1024))\n' + small,
    'inherited': 'subprocess.Popen([sys.executable,"-c","import time; time.sleep(20)"])\n' + small,
}
with tempfile.TemporaryDirectory(prefix='refresh-repro-') as tmp:
    folder = Path(tmp)
    (folder/'probe.rs').write_text(rust)
    subprocess.run(['rustc','--edition=2024',str(folder/'probe.rs'),
                    '-o',str(folder/'probe')], check=True)
    for name, body in cases.items():
        script = folder/name
        script.write_text('#!/usr/bin/env python3\nimport json, subprocess, sys\n' + body)
        script.chmod(0o755)
        p = subprocess.Popen([str(folder/'probe'),str(script)],
                             stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                             start_new_session=True)
        try:
            out, err = p.communicate(timeout=25)
            print(name, p.returncode, out.decode().strip(), err.decode().strip())
        except subprocess.TimeoutExpired:
            print(name, 'exceeded external 25s backstop')
        finally:
            try:
                os.killpg(p.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            p.communicate()
```
