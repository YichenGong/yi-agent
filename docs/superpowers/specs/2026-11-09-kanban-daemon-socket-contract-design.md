# Superpowers 看板 daemon socket 契约同步修复设计

日期：2026-11-09

基线提交：`dcfad0d6`；初稿提交：`6d2802c0`；修订：rev3。

状态：设计审查已收敛，待实现。用户已要求审查后编写对应 plan；本轮只修改文档，不安装二进制、不重启真实进程、不操作卡片状态。

## 1. 问题与证据

### 1.1 已确认故障链

宿主提交 `cac5afe0`（2026-11-08 02:02）把业务 daemon socket 迁移到：

```text
<instance_home>/sockets/<canonical-runtime-path-sha256-first-16-hex>/runtime.sock
```

宿主实现：`yi-agent-rs/crates/yi-agent-lifecycle/src/scope.rs::socket_directory` / `daemon_socket`，`yi-agent-store/src/ipc/client.rs::socket_path_for` 调用该实现。

插件 `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/socket.rs::daemon_socket_for` 仍使用旧布局：短路径 `<runtime_dir>/runtime.sock`，深路径 `$TMPDIR/yi-agent-<raw-runtime-path-hash>.sock`。

`runner/src/main.rs::run_daemon` 用旧规则构造 `BoardDaemon`；`runner/src/client.rs::is_alive` 将所有 Status 请求错误折叠为 false。连续三次失败后进程退出，被监督器拉起，再次重复。

**宿主地址迁移 → 插件连接旧地址 → Connection refused → 误报 daemon 已消失 → 退出/拉起循环 → 看板后台不稳定。**

### 1.2 初次诊断的只读现场证据

以下是 2026-11-09 初次诊断快照，不是审查时重新部署后的验收，不据此推断其他项目或所有历史故障同源。

| 检查 | 结果 |
|---|---|
| 正式 sidecar 版本 | `yi-agent 0.1.13-dev+dcfad0d6` |
| 正式 daemon | `/Applications/yi-agent.app/Contents/MacOS/yi-agent daemon serve`，PID 43602；检查时存活 |
| lsof 监听地址 | `/Users/gongyichen/.yi-agent/sockets/51bccf606c7e2ae7/runtime.sock` |
| 插件旧规则地址 | `$TMPDIR/yi-agent-51bccf606c7e2ae7.sock` |
| 旧地址只读连接 | `ConnectionRefusedError [Errno 61] Connection refused` |
| 新地址只读 Status | 协议 3、request_id 匹配、结果 Status，`high_water_event_id: 48882` |
| 日志实例 | 00:58:37 启动 PID 46547 / 00:58:57 退出；00:59:07 启动 PID 49851 / 00:59:27 退出 |
| 日志报错 | `daemon is gone; exiting so a fresh instance can take over` |
| 运行/磁盘 inode | 插件 txt 与 `/opt/homebrew/bin/superpowers-kanban` 均为 181093952 |
| skill 一致性 | 已安装副本与仓库 cmp 一致 |
| 数据与开关 | list --all 可读 4 张 Done、1 张 Failed，均归档；项目开关 true |

日志：`<项目>/.yi-agent/logs/superpowers-kanban.log`。旧 inode、skill 漂移、开关关闭不能解释此次复现的地址故障。

### 1.3 独立故障与边界

当前普通会话 PATH 不含 `/opt/homebrew/bin`；历史卡片报开发 worktree 未授权；accepted socket 非阻塞完整帧问题见[查询 IPC spec](2026-11-03-kanban-query-io-design.md)。三者均不纳入本次修复。

本次保证地址契约与探活行为，不保证所有桌面“无法读取”或真实任务启动均恢复。独立故障不能被吞掉，也不能全部归因于本次地址问题。

## 2. 目标与非目标

### 2.1 不变量

