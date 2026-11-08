# Superpowers 看板 daemon socket 契约同步修复设计

日期：2026-11-09

基线提交：`dcfad0d6`

状态：待评审、待实现。本轮只提交 spec 和项目进度记录，不修改生产代码、不安装二进制、不重启真实进程、不操作卡片状态。

## 1. 问题与证据

### 1.1 已确认故障链

宿主提交 `cac5afe0`（2026-11-08 02:02）把业务 daemon socket 迁移到实例目录：

```text
<instance_home>/sockets/<canonical-runtime-path-sha256-first-16-hex>/runtime.sock
```

宿主实现位于 `yi-agent-rs/crates/yi-agent-lifecycle/src/scope.rs` 的 `socket_directory` / `daemon_socket`，`yi-agent-store/src/ipc/client.rs::socket_path_for` 调用该实现。

插件仍使用 `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/socket.rs::daemon_socket_for` 的旧规则：短路径用 `<runtime_dir>/runtime.sock`，深路径用 `$TMPDIR/yi-agent-<raw-runtime-path-hash>.sock`。

`runner/src/main.rs::run_daemon` 据旧规则构造 `BoardDaemon`；`runner/src/client.rs::is_alive` 将 `Status` 请求的所有错误折叠为 false。连续三次失败后进程退出，监督器重新拉起，再次重复同一错误。

故障链为：**宿主地址已迁移 → 插件探活地址未同步 → 连接被拒绝 → 误报 daemon 已消失 → 退出/拉起循环 → 看板后台无法稳定提供服务。**

### 1.2 本轮只读现场证据

正式应用 sidecar 自报 `yi-agent 0.1.13-dev+dcfad0d6`。以下观测发生在本项目，不能据此推断所有项目和历史故障均同源。

| 检查 | 结果 |
|---|---|
| 正式 daemon | `/Applications/yi-agent.app/Contents/MacOS/yi-agent daemon serve`，PID 43602；检查时仍存活 |
| `lsof` 的实际监听地址 | `/Users/gongyichen/.yi-agent/sockets/51bccf606c7e2ae7/runtime.sock` |
| 插件按旧规则定位的地址 | `$TMPDIR/yi-agent-51bccf606c7e2ae7.sock` |
| 旧地址只读连接 | `ConnectionRefusedError [Errno 61] Connection refused` |
| 新地址只读 `Status` | 返回 `protocol_version: 3`，request_id 匹配，结果类型 `Status`，`high_water_event_id: 48882` |
| 插件日志 | 启动约 20 秒后打印 `daemon is gone; exiting so a fresh instance can take over`，随后重复拉起 |
| 日志实例 | 00:58:37 启动 PID 46547，00:58:57 退出；00:59:07 启动 PID 49851，00:59:27 退出 |
| 二进制一致性 | 运行插件的 txt inode 与 `/opt/homebrew/bin/superpowers-kanban` 的 inode 一致：181093952 |
| skill 一致性 | 已安装 skill 与仓库副本 `cmp` 一致 |
| 数据与开关 | `list --all` 能读到 4 张 Done、1 张 Failed，均归档；项目推进开关为 true |

日志来源：`<项目>/.yi-agent/logs/superpowers-kanban.log`。旧 inode 残留、skill 未同步和推进开关关闭不能解释此次已复现的探活故障。

### 1.3 不混同其他问题

- 当前普通会话 PATH 不含 `/opt/homebrew/bin`，裸调用命令失败；绝对路径查询成功。这不是后台 daemon 地址修复的替代方案。
- 历史失败卡报开发 worktree 未获授权，属于项目绑定/授权问题；本次不复活该卡、不给 worktree 追加授权。
- accepted socket 的非阻塞完整帧问题已有独立 [查询 IPC spec](2026-11-03-kanban-query-io-design.md)，仍可能导致独立的查询失败。
- 本 spec 不承诺修复所有“无法读取”、所有无法启动卡片的原因；验收报告必须区分这些故障。

## 2. 目标与非目标

### 2.1 必须成立的不变量

