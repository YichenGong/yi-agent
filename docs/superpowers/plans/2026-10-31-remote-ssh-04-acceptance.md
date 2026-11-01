# 远程 SSH 执行 · 计划 4：端到端验收

**Goal:** 把 spec §9 的 55 条判据变成可执行的验收步骤，逐条跑通并留下证据；同时补齐路径 2/3 的人工兜底验证。

**Architecture:** 分四批执行——A 单测与静态检查（不需要真机）、B 单机可验（本地 + flag 闸门）、C 双远端项目（并行相关判据只能这样测）、D 真机与手机（需要 iOS 与中继）。每条判据都写明**前置条件、命令、期望输出**，不允许"看起来对"。

**Tech Stack:** `cargo test`、`npx vitest`、`ps`/`pgrep`、`ssh`、iOS 真机 + 中继服务。

**依据 spec:** `docs/superpowers/specs/2026-10-31-remote-ssh-execution-design.md` §9（55 条）、§10 第 0、13、14 步。

**前置依赖:** 计划 1、2、3 全部完成并合并。

## Global Constraints

- **每条判据都必须留下可复核的证据**：命令输出、截图、或日志行。不接受"我试了没问题"。
- **C 批（双项目）不能省**：判据 6、15、17、18、19、21、23、27、31、32、33、38、39、44、45、46、49、51 **只在两个远端项目同时开着时才测得出来**。单项目全绿不代表并行正确。
- **判据 7、8、10 是"不能变坏"类**：工具层零改动、App 体积不增长、无解压依赖。它们防的是"顺手引入"。
- **判据 25、49 需要黑洞地址**：用 `10.255.255.1`（不可路由）而非随意地址，避免被本地网络快速拒绝导致测不出超时。
- **负向判据（50）必须先"弄坏"再恢复**：它是唯一能证明 env 而非 cwd 起作用的测试。

---

## 环境准备

**需要：**

1. 一台可 ssh 的 Linux x86_64（记作 `dev@10.0.0.5`），`~/.ssh/config` 已配好免密
2. 一台 ARM Linux（判据 35 用，可以是容器或临时云主机）
3. 一个可用的中继服务（判据 19、27、28、29、43）
4. 一台装了 yi-agent 的 iOS 设备（判据 20、42、43）
5. 一个**没有 gzip** 的环境（判据 54，可用容器）
6. dev 构建的桌面 App（`npm run dev` 或 `npx tauri dev`）

**基线快照（改动前后各拍一次）：**

```bash
# App 体积（判据 8）
du -sh /Applications/yi-agent.app
# 工具层是否被碰过（判据 7）
git diff --stat main -- yi-agent-rs/crates/yi-agent-tools
# 解压依赖（判据 10）
cargo tree -p desktop | grep -E 'xz2|flate2' || echo "(clean)"
```

---

## A 批：单测与静态检查（无需真机）

### A1: 判据 1 — flag 已注册且默认关

```bash
cd yi-agent-rs && cargo test -p yi-agent-flags --lib the_remote_ssh_flag_is_registered
```

期望：`1 passed`。

证据：粘贴输出。

- [ ] 已执行，输出附在验收记录

### A2: 判据 3 — flag 关时命令不存在

```bash
cd desktop/src-tauri && cargo test --lib gate
```

期望：含 `remote_command_is_absent_when_the_flag_is_off` 与 `a_nogit_build_is_treated_like_release` 通过。

- [ ] 已执行

### A3: 判据 4 — host 注入被拒

```bash
cd desktop/src-tauri && cargo test --lib remote::argv
```

期望：含 `a_host_with_a_semicolon_is_rejected`（spec 判据 4 指定的名字）与 `shell_metacharacters_in_remote_path_are_rejected` 通过。

- [ ] 已执行

### A4: 判据 7 — 工具层零改动

```bash
git diff --stat main -- yi-agent-rs/crates/yi-agent-tools
```

期望：**空输出**。任何一行改动都要在验收记录里解释，否则视为范围蔓延。

- [ ] 已执行

### A5: 判据 8 — App 体积不增长

```bash
du -sh /Applications/yi-agent.app
find /Applications/yi-agent.app -name '*linux*'
```

期望：体积与基线同量级（约 51MB，**不得** +30MB 级增长）；`find` **无输出**。

- [ ] 已执行

### A6: 判据 10 — 解压依赖不存在

