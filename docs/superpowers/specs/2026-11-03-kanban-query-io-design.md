# 看板查询 IPC：非阻塞读写、完整帧与截止时间设计

日期：2026-11-03
基线提交：`0c615bccc7eb0f667fe91b5b30d5ae0f1c8b68ec`
状态：待实现。本轮只交付修复 spec 与文档自检，不修改生产代码，不安装二进制，不操作真实看板状态。

## 1. 问题与证据

### 1.1 已确认根因

`plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs`：

- `serve_with` 将监听 Unix socket 设置为非阻塞（基线约 :149）。
- `serve_connection` 对 accepted socket 直接 `read_until`、`write_all`，没有显式设定连接模式，也没有处理 `WouldBlock`（约 :93–109）。
- macOS 的 accepted Unix socket 继承监听 socket 的 `O_NONBLOCK`。设置 `SO_RCVTIMEO` 不会清除此标志。

形成同源的两类故障：

1. **F1：请求到达竞态。** accept 先于客户端发完请求时，读取返回 `WouldBlock`，整个连接处理提前结束。客户端观察到断管、EOF 或连接重置。
2. **F2：回复背压截断。** 回复未一次写完且发送缓冲区暂时无空间时，`write_all` 返回 `WouldBlock`，连接处理结束；已经发送的前缀留在客户端，缺失余下 JSON 与换行。

### 1.2 本轮实际观测

2026-11-03 现场，插件 PID 1871，二进制 `/opt/homebrew/bin/superpowers-kanban` 的文件时间为 2026-11-01 22:53:46。文件时间不是构建提交号，不以它单独证明版本；同时核对该时点仓库源码，相关连接处理与基线一致。

| 检查 | 结果 |
|---|---|
| 看板文件解析 | 合法 JSON，共 18 张卡、11 张未归档卡；文件仍更新 |
| 经真实项目 daemon 转发 `switch.read` | 一轮 12/12 返回 `PluginResult` |
| 经同一 daemon 转发 `list` | 一轮 12/12 返回 `Error { code: validation }` |
| 直连插件查询 `list` | 取得的非空回复截断于 8192 字节，无结尾换行；其余为断管或空 EOF |
| 连接后延迟 30ms 再发送请求 | 12/12 断管 |
| 独立 macOS Unix socket 实验 | accepted fd 有 `O_NONBLOCK`；空读报 errno 35；20,000 字节首次写仅 8192 字节，下一次写报 errno 35 |
| 按运行版可见卡字段计算回复下界 | 至少 8392 字节，大于现场 8192 字节发送缓冲区 |

最后按 2 秒间隔复核四轮：每轮 `switch.read` 成功，`list` 返回 `validation`。**8192 是这次环境观测，不是协议上限或跨平台常量。**

故障链：插件发送残缺 JSON → daemon JSON 解析失败 → 宿主转为插件查询失败 → 前端摘要显示“无法读取”。daemon 转发代码在 `yi-agent-store/src/ipc/plugin_socket.rs`；宿主映射在 `yi-agent-app-server/src/server/plugin_bridge.rs`。

`desktop/src/hooks/useBoardState.ts` 的 `refreshBoards` 把单次异常直接写为“无法读取”，下一次成功直接覆盖；`desktop/src/App.tsx` 定时每 2 秒刷新。F1 以及受调度影响的 F2 能产生短暂失败后恢复，但没有历史带时间戳请求记录，**不声称过去每次一秒闪现均已逐次归因**。

历史多项目插件转发表互相覆盖已在 `ae4e6dad` 修复，不是本次已复现故障的解释。历史 EINVAL 探活日志也不能混同于 F1/F2。

## 2. 目标与范围

### 2.1 必须成立的不变量

