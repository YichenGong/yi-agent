# 互动延时剖析：一次「发消息 → 收到回复」到底慢在哪（2026-11-06）

本文件是一次实测剖析的记录，不是设计文档。所有数字都注明来源与采集方式，
可复现的脚本在 `scripts/latency_profile.py`。

**一句话结论：慢不在我们的代码里。** 一次交互的墙钟时间里 90%+ 是
provider（`HORIZON-DeepSeek` 网关）的首字节等待；我们自己 agent 循环、
事件下发、落盘加起来是**毫秒级**。真正可优化的是**长任务里 provider 被调用的
次数**（每次 2~4 秒，一轮任务几十次），以及一个**同步阻塞的文件搜索工具**。

---

## 1. 采集方式（无侵入，全部基于已有能力）

| 手段 | 用途 |
|---|---|
| `~/.yi-agent/trace/session-*.jsonl` | app-server 已内建的 tracing JSONL，含 `sending request` / `response received` / `accumulate_stream: received Stop` / `tool call start|done` 等锚点 |
| `sample <app-server-pid>` | macOS 采样，判断主消息循环是否被阻塞 |
| `yi-agent run` | 端到端计时（含进程启动、prompt 组装、一次真实 LLM 往返） |
| 直连网关的 HTTP 探测 | 分离「网络/TLS」「网关排队」「上下文预填充」「生成」四段 |

环境：macOS 26.5.1 / 18 核 / load average 3.8；app-server 为该时刻 CPU 4.6%、
常驻内存 39.8 MB —— **排除了本机 CPU 拥塞与内存压力**。

---

## 2. 结论总表

### 2.1 单轮交互的组成（今天 870 次 provider 调用）

```
provider TTFB        n= 870  p50= 2397ms  p90= 4733ms  p99= 9430ms   ← 占绝对多数
stream consume       n= 868  p50= 1734ms  p90= 6571ms  p99=19073ms
agent loop overhead  n=1017  p50=    1ms  p90=    1ms  p99=    2ms   ← 我方代码
```

`agent loop overhead` 的定义：上一次 `tool call done` 到下一次 `sending request`
之间的间隔。它覆盖了 OBSERVE 回填、消息组装、system prompt 拼接、事件入队。
**p50 = 1 ms，全天总计 0.8 秒**。这一条基本可以判我们自己的编排链路无罪。

（注：n 会随时间增长，上面的快照取自剖析过程中的某次采样；用
`scripts/latency_profile.py 2026-11-06` 可复算当前值。）

一个纯文本单轮（`session-20261106-143241`）的逐段分解：

```
0.00s  tracing initialized           进程起来
0.01s  sending request               请求发出
2.40s  response received             首字节到达        ← 2.39s，占 97.6%
2.45s  accumulate_stream: Stop       正文收完（50ms，短回答）
2.45s  agent loop done: end_turn
```

跨日复核（昨天 2026-11-05，18 个会话 / 132,589 条事件）：

```
provider TTFB        n=5308  p50= 2035ms  p90= 4274ms  p99=13706ms
stream consume       n=5304  p50= 1200ms  p90= 6056ms  p99=21110ms
agent loop overhead  n=6614  p50=    1ms  p90=    1ms  p99=    3ms   ← total 41.5s
```

两天数据的形状一致：**TTFB 是主项，我方开销 p50 恒为 1 ms**。

### 2.2 为什么首字节要 2 秒（对照实验）

对同一网关、同一 key，用最小请求（`hi`，8 token 上限）测 TTFB：

| 场景 | TTFB |
|---|---|
| 新建连接（DNS 3ms → TCP 369ms → TLS 790ms） | 2.2 ~ 3.6s |
| 连接复用后连续 3 次 | 2209 → 1458 → 1181 ms |
| 走本地代理（127.0.0.1:7897） | 稳定 2.1 ~ 2.3s |
| 直连（不经代理） | 2.2 ~ 3.6s，TLS 抖动 0.8~2.4s |
| `GET /v1/models`（非推理端点） | 1.5s |

**结论**：即使一个 5 token 的空请求，也要 ~1.2s（复用连接）/ ~2.0s（新连接）。
这是「网关 + 这条链路」的底噪，不是我们发的东西多。

可以据此把一次 provider 往返拆成三段：

```
TCP+TLS 握手    ~1.1s（仅新连接；连接池命中时省掉）
网关排队+底噪   ~1.2~2.0s（不可控）
预填充+首 token = 余量（与上下文长度相关）
```