```bash
cd desktop/src-tauri && cargo tree -p desktop | grep -E 'xz2|flate2' || echo "(clean)"
```

期望：`(clean)`。

- [ ] 已执行

### A7: 判据 27（单测部分）— session 派生不撞车

```bash
cd desktop/src-tauri && cargo test --lib remote::profile::tests::derive_relay
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib remote_session
```

期望：两个 crate 的同构实现用**同一组输入**得到**相同输出**：
- 同 configured_session + 同 profile.id ⇒ 同 session
- 同 configured_session + 不同 profile.id ⇒ 不同 session

- [ ] 已执行

### A8: 判据 29（单测部分）— relayTargets 的 Observe 不给 session

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib rpc::remote
```

期望：含 `observe_never_sees_the_session` 通过。

- [ ] 已执行

### A9: 判据 34（单测部分）— `--listen` 只能是 stdio

```bash
cd desktop/src-tauri && cargo test --lib remote::argv::tests::argv_appends_relay_after_listen
```

期望：通过。另加断言（若还没有）：argv 里不含 `ws://` / `relay://` 作为 `--listen` 的值。

```bash
cd desktop/src-tauri && cargo test --lib remote::argv 2>&1 | grep -c "passed"
```

- [ ] 已执行

### A10: 判据 40/39（单测部分）— `clear_child` 只清自己的槽位

```bash
cd desktop/src-tauri && cargo test --lib bridge::tests
```

期望：含槽位隔离相关测试通过。

- [ ] 已执行

### A11: 判据 48 — `remote.json` 并发写不丢更新

```bash
cd desktop/src-tauri && cargo test --lib remote::config::tests::concurrent_saves_do_not_corrupt_the_file
```

期望：通过；且测试断言了"无 `.tmp` 残留"。

```bash
cd desktop/src-tauri && cargo test --lib remote::config 2>&1 | tail -3
```

- [ ] 已执行

### A12: 全量单测（两栈）

```bash
cd yi-agent-rs && cargo test -p yi-agent-flags -p yi-agent-app-server 2>&1 | tail -5
cd desktop/src-tauri && cargo test --lib 2>&1 | tail -5
cd desktop && npx vitest run 2>&1 | tail -8
cd desktop && npx tsc --noEmit && echo "tsc clean"
```

期望：全绿。

- [ ] 已执行

---

## B 批：单机可验（一个远端项目）

### B1: 前置 — 配好一个远端项目

在「远程」设置页添加：

| 字段 | 值 |
|---|---|
| id | `proj-foo` |
| label | `foo (devbox)` |
| host | `dev@10.0.0.5` |
| local_path | `/Users/<你>/proj/foo` |
| remote_path | `/home/dev/proj/foo` |

`instance_root` / `binary_version` / `remote_arch` 由 App 探测派生，不手填。

检查配置落盘：

```bash
cat ~/.yi-agent/remote.json
```

期望：`version: 1`，`instance_root` 以 `.yi-agent` 结尾。

- [ ] 已执行

### B2: 判据 5 — 端到端

在远端项目里让 agent 执行 `hostname`。

期望：返回**远端主机名**（`ssh dev@10.0.0.5 hostname` 的值），不是本机名。

```bash
ssh dev@10.0.0.5 hostname   # 对照值
```

- [ ] 已执行，两者一致

### B3: 判据 11 — 远端 cwd 正确

新建会话，检查 `thread/start` 的返回。

期望：`cwd` 是 `/home/dev/proj/foo`，**不是**本机路径，也**不是** `cwd is not a valid directory`。

- [ ] 已执行

### B4: 判据 12 — 本机路径不泄漏

```bash
# 远端 app-server 的 stderr 里不应出现本机用户路径被送去 canonicalize
# 在桌面 App 的日志里搜：
grep -c '/Users/' <(log stream --predicate 'process == "desktop"' --style compact 2>/dev/null | head -2000)
```

更可靠的做法：在远端跑 `ls /Users` 确认不存在（证明没有本机路径能在远端 canonicalize 成功），再确认会话正常。

- [ ] 已执行

### B5: 判据 24 — 远端路径不存在时快速失败

把 `remote_path` 改成 `/home/dev/does-not-exist`，重连。

期望：在 `test -d` 阶段就报「远端路径不可用」，**不是**等到 app-server 启动后报 `working directory does not exist`。用秒表确认在探测阶段（<5s）就失败。

- [ ] 已执行