- 已接受连接的模式跨平台显式确定，不依赖 accept 的继承行为。
- 暂时不可读/不可写不是连接失败；部分读、部分写不丢数据、不重复字节。
- 成功回复必须是完整的单行 JSON，含唯一结尾换行，且内容与 dispatch 输出一致。
- 收到完整、合法、未超限请求之前，dispatch 不执行；一条连接最多执行一次。
- 每条连接具有不可续期的单调时钟截止时间；小步 IO、`Interrupted`、虚假就绪均不能延期。
- 请求累积缓冲和序列化回复缓冲有明确上限。
- 单条失败不能退出监听循环；探活不是错误；停止请求能打断等待中的 IO。

### 2.2 保持不变与非目标

- 保留一连接一请求、`plugin.query` / `plugin.result` / `plugin.error` 帧形状、socket 路径与清单契约，不 bump 协议版本。
- 保留同步、串行连接服务，无新增异步框架、线程池或依赖。
- 不修改看板状态、槽位、merge、归档、安装/卸载、路由分区、宿主错误映射。
- 不通过前端保留旧摘要、静默重试或吞错掩盖失败；本次不保证消除网络中断等其他原因的“无法读取”。
- 不抢占任意同步 dispatch，也不保证排队尚未 accept 的连接在 daemon 的 5 秒内得到服务。

## 3. 方案选择

1. **推荐：保留非阻塞 IO，用 poll + 偏移量 + 统一截止时间实现连接状态机。** 同时处理 F1/F2、慢客户端、取消与资源界限，沿用现有 libc，无新增 runtime。
2. accepted socket 改阻塞 + 读写超时：可解决 F1/F2，但逐次 IO 超时不能约束持续小步流量；加入全阶段限时/取消后仍需 readiness 管理，不采用。
3. 客户端重试、增大发送缓冲区或前端旧摘要兜底：只能改变概率或隐藏现象，不能建立完整帧保障，不采用。

私有连接执行器留在 `superpowers-kanban-ipc/src/server.rs`。不抽成跨 workspace 通用框架；为了确定性测试，可设置私有策略及窄的 clock/readiness/IO 接缝，不能改变公开 `Dispatch` 与 `serve_with` 的调用契约。

## 4. 时间、取消与资源策略

| 项目 | 生产要求 |
|---|---|
| 连接预算 | 从成功 accept 时刻起 4 秒，使用 `Instant`，只生成一次 deadline |
| readiness 检查片 | 不超过 200ms，且不超过剩余预算；连接就绪时立即唤醒，不盲睡 |
| 请求 JSON 上限 | `MAX_FRAME_BYTES` = 1,048,576 字节，不含结尾 LF |
| 回复 JSON 上限 | 同上，涵盖整个 envelope 的编码长度，不含结尾 LF |
| 请求缓冲上限 | 最多 `MAX_FRAME_BYTES + 1` 字节，用于区分正好上限后的 LF 与超限 |
| 回复编码缓冲上限 | 最多 `MAX_FRAME_BYTES + 1` 字节，含 LF；超限成功回复不能发送 |
| IO 公平性 | 每次有界 read/write 前、后检查停止与 deadline，即使 fd 持续就绪也不能跳过 |

`keep_going` 沿用现有“可反复调用的状态查询”语义，传入私有连接执行器；停止为 false 时关闭当前连接，不再 dispatch，不再 accept 新连接。等待调用的最长检查片为 200ms；正常调度下 IO 等待取消延迟不超过一片加调度开销。

**deadline 包括 dispatch 消耗，但不能抢占 dispatch。** 在执行前、执行后分别检查；dispatch 或序列化期间如果已经过期/停止，返回后直接关闭，不继续发送。由此只承诺用户态 IO/等待有界，不虚称任意业务调用具有硬 4 秒完成保证，也不承诺内核长期不可中断或线程未被调度时的绝对墙钟上界。

同步串行服务在多个慢连接下仍可排队；本次不引入调度队列与并发能力。验收正常流量不得排入故意阻塞的负例连接后再要求所有请求小于 5 秒。

## 5. 连接状态机

