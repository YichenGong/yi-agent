# Spec 2：插件命令面（CLI + skill + INSTALL.md）

日期：2026-10-01
状态：设计待评审

## 1. 目标

让"在对话框里说一句，把需求加到看板"成为可行：模型经 **skill** 得知怎么做，经 **CLI** 真正入队。CLI 属于插件，**安装后才有**，主程序不提供。

## 2. 插件二进制 `superpowers-kanban`

一个二进制，两类职责：

- **守护**：`superpowers-kanban run --runtime-dir <d> --state-dir <d> --project-root <d> [--interval-secs 60]`——即现有 `board-runner` 的守护循环，supervisor 清单改调此子命令。
- **命令**：
  - `superpowers-kanban add <spec> <plan>`——入队（复用既有的 `deliver_card` 原语：写 `<state_dir>/inbox/<id>.json`，原子 + 幂等）。
  - `superpowers-kanban list`——打印看板（含各卡片状态）。
  - `superpowers-kanban on` / `off`——写开关。

`add` 的语义与 TUI/桌面一致：**只投递**，由守护循环在下一 tick 校验成对后入队；校验失败进 `inbox/rejected/` 并给出原因。

## 3. skill：`superpowers-kanban`

- 位置（随插件分发）：`plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`
- 安装位置（全局）：`~/.yi-agent/skills/superpowers-kanban/SKILL.md`
- 职责（用户确认）：**只管入队**。步骤：
  1. 识别"把 X 加到看板"的意图；
  2. 确认 spec 与 plan **两个文件都已存在**（不自己生成）；
  3. 若缺失，明确告知用户先走 `brainstorming` / `writing-plans`；
  4. 两文件就绪则调用 `superpowers-kanban add <spec> <plan>`；
  5. 回报结果（已投递 / 被拒原因）。

skill **不**承担 spec/plan 的生成，避免与既有 skill 职责重叠。

## 4. INSTALL.md（指导模型安装，非全自动）

用可被模型逐步执行的命令与验证步骤写成：

1. 构建：`cargo build -p superpowers-kanban --release`（插件工作区内）。
2. 放置二进制到 PATH（或清单里写绝对路径）。
3. 复制 skill 到 `~/.yi-agent/skills/superpowers-kanban/`。
4. 复制清单 `<项目>/.yi-agent/supervisors/superpowers-kanban.json` 并按需改 `command` 为绝对路径。
5. 开关：`superpowers-kanban on`（或 TUI/桌面里打开）。
6. **验证**：`superpowers-kanban list` 有输出；daemon 在跑；`supervisors` 扫描已拾取清单。
7. **卸载**：删清单（daemon 随即停掉进程）→ 删 skill → 删二进制。正在运行的会话照常跑完。

## 5. 测试重点

1. `add` 写出正确的 inbox 投递文件（原子、幂等：同 id 覆盖）。
2. `add` 缺参数 / 文件不存在时给出可读错误且不写投递。
3. `list` 与 `on`/`off` 的输出与该插件状态一致。
4. skill 文档中的命令与真实 CLI 表面一致（防文档漂移）。