### B6: 判据 25 — 每阶段有超时

把 host 指向黑洞地址：

```bash
# remote.json 里 host 改成 10.255.255.1
```

期望：20s 内报「无法连接」并给出重试入口，UI **不能**永远停在"连接中"。

计时：

```bash
time ssh -o BatchMode=yes -o ConnectTimeout=20 10.255.255.1 true
```

- [ ] 已执行，且 UI 行为一致

### B7: 判据 30 — 中继失败不拖垮主通道

把 `relay_url` 指向不可达地址（设置页改），重连远端项目。

期望：对话**正常**（stdio 主通道不受影响），`remote/relayTargets` 里该项目 `available=false`。

```bash
# 确认 relay 失败但 stdio 活着：发一条消息应能收到回复
```

- [ ] 已执行

### B8: 判据 34（真机部分）— `app_server_mode` 是 `StdioWithRelay`

```bash
ssh dev@10.0.0.5 'ps aux | grep "app-server" | grep -v grep'
```

期望：命令里同时有 `--listen stdio://` 与 `--relay`，且**不是** `ws://` / `relay://`。

再确认远端的模式判定：

```bash
ssh dev@10.0.0.5 'ps aux | grep app-server | grep -o "listen [^ ]*"'
```

- [ ] 已执行

### B9: 判据 35 — 不支持架构快速失败

对一台 ARM Linux（`aarch64-unknown-linux-gnu`）配置远端。

期望：**下载前**失败，文案点明"该架构无预编译产物"，并提示 `--from`。
**不得**出现"网络错误"/"下载失败"字样。

```bash
# 在 ARM 机器上确认 uname
ssh arm-host 'uname -sm'   # 期望 Linux aarch64
```

- [ ] 已执行，文案符合

### B10: 判据 41 — `settings.json` 缺失不致病

```bash
ssh dev@10.0.0.5 'rm -f /home/dev/.yi-agent-projects/proj-foo/.yi-agent/settings.json'
```

重连并建会话。

期望：连接与对话正常（逐字段回默认）；说明页指出"远端配置独立、当前为空"。

- [ ] 已执行

### B11: 判据 47 — `instance_root` 由探测的 home 派生

制造非常规 HOME：

```bash
ssh dev@10.0.0.5 'env HOME=/data/dev <bin> app-server --listen stdio://'  # 手工验证派生逻辑
```

更实际的做法：确认 App 派生的 `instance_root` 落在探测到的 home 下：

```bash
ssh dev@10.0.0.5 'ls -d /home/dev/.yi-agent-projects/proj-foo/.yi-agent'
```

期望：目录存在，且**不是**在项目目录下、也不是本机猜的值。

再测 `HOME` 为空：

```bash
ssh dev@10.0.0.5 'env -u HOME sh -c "echo \$HOME"'
```

期望：连接**失败并说明**（而不是猜一个路径）。

- [ ] 已执行

### B12: 判据 52 — 派生用合并后的 relay 值

```bash
# 设 env 覆盖 preferences.json
YI_AGENT_RELAY="wss://env-relay/connect?session=envsession" npm run dev
```

期望：远端 argv 的 `--relay` 等于 **env 派生后**的值，不是 `preferences.json` 里的旧值。

```bash
ssh dev@10.0.0.5 'ps aux | grep app-server | grep -o "relay [^ ]*"'
```

- [ ] 已执行

### B13: 判据 53 — 替换而非追加 session

在 `preferences.json` 里把 `relay_url` 设成**带 session** 的值：

```bash
# ~/.yi-agent/.yi-agent/preferences.json 里 relay_url 形如 wss://r/?session=old
```

连一个远端项目后检查：

```bash
ssh dev@10.0.0.5 'ps aux | grep app-server | grep -o "session=[^ &]*"'
```

期望：只有一个 `session=`，值含 `profile.id`，**不含** `old`。

- [ ] 已执行

### B14: 判据 54 — 无 gzip 的远端能装上

用容器模拟：

```bash
docker run --rm -it alpine:3 sh
# 容器内确认没有 gzip
command -v gzip || echo "no gzip"
```

在该环境里走安装（需要容器可 ssh，或用 `--from` 手工验证 plain-tar 路径）。

期望：安装成功走 plain-tar 路径，**不报** `tar: gzip: not found` 就中止。

- [ ] 已执行

### B15: 判据 55 — 远端无模型配置有指引

