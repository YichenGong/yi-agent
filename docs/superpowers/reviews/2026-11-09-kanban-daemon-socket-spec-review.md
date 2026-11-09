# Kanban daemon socket spec 复审记录

日期：2026-11-09

对象：[设计 spec rev3](../specs/2026-11-09-kanban-daemon-socket-contract-design.md)；初稿提交 `6d2802c0`；源码基线 `dcfad0d6`。

方法：本会话人工反复对照真实源码、契约边界、测试可执行性和计划接口。没有派出独立审查 agent，不把本记录称为外部评审。结论是当前未发现未决设计阻断项，不保证未来实现没有 bug。

本轮只修改文档；未执行计划中的测试、未部署、未重启正式进程、未操作真实卡片。初次现场诊断是历史快照，不能当成修复验收。

## 审查清单

- [x] 对照宿主与插件真实路径/环境函数，确认根因和接口类型。
- [x] 复核独立 workspace 与宿主不认识看板的边界。
- [x] 审查首次connect、部分IO、deadline、SIGPIPE、帧上限。
- [x] 区分单次IO预算、空载退出验收和任意工作负载墙钟承诺。
- [x] 审查启动副作用、正常迁移/归档与解析失败负例。
- [x] 固定公共fixture、跨进程验证、ignored必执行gate、两平台CI链。
- [x] 写plan并反查A01–A14覆盖、命令/类型/辅助函数、无未决占位。
- [x] 同步进度文档，功能保持未完成19/23，不虚报恢复。

## 第一轮：架构与启动行为

| 发现 | 证据 | 处置 |
|---|---|---|
| 宿主dev-depend插件虽然只用于测试，仍破坏独立发布边界 | [公共契约说明](../../../plugins/contracts/README.md)、[宿主边界检查](../../../scripts/plugins/check-host-boundary.py) | 撤回跨crate依赖；改成两边独立fixture+host example跨进程对照 |
| 正常run“数据永远不变”的承诺会禁止现有迁移/归档 | [run_daemon](../../../plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs) 包含recover_merge、migrate_legacy_running、adopt_running_leases、archive_due | 仅要求本修复不新增变化；解析失败必须在锁/calendar/service前exit1，另验零副作用 |
| 解析失败折叠为没有daemon会隐藏实例环境/长度错误 | 同文件socket_path(...).ok()；[旧地址函数](../../../plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/socket.rs) | 明确业务错误类型、提前解析和诊断，移除静默.ok |

## 第二轮：时间与路径语义

| 发现 | 证据 | 处置 |
|---|---|---|
| 从三次探活预算直接推导整个进程退出上界错误 | run两拍之间还有inbox、归档、文件锁与动态interval | 只承诺Status单次IO2秒；空载--interval-secs 1测试12秒，明确P/I/W口径 |
| 环境规则容易遗漏INSTANCE优先级/空值/后缀 | [scope::instance_home_for_sockets](../../../yi-agent-rs/crates/yi-agent-lifecycle/src/scope.rs) 与低层instance_home_from_env语义不同 | 纯显式值测试+实际env child包装测试，禁止并行测试改全局env |
| canonicalization先后顺序及非UTF8可能产生不同hash | scope.rs先词法normalise再canonicalize；错误时canonical_or_self回退输入；hash原始encoded bytes | 与当前宿主逐字对齐；symlink+..、缺失尾部、删除cwd子进程、103/104及非UTF8纳入fixture |

## 第三轮：连接正确性与gate

