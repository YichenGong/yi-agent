# daemon 命令响应帧被截断（长交付无法回传）设计

**目标：** 让 daemon 的**命令响应**能完整传输到协议声明的 1 MiB 上限，而不是在内核
发送缓冲（macOS 默认 8192 B）处被静默截断；并把「帧被截断」与「帧超限」拆成两个可
区分的错误，消除误导性的排障信息。

**状态：** 设计已确认，待转实现计划。

---

## 1. 问题

用户在用 subagent 调研并等待结果时，`wait_agent` 返回：

```
daemon is unavailable: IPC frame exceeds 1048576 bytes
```

但实际落库的报告只有 **11,468 字节**（占 1 MiB 上限的 1.09%），重建后的
`WaitCompleted` 帧也只有 **26,273 字节**（2.51%）。**从未接近 1 MiB。**

真实复现（对正在运行的 daemon 直接发请求）：

| 请求 | 响应实收 | 结尾 |
|---|---|---|
| `InspectTask`（小报告，约 3.6 KB） | 3,585 B | 换行 ✓ |
| `InspectTask`（11 KB 报告） | **恰好 8,192 B** | **无换行、立即 EOF** |

三个特征排除了「载荷超长」：字节数恰好是 **8192**（不是 1048576，差 128 倍）；
**0.01 秒内即 EOF**（不是慢传输被超时切断）；与客户端接收缓冲无关（`SO_RCVBUF`
调到 1/4/16 MB，结果恒为 8192）。

影响面：任何**命令响应**超过约 8 KB 即失败，涉及 `WaitCompleted`、`TaskDetail`、
`TaskDiff`、`TaskEvents`、`TaskMailbox`。其中 `WaitCompleted` 正是 subagent 回传
交付报告的通道——**任何超过 8 KB 的子 agent 报告都无法回传**，而这恰是 subagent
机制的核心用途。

## 2. 根因（已核实，含实测复现）

三处代码的组合：

```rust
// ① ipc.rs:710 —— listener 非阻塞
listener.set_nonblocking(true)?;
//    Rust 语义：accept() 出的连接「继承」O_NONBLOCK（BSD/macOS 行为）

// ② ipc.rs:1016-1017 —— 只设超时，未清 O_NONBLOCK
stream.set_read_timeout(Some(Duration::from_secs(1)))?;
stream.set_write_timeout(Some(Duration::from_secs(1)))?;
//    Rust 的 set_*_timeout 只设 SO_RCVTIMEO/SO_SNDTIMEO，不碰 O_NONBLOCK

// ③ ipc.rs:1753 —— 命令响应用裸 write_all，遇 WouldBlock 不重试
stream.write_all(&frame)?;   // 一线失败即返回 Err
```

在**非阻塞**连接上，`write` 只把能立刻进内核缓冲的部分写进去，剩余部分立即返回
`EWOULDBLOCK`。macOS 上 AF_UNIX 的默认 `SO_SNDBUF` 是 **8192 B**（实测），于是
「发送缓冲大小」偷偷变成了「协议能传多大的事实上限」。

`write_all` 收到 `WouldBlock` 即返回 `Err` → `handle_client` 返回 `Err` → 连接被
drop → 客户端读到 8192 B 后撞 EOF。

**用 Rust 逐字节复现：**

```
[1] accept 后            O_NONBLOCK = true     <- 继承 listener 的非阻塞
[2] 设完两个 timeout 后   O_NONBLOCK = true     <- 未被清理
[3] write_all(26000B) -> Err(WouldBlock)       <- 一碰缓冲满即放弃
[4] 客户端收到 8192 字节, 以换行结尾 = false
```

阈值精确：发送 8192 B 完整，**8193 B 即截断为 8192 B**。

### 2.1 为什么报的是「超过 1048576 字节」

客户端 `read_limited_frame`（`ipc.rs:2300`）把两种截然不同的情况合并成一个错误：

```rust
if frame.len() > MAX_FRAME_BYTES || !frame.ends_with(b"\n") {
    return Err(IpcError::FrameTooLarge);
}
```