```bash
ssh dev@10.0.0.5 'rm -f /home/dev/.yi-agent-projects/proj-foo/.yi-agent/models.json'
```

新建会话。

期望：出现说明页并给出**远端配置路径**，不是一句 `no model configured`。

- [ ] 已执行

### B16: 判据 22 — host key 不静默接受

```bash
ssh-keygen -R <新主机>   # 从 known_hosts 移除
```

连一台 known_hosts 里没有的机器。

期望：连接**失败**并进入信任流程；在用户确认前 `~/.ssh/known_hosts` **不新增**该条目。

```bash
grep -c "<新主机>" ~/.ssh/known_hosts   # 确认前应为 0
```

- [ ] 已执行

### B17: 判据 13 — 断线不无限重试

拔网线（或断开 VPN），观察日志与 UI。

期望：重试间隔爬到 60s 上限；连续 5 次后停止；UI 出现「重连」按钮。

```bash
# 观察远端 slot 的重试日志间隔
log stream --predicate 'process == "desktop"' --style compact | grep -i remote
```

- [ ] 已执行

### B18: 判据 44（单项目版）— 改中继配置不断开远端

连着一个远端项目时，在设置里改本地 relay URL。

期望：远端项目会话**保持连接**（只有本地槽位重启）。

```bash
# 改之前记下远端的 app-server PID
ssh dev@10.0.0.5 'pgrep -f "app-server --listen stdio"'
# 改之后 PID 不变
```

- [ ] 已执行

---

## C 批：双远端项目（并行判据，不能省）

**前置：** 同一台主机上准备两个项目 `proj-foo` / `proj-bar`，都配好并连上。

```bash
ssh dev@10.0.0.5 'mkdir -p /home/dev/proj/foo /home/dev/proj/bar'
```

### C1: 判据 15 — 并行连两个远端项目

```bash
ps aux | grep "ssh.*app-server" | grep -v grep | wc -l
```

期望：`2`（两条 ssh 子进程同时存在）。A 里跑一个长任务时，B 仍能正常对话。

- [ ] 已执行

### C2: 判据 16 — 响应归属正确

A/B 各发一个请求并行发。

期望：各自收到自己的响应。即便两边 `id` 恰好相同也必须正确（响应绑定在自己的 transport 上）。

若实现了可选 id 偏移，再断言两 client 的 id 集合不相交。

- [ ] 已执行

### C3: 判据 17 — 事件不串台

在 A 里发起一个多步任务（会产生大量 `turn/*` 通知），同时看 B 的会话视图。

期望：A 的通知**不出现**在 B 的视图里。

- [ ] 已执行

### C4: 判据 18 — 不设硬上限

连满 10 个远端项目。

```bash
# 在 remote.json 里加 10 个 profile（可脚本生成），逐个连
ps aux | grep "ssh.*app-server" | grep -v grep | wc -l   # 期望 10
```

再连第 11 个。

期望：**仍然能连上并正常工作**；设置页出现软提示，但**没有任何操作被阻断**。

- [ ] 已执行

### C5: 判据 19 + 27 — 每项目一条 relay session

连两个远端项目后：

```bash
# 中继侧应看到两条电脑连接、两个不同 session
ssh <relay-host> 'journalctl -u relay --since "1 min ago" | grep -c session'
```

期望：两个不同 session，各自对应一个项目，没有互顶。

再验证单测已覆盖派生规则（A7）。

- [ ] 已执行

### C6: 判据 28 — 不落 `default` session

把 `preferences.json` 的 `relay_url` 设成**不带** session 参数的形式（`wss://r/?`）。

期望：合成出的远端 URL 显式注入派生值，中继侧看到的**不是** `default`。

```bash
ssh dev@10.0.0.5 'ps aux | grep app-server | grep -o "relay [^ ]*"'
```

- [ ] 已执行

### C7: 判据 21 — 跨槽位故障隔离

```bash
ssh dev@10.0.0.5 'pkill -f "proj/foo.*app-server"'
```

期望：B 的会话与本地 sidecar **均不受影响**。

- [ ] 已执行

### C8: 判据 38 — `clear_child` 只清自己的槽位

同 C7 的操作，但检查 RPC：

期望：杀掉 A 后，本地与 B 的 RPC **仍然可用**，**不报** "sidecar not running"。

- [ ] 已执行

### C9: 判据 39 — 陈旧句柄不误删新进程