### 5.1 初始化与等待

accept 后立即记下 deadline，显式 `set_nonblocking(true)`。不再用 `set_read_timeout` 作为等待/探活判据，取消其历史 EINVAL 特判分支；探活通过真实 EOF 判断。

readiness helper 接收方向、deadline 与停止查询，每次 poll 等待不大于剩余时间及 200ms。毫秒换算向上取整但不能出现零预算忙循环：每次返回后先检查 deadline；到期立即结束。

- `poll` 被信号中断：重算剩余时间，检查停止，重试。
- `POLLIN` / `POLLOUT`：尝试实际 read/write；就绪不是保证，仍处理 `WouldBlock`。
- 读取时 `POLLHUP`：先尝试读尽已到达的数据，不能丢掉“发完请求后半关闭写端”的有效请求。
- `POLLERR`：检查并报告 socket 错误；`POLLNVAL`：终止当前连接。无有效进展的 HUP/ERR 不能无限 poll。
- read/write `Interrupted`：在下一次尝试前重查 deadline/停止。
- `WouldBlock`：等待对应 readiness，不 dispatch、不把它记为异常日志。

### 5.2 Reading

读取第一条 LF 终止的帧，逐段累积；不得先无界 `read_until`，最后再检查长度。

- 空 EOF（没有字节）：正常探活，关闭，无 dispatch、无错误日志。
- LF 前已超过上限：不再累积、不 drain 客户端无限输入；准备固定的小型 `plugin.error`，只用原剩余预算尝试发送。
- EOF 且已有字节但没有 LF：不完整请求，关闭并记录结构化原因，不 dispatch。此行为明确拒绝旧实现偶然接受的无换行 EOF；正式客户端已经发送 LF。
- 找到 LF：只解析第一帧；本协议不支持 pipelining，读到同次 chunk 中的额外字节也不处理第二条请求，回复后关闭。
- 正好上限 JSON + LF 合法，上限 + 1 的 JSON 拒绝；空行、非法 UTF-8、坏 JSON、错误 type/missing method 保持 `plugin.error`。

### 5.3 Dispatch 与 Encoding

完整合法请求且仍在预算内才执行 dispatch，一条连接只执行一次。`respond_line` 的纯编解码测试能力应保留，但 socket 路径必须先做有界读取。

回复发送前完整编码。编码 writer 在增长之前检查上限，超限立即停止，丢弃未发送的成功回复，生成固定小型 `plugin.error`，例如 `query reply is too large`。不能先生成无界 `Vec` 再验长度；成功与业务错误 envelope 使用同一个限长编码器，不能让巨型业务错误消息绕过上限。序列化失败同样回固定小型错误。

资源上限只约束传输层拥有的帧/编码缓冲；`Dispatch` 返回的 `serde_json::Value`、业务内存与业务耗时不在此硬上限内。不得用“编码缓冲有界”宣称插件总内存有界。

### 5.4 Writing

持有完整且已校验的回复帧与 `offset`。每次 write 只推进实际成功字节数，直到全部字节（包括 LF）发送。

- 部分写：更新 offset 后继续，不重新编码、不重新 dispatch。
- `WouldBlock`：等可写后从原 offset 续写。
- 返回 0：`WriteZero`，结束当前连接。
- 断管/重置/其他失败：结束当前连接。
- deadline/停止：结束当前连接，不发新错误帧，不 reset 预算。

一旦成功回复开始发送，不能把 `plugin.error` 接到残缺成功 JSON 后。deadline/对端离开仍可能产生残缺前缀，这是允许的失败，不得标成成功。**写超时不能证明业务没有执行**；不新增自动重试，尤其不重试 `enqueue` / 开关写入等变更请求。

## 6. 错误与可观测性

现有 stderr 日志形式可保留，但私有执行器应区分：空探活、停止、请求超限、请求不完整、读取超时、写入超时、编码失败、真正的 IO 错误。