1. 相同 runtime 和实例环境，插件与当前宿主推导出相同业务 daemon 地址，短/深项目统一用新布局。
2. 实例目录来源、后缀、canonicalization、原始路径字节、哈希及长度限制受公共契约与跨进程真实函数对照约束。
3. 业务 daemon 地址不依赖 TMPDIR；插件查询 socket 保持旧契约，不能混用。
4. 解析失败显示原因并尽早退出；探活失败显示目标地址、类别、失败次数，不断言宿主进程已死。
5. 活宿主不因旧路径误判而反复重启；真正连续失联保留三次失败退出策略。
6. 单次 Status 探活连接、写入、读取共享不可续期的 2 秒预算，沉默连接不能无限占住探活。
7. 不新增看板状态、授权、槽位、合并规则和数据格式变更。保留已有正常迁移/归档行为，不能要求正常 run 全程 board.json 字节不变。

### 2.2 非目标

不改宿主 socket 布局；不迁移插件查询 socket；不扫描/删除旧 daemon socket、不创建新旧软链、不回退旧路径；不改 PATH、不补授权或绑定、不自动开关/重启生产实例、不投递真实测试卡。

宿主与插件 **不得新增相互的生产或 dev-dependency**。不新增通用 SDK 或宿主地址注入协议，不 bump 业务协议 3，不统一重写其他业务 IPC、不修宿主控制客户端或查询服务端 IO。

## 3. 方案选择

| 方案 | 权衡 | 决定 |
|---|---|---|
| 同步算法 + 公共 fixture + 跨进程真实函数对照 | 小范围、独立 workspace，必须防止复制算法继续漂移 | 采用 |
| 宿主注入权威地址 | 减少插件算法，但需传参/能力协商/standalone 设计 | 另立设计 |
| 新旧探测或软链 | 容易误连旧实例、隐藏错配 | 不采用 |

仅与新布局宿主配套。旧宿主混装时报告连接或兼容错误，不假称能识别未连接对端的具体版本。升级顺序、排空及现有 supervisor 恢复规则继续沿用 INSTALL，不新增自动升级流程。

## 4. 路径组件

### 4.1 环境解析

插件提供可显式传值的纯解析函数，生产包装读取环境，对齐 `lifecycle::scope::instance_home_for_sockets`：

1. 非空 `YI_AGENT_INSTANCE_HOME`：直接使用，不追加后缀。
2. 否则非空 `YI_AGENT_HOME`：末级名为 `.yi-agent` 时原样，否则追加 `.yi-agent`。
3. 否则非空 HOME：追加 `.yi-agent`。
4. 三者缺失/空值：`InstanceHomeUnavailable`，不以 cwd/state_dir/TMPDIR 猜目录。

优先级入口必须过滤空值，不能只测试宿主的低层 `instance_home_from_env` 而遗漏 INSTANCE_HOME。并行测试不得修改全局环境或 cwd；测试生产环境包装时单独起子进程。

### 4.2 canonicalization 与地址

逐字对照宿主 `canonicalize_allowing_missing` / `canonical_or_self`：

- 先基于 cwd 绝对化，并按宿主当前顺序做词法 `.`/`..` 处理，再 canonicalize；不改变“符号链接加 ..”的宿主语义。
- 全路径存在则 canonicalize；否则寻找最深已存在祖先，canonicalize 后追加规范化的缺失尾部。
- canonicalization 返回错误时回退原始输入，与宿主相同；测试采用删除 cwd 的独立子进程构造 current_dir 错误，不能依赖 root 下不成立的 chmod 拒绝。
- 仅保证祖先与符号链接映射未改变时，缺失尾部创建前后身份稳定；文件系统身份变化不是需要隐藏的错误。
- runtime 规范化后的 Unix OS 原始字节做 SHA-256，取前 8 字节转 16 个小写 hex；不用 lossy display、state_dir 或原拼写做哈希。
- 实例目录独立规范化，构造 `<home>/sockets/<digest16>/runtime.sock`。
- 最终路径字节数超过 103 返回 `PathTooLong { bytes, limit, path }`；bytes 从 OS 原始字节计算，非 UTF-8 不能用 display 字符串长度代替。
- 包含 NUL 的 Unix socket 地址在连接前明确报 InvalidInput，不交给 C 字符串截断；路径不因此切换旧地址。