### 2.3 上下文长度的影响（可控的那部分）

| 上下文 | TTFB（median） |
|---|---|
| ~5 tok | 1988 ms |
| ~45k tok | 4083 ms |
| ~90k tok | 4031 ms |

真实会话的 `input_tokens`（来自 thread jsonl 的 usage 记录）：
**107,946 ~ 153,417**，其中 `cache_read_input_tokens` 占 ~99%
（如 107,946 里 106,752 命中缓存）。也就是说每轮都在重发十几万 token，
虽然大部分命中缓存，但预填充仍然把 TTFB 从 2.0s 推到 4s 量级。

### 2.4 生成速度与「长回答」

`max_tokens=400` 的 273 字回答：TTFT 2.9s，生成 1.9s，**143 字/秒**。

所以一段 300 字的回答，用户感知的等待是 `2~4s 首字 + 约 2s 吐完`。
流式输出已经是最优形态（首字即显示）；`desktop` 侧还有 60ms 的文本节流
（`desktop/src/lib/useThrottledText.ts`），以及 app-server 侧 100ms 的
delta 合并（`turn_driver.rs` 的 `delta_tick`），两者对「首字」没有影响。

### 2.5 一次「任务」而非「一轮」的成本

今天 870 次 provider 调用、1017 次工具调用，说明一个任务平均要
**几十到上百轮**。按每轮 provider 往返 p50 ≈ 2.4s + 流式 p50 ≈ 1.7s 计算，
一个 50 轮的任务光模型往返就是 **3~4 分钟**，这还没算工具耗时。

这才是「感觉每次互动的速度比较慢」的结构性来源：
**单轮 4 秒不算离谱，几十轮串起来就是几分钟。**

---

## 3. 工具执行：中位数很好，尾部由一个真实缺陷造成

```
bash        n=920  p50=   24ms  p90= 11.9s  p99=240s   total=9290s
grep        n=  8  p50=  396ms  p90= 178.5s p99=178.5s total= 358s
wait_agent  n=  4  p50= 14.5s   p90=  30.0s            total=  57s
read/edit/write/process_*       p50 ≤ 3ms
```

`bash` 的 p50 只有 24ms —— 工具本身不慢。尾部那些 120~340 秒的调用，全部是
模型**主动写的 `sleep 340; tail ...`**（等 CI 日志），属于用法问题不是实现问题。

### 3.1 真缺陷：`grep` / `glob` 在 tokio 任务里同步扫描整个项目

> **状态：已修复（2026-11-07）。** 本节保留的是修复前的观测，作为判断依据。
> 修复内容与改造后实测见本节末尾「修复结果」。所有原始数字未改动。

`yi-agent-rs/crates/yi-agent-tools/src/fs/grep.rs:76` 的 `call` 签名是
`async fn`，但内部直接调用同步的 `grep_search`（`:103`），后者用
`WalkDir` + `std::fs::read_to_string` 逐个文件读（`:168`、`:186`），
**没有任何 `spawn_blocking`**。整个遍历期间它占着一个 tokio worker 线程
不让出。`fs/glob.rs:53` 的 `glob::glob()` 同理（同步迭代器）。

它们**没有跳过构建产物目录**。本仓库的体积分布：

```
yi-agent-rs/target        162 GB
项目根总文件数              1,704,405
  .worktrees/ 下             769,914
  desktop/node_modules         9,932
```

于是在项目里 `grep` / `glob **` 实际是同步遍历百万级文件、且包含 162 GB 的
`target/`。今天观测到的两次根目录 `grep` 都跑了 **178 秒**：各自约 55 秒出
结果，剩下 120 秒在等被同进程并行的十几个 `bash` 工具抢走的 CPU/IO
（样本里 grep 期间有连续 20+ 个 bash 调用穿插完成，间隔 3~10 秒）。
昨天的样本更能说明问题：

```
126.4s  grep   {path: "yi-agent-rs", pattern: "provider error: \\{", context: 5}   ← 扫 target/
 14.8s  glob   {pattern: "**/Cargo.toml"}                                          ← 全树遍历
```

换句话说：**它既自己慢，又把同进程的其他工具一起拖慢**。

（同样的根目录查询现在跑 0.4 秒，因为文件系统缓存已热、且当时没有并发争抢。
所以这条的严重性取决于「冷缓存 + 并发 + 目录没排除」，不是每次都必然上百秒。）

#### 修复结果

两处根因都改掉了，不是靠调大超时掩盖：