1. 插件与当前宿主对相同 runtime 和实例上下文计算出同一个业务 daemon socket，短项目和深项目均用新布局。
2. 路径身份口径与宿主一致：实例目录来源、后缀规范化、路径 canonicalization、哈希输入及长度限制均受测试约束。
3. 业务 daemon 地址不依赖 `$TMPDIR`；插件自己的查询 socket 保持原规则，两者不能混用。
4. 地址解析失败必须显示原因；连接失败必须显示尝试地址与错误类别，不能只有“daemon 死了”。
5. 活宿主不会因为旧地址误判而造成插件重启循环；真正失联仍能按现有连续失败策略退出，不能通过删掉探活掩盖问题。
6. 探活的等待有界，沉默但接受连接的对端不能让插件永久占有单实例锁。
7. 看板数据格式、卡片状态、项目授权和合并名额规则不变。

### 2.2 非目标

不改宿主 socket 布局，不搬迁插件查询 socket，不扫描或删除旧 daemon socket，不创建新旧路径软链，不改全机 PATH，不补写受管绑定变量，不修改授权数据，不自动开启看板、不自动重启生产实例、不自动投递测试卡。

不引入插件对 `yi-agent-*` Rust crate 的编译依赖；不新增通用 socket SDK 或宿主地址注入协议；不改变业务 IPC 协议版本 3，不重构全部 IPC 客户端，不修查询服务端的非阻塞 IO。

## 3. 方案选择

| 方案 | 收益 | 代价/风险 | 决定 |
|---|---|---|---|
| 同步插件算法，并以两端公共契约/交叉测试锁定 | 小范围修复，保持独立 workspace，沿用公共 fixture 模式 | 必须覆盖路径规范化，不能只改 join 字符串 | **采用** |
| 宿主向 supervisor/插件注入权威 socket 地址 | 插件不再计算宿主内部布局 | 涉及宿主传参和版本能力协商，standalone 还需另定行为 | 后续另立设计，不纳入本次 |
| 同时探测新旧地址/软链兼容 | 表面兼容旧部署 | 陈旧地址可能指向旧实例，掩盖版本错配，增加歧义 | 不采用 |

只支持与当前新布局宿主配套运行。旧宿主与新插件混装时要明确报连接/兼容问题，不能自动退回旧路径。正式升级应先排空正在进行的合并与任务，再按现有安装流程配套升级；本次不建立新的自动升级流程。

## 4. 组件与路径契约

### 4.1 实例目录解析

在插件 IPC 的 socket 模块内提供可独立测试的纯环境解析接缝，生产包装读取环境。规则逐字对齐 `lifecycle::scope::instance_home_for_sockets` / `instance_home_from_env`：

1. 非空 `YI_AGENT_INSTANCE_HOME` 优先，值直接作为实例目录，不追加 `.yi-agent`。
2. 否则取非空 `YI_AGENT_HOME`：末级目录名已经是 `.yi-agent` 就原样使用，否则追加 `.yi-agent`。
3. 否则取非空 `HOME` 并追加 `.yi-agent`。
4. 全部缺失或为空时，返回明确的实例目录不可用错误；不拿 cwd、state_dir、TMPDIR 猜实例。

相对路径、非 UTF-8 路径按宿主当前语义处理；环境测试传显式值，不在并行测试中修改全局环境。

### 4.2 路径规范化与哈希

以宿主 `canonicalize_allowing_missing` / `canonical_or_self` 为对照，插件实现等价行为：

- 相对路径基于 cwd 转为绝对路径并处理 `.` / `..`。
- 存在的路径 canonicalize，符号链接别名指向同一身份。
- 尾部不存在时 canonicalize 最深的已存在祖先，再追加规范化后的尾部；首次启动不能因为 runtime 目录还没创建就改用旧布局。
- canonicalization 返回错误时，匹配宿主 `canonical_or_self` 的原路径回退语义；交叉测试明确覆盖这一分支，不扩大为另一个路径策略。
- runtime 的规范化路径使用 Unix OS 路径原始字节做 SHA-256，取前 8 个 digest 字节转成 16 个小写十六进制字符，不使用 display 字符串或 state_dir 哈希。
- 实例目录独立规范化后构造 `<home>/sockets/<digest16>/runtime.sock`。
- 最终 Unix socket 路径字节数超过 103 时返回路径超长错误，携带实际字节数、上限和路径；不再回退 TMPDIR。

生产 `daemon_socket_for` / `client::socket_path` 改为以上规则，错误类型须能区分实例目录不可用与路径超长。调用方不以 `.ok()` 静默丢弃错误；解析失败在开始回答查询和消费 inbox 前报错并非零退出，不持有一个没有有效探活目标的常驻实例。

