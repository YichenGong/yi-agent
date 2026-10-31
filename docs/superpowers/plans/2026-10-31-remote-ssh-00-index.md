# 远程 SSH 执行 · 实施计划索引

**设计文档（spec）：** [`docs/superpowers/specs/2026-10-31-remote-ssh-execution-design.md`](../../specs/2026-10-31-remote-ssh-execution-design.md)

**总目标：** 让 yi-agent 能经 ssh 在远端项目里跑常驻 `app-server`，一个项目一个远端进程，手机经中继可见并可切换，整个功能由实验开关 `remote_ssh` 管控，dev-only。

## 为什么拆成四份

spec §10 的实施顺序有 15 个步骤，跨度很大：从"注册一个 flag"到"改整个桥接层的进程模型"再到"手机端多连接"。按 `writing-plans` 的 Scope Check，它们不是同一个可独立验收的子系统。拆成四份的依据是**每份都能独立合并、独立验收**：

| 计划 | 覆盖 spec §10 步骤 | 独立可验收的产物 |
|---|---|---|
| [01 多槽位桥接层](2026-10-31-remote-ssh-01-multislot-bridge.md) | 1、2、4、5、6、8、10 | 本地 sidecar 改成槽位表后**行为零变化**；一个远端项目能连上并对话 |
| [02 二进制安装](2026-10-31-remote-ssh-02-binary-install.md) | 3 | 首次连接自动把正确架构的二进制装到远端 |
| [03 UI 与手机](2026-10-31-remote-ssh-03-ui-and-mobile.md) | 7、9、11、12 | 设置页可用、手机能看到并连上多个远端项目 |
| [04 端到端验收](2026-10-31-remote-ssh-04-acceptance.md) | 0、13、14 | 55 条判据逐条可复核 |

## 执行顺序与依赖

```
01 (必须最先，纯重构打底)
 ├── 02 (可与 01 的 Task 7 并行开发，但合并要在 01 之后)
 └── 03 (依赖 01 的槽位表)
       └── 04 (依赖全部)
```

**01 必须最先完成并单独验证**：spec §10 开头明确写了理由——先把单例改槽位、事件名改带 slot，在**只有本地 sidecar** 的情况下验证零回归，再挂远端。反过来做会陷入"远端 bug 与重构 bug 分不清"。

## 各计划的硬约束（跨计划生效）

- 远端模式下前端**不传 `cwd`**（spec §4.4）
- `YI_AGENT_HOME` **必须显式传给远端进程**，`cd` 不能替代（spec §4.6）
- 事件名格式 `app-server://{kind}/{slot}`，本地固定 `local`
- 支持矩阵只有三个 target，其余显式拒绝（spec §2.4.5）
- 所有 flag 默认 `false`，stable 恒关（spec §5.3）
- `Observe` scope 拿不到 relay `session`（spec §4.6、§8.9）

## 验收门槛

55 条判据（spec §9）全部 PASS 才能进正式版评审。其中 C 批的并行判据**只在两个远端项目同时开着时才测得出来**，单项目全绿不代表并行正确——这是本设计最容易漏的一类。

## 执行方式

每份计划都可以用 `superpowers:subagent-driven-development`（每任务一个子代理，任务间评审）或 `superpowers:executing-plans`（本会话批量执行，带检查点）来跑。
