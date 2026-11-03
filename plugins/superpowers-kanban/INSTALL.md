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
superpowers-kanban: usage: superpowers-kanban <run|add|add-prequeued|promote|add-merge|merge-request|merge-finish|merge-cancel|done|state|archive|purge|list|on|off|workdir|manifest> [...]
```

若报 `command not found`，说明没进 `PATH`，回到上一步。

### 2a. 升级：覆盖二进制后，让 `run` 进程重启

`install` 覆盖二进制会写入**新的 inode**；而已经在跑的 `superpowers-kanban run`
仍持有旧 inode，会继续用**旧代码**运行——监督器只检查「查询 socket 是否有人应答」，
不会发现二进制换了（unix 语义下这也是合法的）。

症状：升级后投递被静默拒绝，卡片不出现。

处理：让 `run` 进程退出一次，监督器会按清单的 `restart_backoff_ms` 拉起新的：

```bash
pkill -f "superpowers-kanban run"   # 监督器 1s 内拉起新进程
```

确认新进程用的是磁盘上的二进制：

```bash
lsof -p "$(pgrep -f 'superpowers-kanban run' | head -1)" | awk '$4=="txt" && /superpowers-kanban/ {print $7}'
stat -f %i "$(command -v superpowers-kanban)"
```

两个 inode 一致即已同步。

（第 0 节那句「装完不需要重启 daemon」说的是**新装**：daemon 会发现该守护的新清单。
升级的场景不同，需要重启的是 `run` 子进程，不是 daemon。）

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

### 3a. 升级：skill 也要重新复制

skill 是**独立的文件副本**，不会跟着二进制一起更新。升级插件后必须重跑一次第 3 步的
`cp`，否则模型仍按旧命令面干活（例如不知道新增的 `promote` / `merge-request`），
表现为"插件明明支持、模型却说不会"。

复制后同样要**开新会话**才生效。核对两边是否一致：

```bash
diff <插件目录>/skills/superpowers-kanban/SKILL.md \
     ~/.yi-agent/skills/superpowers-kanban/SKILL.md && echo in-sync
```

预期：无差异输出并打印 `in-sync`。

---

## 4. 安装 supervisor 清单（每个项目一次）

清单告诉 daemon：开关打开时该拉起哪个进程。

首选**让宿主装**（推荐）：桌面 app / app-server 的通用 `plugin/install` 会先执行
`superpowers-kanban manifest` 拿到本插件自述的清单，校验后写到
`<项目>/.yi-agent/supervisors/superpowers-kanban.json` 并打开开关——校验失败则**什么都不写**，
不会留下半个安装。宿主源码里不含任何插件专属名字，走的全是通用路径。

手工装（无宿主时）等价于下面两步：

```bash
mkdir -p <项目>/.yi-agent/supervisors
cp <插件目录>/supervisors/superpowers-kanban.json <项目>/.yi-agent/supervisors/superpowers-kanban.json
```

清单里除 `command` 外的内容由插件自述保证与代码一致（有测试钉住
`superpowers-kanban manifest` 的输出与随包发布的
`supervisors/superpowers-kanban.json` 逐字段相等），所以**优先用 `manifest` 的输出**，
不要在手工复制后手改其它字段。

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

### 4.1 后台值守（开机自启，仅 macOS）

清单让 daemon 守护插件；但 **daemon 自己没人守护**——桌面 app 关着时机器重启或
daemon 崩溃，看板就停了。所以**宿主**（桌面 app / app-server）会装一个
LaunchAgent 兜底：`~/Library/LaunchAgents/ai.yi-agent.resident-daemon-watchman.plist`，开机自启 +
崩溃自动拉起，跑宿主子命令 `yi-agent resident watch`，日志在 `~/.yi-agent/logs/resident-daemon-watchman.log`。

- **它是宿主装的，不是这一步装的**：通用 `plugin/install` 收尾时自动装，受宿主级开关
  `resident_daemon_watchman_enabled`（默认开，旧键 `board_watchman_enabled` 兜底）控制。
- **开关**：桌面「看板设置 → 后台值守（开机自启）」。关掉即卸（`launchctl bootout` + 删 plist）。
  注意它和 `superpowers_kanban` 是两件事：前者管「重启后看板还活着吗」，后者管「要不要推进队列」。
- **不想让它动 launchd**：以 `YI_AGENT_DISABLE_WATCHMAN=1` 启动宿主。

验证（装了的话）：

```bash
launchctl print gui/$(id -u)/ai.yi-agent.resident-daemon-watchman >/dev/null && echo installed || echo not-installed
```

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
下一次 tick（默认 10 秒；可在桌面「设置 → 插件 → superpowers-kanban」里改 `推进间隔秒数`）后变成排队中的卡。
**刚 add 完看到 `pending` 是正常的，不是失败。**

### 6.4 失败也要能被看见

```bash
superpowers-kanban add docs/nope.md docs/<plan>.md
```

预期：非零退出码 + `superpowers-kanban: spec file does not exist: …`。
校验在 `add` 这一步就做，不会等到 tick 之后才悄悄进 `inbox/rejected/`。

---

## 6.5 并发配额（整机一份）

峰值是**整机**语义：限流限的是总流量，所以配额只有一个来源，放在插件自己的
全局目录，而不是每个项目各配一份：

```
~/.yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml
```

```toml
default_max_tasks = 4          # 全机同时运行的会话上限