### 4.3 查询 socket 保持隔离

`plugin_socket_for`、查询清单模板与宿主插件转发表仍使用现有 direct-path/TMPDIR 回退契约。只替换业务 daemon 地址推导，不把 `plugin-*.sock` 迁进实例目录。

`run_daemon` 和所有使用插件 `client::socket_path` 的业务调用应共用新解析入口，不允许探活用新地址、其他业务客户端仍用旧地址。实施时列出调用点清单，旧文档中“逐字复刻 TMPDIR daemon 规则”的说明同步纠正。

## 5. 探活与错误策略

### 5.1 保留连续失败机制

保留 `Liveness` 连续三次失败退出、成功清零、开关判断之前探活的顺序。启动输出一次解析后的业务 daemon socket，明确区别于查询 socket。

探活由 bool 内部接口改为可保留原因的结果，调用方再投影为连续失败计数。诊断至少包含：`runtime_dir`、实际 socket、失败次数、IO errno 或解码/协议错误类型。不得打印模型配置、凭据或整个环境。

只有合法、完整、request_id 匹配、协议版本匹配的响应才算通信成功；保留现有“合法响应可证明对端应答”的语义，不用 socket 文件存在或 PID 存在替代应答。非法帧、EOF、超时与拒绝连接都算失败，但日志不得把所有情况断言成“宿主进程已死”。

每次失败最多一条诊断，第三次附带“连续失联，退出让监督器接管”。正常探活不每拍打印日志。

### 5.2 探活 IO 预算

只为 `Status` 探活提供有界发送入口，其他业务 IPC 不借本次机会统一重写。

- 从连接尝试前起创建一次 `Instant` 截止时间，探活预算固定 2 秒，不被逐字节响应、`Interrupted` 或重新等待续期。
- 连接、完整写请求、读至 LF 都受同一预算限制；连接等待须有界，不能只给已连上的 stream 加超时。
- 回复读取上限为现有 `MAX_FRAME_BYTES + 1`（包含 LF），达到帧上限后仍未读到合法 LF 则失败，不无界 `read_until`。
- 失败关闭当前连接，按现有 interval 进入下一次观测，不在一拍中无限重试或切换候选地址。
- 第一次探活失败起，正常调度下最迟 `3 × 2 秒 + 2 × effective_interval + 调度余量` 退出。故障测试配置短 interval，只判本次有界退出，不误把监督器拉起的新 PID 当成旧进程未退出。

这不是完整查询 IO spec 的实现；它仅约束业务 daemon 的只读探活连接。

## 6. 防漂移验证

新增独立 `plugins/contracts/fixtures/daemon-socket.json`，不把业务 daemon 契约塞进现有 `plugin-socket.json`（后者只管查询通道）。公共数据声明算法、环境优先级、文件名、103 字节上限和确定性例子。

fixture 的确定性路径用测试根占位模板，在每次测试创建的临时目录中解析，不能依赖机器上 `/tmp` 是否是符号链接或用户项目目录是否存在。固定的哈希向量另用无歧义 OS 字节输入检验，不把环境生成的期望哈希硬写成普适常量。

宿主和插件各自读取同一 fixture，并各自调用真实生产路径函数。另提供一个测试用交叉对照入口：在宿主侧测试工程调用真实 lifecycle 函数，并调用真实插件 IPC 路径函数，逐项比较输出；可用 dev-dependency 跨 workspace 路径引用，绝不能变成插件生产依赖。测试放置点以避免循环依赖为约束，测试构建纳入宿主和插件门禁，不能只有一个未被 CI 调用的脚本。

**必须有变异证据**：分别恢复插件旧 TMPDIR/短路径规则、交换实例目录优先级、移除路径 canonicalization，相关用例均须失败。固定字符串测试或 fixture 自身与自身比较不能满足验收。

## 7. 自动验收矩阵

