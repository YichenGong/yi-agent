# app-server 的 diff 阻塞与内存泄漏修复设计

**日期**：2026-10-29
**模块**：`yi-agent-rs/crates/yi-agent-app-server/`
**状态**：待评审

## 1. 背景

本设计来源于 2026-10-29 对 `yi-agent-app-server` 的一次随机模块审计。审计基线健康：
`cargo test -p yi-agent-app-server --lib` → `1054 passed; 0 failed; 4 ignored`。

因此下面四个问题都不是回归，而是「全部测试通过的前提下仍然存在的缺口」。

审计用隔离探针实测了其中三条（探针已删除，未留在仓库），另有一条由读码定位。

## 2. 问题陈述

### P1（严重，已实测）：`thread/diff/read` 把整个 server 卡住

`rpc/thread.rs:1913-1928` 在 async 的 RPC 处理函数里**同步**调用
`crate::git_diff::thread_diff()`；同一条路径上 `thread/view/read` 的
`refresh: true` 分支（`rpc/thread.rs:1991`）也同步调用 `structured_frame()`。

`serve()` 直接 `await` 这些处理函数，没有 `spawn_blocking`。而往下每一层都是同步的：

- `git_ok()`（`git_diff.rs:160`）用 `std::process::Command::output()`，**无超时**；
- `files()`（`git_diff.rs:300`）对**每个未跟踪文件**全量 `std::fs::read` 只为数换行（`:311`）；
- 紧接着 `is_binary()`（`:372`）**再全量读一遍**；
- `untracked_diff()`（`:383`）**第三遍**全量读（`:386`）。

截断发生在最后（`thread_diff` 末尾 `truncate`，`git_diff.rs:415`）：`MAX_DIFF_BYTES = 256 KiB`
只约束**输出**，完全不约束**输入**。

`thread_diff` 一次调用至少串行跑 5 条 git 子进程（`rev-parse` 候选链、`merge-base`、`log`、
`diff --numstat`、`diff`），全部无超时。

**实测证据**（60 个 8 MiB 未跟踪文件，模拟忘记写 `.gitignore` 的 `dist/`）：

```
PROBE baseline=553.5µs list_behind_diff_read=609.144208ms ratio=1101x
```

一次 `thread/diff/read` 让紧随其后的 `thread/list` 延迟 1101 倍。期间所有客户端的请求、
所有 thread 的事件转发全部排队。

**实测证据**（单文件，量化「输出截断但输入无界」）：

```
PROBE untracked file=512 MiB elapsed=4.23s output=262164 bytes truncated=true
```

512 MiB 文件读满 4.23 秒、占用等量内存，最终只输出 262 KB。

### P2（中等，已实测）：`view_frames` 随 `thread/delete` 永久泄漏

`ViewFrames = Arc<Mutex<HashMap<String, ViewFrame>>>`（`server.rs:390`），键是 `thread_id`。

- 全仓库唯一写入点：`server.rs:3090`（`pump_git_diff_notifications`）；
- 全仓库**没有任何** `remove` / `retain` / `clear`；
- `thread/delete`（`rpc/thread.rs:1571` 起）清理了 `threads`、`pending_activation`、
  `thread_roots`、`trace_watches`、`children_watches`、`process_watches`，
  **唯独漏了 `view_frames`**。

**实测证据**：

```
PROBE view_frames before=1 after=1 still_present=true
```

功能上不错误（`thread/view/read` 前有 `require_known_thread` 挡住），但每条残留帧持有
`stdout` + `stderr`，单条上限 256 KiB。长期运行的桌面端每删一个看过 diff 的会话就永久留一份。

### P3（中等，已实测）：`image/read` 每翻一页都重读整个文件

`rpc/image.rs:141` 无条件 `std::fs::read(&canonical)` 把**整个文件**读进内存，然后才在
`:194-195` 按 `offset` / `maxBytes` 切片。上限 `IMAGE_READ_MAX_BYTES = 20 MiB`，
单页 `IMAGE_READ_CHUNK_BYTES = 512 KiB`。

客户端逐页拉取到 `nextOffset == null`（`desktop/src/lib/useImageData.ts:137`），
读一张 20 MiB 的图就是 **40 次全量 20 MiB 读取**，I/O 放大约 40 倍，且全在 async 主循环上。

**实测证据**：

```
PROBE last-page-of-20MiB elapsed=2.694083ms
PROBE 1KiB elapsed=225.083µs
PROBE ratio = 12.0x            # 只取 1 字节，开销却与 20 MiB 成正比
PROBE baseline_list=725.792µs list_behind_8x20MiB=11.373667ms ratio=15.7x
```