`socket.rs` 只保留查询规则；业务 daemon 实现放在同 crate `daemon_socket.rs`，通过原 `socket::daemon_socket_for` 兼容包装调用。`client::socket_path` 与 run 共用同一入口。新业务错误类型不改变查询 `SocketPathError`。

### 4.3 启动副作用顺序

run 在受管上下文自检之后、calendar/单实例锁与 BoardService 构造之前解析业务 daemon 地址；失败报错并 exit 1。不能用 `.ok()` 静默丢弃。

地址可解析不等于对端可用：连接失败仍按三次策略观测；不增加“首次失败立即退出”或禁止读取看板的规则。缺实例目录时不存在可打印的实际 socket：诊断显示 runtime、`socket unresolved` 与 `InstanceHomeUnavailable`；超长时显示候选地址及原始 bytes/limit，不能虚构目标。

解析失败的负例应证明无 plugin.lock、无查询 socket、无 board/inbox 修改；地址成功后的正常启动迁移、合并恢复、归档仍沿用原逻辑。本修复不得新增这些状态变化。

## 5. 探活与 IO

### 5.1 连续失败与诊断

`BoardDaemon::probe` 返回 `Result<(), ClientError>`，runner 投影为 Liveness；成功清零，三次失败退出，探活仍在开关判断之前。新增只读 misses accessor 用于输出次数，不复制第二套计数器。

启动日志区分 `daemon socket at` 和 `query socket at`。每次失败一行含 runtime、实际 socket、计数、IO errno/超时阶段/解码或版本错误；第三次在同一行附退出原因。成功不每拍打印。

完整、合法、协议/request_id 匹配的任何响应仍可证明通信应答（保留当前语义，包括合法业务 Error）；非法/截断/超限/版本错配算失败。不能仅判断文件或 PID 存在。

### 5.2 不可续期预算

生产 `send_status_with_budget(socket, Duration::from_secs(2))`，只允许 Status；私有 IO 执行器保留窄测试接缝验证短写、信号中断和连接积压，其他 `send` 保持不变。

- 从首次连接尝试前创建唯一 Instant deadline，每次 IO/readiness 前后检查，重试不可续期。
- 必须在首次 connect 前创建并配置 O_NONBLOCK、CLOEXEC fd；不能先 `UnixStream::connect` 再切非阻塞。现有宿主 `client.rs::connect_with_timeout` 也有这个先阻塞的问题，不以其注释作为正确性证据、不在本次修它。
- EINPROGRESS 等待 readiness 并检查 SO_ERROR；Linux AF_UNIX EAGAIN/积压不当成已完成连接，在剩余预算内有界重试新 fd，避免 POLLOUT+SO_ERROR=0 假成功。EINTR 重查预算；其他 IO 错误保留 errno。macOS sockaddr 的 sun_len 与所有平台 NUL/103 字节验证显式处理。
- 部分写使用偏移；部分读积累至首个 LF；WouldBlock 等 poll；Interrupted 重查 deadline。真实 read 判断 EOF，HUP 不丢已送达帧，POLLNVAL/不可恢复错误终止。
- 收包缓冲最多 MAX_FRAME_BYTES+1，完成 LF 后使用现有 decode_response 验证；上限计 envelope JSON 原始字节，不含 LF。
- 发送保护 SIGPIPE：macOS 配 SO_NOSIGPIPE，其他支持的平台使用 MSG_NOSIGNAL 或等效的局部 socket 方案，不能全局忽略信号；RAII 关闭失败 fd，不 fork 永久后台 IO 线程。
- 超时返回带阶段（connect/write/read）的错误，关闭连接；第三次超时仍走正常退出。

预算保证是用户态 IO/等待的不可续期限制，不承诺线程长期未被调度或内核不可中断时的严格墙钟上界，也不把同步 JSON 解码/文件系统工作宣传成硬实时。

### 5.3 退出时间的准确口径