| 发现 | 证据 | 处置 |
|---|---|---|
| “复制宿主有界connect”会把阻塞带入新实现 | [lifecycle client.rs](../../../yi-agent-rs/crates/yi-agent-lifecycle/src/client.rs) connect_with_timeout先UnixStream::connect再nonblocking | 不复制注释/实现；插件在首次connect前设O_NONBLOCK，明确EAGAIN/pending/SO_ERROR及RAII；宿主helper不扩范围 |
| Status很小，不能靠它自然填满写缓冲验证shortwrite/背压 | [插件client.rs](../../../plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/client.rs) encode_request及wire::Status | 私有完整帧执行器+窄fake IO接缝，配真实沉默/滴灌/积压/断管测试；保护SIGPIPE、唯一deadline |
| 跨端test缺helper若跳过会留下假绿 | ignored测试在正常cargo test不会执行，CI当前无此gate | 专门--ignored --exact测试，helper缺失/非法输出/0test拒绝；recipe与静态守卫、自测都接链 |
| 只接Linux会漏macOS AF_UNIX与SIGPIPE行为 | [GitHub ci.yml](../../../.github/workflows/ci.yml) 固定macos-15 job；[justfile](../../../yi-agent-rs/justfile) plugin-contracts-check不在宿主ci | Linux/GitLab沿现有contracts依赖运行；macOS安装just并加Rust-onlygate，不新增宿主Node依赖 |

## 第四轮：最后的可实施性反查

| 发现 | 处置 |
|---|---|
| 缺home错误不存在可打印的实际socket | spec §4.3明确runtime+socket unresolved；超长另有候选地址和bytes |
| host路径函数并非Result，长度由客户端检查 | probe调用Option home与PathBuf daemon_socket后验证103；不声称调用store wrapper，不改变host算法 |
| 深路径示例用了超长单个目录名，会混淆ENAMETOOLONG | plan Task1改40级deep目录，短canonical临时home，断言最终socket<=103 |
| 正常保全测试默认开关关闭可能空过 | plan Task6调用真实layout::project_preferences_path+switch::write_layer写Enabled；三张已绑定thread卡>=6次探活后比较ID/order/state/thread |
| macOS不能假设已有just | plan Task7明确Install just+Rust-onlyrecipe；跨编译目标不误找本机artifact |
| 超时日志、SIGPIPE、partial IO与cross输出格式需具体化 | plan固定ProbeStage与Timeout Display，私有ProbeIo接口、默认SIGPIPE child，固定1test且0ignored的输出检查 |

最后重新按spec各节与A01–A14对照plan：路径Task1/2，IO Task3/4，runner Task5/6，gate Task7，安装和真实验收Task8。无遗漏的验收编号；所有helper有接口/责任说明；查询IPC、PATH、授权、生产重启仍留在明确边界之外。

## 文档验证口径

交付前实际执行：A01–A14在spec与plan各出现14条且唯一；Task1–8连续；相对Markdown链接存在；禁用占位与状态扫描；固定根目录SHA-256前16hex复算；模块Features19/23与索引一致；两个workspace格式检查及git diff --check；确认变更仅docs/。

这些检查不能证明计划代码可编译、生产恢复、变异测试变红或Linux/macOS云端CI已通过。代码、隔离真机与获准正式复核均尚未执行，功能保持 `[ ]`。

实际结果（本轮文档交付）：

| 检查 | 结果 |
|---|---|
| Python文档自检（本地链接/状态/新增占位） | 5份文档通过；初次误把索引既有禁用状态说明当违规，修正判据后整套重跑exit0 |
| Spec / plan验收表 | 各14条，A01–A14唯一且连续 |
| Plan结构 | 8任务、checkbox步骤、必需header及全局约束通过 |
| 模块计数 | 完成19/总计23，与索引一致；修复feature未勾 |
| SHA256根目录向量 | `sha256(b'/')[:16] = 8a5edab282632443` |
| 宿主格式 | `cargo fmt --manifest-path yi-agent-rs/Cargo.toml --all` 及 `-- --check`，exit0，无代码diff |
| 插件格式 | `cargo fmt --manifest-path plugins/superpowers-kanban/Cargo.toml --all -- --check`，exit0 |
| 变更范围 / whitespace | 仅5份docs Markdown；git diff --check exit0 |
| 代码单测 / just ci / 插件gate / 真机 | 未运行（纯文档改动按项目免CI规则）；计划测试不冒充实际通过 |


## 交付与下一步

- Spec：rev3，当前未发现未决设计阻断项。
- [实施计划](../plans/2026-11-09-kanban-daemon-socket-contract-impl.md)：8任务，逐项红绿测试/提交/验收映射，全部未执行。
- 可选择subagent-driven-development逐任务审查，或executing-plans在当前会话分批执行；没有自动入队、实现、合并或安装。