### P4（中等，读码定位）：`git_ok` 无超时，与 P1 叠加

`git_ok()`（`git_diff.rs:160`）同步 `std::process::Command::output()`，无超时，
被 `thread_diff` 在同步路径上连续调用 5 次以上。

`VIEW_TIMEOUT`（10 s，`git_diff.rs:14`）只保护 `command_frame` 那条走
`tokio::process` + `kill_on_drop` 的路径；`structured_frame` / `thread_diff` 这条
同步路径完全不经过它。

一个 index 损坏、残留 `index.lock`、或位于网络挂载卷上的仓库，会让 `thread/diff/read`
无限期占住主循环，且没有任何上限能打断它。

## 3. 范围与不做的部分

**做**：P1 / P2 / P3 / P4 四条。

**不做**（本次审计中发现但不在本设计范围内，需要时可另开）：

- `is_liveness_error` 覆盖不到的协议级失败（属于 `yi-agent-subagent`，已有独立 spec）；
- `thread/clear` / `thread/compact` 是否也应清 `view_frames`——见 §4.4 的语义裁决；
- `untracked_diff` 的 `git add -N` 替代方案（会碰用户索引，明确拒绝）。

## 4. 设计

### 4.1 P1：把 diff 计算挪出 async 主循环

**做法**：完全套用 `thread/listAll`（`rpc/thread.rs:106-160`）已验证的两层模式：

```
快照纯值参数 → tokio::spawn(async move {        // 脱离本臂，主循环立刻回到 select!
    let body = spawn_blocking(move || 计算).await;// 真正的阻塞计算在 blocking 池
    write_response(&hub, &client, ok_response(id, body)).await
});
return Ok(Flow::Continue);                      // 本臂不等结果
```

**关键点 1：必须是 `tokio::spawn` 包住 `spawn_blocking`，不能在本臂里就地 `await`。**

就地 `await` 一个 `spawn_blocking` 只把**工作**挪到 blocking 池，本臂这个 future 仍然
挂在这个主循环 task 上等待——`serve()` 的 `select!` 不会转起来，其它请求照旧排队。
「卡住」的原因从来不只是「计算在错误的线程上」，而是**这个 task 不再回到 `select!`**。
`thread/listAll` 之所以真解决了问题，靠的是 `tokio::spawn` 那一层。

**关键点 2：写响应要在 spawned task 里做，故必须把 `hub` 与 `client` 的拥有些拷走。**

`AppState` 里 `hub: &'a Arc<Broadcaster>`、`client: &'a ClientId`，`ClientId` 是
`#[derive(Clone)]`（`broadcast.rs:47`）。故：

```rust
let hub = Arc::clone(state.hub);
let client = state.client.clone();
```

这与 `thread/listAll` 的 `Arc::clone(state.hub)` / `state.client.clone()` 逐字同源。

**失败处理**：`spawn_blocking` 的 `JoinError` 只会在 panic 时出现。按 `thread/listAll`
的先例记日志并回一个「空结果」形状的响应（`thread/diff/read` 回
`json!({"unifiedDiff": "", "truncated": false})`），不静默丢响应——否则客户端会一直等。

**命令帧也要移出**：`command_frame`（`git_diff.rs:129`）虽然签名是 `async`，但它
开头就同步构造 `SandboxPolicy::new(...)`，而后者会调 `canonicalize_root()`
（`yi-agent-tools/src/sandbox.rs:135-137`，同步 `Path::canonicalize`）。
在 NFS / 已卸载挂载点上 `canonicalize` 会卡住，且这一步不经过任何 `await`。
故整个「读快照 → 取命令/结构化帧 → CAS 写回」全部搬进 `spawn_blocking`，
即在该闭包内自行 `tokio::runtime::Handle::current().block_on(...)` 不现实——
正确做法是：**只把同步部分（`structured_frame`、`valid_rev`、`canonicalize`）放进
`spawn_blocking`，命令帧仍用现有的 `command_frame` async 路径**，但把它放在
`tokio::spawn` 出来的独立 task 里（不是本臂内联 `await`），从而不占用主循环。

具体分派：

| 帧类型 | 计算方式 | 为什么 |
|---|---|---|
| 命令帧（`command.is_some()`） | `tokio::spawn` 的 task 内 `await command_frame` | 它是 async（`tokio::process` + `timeout`），但要脱离主循环 |
| 结构化帧 | task 内 `spawn_blocking(structured_frame)` | 全同步，必须进 blocking 池 |

两者都不在 `serve()` 的 `select!` 上等待。