[[window]]                     # 可选：按时段收放
days = "Mon-Fri"
start = "09:00"
end = "24:00"
max_tasks = 3
```

`days` / `start` / `end` 一律按**运行插件的机器**的本地时间判定（插件进程的
`chrono::Local`），不是看板界面所在设备的时区。桌面经 relay 连远端宿主时两者
可能不同，跨时区请按宿主机器的时间填写。

判定口径是「全机峰值 − **本进程**当前运行中的会话数」：

- 「运行」= 正在调模型或正在执行进程。**等待人决策不算**，已结束的不算。
- 计数覆盖**所有**会话：手开的、别的插件起的、看板起的都算。
- **合并轮也占额度**：一次合并是一个真实的模型回合，和实现会话吃同一份余量。
  余量到 0 时合并轮不开局，卡片留在「等待验收」，下一拍余量回来再开。
- 只在**桌面 app-server 这一个进程**内统计。TUI / `yi-agent run` 里的会话不计入。

项目里的 `.yi-agent/superpowers-kanban/superpowers-kanban.toml` **不再**决定并发，
即便还写着 `default_max_tasks` / `window` 也会被忽略；它只留三个与配额无关的字段：

| 字段 | 归属 | 说明 |
|---|---|---|
| `default_max_tasks` / `window` | 全局 | 整机峰值 |
| `interval_secs` | 项目 | 推进节拍 |
| `archive_grace_hours` | 项目 | 归档宽限 |
| `instance_wait_secs` | 项目 | 抢 `plugin.lock` 的等待预算 |

文件缺失时用内置默认 3。`settings.write`（桌面设置页）会按字段拆开写：
并发两半进全局文件，其余留项目文件。

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

**后台值守不受本节影响**（它是宿主的 LaunchAgent，不是插件的一部分）：删本插件不会
卸掉它。要一并停掉，先在桌面「看板设置」里关掉「后台值守」开关，或：

```bash
launchctl bootout gui/$(id -u)/ai.yi-agent.resident-daemon-watchman 2>/dev/null
rm -f ~/Library/LaunchAgents/ai.yi-agent.resident-daemon-watchman.plist
```

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
| 重启后看板不动 | 宿主值守没装（app 没开过 / 开关是关的） | 打开桌面「后台值守」开关；`launchctl print gui/$(id -u)/ai.yi-agent.resident-daemon-watchman` |

### 插件说了什么？看项目日志

被托管进程的 **stderr** 不再是黑洞，落在：

```
<项目根>/.yi-agent/logs/<清单 name>.log      # 本插件即 superpowers-kanban.log
```

两种行：监督器写的事件带 `YYYY-MM-DDTHH:MM:SS` 时间戳前缀，插件自己打印的行原样落盘
（插件不打时间戳）。例如：

```
2026-10-18T21:03:09 started superpowers-kanban (pid 41207)
superpowers-kanban: query socket at /var/folders/…/plugin-504fba94ad6c5e6b.sock
superpowers-kanban: rejected my-card (spec file does not exist: )
2026-10-18T21:03:11 stopped superpowers-kanban (switch off)
```

监督器会记的事件：`started <name> (pid …)`、`could not start <name>: …`、
`exited <name>; retrying in …`、`stopped <name> (manifest removed|switch off|supervisor shutting down)`。

`rejected` 说明投递被插件拒了；被拒的文件留在
`<项目根>/.yi-agent/superpowers-kanban/inbox/rejected/`。先读日志里的括号原因，
再看那个文件的实际内容。

> 只接了 **stderr**，stdout 仍被丢弃——长驻的 `run` 全程用 stderr 报告，
> stdout 留给一次性命令（`add`/`list`/`on`/`off`）的结果通道。
> 日志按 append 写，不轮转（这类日志量极小）。

---

## 9. 合并的执行位置：升级、降级与人工恢复

实现卡的合并**不是**新开一个会话去跑，而是回到这张卡自己的开发会话：

| 情况 | daemon 怎么做 | 你的会话看到什么 |
|---|---|---|
| 原会话还在内存里 | 追加一轮（`resume_append`） | 同一段上下文里收到合并指令 |
| 进程重启过、会话已冷掉 | 按原 `thread_id` + 开发 worktree 的持久历史冷恢复 | 恢复出来的仍是同一会话、同一份工具结果 |
| 开发树被删、历史损坏、锁被占 | **不派发**，卡转 `needs_you` 并附原因 | 不会换一个会话顶替，需要人工处理 |

`thread_id` 与 `workdir` 从入队到结束都不会被改写。这点是硬约束：换会话意味着
上下文丢失，模型会重新踩一遍已经踩过的坑。

### 9.1 升级顺序（宿主 / 插件 / daemon / skill 要一起换）

四块里**顺序错了会静默新开会话**，所以按下面来：

1. **停插件并排空**：让当前没有在飞的合并。
   ```bash
   superpowers-kanban list
   ```
   预期：末尾**没有** `merge attempt …` 行。有的话先按 9.3 处理，再继续。
2. **换插件二进制 + 重铺 skill**（第 2a、3a 节）。
3. **升级宿主 / app-server**，然后重启 daemon，让旧 daemon 进程退出
   （`yi-agent daemon` 只有 `start`/`status`/`stop`，**没有** `restart`）：
   ```bash
   yi-agent daemon stop
   yi-agent daemon start
   ```
   旧 daemon 的内存里可能还留着旧插件契约，重启才算真的换掉。
4. **重新拉起插件**并核对能力协商：
   ```bash
   pkill -f "superpowers-kanban run"
   pgrep -fl "superpowers-kanban run"    # daemon 1 秒内拉起新进程
   ```
   再挑一张卡走一遍合并，确认它真的从 `merge_queued` 走进 `[merging]`：
   ```bash
   superpowers-kanban list
   ```
   卡一直停在 `merge_queued`（没有 `⇄ [merging]`）说明第 3 步的 daemon 没重启干净，
   旧进程还在按旧契约干活，回去重做第 3 步。

   具体原因（`the host does not declare the resume_append action`）写在卡片 `detail`
   里。文本 `list` 只打 id/状态/workdir，**不带** detail；要看就读状态目录：

   ```bash
   grep -o '"detail":"[^"]*"' \
     '<项目根>/.yi-agent/superpowers-kanban/board.json'
   ```

   （桌面看板把同一份 detail 显示在卡片上。）

为什么要这个顺序：

- **新插件 + 旧宿主**：安全。插件发现宿主没声明 `resume_append`，会一直 defer
  合并，卡停在 `merge_queued`，不做任何有损的事。
- **旧插件 + 新宿主**：危险。旧插件不懂"回到原会话"，仍会 `launch` 一个新会话去合并，
  正是本版要修的行为。所以**不能**让旧插件跑在新宿主上。

### 9.2 降级前必须先排空

降级（装回旧版插件）前：

```bash
superpowers-kanban list        # 末尾不应有 merge attempt 行
superpowers-kanban list | grep '\[merging\]'   # 不应有输出
```

有在飞的合并就等它跑完，或用 `merge-cancel` 把**还在队列里**的卡退出来
（已开跑的不能取消）。确认干净后再降级。

**不要直接删 `merge.attempt.json` 来"清干净"。** 那条记录是安全证据：它证明这一轮
可能已经把指令交给了某个会话。删掉之后 daemon 会认为没有在飞的合并，可能再派一次，
造成同一张卡并发合并。要清就走 9.3 的 `merge-finish --recover`。

### 9.3 卡在 `merge attempt` 上时的人工恢复

`list` 打印的那行给出了精确的 `action_id`：

```
merge attempt act-9f3a-7 phase accepted for alpha-spec-alpha-plan (thread 6f2c…) @ /…/card
```

确认**真的没有东西在跑**之后再执行：

1. **真正停掉插件**——只 `pkill` 不够：监督器会在 `restart_backoff_ms` 后把它重新拉起，
   而恢复要求 `plugin.lock` 空闲。先摘清单，再确认进程没了：
   ```bash
   rm -f <项目根>/.yi-agent/supervisors/superpowers-kanban.json   # 监督器 500ms 内停掉它
   sleep 1
   pgrep -fl "superpowers-kanban run"    # 必须没有输出
   ```
   恢复完再按第 5 步把清单放回去（`superpowers-kanban manifest` 打出来的那份）。
2. 自己确认相关进程都停了（模型回合、工具子进程、`git`）、Git 状态符合你的判断
   （`git log` / `git status`）。这一步**只能由人做**，命令不会替你验证。
3. 如果卡还写着 `running`，先替它判定结果——恢复命令会拒绝 `running` 的卡，
   它不替你决定合并成没成：
   ```bash
   superpowers-kanban state <card-id> done        # 合并确实落了
   superpowers-kanban state <card-id> needs_you   # 没落，留给人处理
   ```
4. 跑离线恢复，把上面那行的 `action_id` 原样填进去：
   ```bash
   superpowers-kanban merge-finish <card-id> \
     --recover --attempt <action-id> --ack-stopped \
     --state-dir '<项目根>/.yi-agent/superpowers-kanban'
   ```
   - `--ack-stopped` 是**必填**的：它表示"我已经确认停下来了"。少了它命令直接拒绝。
   - `--state-dir` 请写**绝对路径**。命令本身不校验，写相对路径就按你的当前目录解析，
     很可能指到另一个目录的看板上去。
   - 命令会依次拿到 `plugin.lock`、非阻塞的 `merge.lock`、`board.lock`，再核对
     `card_id` + `action_id` 都吻合才清记录；对不上就拒绝，不会误清别的合并。
     `plugin.lock` 被占（插件或 daemon 还在跑）或被 `merge.lock` 挡住时同样拒绝。
   - 卡仍是 `running` 时**拒绝**——先用 `state <card-id> done` 或 `needs_you` 判定它。
   - 成功输出 `recovered` 并删掉记录、清 holder；没有任何记录时报
     `there is no merge attempt record to recover`。
5. 恢复插件：把第 1 步摘掉的清单放回去，daemon 会在 500ms 内拉起它。
   ```bash
   pgrep -fl "superpowers-kanban run"
   ```
   没有输出就检查清单路径与开关（第 4、5 节）。

> 这条命令**只记录你做过的确认**，它不证明任何进程真的停了。"停没停"是你的判断，
> 不是程序的验证。别把它当成自动清障开关。

### 9.4 不能自动恢复的三种情形

遇到就别指望 daemon 修好，按下面处理：

| 情形 | 表现 | 怎么办 |
|---|---|---|
| **meta-only**：卡片记着 `thread_id`，但那份会话历史已经没了 | 每拍都 deferred / `needs_you`，重复同一句原因 | 人工判断合并是否还需要做；需要就在普通会话里自己合并，再 `state <card-id> done` |
| **旧版覆写过的 `thread_id`**：升级前那个 id 指向的是旧版新开的合并会话，不是开发会话 | 能恢复，但恢复出来的是"那轮合并"，上下文不对 | 不要把结果当开发上下文用；按 9.3 清掉安全记录后人工处理 |
| **开发 worktree 被删** | `needs_you`，reason 提到 `workdir` 不存在 | 恢复不了原树就人工合并；`git worktree add` 重建同名树**不能**救回会话历史 |

通用原则：**不要新开一张卡或新开会话"重跑一遍合并"**。那会绕过 `merge.lock` 的
单合并约束，也可能把已经合进去的东西再合一次。

### 9.5 安全记录坏了

`list` 打出 `merge attempt UNREADABLE: …` 说明 `merge.attempt.json` 解析不了。
这**不是**"没有记录"，daemon 会因此停下所有合并。处理：

1. 停插件（同上）。
2. 备份坏文件，**不要**直接删：
   ```bash
   cp <state-dir>/merge.attempt.json <state-dir>/merge.attempt.json.broken-$(date +%s)
   ```
3. 用 `git log` / `git status` 人工核对目标分支与卡片状态，判定这一轮到底做没做。
4. 判定结果写进卡片（`state <card-id> done` 或 `needs_you`），再由你决定是否移走
   `<state-dir>/merge.attempt.json`。
5. 重新拉起插件。

> 从备份恢复时优先用你**自己**的备份目录/Time Machine，而不是删掉重来。

### 9.6 升级 skill 后，热会话仍可能记得旧做法

skill 是文件副本，已开的会话不会重新读取。升级后**只对新会话生效**；一个正在
合作的旧会话可能还按"新开合并会话"的旧说法回答。遇到这种情况先开新会话，
不要顺着旧说法去做。

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