- 空探活、停止不记错误；处理过程中的 `WouldBlock` / `Interrupted` 不记错误。
- 一条最终失败最多一条日志，包含阶段及原因，不记录完整请求/回复、卡片内容或凭据。
- 单条失败返回 accept 循环；监听级不可恢复错误才使 `serve_with` 返回 Err。
- 不承诺前端展示上述细分类别，宿主现在的错误映射保持不变。

## 7. 验收矩阵与测试有效性

测试必须验证内容、帧终止、dispatch 次数与资源释放，不能只断言“进程没死”。所有 socket 客户端设置外层限时；失败分支也关闭 fd、设置 stop、join 服务线程，避免测试自身挂住。

| ID | 场景 | 必须证明 |
|---|---|---|
| A01 | accepted socket 模式 | 显式非阻塞；macOS/Linux 均不依赖继承 |
| A02 | 已 accept，客户端暂未发送 | 首次 `WouldBlock` 不关闭；随后合法请求完整成功，dispatch 一次 |
| A03 | 请求分段且 LF 最后到 | 不提前 dispatch；完整帧成功，无字节丢失 |
| A04 | 大回复至少 256 KiB，暂缓读取制造背压 | 实际经历部分写/`WouldBlock`，恢复读取后完整 JSON + LF，内容完全一致 |
| A05 | 注入部分写、Interrupted、虚假就绪 | offset 正确、不重复字节、不重新 dispatch、不忙循环 |
| A06 | 空探活、合法请求后 `shutdown(Write)`、不完整 EOF | 空探活正常；半关闭完整请求仍有回复；不完整请求 dispatch 零次 |
| A07 | 请求上限及上限 + 1、无限无 LF 输入 | 上限合法；超限有界拒绝；不继续累积或 drain，dispatch 零次 |
| A08 | 成功/错误回复编码上限及超限 | envelope 总长度计入；超限不发送成功前缀，返回小型合法错误 |
| A09 | 不发送、持续慢速发送、不读回复 | 统一 deadline 不续期，各自终止；后续正常连接成功 |
| A10 | readiness 持续就绪、反复 EINTR | deadline/停止仍被检查，不绕过公平性 |
| A11 | 读等待/写等待中停止 | 关闭 fd、回到监听退出，正常调度下不超过一个检查片；后续不 dispatch |
| A12 | dispatch 前过期/停止、dispatch 后过期/停止 | 前者零次 dispatch；后者不发回复、不重试业务 |
| A13 | 单条断管/超时/坏帧后再查询 | 监听仍服务；失败最多一条日志，探活无错误日志 |
| A14 | 真 daemon 转发，大回复及小回复对照 | 两者正常请求均成功，不再出现截断导致的 validation；使用隔离实例 |
| A15 | 连续小查询、深路径 fallback | 原正常路径仍成功，poll 不引入每请求固定 200ms 睡眠 |

### 7.1 确定性要求

- A02/A03 使用通道或屏障确认已 accept/首次进入等待后才发送，不能只靠 sleep 恰好抢到竞态。
- A04 在测试 accepted fd 上设小发送缓冲区，客户端收到“发送进入背压”同步信号后再开始 drain。不把 setsockopt 请求值当实际容量；读取实际设置并保证 payload 足够大，断言确实遇到背压。平台不支持缩小缓冲时使用 IO 接缝稳定制造背压，macOS 另有真实 socket 回归，不能静默跳过全部实测。
- A05/A09/A10 的精确截止时间、公平性通过私有 fake clock/IO/readiness 接缝验证；真实 socket 另以短测试预算验证等待能返回，不用紧张墙钟阈值代替语义断言。
- A06 的 HUP 语义必须是真实 socket；完整请求发完后半关闭写端，客户端继续读取。
- A08 的边界按最终 UTF-8 envelope 字节计数，不按字符数或 value 的裸长度。
- A11 测量 IO 等待取消；不把不可抢占 dispatch 放进这个限时判据。
- 不依赖真实 LLM、真实看板数据或全机空闲来生成大回复。