被截断的帧（8192 B、无换行）走的是**第二个**条件，但错误文案只提「超过
1048576 字节」。真相是「服务端写了一半就断了」，报出来却像「你的数据太大」。

### 2.2 为什么长期未被发现

1. **该缺陷是平台相关的**：`accept()` 继承 `O_NONBLOCK` 是 BSD/macOS 语义，Linux
   不继承。CI 跑在 `ubuntu-latest`（`.github/workflows/ci.yml:10`），**CI 永远绿，
   macOS 用户永远坏**。
2. **单元测试绕过了 `accept`**：`normal_oversized_response_uses_a_typed_error_not_subscription_resync`
   （`ipc.rs:1999`）等 7 处用 `UnixStream::pair()`（main 原有 5 处，本设计新增 2 处）——`pair()` 是阻塞的，不继承
   `O_NONBLOCK`，所以恒过。

### 2.3 关键观察：仓库里已有正确范式，只是命令路径没用它

同一文件里三条路径的处理完全不对称：

| 路径 | 位置 | 处理 WouldBlock |
|---|---|---|
| 服务端**读**请求 | `read_incoming_frame` `ipc.rs:2230`（与其 WouldBlock 分支同处） | **有**（sleep 1ms 重试至 1s deadline） |
| 服务端**写**订阅帧 | `write_subscription_frames` `ipc.rs:1646` | **有**（分片续写 + 重试） |
| 服务端**写**命令响应 | `write_envelope_frame` `ipc.rs:1735` | **没有**（裸 `write_all`） |

而 `PendingFrameWrite`（`write_frame_chunk`，`ipc.rs:1592`）已实现「记录 `written`
进度、支持断点续写」——正是命令路径缺的那块拼图。

## 3. 设计

三条并行修复线，对应三个独立缺陷。**不改协议常量、不调大 `SO_SNDBUF`**——8192 不
该是约束，把缓冲调大只是把问题推后而非消除。

### 3.1 修复 A：写路径根治（根因）

**A1（主）：命令响应复用已有的分片写范式。**

抽出不依赖订阅队列的通用写函数，与订阅路径共享同一语义：

```rust
/// 把一个完整帧写到命令连接上，允许部分写与 WouldBlock，带总超时。
fn write_frame_until(
    stream: &mut UnixStream,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), IpcError> {
    stream.set_nonblocking(true)?;
    let mut written = 0usize;
    while written < bytes.len() {
        match stream.write(&bytes[written..]) {
            Ok(0) => return Err(IpcError::Io(io::ErrorKind::WriteZero.into())),
            Ok(n) => written += n,
            Err(e) if matches!(e.kind(), WouldBlock | TimedOut) => {
                if Instant::now() >= deadline {
                    return Err(IpcError::FrameWriteTimeout {
                        written,
                        total: bytes.len(),
                    });
                }
                thread::sleep(Duration::from_millis(1));   // 与 ipc.rs:1698 一致
            }
            Err(e) if e.kind() == Interrupted => {}
            Err(e) => return Err(IpcError::Io(e)),
        }
    }
    Ok(())
}
```

`write_envelope_frame`（`ipc.rs:1735`）把裸 `write_all` 换成它。

**A2（配套）：放宽写超时。** 当前 `set_write_timeout(1s)`（`ipc.rs:1017`）对 1 MiB
载荷偏紧——慢客户端下即使分片写也可能在 1s 内写不完。写超时放宽到 10–30 s；
**读超时 1 s 保持不变**（它保护的是「半开连接」，与本次问题无关）。

**A3（备选，更简但语义弱）：** 在 `handle_client` 开头 `stream.set_nonblocking(false)?`，
让 `write_all` 回到阻塞语义。实测可行（阻塞语义下 1 MiB 完整传输），但依赖「写超时
足够宽松」这一隐含前提。**A1 更鲁棒**，因为它显式处理背压而非把问题交给超时。

### 3.2 修复 B：拆分错误语义

`IpcError`（`ipc.rs:123`）新增变体，并把 `read_limited_frame` 的两个条件拆开：

