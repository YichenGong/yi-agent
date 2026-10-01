# 安装 Superpowers 看板

这个插件由**三块**组成，缺任何一块都不算装好：

| 组件 | 作用 | 装到哪 |
|---|---|---|
| `superpowers-kanban` 二进制 | 推进队列（`run`）+ 入队/查询/开关（`add` `list` `on` `off` `workdir`） | `PATH` 上任意位置 |
| `superpowers-kanban` skill | 让模型知道"用户说加到看板"时该干什么 | `~/.yi-agent/skills/superpowers-kanban/` |
| supervisor 清单 | 让 daemon 在开关打开时守护 `run` 进程 | `<项目>/.yi-agent/supervisors/`（每个项目一份） |

**这一步不需要写任何脚本。** 下面的命令照抄执行即可；每一步都给了可验证的预期输出，
对不上就停在那一步排查。

---

## 0. 前置条件

- 有 Rust 工具链（`cargo` 可用）。
- 项目里有一个在跑的 daemon：`yi-agent daemon start`。
  daemon 每 500ms 对账一次 supervisor 目录，所以**装完不需要重启 daemon**。
- 你手上有至少一对**已经写好**的 spec + plan。看板是 Superpowers SDD 的**下游**，
  它不生成 spec/plan。

---

## 1. 构建二进制

```bash
cd <插件目录>            # 即本 INSTALL.md 所在的 plugins/superpowers-kanban/
cargo build --release
```

预期：编译成功，产物在 `target/release/superpowers-kanban`。

```bash
test -x target/release/superpowers-kanban && echo ok
```

预期：打印 `ok`。

---

## 2. 安装二进制到 PATH

```bash
install -m 0755 target/release/superpowers-kanban ~/.cargo/bin/superpowers-kanban
```

若 `~/.cargo/bin` 不在 `PATH` 上，换成你 PATH 里的任意目录。

验证：

```bash
superpowers-kanban
```

预期：打印用法并以退出码 2 结束（没有子命令即用法错误，这是正常的）：

```
superpowers-kanban: usage: superpowers-kanban <run|add|list|on|off|workdir> [...]
```

若报 `command not found`，说明没进 `PATH`，回到上一步。

---

## 3. 安装 skill

skill 决定模型在"加到看板/排队/入队"这类说法下会做什么。**手工复制**：

```bash
mkdir -p ~/.yi-agent/skills/superpowers-kanban
cp <插件目录>/skills/superpowers-kanban/SKILL.md ~/.yi-agent/skills/superpowers-kanban/SKILL.md
```

验证：

```bash
head -3 ~/.yi-agent/skills/superpowers-kanban/SKILL.md
```

预期：第一行是 `---`，第三行是 `name: superpowers-kanban`。

**skill 在下一个会话才生效**，当前已开的会话不会重新扫描。开一个新会话验证。

> 也可以装到项目层 `<项目>/.yi-agent/skills/superpowers-kanban/`，
> 只对该项目生效。三层优先级：项目 > 用户 > 系统。

---

## 4. 安装 supervisor 清单（每个项目一次）

清单告诉 daemon：开关打开时该拉起哪个进程。

```bash
mkdir -p <项目>/.yi-agent/supervisors
cp <插件目录>/supervisors/superpowers-kanban.json <项目>/.yi-agent/supervisors/superpowers-kanban.json
```

**若二进制不在 `PATH` 上**，把清单里的 `command` 改成绝对路径：

```json
"command": "/absolute/path/to/superpowers-kanban"
```

验证：

```bash
cat <项目>/.yi-agent/supervisors/superpowers-kanban.json
```

预期：`name` 为 `superpowers-kanban`，`switch_key` 为 `superpowers_kanban`，
`args` 里含 `--state-dir {state_dir}` 与 `--project-root {workdir}`（占位符由 daemon 填充，不用你改）。

---

## 5. 打开开关

在**项目根目录**下执行：

```bash
cd <项目>
superpowers-kanban on
```

预期：

```
superpowers_kanban = true
wrote <项目>/.yi-agent/preferences.json
now enabled
```

`on` 只写**项目层**开关。想全局生效就手工在 `~/.yi-agent/preferences.json`
里写 `{"superpowers_kanban": true}`——插件不替用户改全局偏好。

