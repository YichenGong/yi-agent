# Kanban daemon socket plan 专项复审记录

日期：2026-11-09

对象：[实施计划 rev2](../plans/2026-11-09-kanban-daemon-socket-contract-impl.md)；权威[spec rev3](../specs/2026-11-09-kanban-daemon-socket-contract-design.md)；原plan提交 `2657150c`，代码基线 `dcfad0d6`。

用户要求反复检查plan。此次只修改plan及进度/复审文档，不实现修复、不修改spec契约、不操作真实卡片或进程、不安装、不合并main。

方法：五轮专项人工审查，包含临时最小Rust编译与计划内嵌shell的真实Bash3.2语法/mock执行。没有派独立审查agent。最终结论：修订后当前未发现未决计划阻断项；不是对未来实现无bug的保证。

## 审查清单

- [x] 对照真实模块边界、函数签名与当前路径算法。
- [x] 最小编译验证环境过滤生命周期与模块可见性。
- [x] 检查跨端cwd/原始字节/缺失路径身份是否可比较。
- [x] 检查真实探活计时、私有IO/信号子进程、清理与状态保全。
- [x] 核对GitLab/GitHub依赖及gate调用链。
- [x] 对内嵌shell做语法、正常8阶段和六类失败注入检查。
- [x] 修订后重走Task1–8与A01–A14，检查新增接口/辅助程序无跨crate测试复用。
- [ ] 原项目代码编译、单测、真实跨端路径函数、Linux/macOS CI与真机部署（尚未实施）。

## 第一轮：接口/编译与身份

| ID | 问题与证据 | rev2处置 |
|---|---|---|
| P01 | 原 `let nonempty = \|Option<&OsStr>\| ...` 闭包返回借用，最小rustc报lifetime may not live long enough | 改为具名 `fn nonempty(Option<&OsStr>) -> Option<&OsStr>`，最小示例编译exit0 |
| P02 | 普通私有status_io::exchange无法从兄弟client调用，最小rustc报E0603 | exchange设pub(super)，其余IO函数/trait留模块私有；修正版编译exit0 |
| P03 | host与插件child采用两个不同cwd，相对runtime/home产生不同canonical身份，是正常行为不是契约漂移 | 同一canonical cwd顺序运行；删除cwd用例在同路径重建，再启动第二child |
| P04 | 初始cross示例用了无限制Command::output | 改为明确定义的5秒run_contract_child，临时日志、超时kill/wait和RAII清理 |

源码依据：[lifecycle scope](../../../yi-agent-rs/crates/yi-agent-lifecycle/src/scope.rs)；[IPC模块表](../../../plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/lib.rs)。

## 第二轮：探活与负例可信度

| ID | 问题与证据 | rev2处置 |
|---|---|---|
| P05 | Rust程序默认忽略SIGPIPE；最小程序读到旧handler=1(SIG_IGN)，仅声明“默认信号”会假绿 | child内明确signal(SIGPIPE,SIG_DFL)，只改child；断开peer后GO握手，用私有exchange_connected强制写，去局部保护必须signal13红 |
| P06 | 从首条失败诊断计12秒会漏掉第一次2秒探活 | 首次probe前仅一次loop-start日志，12秒从该标记开始，启动等待另有5秒watchdog |
| P07 | private worker/runner单测跨crate复用Task4私有桩不可编译 | signal/shortIO worker留IPC status_io::tests；runner本侧独立只读Error桩，无dev依赖 |
| P08 | connect EINTR未经确认就按SO_ERROR0成功有误判风险；socket -1不得构造OwnedFd | EINTR重查原deadline后关fd重建；先验证raw fd非负，再立即OwnedFd |
| P09 | fd总数/旧fd号可能受并发及复用影响；已正确行为不保证新测试第一次红 | 资源失败配对接缝+独占真实检查；真实红证据用受控变异，不伪造首次红 |

源码依据：[runner client](../../../plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/client.rs)、[run_daemon](../../../plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs)、[liveness](../../../plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/single_instance.rs)。

## 第三轮：CI、产物与gate假绿