```rust
if frame.len() > MAX_FRAME_BYTES {
    return Err(IpcError::FrameTooLarge);
}
if !frame.ends_with(b"\n") {
    return Err(IpcError::TruncatedFrame { received: frame.len() });
}
```

文案必须指向真正的原因，例如：

```
"daemon closed the connection mid-frame after {received} bytes (truncated response)"
```

这一点是用户直接要求的：**旧文案曾把人误导成「载荷过大」**，浪费排障时间。错误的
错误信息本身就是缺陷。

### 3.3 修复 C：补测试，堵住 macOS-only 盲区

**C1：走真实 `Daemon` + 真实 `accept` 的集成测试**，放在 `tests/runtime_ipc.rs`
（该文件已有 `Daemon::start_with_factory`、`raw_request` 等基础设施）：

```rust
#[test]
fn a_response_payload_larger_than_the_socket_send_buffer_arrives_intact() {
    // 起真实 daemon（走真实 accept，而非 UnixStream::pair）
    // 构造 > 8192 B 的响应（写入长 delivery_json 后 InspectTask）
    // 断言：读回完整一行、以 '\n' 结尾、反序列化后字段完整
}
```

要点：载荷**必须超过 8192 B**（触发条件），且必须断言「以换行结尾」（「完整帧」判据）。

**C2（已有 red 测试）：** 本 worktree 已写入两条失败测试，覆盖 B 的两个分支
（`a_truncated_response_is_reported_as_truncated_not_as_oversized`、
`a_frame_over_the_cap_is_reported_as_too_large_not_truncated`），保持它们在 A/B 实现后转绿。

## 4. 非目标

- **不做**大载荷的落盘/按块拉取重构。当前实测最大报告仅 11 KB，距 1 MiB 上限差两个
  数量级，重构传输架构属 YAGNI。若未来报告规模接近 1 MiB，再单独立项。
- **不调** `SO_SNDBUF`。它依赖平台、受 `net.local.*` 限制，且慢客户端下同样会
  `EWOULDBLOCK`——是把问题推后而非解决。
- **不改**协议常量 `MAX_FRAME_BYTES = 1 MiB`（`ipc.rs:35`）。协议声明没问题，
  是实现没兑现声明。

## 5. 验证

| 层级 | 验证 |
|---|---|
| 单元 | 已有两条 red 测试转绿：截断→`TruncatedFrame`，超限→`FrameTooLarge` |
| 集成（关键） | 新增 `a_response_payload_larger_than_the_socket_send_buffer_arrives_intact`，**修复前必定失败、修复后通过** |
| 手工复现 | 对运行中 daemon 发 `InspectTask`，11 KB 报告的响应应完整（>8192 B 且以换行结尾） |
| 回归 | `cargo test -p yi-agent-store`、`just fmt-check` |

已实测的修复方向可行性（真实 `accept` 连接）：

| 场景 | 实收 | 完整帧 |
|---|---|---|
| 命令路径（现状）8193 B | 8192 | **false** |
| 订阅范式（分片+重试）**200000 B** | 200001 | true |
| 阻塞语义 1000000 B + 慢客户端 | 1000001 | true |

## 6. 附带

- 更新 `docs/bug-list.md`：当前仅有 app-server 一条同类记录（`bug-list.md:21`），
  **store/daemon 命令响应截断未被记录**。
- 评估 CI 平台覆盖：`release.yml:16` 已用 `self-hosted`（Mac mini）runner，可考虑
  让 IPC 集成测试在 macOS 上也跑一次，否则此类平台相关缺陷会继续漏网。

## 7. 影响面

改动集中在 `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（写路径 + 错误枚举 + 读帧
判定）与 `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`（集成测试）。
受影响命令的行为**不变**（仍是完整帧），变化的是它们**不再被静默截断**。
`FrameTooLarge` 的语义收窄为「真的超过 1 MiB」，故依赖它的既有断言需同步更新
（`ipc.rs:2000` 一处）——这是预期的契约变更。