验证开关真的生效（而不是只写了文件）：

```bash
cat <项目>/.yi-agent/preferences.json
```

预期：含 `"superpowers_kanban": true`，且**原有的其他键仍在**（`on` 是读-改-写）。

---

## 6. 端到端验证

### 6.1 daemon 是否拉起了插件进程

daemon 每 **500ms** 对账一次，所以清单放下后最多等半秒。

```bash
pgrep -fl superpowers-kanban
```

预期：有一行是 `superpowers-kanban run --runtime-dir … --state-dir …`。

**没有输出**说明进程没被拉起，依次检查：
清单是否在 `<项目>/.yi-agent/supervisors/` 下、开关是否为 `true`（第 5 步）、
`command` 路径是否可执行（第 4 步）、daemon 是否在跑（`yi-agent daemon status`）。

> `yi-agent daemon status` 报告的是 daemon 事件水位，**不是**托管进程列表，
> 所以判断插件有没有跑起来要看 `pgrep`，而不是它。

### 6.2 投递一张卡

```bash
cd <项目>
superpowers-kanban add docs/<spec>.md docs/<plan>.md
```

预期：

```
delivered <card-id> to <项目>/.yi-agent/superpowers-kanban/inbox
<spec>
<plan>
```

### 6.3 看队列

```bash
superpowers-kanban list
```

预期：刚投递的卡先显示 `pending (waiting for the next tick)`，
下一次 tick（默认 60 秒）后变成排队中的卡。
**刚 add 完看到 `pending` 是正常的，不是失败。**

### 6.4 失败也要能被看见

```bash
superpowers-kanban add docs/nope.md docs/<plan>.md
```

预期：非零退出码 + `superpowers-kanban: spec file does not exist: …`。
校验在 `add` 这一步就做，不会等到 tick 之后才悄悄进 `inbox/rejected/`。

---

## 7. 卸载

按相反顺序，**三块都删**，别留半装状态：

```bash
# 7.1 关开关并停掉进程：删清单，daemon 会在 500ms 内停掉该进程
rm -f <项目>/.yi-agent/supervisors/superpowers-kanban.json

# 7.2 删 skill（用户层）
rm -rf ~/.yi-agent/skills/superpowers-kanban

# 7.3 删二进制
rm -f ~/.cargo/bin/superpowers-kanban
```

仍然保留的（**故意不删**，删了会丢数据）：

- `<项目>/.yi-agent/superpowers-kanban/`（状态目录：`board.json`、`inbox/`、日历）
- `<项目>/.yi-agent/preferences.json` 里的 `superpowers_kanban` 键

确认不再需要时，再由用户自己删。正在 daemon 中运行的会话会照常跑完，
不会因为卸载被取消。

验证：

```bash
superpowers-kanban list
```

预期：`command not found`（二进制已删）。

---

## 8. 排查

| 现象 | 原因 | 处理 |
|---|---|---|
| `command not found` | 二进制没进 PATH | 回到第 2 步 |
| 进程没被拉起 | 清单没放对位置，或 `command` 路径不对 | 第 4 步；确认 daemon 在跑 |
| `list` 长期只有 `pending` | 开关是关的，或 daemon 没跑 | `superpowers-kanban on`；`yi-agent daemon status` |
| 模型不知道"加到看板" | skill 没装，或会话是装之前开的 | 第 3 步；开新会话 |
| `spec file does not exist` | 路径不对 | 相对路径是相对**当前目录**，不是项目根 |
| 卡进 `inbox/rejected/` | 两份文件不成对或相同 | 看 `inbox/rejected/` 里的原因文件 |

---

## 附：迁移（旧名 `board` → `superpowers-kanban`）

旧布局仍会被**读取**，不会被修改或删除：

| 旧 | 新 |
|---|---|
| 开关键 `superpowers_board` | `superpowers_kanban` |
| 状态目录 `.yi-agent/board/` | `.yi-agent/superpowers-kanban/` |
| 日历 `kanban.toml` | `superpowers-kanban.toml` |

新位置确认就绪后，旧文件可自行删除。插件每次只**写新位置**，因此不会在迁移期
把旧数据写坏。