| ID | 问题与证据 | rev2处置 |
|---|---|---|
| P10 | GitLab ci-plugins当前apt列表无Python，新增gate依赖Python不可赌镜像 | Task7明确在该job安装python3，保留权限/rules及contracts调用；macOS显式装just |
| P11 | 从metadata目录碰巧发现旧native探针可能错用build.target下陈旧产物 | 本次cargo build --message-format=json取example executable，与metadata预期目录交叉验证，不一致失败 |
| P12 | 仅负例通过可放行永远exit1的脚本；只核cross数量可能漏掉关键IO/进程测试 | 正例必须exit0且8阶段argv完整；六类失败注入；关键测试成功名称列入gate，删除/ignored即失败 |
| P13 | 继承RUST_TEST_NOCAPTURE/color可能改变日志；私有worker未明确nocapture会阻塞握手 | gate清除继承capture/worker输入、固定pretty/never；signal worker显式--nocapture与1线程 |

源码依据：[GitLab ci-plugins](../../../.gitlab-ci.yml)、[GitHub plugins/macos](../../../.github/workflows/ci.yml)、[justfile](../../../yi-agent-rs/justfile)。

## 第四轮：内嵌shell实际执行与修订

首次对计划内嵌shell直接执行mock：`bash -n`通过，正常分支exit0，却仅记录metadata/build两阶段，未跑测试。`/bin/bash -x`定位到Bash3.2的空数组 `flags[@]` 在set -u下unbound，条件分支最终错误返回0。没有将这个0记为通过。

**P14修订**：参数数组从空数组改为始终非空 `flags=(--exact)`，ignored时 `flags+=(--ignored)`；自测明确使用 `/bin/bash` 并检查全8阶段，不只检查exit code。

修订后重新执行完全相同的内嵌shell（临时fake cargo仅模拟构建/测试输出，不启动真实cargo）：

| 模式 | 实际exit code | cargo调用阶段 | 判据 |
|---|---:|---:|---|
| 正常合法输入 | 0 | 8 | 所有fixture/cross/IPC/runner proc/lib执行齐全 |
| build成功但缺探针 | 1 | 2 | 拒绝 |
| 编译artifact指向错误target | 1 | 2 | 拒绝 |
| 非法metadata JSON | 1 | 1 | 拒绝，未继续构建 |
| cross零测试且exit0 | 1 | 5 | 拒绝 |
| IPC关键connect测试记录缺失 | 1 | 6 | 拒绝 |
| cross输出成功但exit7 | 1 | 5 | 拒绝，错误传播有效 |

语法检查 `/bin/bash -n` exit0；实际shell版本 `GNU bash 3.2.57`（macOS）。临时源文件/二进制/mock目录用TempDirectory位于现有worktree下，检查结束已自动移除。没有写实现代码或构建原项目。

## 第五轮：修订版覆盖及执行边界

**P15修订**：TestLayout精确定义短canonical root/i、root/r、root/p与state；预建home/tmp/runtime/project，但不提前创建零副作用用例的state；stub仅在本布局内创建socket父目录。query实现/fixture不动，Task1同步旧daemon注释，避免修好代码后留下旧地址说明。

之后按各任务重新反查：

- Task1环境具名过滤、错误类型、后缀/字节/查询回归与注释一致。
- Task2两端真实生产包装、公用cwd、删除/重建、helper缺失及有界child完整。
- Task3/4 pub(super)桥接，其余私有测试接缝、唯一deadline、SIGPIPE/部分IO/帧限制有对应步骤。
- Task5/6首次probe前计时、一个misses计数、真实Status计数与无副作用/正常迁移边界一致。
- Task7正例/负例、Bash3.2、metadata+artifact、两平台依赖、关键测试清单完整。
- Task8报告/正式许可/不自动合并边界未扩大；19/23仍未完成。
- A01–A14对应任务无遗漏，辅助函数/worker职责与调用点一致；没有发现新增未决阻断项。

## 文档验证与非验收声明

交付检查已实际执行并通过：spec/plan A01–A14各14条且唯一；plan Task1–8连续；四份变更文档的相对链接存在；新plan/review无TODO/TBD/FIXME或违规状态条目；模块与索引计数19/23一致；`cargo fmt --manifest-path yi-agent-rs/Cargo.toml --all`、宿主和插件workspace的 `cargo fmt --all -- --check`、`git diff --check` 均exit0；变更范围核验恰为四份docs Markdown，未修改源码或CI配置。纯文档变更按项目规则免跑原项目CI/单测。

最小rustc与mock验证证明的是**计划示例及gate结构**。没有证明真实路径修复、原项目可编译、原项目测试/云端CI通过或正式插件已恢复。实现功能保持 `[ ]`，没有自动开始执行。