**为什么两个臂都要改**：`thread/view/read` 的 `refresh: true` 分支同样在同步跑 git
（`structured_frame` → `thread_diff`），同一类卡顿。`refresh: false` 只读内存表，
保持内联。

`thread/view/read` 的 refresh 分支同理：把 `command_frame`（本来就是 async，但内部
`sandbox` 构造与 `structured_frame` 的同步 git 调用都要挪走）改为：

- 命令帧：`command_frame` 已在 `tokio::process` 上，保持；但它前面的
  「读快照」部分不变；
- 结构化帧：`structured_frame` → `spawn_blocking`。

**为什么不是「给 git_ok 加超时」就够了**：超时只解决「永久卡住」，不解决
「慢但没超时」——609 ms 的实测就是完全正常返回的慢，超时救不了它。两件事都要做（P4 见 §4.3）。

### 4.2 P1 的第二半：给未跟踪文件加**读前**大小闸

`files()` / `is_binary()` / `untracked_diff()` 三处都对同一文件全量读取。改法：

**先 `metadata` 拿 `len`，超过阈值就不读内容**：

```rust
/// 未跟踪文件参与 diff 合成的单文件上限。
///
/// 超过它的文件在清单里标 `binary: true`、正文里给一行说明，**不读内容**。
/// 为什么是 1 MiB：`MAX_DIFF_BYTES` 是 256 KiB，一个超过 1 MiB 的未跟踪文件
/// 无论如何都不可能完整出现在最终 diff 里（前面还有 `truncate`）；真正需要
/// 完整读它的场景不存在，继续读只是白白占用主循环时间与内存。
pub const MAX_UNTRACKED_FILE_BYTES: u64 = 1024 * 1024;
```

三处统一走一个 helper，保证「读一次、判一次」：

```rust
/// 读未跟踪文件，超过 `MAX_UNTRACKED_FILE_BYTES` 或读失败返回 `None`。
///
/// `None` 的语义 = 「不参与内容合成」，调用方据此标 binary / 跳过正文。
fn read_untracked_capped(full: &Path) -> Option<Vec<u8>> {
    let len = std::fs::metadata(full).ok()?.len();
    if len > MAX_UNTRACKED_FILE_BYTES {
        return None;
    }
    std::fs::read(full).ok()
}
```

同时把 `files()` 与 `untracked_diff()` 的重复读取**合并**：`files()` 里数换行与
`is_binary()` 用同一份 `bytes`，不再读两遍。

`untracked_diff` 对超大文件输出：

```
diff --git a/<path> b/<path>
new file mode 100644
Binary files /dev/null and b/<path> differ (too large to render)
```

前端对 `binary: true` 已有占位渲染（`desktop/src/components/GitDiffView.tsx:187`、
`:388`），无需改前端。

**注意语义变化**：超大文件此前是「截断后的前 256 KiB 文本」，改后是「binary 占位」。
这是**有意的**——对 512 MiB 文件显示前 256 KiB 文本，读者以为那就是全部，比明确的
binary 占位更误导。`.gitattributes` 里 `-diff` 的文件在 git 自己眼里也是 binary。

### 4.3 P4：给 `git_ok` 加有界等待

`git_ok` 保持同步（它被 `resolve_base`、`files`、`file_diff`、`commit_diff`、`valid_rev`
等十余处调用，全改 async 会把整条 diff 链路 async 化，收益不抵风险），
改为**有界**执行：

```rust
/// `git_ok` 的单次时间上限。
///
/// 与 `VIEW_TIMEOUT` 分开：那个约束「模型给的展示命令」（可能合理较慢，
/// 且走沙箱），这个约束「我们自己发的只读内省命令」（`rev-parse` / `diff`
/// / `log`，正常都是毫秒级）。给 5s 是因为大仓库首次 `git diff` 可能真慢，
/// 但「index.lock 残留」这种情况应当失败而不是无限等待。
pub const GIT_OK_TIMEOUT: Duration = Duration::from_secs(5);
```

实现要点（**不是**朴素轮询——朴素写法会死锁）：