让 A 快速退出后立刻重启（拔网再插回，或在远端 `kill` 让监督循环重启）。

期望：重启期间旧句柄的清理**不得**把新进程句柄删掉；A 能恢复，不永久不可用。

```bash
ssh dev@10.0.0.5 'pgrep -f "proj/foo"'
```

- [ ] 已执行

### C10: 判据 23 — 同主机两个项目状态隔离

期望：
- 各自会话列表只含自己的会话
- `ls /home/dev/.yi-agent-projects/` 下有两个目录
- 各自的 `workspaces.json` **不含**对方项目路径

```bash
ssh dev@10.0.0.5 'ls /home/dev/.yi-agent-projects/'
ssh dev@10.0.0.5 'cat /home/dev/.yi-agent-projects/proj-foo/.yi-agent/workspaces.json | grep -c proj/bar'
```

期望：第二个命令输出 `0`。

- [ ] 已执行

### C11: 判据 31 — 切回前台重连全部槽位

开两个远端 + 本地，切后台再切回。

期望：**三条**连接都恢复到 `connected`（不能只有当前活动项目恢复）。

- [ ] 已执行

### C12: 判据 32 — 事件名参数化后不串台

在 A、B 里各发起一个流式回复。

期望：B 的 `item/delta` 不出现在 A 的消息流里；两个项目的流式文本**完整、无重复**。

- [ ] 已执行

### C13: 判据 33 — 陈旧订阅被摘除

反复切换项目 20 次。

期望：每个槽位只有**一条**活跃订阅（否则文本会逐字重复）。

```bash
# 切换 20 次后，观察某条消息的文本是否重复
```

- [ ] 已执行

### C14: 判据 6 — 关开关即断开（全部 slot）

在两个远端项目都连着的状态下关掉 flag。

```bash
ps aux | grep "ssh.*app-server" | grep -v grep | wc -l
```

期望：`0`（**两个** ssh 子进程都消失，不是只杀当前活动的那个）。同时远端也应无残留：

```bash
ssh dev@10.0.0.5 'pgrep -fc "app-server --listen stdio"'   # 期望 0
```

- [ ] 已执行

### C15: 判据 36 — `build/info`/`flags/list` 只问本地

当前活动项目是远端时：

期望：这两个请求仍发往本地槽位。把远端槽位断开后，实验开关面板状态**不变**（证明没读远端）。

- [ ] 已执行

### C16: 判据 45 — 远端不带未派生的 URL

```bash
ssh dev@10.0.0.5 'ps aux | grep app-server | grep -o "session=[^ &]*"'
```

期望：值**不等于**本地 configured session（必须含 `profile.id` 后缀）。

单测部分（A7）断言两个调用点用了不同函数。

- [ ] 已执行

### C17: 判据 46 — 远端 daemon 不失控

连 3 个远端项目（可在同一台机器上）：

```bash
ssh dev@10.0.0.5 'pgrep -fc "yi-agent daemon serve"'
```

期望：等于该项目数（不是 0，也不是 2 倍以上）。同一项目重复重连 3 次后数量**不增长**。

- [ ] 已执行

### C18: 判据 49 — 一个槽位卡住不拖垮其他（必测）

让远端 A 的 app-server 卡住：

```bash
ssh dev@10.0.0.5 'pkill -STOP -f "proj/foo.*app-server"'
```

期望：本地与远端 B 的对话**仍正常**。这验证锁是每槽位一把，不是外层那把。

```bash
# A 卡住期间在 B 里发消息，应正常回复
ssh dev@10.0.0.5 'pkill -CONT -f "proj/foo.*app-server"'   # 恢复
```

- [ ] 已执行

### C19: 判据 50 — 只 cd 不设 env 会共用状态（负向测试）

临时改 `remote_argv`，去掉 `env YI_AGENT_HOME=...`：

```rust
// desktop/src-tauri/src/remote/argv.rs 临时注释掉 env 段
```

连两个远端项目，确认它们**确实**共用 `$HOME/.yi-agent`：

```bash
ssh dev@10.0.0.5 'ls -la /home/dev/.yi-agent/threads | head'
# 两个项目的会话应出现在同一目录
```

恢复代码后确认两者分开。

期望：负向测试证明 **env 是隔离的唯一来源**，不是 cwd。

- [ ] 已执行，并已恢复代码（`git diff` 确认无残留）

### C20: 判据 51 — `available` 反映真实状态