保留现有循环调度，不引入探活后台线程，不承诺整个 run 的无条件墙钟退出上界：两次探活之间的 inbox、归档与锁等待可能耗时。

空 inbox、无待归档/合并、无锁争用的失联验收，从首次连接尝试前计时，预算为 `3 × P + I1 + I2 + W1 + W2 + 调度余量`，P=2 秒；I 是实际睡眠，W 是两轮其余工作耗时。自动进程测试固定 `--interval-secs 1`，允许总计 12 秒且验证三次诊断，不只看进程是否退出。单次 IO 的确定性预算单独测试，不能靠放宽进程等待替代。

## 6. 防漂移与门禁

新增公共 `plugins/contracts/fixtures/daemon-socket.json`：算法、环境优先级、文件名、103 字节上限、fixture case IDs 和固定原始字节哈希向量。测试根用模板展开，真实临时根先 canonicalize；不得把本机 /tmp、用户目录、TMPDIR 哈希当普适向量。

宿主独立 `tests/daemon_socket_contract.rs` 与插件独立 `tests/daemon_socket_contract.rs` 分别读取 fixture，调用真实本侧函数。跨端不增加 crate 依赖：

- 宿主新增 **仅测试使用、不会打包** 的 example `yi-agent-lifecycle/examples/daemon_socket_contract_probe.rs`，从十六进制 Unix 路径参数和进程环境调用真实 `scope::instance_home_for_sockets` / `daemon_socket`，返回一行 JSON 的原始字节 hex 或错误类别。`daemon_socket` 本身返回 PathBuf 而非 Result；探针先处理实例 Option，再按 lifecycle 的 103 常量验证字节长度，明确这是客户端校验而不是宿主路径函数报错，不 bind socket、不读取看板。
- 插件跨端集成测试调用本侧真实解析函数，并启动 example 对照生产环境包装。example 不知道看板名称/目录；子进程环境独立，不能运行时 cargo build。
- 跨端测试单独标 `#[ignore = "requires separately built host probe"]` 以保持插件独立测试；专门 gate 必须 `--ignored --exact` 明确执行，缺探针/非零退出/非法输出均失败，不能 return 或 SKIPPED。
- `scripts/plugins/check-daemon-socket-contract.sh` 使用 cargo metadata target_directory 定位 example（支持 CARGO_TARGET_DIR）；分别构建宿主 example 和运行插件 ignored 测试，固定输出测试数量/结果，保证零测试不能报绿。
- 新 recipe `plugin-daemon-socket-check` 调脚本，成为 `plugin-contracts-check` 的依赖。现有 GitHub plugins job、GitLab ci-plugins 均调用 plugin-contracts-check，从而必执行；macOS job另加 Rust-only recipe 调用，覆盖两种 Unix 行为。不把 Node 插件门禁塞入宿主 just ci。
- 脚本/CI 接线静态守卫与自身测试接入 plugin-contracts-check，验证 recipe 存在、显式 ignored 用例名、workflow 调用链、缺 helper 失败与零测试拒绝。

变异：分别恢复旧路径、交换环境优先级、移除 canonicalization、删除 gate 依赖，对应验收必须失败。需记录命令、测试名、退出码；只比两份手抄字符串不算交叉验证。

## 7. 自动验收矩阵