```rust
fn git_ok(directory: &Path, args: &[&str]) -> Option<String> {
    // stdout 走**临时文件**而不是管道。原因：`stdout(Stdio::piped())` 时
    // 子进程写满管道缓冲区（macOS 64 KiB）就会阻塞在 write 上，而我们在
    // 轮询 `try_wait`、没有任何人在读管道 —— 这是教科书式的死锁：
    // 子进程等我们读，我们等子进程退出，直到 GIT_OK_TIMEOUT 才靠 kill 解开。
    // `git diff` 输出远超 64 KiB 是常态，所以管道方案对本项目直接不可用。
    // 临时文件同时给了「输出多大都不阻塞子进程」这个性质。
    let out_file = tempfile::NamedTempFile::new().ok()?;
    let mut child = std::process::Command::new("git")
        .args(args)
        .current_dir(directory)
        .stdout(Stdio::from(out_file.reopen().ok()?))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + GIT_OK_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                return std::fs::read_to_string(out_file.path()).ok();
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();   // 必须回收，否则留僵尸
                    return None;
                }
                std::thread::sleep(GIT_POLL_INTERVAL);
            }
            Err(_) => return None,
        }
    }
}

/// `git_ok` 的轮询间隔。git 正常返回是毫秒级，10 ms 最多多等一轮。
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
```

`tempfile` 已是 dev-dependency（`Cargo.toml:59`），需提为正式依赖。它是轻量 crate，
且已被 `yi-agent-store` 等使用，不引入新的传递依赖风险。

**为什么不用 `wait_with_output`**：那是阻塞等待，没有超时。
**为什么不干脆全改 `tokio::process` + `timeout`**：`git_ok` 有十余处同步调用点
（`resolve_base`、`files`、`file_diff`、`commit_diff`、`valid_rev`、`merge_base`、
`repo_root` 等），全 async 化会把整条 diff 链路连同 `file_diff` 的公开签名一起改掉，
收益不抵风险。P1 已经把这些调用整体挪出了主循环，同步本身不再是问题；
P4 只需补上界。

**`git diff` 的输出可能很大**（当前无上限，`truncate` 在最后才切）。
保持现状不另加读上限：P1 的 `spawn_blocking` 已把它挪出主循环，
P4 的超时给了它上界；再加一个读上限属于独立优化，会改变 `truncate` 的语义边界。

### 4.4 P2：`thread/delete` 清理 `view_frames`

在 `rpc/thread.rs` 的 `thread/delete` 清理序列里，紧跟 `process_watches` 之后加：

```rust
// 视图帧与 thread 同生命周期:帧表按 thread_id 索引,不清理就永久驻留。
// `thread/view/read` 有 `require_known_thread` 前置检查,故此处不影响正确性,
// 只是不让已删会话的帧继续占内存。
state.view_frames.lock().await.remove(&thread_id);
```

**`thread/clear` 不清理 —— 语义裁决**：`thread/clear` 清的是对话历史
（`rpc/thread.rs:1776` 起），会话本身还在，`thread/view/read` 仍可读回它的帧。
清帧会让用户在 clear 后打开 diff 面板看到空白，而 clear 的承诺只是「清空对话」。
故只清 delete。

**`thread/compact` 不清理**：同理，compact 只压缩历史。

### 4.5 P3：`image/read` 改区间读取

不再全量读，而是「打开 → seek 到 offset → 读 maxBytes」：

```rust
use std::io::{Read, Seek, SeekFrom};

fn read_range(path: &Path, offset: u64, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; max_bytes as usize];
    let mut filled = 0usize;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}
```

但 `image/read` 还需要 `size` 与 `guess_format` 的魔数校验，这两件事**不能**靠区间读完成：

- `size`：`metadata.len()` 已有，无需读过内容（`rpc/image.rs:100` 已经在取）；
- 格式校验：只需文件**头部**若干字节。`guess_format` 的实现是
  `MAGIC_BYTES` 逐条 `starts_with`（`image-0.25.10/src/io/free_functions.rs:145-158`），
  本 crate 启用四个 feature 后最长签名是 WebP 的 `RIFF\0\0\0\0WEBP`（12 字节）。
  取 `HEADER_BYTES = 16` 覆盖全部四种格式并有 4 字节余量。

于是新流程：

1. `metadata` 挡非普通文件与 `> IMAGE_READ_MAX_BYTES`（现有逻辑不变）；
2. 读前 `HEADER_BYTES = 16` 字节 → `guess_format`（现有报错文案不变）；
3. `read_range(canonical, offset, min(maxBytes, IMAGE_READ_CHUNK_BYTES))`；
4. `size` 用 `metadata.len()`；
5. 原有的「实际字节数复核」保留（`bytes.len()` 现在是本块长度，改为复核
   `offset + 本块长度 <= declared_len` 这一致性）。

**为什么 `HEADER_BYTES = 16` 足够**：`image` crate 的 `guess_format` 只做魔数
`starts_with` 比较（已核对 `image-0.25.10/src/io/free_functions.rs:104-131`）。
本 crate 启用 png/jpeg/gif/webp 四个 feature，对应签名分别是 8 / 3 / 6 / 12 字节，
16 覆盖最长者并留余量。