| ID | 场景与完成判据 |
|---|---|
| A01 | 短 runtime 与深 runtime 均落 `<instance_home>/sockets/<hash>/runtime.sock`；不存在旧直连分支 |
| A02 | 非空 INSTANCE_HOME 与 HOME override 同时设置时，宿主/插件都选 INSTANCE_HOME，且不追加后缀 |
| A03 | HOME override 根目录/已带 `.yi-agent`/空值/全部变量缺失逐项测试，含缺失时明确报错 |
| A04 | runtime 和实例目录各自的符号链接别名、相对路径与 `.`/`..` 与宿主真实函数输出一致 |
| A05 | runtime 末尾尚未创建，含符号链接祖先；创建前后身份与宿主一致 |
| A06 | 两个 runtime 在同实例下不同地址；同 runtime 在两个实例下地址隔离；TMPDIR 改变不影响 daemon 地址 |
| A07 | 非 UTF-8 路径字节、SHA-256 16 字符向量、canonicalization 错误回退与宿主一致 |
| A08 | 超过 103 字节的实例 socket 明确报超长；等于上限可用；不会转向 TMPDIR |
| A09 | 查询 socket 的原公共 fixture 和深路径实际 bind 测试继续通过，不改变其布局 |
| A10 | 仅新地址监听的合法 Status 桩至少跨 6 次探活，插件同 PID 存活；旧地址桩的请求数为零 |
| A11 | daemon 失联连续三次退出；中间一次合法应答清零；开关关闭不跳过失联检测 |
| A12 | 沉默对端、慢速逐字节、截断/非法/超限帧、协议/request_id 不匹配都在不可续期预算内失败并输出原因 |
| A13 | 解析失败在查询/inbox 副作用前非零退出；失败日志带地址/类别，不断言宿主进程已死；测试不改卡片状态 |
| A14 | 两端公共 fixture 与真实函数交叉测试进入门禁；三类变异各自变红，列出测试名、命令、退出码 |

命令入口：

```bash
cd plugins/superpowers-kanban
cargo test -p superpowers-kanban-ipc
cargo test -p superpowers-kanban-runner
# 宿主侧的契约交叉测试归属 yi-agent-lifecycle 的测试目标
cd ../../yi-agent-rs
cargo test -p yi-agent-lifecycle
just plugin-check
cargo fmt --all
just ci
```

实施计划必须固定 A01–A14 对应测试名和 CI 接线位置。插件 workspace 的格式检查同时遵循其现有门禁；代码改动提交前和合并后都跑宿主 `just ci`，不能以本 spec 的纯文档免测规则代替实现门禁。

## 8. 部署与真机验收

先在隔离实例验证，实例 home 和 runtime 均独立，不能让第二个实例使用生产 runtime 数据库。正式应用验收须由用户明确允许更新/重启后再进行，不盲目 `pkill` 全机进程。

1. 记录宿主版本、插件产物摘要、插件进程 txt inode 与磁盘 inode；确认配套使用新布局宿主与修复后插件，skill/INSTALL 文档同步。
2. 通过 `lsof` 获取目标 daemon 实际监听地址，与插件启动日志的地址逐字比较。新地址发送只读 Status 成功；不再连接旧地址。
3. 目标宿主保持存活，持续观测至少 120 秒且至少 6 次探活，插件 PID 不因本故障变化，日志没有重复的误判退出。
4. 隔离环境通过真实宿主转发至少 12 次小型只读插件查询，确认不因进程退出而 Connection refused。故意请求延迟或大帧所触发的旧查询 IPC 问题另行报告，不能当成本次根因复发，也不能隐瞒。
5. 在隔离实例让目标 daemon 真正退出，观察本次插件 PID 在 §5 预算内退出；监督器后续启动新 PID 不影响该判断。不得触碰正在进行的真实合并。
6. 用户同意的正式部署后复核地址、PID稳定性、只读查询和历史卡片字段未被本修复改变。部署所需的其他安装/版本故障如实列为独立阻塞。

不通过投递新卡或付费 LLM 调用验证探活。真正启动卡片的项目授权问题不在本 spec 完成判据内；如果修完探活后仍无法启动卡片，要另行设计绑定修复。

## 9. 完成定义与交付物

本修复只有在 A01–A14、隔离真机验收、获准后的正式部署复核均有可复算证据时才标完成。未获得生产更新许可时明确记录“代码与隔离验收完成，正式部署待确认”，不能声称用户当前安装已恢复。

交付物：插件路径解析修复、只读探活诊断/预算、公共 daemon 地址 fixture、两端真实函数交叉测试及门禁接线、更新后的安装/契约说明、脱敏验收记录、同提交内的项目管理状态更新。

本轮交付仅为该 spec：实现卡保持 `[ ]`。文档自检不得冒充 A01–A14 已通过；不生成实施 plan，不自动入队，先由用户评审设计。