| ID | 场景与完成判据 |
|---|---|
| A01 | 短/深 runtime 均用新布局，没有旧直连/TMPDIR daemon 分支 |
| A02 | INSTANCE_HOME 优先于 YI_AGENT_HOME/HOME，不追加后缀；实际环境包装也被测 |
| A03 | override 根目录/已带后缀/空值/全部缺失符合规则，缺失明确报错 |
| A04 | runtime 与实例目录符号链接、相对路径、点段及 symlink+.. 的宿主语义匹配 |
| A05 | 不改变祖先映射时，含 symlink 祖先的缺失尾部创建前后身份稳定且跨端一致 |
| A06 | 不同 runtime/实例隔离；TMPDIR 改变不影响业务地址 |
| A07 | 非 UTF-8 原始字节、固定 SHA-256 向量、删除 cwd 的错误回退与宿主一致 |
| A08 | 103 字节边界、超长 bytes 计数准确；NUL 在连接前拒绝；不回退 TMPDIR |
| A09 | 查询通道原 fixture 与深路径 bind 测试继续通过，不改变布局 |
| A10 | 新地址合法 Status 桩至少 6 次探活后 run 同 PID 存活；已监听的两个旧候选零请求 |
| A11 | 连续三次失败退出、成功清零、开关关闭也探活；空载进程 12 秒预算；合法 Error 响应仍证明应答 |
| A12 | connect积压/沉默/滴灌/短写/Interrupted/断管/截断/非法/超限/版本与 ID 错配均正确失败，单次预算不可续期，无SIGPIPE退出/资源泄漏 |
| A13 | 解析失败在锁/查询/看板副作用前 exit 1；诊断带实际地址/类别/次数，计数一致，不误判正常卡片状态 |
| A14 | 公共 fixture、真实跨进程函数、显式 ignored gate、Linux/macOS CI 链有效；缺 helper/零用例/移除接线失败，四类变异变红 |

验证入口（仓库根）：

```bash
cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-ipc
cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-runner
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-lifecycle
(cd yi-agent-rs && just plugin-daemon-socket-check && just plugin-check)
```

代码提交前及合并后，两个 workspace 格式检查、插件 gate、宿主 just ci 均通过。实施 plan 固定 A01–A14 的测试名、任务与 gate，不让“独立插件不在宿主 ci”变成少测。

## 8. 隔离及部署验收

先用独立实例 home、独立 runtime、独立插件 state 验证，不能共享生产 sqlite。进程测试 env_clear 后白名单 HOME/PATH/必要 OS 环境，清除受管 binding、实例、runtime、卡号的继承值；纯路径测试以显式环境值工作，生产包装在子进程测试。自动测试不会读取 ~/.yi-agent 配置。

1. 记录宿主/插件版本和摘要、插件 txt/磁盘 inode；目标实例环境确实一致。部署前排空活跃合并/任务，记录原 supervisor 是否存在，不能无条件全机 pkill。
2. lsof 实际地址与插件启动地址一致，新地址只读 Status 成功；旧候选没有被请求。
3. 活宿主持续至少 120 秒并跨至少 6 次探活，插件 PID 不因本问题变更，无误判退出循环。
4. 真宿主转发至少 12 次小型只读 switch.read，记录每次结果。Connection refused 必须解释；若命中已知查询 IPC 缺陷，记录独立阻塞，不能算本项通过，也不能擅自扩大实现范围。
5. 隔离实例停止目标 daemon 后，按实际 interval 和空载条件观测本次插件 PID 的三次失败退出及锁释放；监督器拉起的新 PID 不当成旧进程未退出。所有子进程有超时 kill/wait 清理。
6. 正式安装/重启须获用户明确允许。复核以上地址、稳定性和只读查询；检查卡片身份、顺序、会话关联没有本修复新增变化，已有迁移/到期归档分别记录。

不投递真实卡、不花费 LLM API；worktree 授权/任务启动另行处理。缺生产许可、查询 IPC 阻塞或真实产物缺失时如实未完成，不把 spec/plan 自检冒充真机验收。

## 9. 完成定义与审查结论

实现项只有 A01–A14 和隔离真机及获准后的正式复核均有证据才勾完成。仅代码/隔离完成时写“正式部署待确认”，不声称用户安装已恢复。

交付：路径修复、探活预算/诊断、公共 fixture、双侧及跨进程测试/接线、安装契约说明、脱敏验收记录、项目进度。

本轮交付 rev3 与对应 plan，功能保持 `[ ]`，不自动入队或执行。审查发现与复核记录见[审查报告](../reviews/2026-11-09-kanban-daemon-socket-spec-review.md)；plan 见[实施计划](../plans/2026-11-09-kanban-daemon-socket-contract-impl.md)。