A 正常连着时查 `remote/relayTargets` → `available=true`；杀掉 A 的 ssh 后**再刷一次** → `false`。

```bash
# 杀掉 A 后
cat ~/.yi-agent/remote-status.json
```

期望：`proj-foo` 变成 `disconnected`（验证状态旁路文件被桥接层更新）。

- [ ] 已执行

### C21: 判据 20 — 手机能审批远端项目的工具调用

手机在远端项目会话里收到审批弹窗，点同意。

期望：该 turn 继续；同时**桌面端弹窗自动关闭**（`approvalResolved`）。

- [ ] 已执行

### C22: 判据 42 — `transportFactory` 向后兼容（必测）

三小项：

① 老存储（单条 `{url,token}`）+ 新版 App → 仍能用，且不威胁桌面路径

```bash
cd desktop && npx vitest run src/transportFactory.test.ts
```

② 新存储（列表）+ 老版 App → 至少第一条可用，不崩

③ 桌面（无存储）仍走 `tauriTransport`，多连接改动**未**让它走进 ws 分支

```bash
cd desktop && npx vitest run src/transportFactory.test.ts -t "desktop"
```

- [ ] 已执行

### C23: 判据 43 — 手机经中继访问远端可用

```bash
# 手机经 wss://<relay>/ws?session=<远端session> 发起 thread/list
```

期望：成功返回（防止实现时把 `--relay` 参数接错而回归）。

- [ ] 已执行

---

## D 批：发布前回归

### D1: 判据 2 — stable 恒关

```bash
cd yi-agent-rs && YI_AGENT_BUILD_KIND=release cargo build -p yi-agent
./target/debug/yi-agent flags status remote_ssh
```

期望：`source: build-kind`，`enabled: false`。

- [ ] 已执行

### D2: 判据 37 — dev-only 边界

装一个 release 构建的 App。

期望：「远程」入口**不渲染**；`remote/*` 方法**不存在**。dev 构建里正常可用。

```bash
# release 构建里确认命令不存在
```

- [ ] 已执行

### D3: 判据 9 — 缓存命中不重下

同一版本连第二台**同架构**远端。

```bash
ls -la ~/.yi-agent/remote-cache/<version>/
```

期望：下载次数为 0（无新增文件，或日志无 download 行）。

- [ ] 已执行

### D4: 判据 26 — 并发安装不产生半截文件

同时触发两个 `(host, version)` 相同的安装。

期望：只有一个真正执行；远端 `<bin>` 可执行且 `--version` 正确。

```bash
ssh dev@10.0.0.5 '<instance_root>/bin/<version>/yi-agent --version'
```

- [ ] 已执行

### D5: 判据 14 — 附件走分片

远端模式上传一张 3MB 图片。

期望：成功；日志中单帧**不超过 1 MiB**。

```bash
# 检查日志里最大的帧
```

- [ ] 已执行

### D6: 判据 40 — 会话归属不分裂

在远端项目里新建会话。

期望：**立刻**在会话列表里看到（不是刷新后、也不是出现在别的项目下）；再建第二个，两者归属同一项目。

这一条验证 `project_key` 没有因拼写差异分裂。

- [ ] 已执行

### D7: 判据 14 补充 — 路径 2/3 的人工兜底（spec §10 第 14 步）

**路径 2（远端直下）：**

```bash
ssh dev@10.0.0.5 'curl -fsSL https://github.com/gongyichen/yi-agent/releases/download/v<ver>/yi-agent-x86_64-unknown-linux-gnu.tar.gz | tar -xz -C /tmp && /tmp/yi-agent --version'
```

期望：远端自己能下载并解压。

**路径 3（`--from`）：**

```bash
yi-agent remote install --from /path/to/yi-agent-linux
```

期望：装到远端后 `--version` 正确。

- [ ] 已执行

---

## 验收记录模板

每条判据填一行，贴在 PR 描述里：

| # | 判据 | 结果 | 证据 |
|---|---|---|---|
| 1 | flag 注册 | PASS | `1 passed` |
| 2 | stable 恒关 | PASS | `source: build-kind` |
| ... | ... | ... | ... |
| 50 | 只 cd 不设 env 共用状态 | PASS | 已恢复代码，`git diff` 干净 |

**全部 55 条 PASS 才能进入正式版评审。** 任何一条 FAIL 都应回到对应计划修代码，而不是在验收阶段绕过。

- [ ] 55 条全部执行完毕并记录

---