1. **排除构建产物**。`grep` 与 `glob` 改接同一个 `ignore` crate 的 walker
   （`yi-agent-rs/crates/yi-agent-tools/src/fs/walk.rs`），开启
   `git_ignore(true)` / `git_exclude(true)`，并额外用 override 强制排除 `.git`。
   `hidden(false)` 保留点目录（`.github/` 仍可搜）。
2. **不再占住 tokio worker**。两者的扫描都移进 `tokio::task::spawn_blocking`，
   并带 10 秒 deadline；截断会显式回一条
   `[scan stopped after 10s: ...]`，不会把不完整的扫描伪装成「没有匹配」。

改造后实测（本分支 worktree 根，含 2.6 GB `target/`，`FILES` 策略）：

```
walk /…/.worktrees/docs-latency-search-spec -> 2046 files in 50.978416ms
```

对照修复前：同一类根目录遍历是 **126.4 秒 / 178 秒**，且结果里含大量
`target/` 命中。现在是 **约 51 毫秒、2046 个文件、0 个 `target/` 路径、0 个
`.git` 路径**。

判据是可执行的，不依赖模型行为：

```bash
cd yi-agent-rs
cargo test -p yi-agent-tools --lib fs::walk -- --ignored --nocapture
```

其中 `real_repository_walk_skips_build_output_within_the_deadline` 直接遍历真实
仓库并断言：不触发 deadline、无 `target/`、无 `.git/`、真实源码仍在结果里。
它标了 `#[ignore]`，不进 CI。

---

## 4. 已排除的怀疑项

| 怀疑 | 证据 | 结论 |
|---|---|---|
| 主消息循环被阻塞（历史上的 IPC 问题） | `sample` 2323 个样本中 2257 个停在 `pthread_cond_wait`，`rpc::agent::handle` 仅 75 个 | 排除 |
| 每轮重建 HTTP client / 每次都握手 | `bootstrap.rs:11` 构建一次 `Arc<dyn Provider>`；app-server 有两条 `ESTABLISHED` 长连到网关 | 排除 |
| 系统代理拖慢 | 走代理 2.1s，直连 2.2~3.6s 且更抖 | 代理不是问题，反而更稳 |
| 进程启动 / prompt 组装 | trace：`tracing initialized` → `sending request` 仅 **6~11 ms** | 排除 |
| checkpoint 每 500ms 全量落盘 | `write_atomic` 对 586 KiB 实测 p50 0.3ms（`fsync` 后）；且这是同步 IO 在 driver 任务里，但成本可忽略 | 排除（不构成耗时） |
| 前端渲染 | delta 合并 100ms + 文本节流 60ms，不影响首字，只影响吐字粒度 | 排除 |

---

## 5. 可做的优化（按性价比排序）

1. **减少 provider 往返次数**（收益最大，直接砍 2~4 秒/次）
   - 让工具结果更聚合：一次 `grep`/`read` 拿到足够信息，避免「查一下→看一眼→再查」
     的 5~6 次往返。
   - system prompt 目前 5041 token（其中 skills/工具 schema 占大头），
     每轮都重发。对简单轮次可考虑裁剪到与任务相关的 skills。
2. **修 `grep` / `glob` 的同步遍历**（`fs/grep.rs:103`、`fs/glob.rs:53`）
   - 把 `grep_search` 与 glob 遍历挪进 `tokio::task::spawn_blocking`；
   - 遍历时默认跳过 `.worktrees/`、`node_modules/`、`target/`、`.git/`
     （可复用 `.gitignore` 语义）。仅 `target/` 一项就是 162 GB，
     这能把百万级文件降到几千。
   - 这两条同时修，既消除百秒级长尾，也消除它拖慢同进程其它工具的问题。
3. **控制上下文增长**（把 TTFB 从 4s 拉回 2s）
   - 十几万 token 的上下文每轮预填充约 +2 秒。更积极地在长会话里压缩/裁剪
     历史（`compact` 已有，但触发点可以更早）。
4. **接受不可控部分**
   - 网关底噪 ~1.2~2.0s/次、TLS 握手 ~1.1s（连接池已覆盖）属于外部依赖。

---

## 6. 复现

```bash
# 今天全部分段统计（含最慢工具调用明细）
python3 scripts/latency_profile.py 2026-11-06 top

# 指定日期
python3 scripts/latency_profile.py 2026-11-05
```

脚本读 `~/.yi-agent/trace/session-<date>-*.jsonl`，不需要改代码、不需要重启，
任何时候都能跑。它同时会去重（同一进程重复初始化 tracing 会把同行写两遍）。