### 7.2 回退必须变红

临时回退连接处理到原 `read_until` / `write_all` 且 `WouldBlock` 直接退出，A02/A04 必须失败。测试在进入原连接处理前显式把 accepted fd 设为非阻塞，使 Linux 也能覆盖 macOS 故障；仍保留真实 macOS accept 路径回归。

移除 deadline 非续期检查应使 A09/A10 失败；移除限长编码应使 A08 失败；移除 stop 检查应使 A11 失败。回退实验不得安装到真实应用，不提交回退代码。

### 7.3 实施时验证入口

```bash
cd plugins/superpowers-kanban
cargo fmt --all -- --check
cargo test -p superpowers-kanban-ipc
cargo test -p superpowers-kanban-runner
cargo clippy -p superpowers-kanban-ipc -p superpowers-kanban-runner --all-targets -- -D warnings

# 宿主回归；独立 workspace，不能假定上面的插件测试已覆盖它
cd ../../yi-agent-rs
cargo test -p yi-agent-store --lib plugin_query_tests
cargo test -p yi-agent-app-server --lib plugin_query_tests
cargo fmt --all
just ci
```

A14 在实施时落为自动化的隔离 daemon 转发测试，位置限定为宿主 `plugin_query_tests` / 插件 IPC integration test 的测试代码；必要时使用其现有测试驱动，不改生产转发协议。测试报告必须注明是否真的覆盖了生产 daemon 转发，不能以直连插件或假转发替代该项。macOS 和 Linux 都跑 IPC 回归，macOS 是上线验收必要条件。

上面的命令是**实施验收入口，不表示本轮已实现或已跑过新增测试**。

## 8. 部署验收与完成判据

实现通过全部 A01–A15 后，部署是另一项需授权的操作；本轮不执行。

- 验收用新构建插件，记录构建提交、二进制摘要、实际进程路径/PID/启动时间；仅覆盖磁盘二进制不会更新已运行进程。
- 使用独立 `YI_AGENT_HOME`、项目/state/runtime 目录隔离，不连接真实 relay 或写真实看板；使用合成卡片或测试 dispatch 产生大回复。
- 普通、非负例流量下，经真实 daemon 至少连续 100 次大回复与 100 次小回复全部成功，完整内容匹配，无残缺 JSON、无 validation、无未处理 errno 35。
- 另跑延迟请求、分段请求、背压、超限、超时与停止场景；负例允许失败，但不得破坏后续服务。
- IPC 单元/集成测试、宿主回归与两平台结果齐备，完成回退必红验证，才可把 bug 与项目进度改为已修复。

## 9. 文档自检记录

本轮自检对照 §2 不变量、§4 策略、§5 状态机及 A01–A15，修订了以下易歧义点：

1. 逐次超时不能续期；deadline 在 accept 时创建，包括 dispatch 消耗，但不能抢占同步业务。
2. poll 需要停止检查、HUP 数据排空、EINTR 和持续就绪公平性，不能只补一次 WouldBlock 重试。
3. LF 与 JSON 分开计长；限制在累积/编码阶段执行，不在无界分配之后执行。
4. 回复开始发送后不能换成错误帧；写超时不能推定业务未执行或自动重试。
5. 探活测试不能仅以“下一条查询成功”冒充“本条没有错误”；需要直接观察执行器结果或日志分类。
6. 回归测试用同步信号和背压接缝触发故障，不用固定 sleep/8192 字节平台常量；保留 macOS 真 socket 与 daemon 转发证据。
7. 传输缓冲有界不等于 dispatch Value/插件总内存有界，串行排队也不等于每条请求都能在宿主 5 秒内完成。

未发现需要拆分的独立功能或未决协议选择。文档自检不是实现正确性证明；修复完成仍以 §7–8 的执行结果为准。