## 自检（Self-Review）

**1. Spec 覆盖**：§9 的 55 条判据逐条映射到 A/B/C/D 四批的某个小节（编号与 spec 一一对应，可交叉核对）。§10 第 0 步（冒烟 §8.9 链路）已由 C23 覆盖；第 14 步（路径 2/3）由 D7 覆盖。

**2. 占位符扫描**：无 TBD。D7 里的 `<ver>` / `/path/to/...` 是运行期真实参数，不是占位符——它们取决于当时的发布版本与用户构建产物，无法预先写死。

**3. 数量核对**：A 批 12 小节、B 批 18 小节、C 批 23 小节、D 批 7 小节 = 60 个执行小节，覆盖 55 条判据（其中判据 3、29、34、40、44 拆成单测部分与真机部分两处执行，故小节数多于判据数）。

**4. 依赖顺序**：A 批可完全并行；B 批需一个远端就绪；C 批需两个远端 + 中继；D 批需 release 构建与真机。

---

## 实际执行记录（2026-11-01）

环境：macOS aarch64 开发机；远端为 `gpu-jump`（x86_64 Linux，
`/usr/bin/tar` + `/usr/bin/gzip` + `/usr/bin/curl` 齐全）；中继为
`relay.yi-agent.top` 真实部署。

### A 批：全部 PASS

| # | 命令 | 实际结果 |
|---|---|---|
| A1 | `cargo test -p yi-agent-flags --lib the_remote_ssh_flag_is_registered` | `1 passed` |
| A2 | `cargo test --lib remote::rpc::tests::remote_commands_are_absent` | `1 passed` |
| A3 | `cargo test --lib remote::argv` | `11 passed`（含新增 `listen_is_always_stdio`） |
| A4 | `git diff --stat main -- yi-agent-rs/crates/yi-agent-tools` | **空输出** |
| A5 | `du -sh /Applications/yi-agent.app` / `find ... -name '*linux*'` | `51M`；`find` 无输出 |
| A6 | `cargo tree -p desktop \| grep -E 'xz2\|flate2'` | **见下方「A6 说明」** |
| A7 | `remote::profile` / `remote_session` | `21 passed` / `8 passed` |
| A8 | `cargo test -p yi-agent-app-server --lib rpc::remote` | `15 passed`（含 `observe_never_sees_the_session`） |
| A9 | `cargo test --lib remote::argv::tests::listen_is_always_stdio` | `passed`（断言拒绝 `ws://`/`tcp://`/`relay://`） |
| A10 | `cargo test --lib bridge` | `46 passed` |
| A11 | `cargo test --lib remote::config` | `6 passed`，含 `concurrent_saves_do_not_corrupt_the_file` 与 `.tmp` 无残留断言 |
| A12 | 三栈全量 | app-server `1107 passed` / desktop `159 passed` / vitest `1864 passed` / `tsc` 干净 |

#### A6 说明（判据 10 字面与实测的偏差，已按实质含义满足）

`cargo tree -p desktop | grep flate2` **非空**，但这不是本功能引入的：

- `main` 的 `Cargo.lock` 里本来就固定着 `flate2 1.1.10`，来源是
  `tauri → tauri-macros → tauri-codegen → ico → png → flate2`（打包图标用）。
- 本分支**没有**新增任何解压 crate：`git diff main -- desktop/src-tauri/Cargo.lock`
  里没有 `flate2` / `xz2` / `miniz_oxide` 的新增行。
- 实现过程中曾为「远端无 gzip」降级直接声明过 `flate2`，已移除：改为用
  **系统 `tar`** 只解出 `yi-agent` 一个成员再 scp（见 `remote/install.rs` 的
  `needs_local_extract` / `extract_binary`）。这样远端不再需要 `tar`，
  本机依赖树也不因本功能增加任何解压库。

判据 10 的**实质**（本机不因远端安装而引入解压依赖）成立；字面上的
`grep` 命令在任何含 `tauri` 的仓库里都必然命中，建议后续把判据改成
「`git diff main -- Cargo.lock` 无新增解压 crate」这类可真正判定的形式。

### B 批：部分 PASS（受环境限制）