**协议不变**：`data` / `nextOffset` / `mediaType` / `size` 四个字段的语义与取值完全一致，
客户端 `useImageData.ts` 无需改动。

## 5. 测试策略（TDD，先红后绿）

每条修复都必须先有一个**能失败的**测试。下面列出每个测试要断言的行为；实现前的红必须是
「行为不符」，不是编译错误或环境缺失。

| 编号 | 测试 | 断言 | 实现前 |
|---|---|---|---|
| T1 | `thread_delete_drops_the_view_frame` | `thread/delete` 后 `view_frames` 不含该 tid | 红（残留） |
| T2 | `untracked_file_over_the_cap_is_not_read_into_the_diff` | 2 MiB 未跟踪文件：`binary=true`、正文含 too large 说明、`unified_diff` 远小于 `MAX_DIFF_BYTES` | 红（读满并输出文本） |
| T3 | `diff_read_does_not_block_the_main_loop` | 480 MiB 未跟踪产物下，`thread/diff/read` 之后的 `thread/list` 在 150 ms 内应答 | 红（约 600 ms） |
| T4 | `git_ok_gives_up_on_a_hung_git` | 用 PATH 注入一个永久 sleep 的假 `git`，`git_ok` 在 `GIT_OK_TIMEOUT + 余量` 内返回 `None` | 红（永不返回/远超） |
| T5 | `image_read_only_touches_the_requested_range` | 20 MiB 文件取最后一页 1 字节，耗时 < 同一文件首页 1 字节的 3 倍 | 红（12 倍） |

T3 与 T5 是**时序断言**，需要给足余量避免 CI 抖动：T3 用「150 ms 上限 vs 实测 600 ms」，
T5 用「比值上限 3 vs 实测 12」。两者都是在同一进程内、同一磁盘上的相对比较，不是绝对耗时。

## 6. 验收标准

1. `cargo test -p yi-agent-app-server --lib` 全绿，且用例数从 1054 增至 ≥1059（5 个新用例）。
2. `cargo fmt --all` 无 diff。
3. `cargo clippy -p yi-agent-app-server --all-targets` 不新增 warning。
4. T1-T5 五个用例在**修复前**确认为红、修复后为绿（逐个记录）。
5. 桌面端 `cd desktop && npm test` 不受影响（`gitDiff.ts` / `useImageData.ts` 的既有用例全绿）。

## 7. 风险与缓解

| 风险 | 缓解 |
|---|---|
| `git_ok` 改轮询后，大仓库首次 diff 变慢 | 10 ms 轮询只在子进程未结束时发生；子进程正常返回时 `try_wait` 首轮即命中 |
| 超大未跟踪文件从「文本截断」变「binary 占位」是外部可见的行为变化 | §4.2 已论证：对 512 MiB 文件显示前 256 KiB 更误导；且 `binary` 有前端占位渲染 |
| `spawn_blocking` 的线程池被 diff 请求打满 | 默认 blocking 池 512 线程；diff 请求是用户交互触发，不在高频路径。若出现打满，表现为排队而非卡主循环——比现在更好 |
| T3/T5 时序断言在慢 CI 上抖动 | 用相对比较（同一测试内的前后比值），并留 2-4 倍余量 |

## 8. 附：本次审计中已核对无问题的部分

列出这些是为了让评审者知道**没有**遗漏，而不是为了让代码「看起来更糟」：

- `process_watches` / `trace_watches` / `children_watches` / `pending_lead_replies` 都有清理；
- `ws.rs` 断连时 `scopes` / `initialized` / `devices` 三表都摘除（`ws.rs:539-545`）；
- `broadcast` 的慢消费者摘除与 reliable 例外逻辑完整（`broadcast.rs:181-241`）；
- 传输层 `MAX_FRAME_BYTES = 1 MiB`（`protocol.rs:9`）双向生效，`MessageReader` 用
  `take(limit)` 防超大行，ws 入站也复核；
- `DriverState.results` 上送成功后 `clear()`，失败保留重发；
- `interrupt_and_wait_for_persist` 有 5 s 超时兜底（`server.rs:4641`）；
- 生产代码 `unwrap` / `expect` 只有 4 处，3 处是 `contains_key` 后紧邻取值，
  1 处是 mint 设备落盘（启动期该暴露）；
- `rpc/plugin.rs` 的阻塞等待、`thread/listAll` 的目录扫描已正确放进 `spawn_blocking`。