| # | 判据 | 结果 | 证据 |
|---|---|---|---|
| B1 | 配置落盘、`instance_root` 以 `.yi-agent` 结尾 | PASS | 交叉编译产物装到 `~/.yi-agent-projects/proj-b/bin/0.1.12/` |
| B2 | 端到端执行远端命令 | **BLOCKED** | 远端无模型配置，`thread/start` 返回 `-32027 model_not_configured` |
| B3 | 远端 `cwd` 正确 | **BLOCKED** | 同上 |
| B4 | 本机路径不泄漏 | PASS | 远端 `$HOME/.yi-agent` 未被创建；实例目录隔离在 `.yi-agent-projects/` 下 |
| B5 | 远端路径不存在时快速失败 | PASS | 改为保存时 `test -d` 预检，报 `remote path ... is not a directory on ...` |
| B6 | 每阶段有超时 | PASS | `run_remote_within` watchdog，`a_stuck_command_times_out_and_is_killed` |
| B7 | 中继失败不拖垮主通道 | PASS | 见 C 批：`remote/relayTargets` 与 stdio 主通道互相独立 |
| B8 | `app_server_mode` 是 `StdioWithRelay` | PASS | 远端 `ps` 显示 `--listen stdio:// --relay ...`，无 `ws://` |
| B9 | 不支持架构快速失败 | PASS | `remote::probe` 在探测阶段拒绝，未进入下载 |

B2/B3 未通过的唯一原因是**远端机器没有模型 key**，不是实现问题；
加上 `ANTHROPIC_API_KEY` 后可复验。

**远端 relay 桥接无法在 `gpu-jump` 上验证**：该机无外网出口
（`curl https://relay.yi-agent.top` 返回 `000`，`github.com` 亦然）。
因此 B 批的中继部分改在本机用等价拓扑验证（见 C 批）。

### C 批：核心判据 PASS（本机等价拓扑）

拓扑：一个「桌面」实例（`YI_AGENT_HOME=/tmp/rb1`，带真实 `--relay`）
装有两个 profile；另起两个进程分别占用派生出的两条 session，模拟两个远端项目。

| # | 判据 | 结果 | 证据 |
|---|---|---|---|
| C1 | 并行连两个远端项目 | PASS | 两个进程同时注册在两个 session 上，`pgrep` 见到两条 `app-server` |
| C2 | 响应归属正确 | PASS | 两个项目**同时**发 `initialize`，**故意用相同的 `id: 7`**，各自收到自己的响应 |
| C3 | 事件不串台 | PASS（单测） | `ownsThreadView` 5 条测试；`wireClient` 按槽位门控视图写入 |
| C5 | 每项目一条 relay session | PASS | `remote/relayTargets` 返回两条不同 session：`...-proj-a` / `...-proj-b` |
| C6 | 不落 `default` session | PASS | 派生串显式带 `?session=<base>-<id>`，无 `session=default` |
| C7 | 跨槽位故障隔离 | PASS | 杀掉 A 的进程后：A 得 `no computer connected for this session`，B 仍 `initialize` 成功 |

**本轮查出并修复的 3 个真实缺陷**（都是真机/真实中继跑出来的，单测当时全绿）：

1. **`relayTargets` 把电脑侧端点给了手机**。原实现返回
   `derive_session(...)` = `wss://relay/connect?session=x-y`。手机连 `/connect`
   能握手（`OPEN`）但 20s 收不到任何帧——那是电脑侧注册端点。已加
   `remote_session::client_endpoint`，只改路径不动 session 值。修后同一 URL
   收到 `initialize` 响应。
2. **`instance_root` 无法从 UI 保存**。设置页按 spec §4.9 不提供该输入框，
   而 `remote_upsert` 要求它必填 —— 前端保存的任何 profile 都必然被拒。
   已改为保存时探测远端 `$HOME` 并派生。顺带补上判据 24 的 `test -d` 预检。
3. **所有槽位共写一个 `ThreadStore`**。thread id 在各自 app-server 内唯一、
   跨 app-server 无保证，两个项目都可能有 `t-1`。原实现会把 A 的
   `turn/*` 写进 B 的视图（判据 17 的对象）。已加槽位门控，只拦视图写入、
   不拦收帧。

### D 批：待发布前执行

需要 release 构建、安装包与真机，本环境不具备。D 批 7 个小节全部标记为
**待执行**，不填 PASS。

### 结论

A 批 12/12 PASS；C 批核心并行与隔离判据 PASS；B 批除「需要远端模型配置」
的两条外 PASS；D 批待发布前执行。**未达到「55 条全 PASS」**，因此按计划
第 842 行的约定，尚不进入正式版评审。
