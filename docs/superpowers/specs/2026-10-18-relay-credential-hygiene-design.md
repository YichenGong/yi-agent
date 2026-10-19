# 设计：中继凭据卫生 —— 脱敏、轮换与「凭据不进仓库」机械门禁

日期：2026-10-18
状态：待评审
分支：`feat/secret-hygiene`
关系：本篇是 2026-10-17 全模块审查（`.superpowers/review-2026-10-17/`）P0-8 的落地设计。
与 `2026-10-03-shared-session-relay-design.md`（中继按 session 配对、**不鉴权**）互补：
那篇定义了中继的信任模型，本篇处理该模型**已经在仓库里造成的后果**并加机械防护。

## 1. 背景与问题

### 1.1 现象

真实部署的域名、VPS 公网 IP、以及**当作凭据使用的 `session_id`** 出现在 **git 跟踪**的
文档里。全文检索（`git grep`，仅跟踪文件）：

```
$ git grep -n "fe42addbfed257dcd396e7fcc45e2725"
docs/bug-list.md:115:  … 用修复后二进制指向真实中继
   wss://relay.yi-agent.top/connect?session=fe42addbfed257dcd396e7fcc45e2725 …

$ git grep -n "101\.47\.76\.100"
docs/bug-list.md:115   (同上，同一行)
docs/bug-list.md:117   (第二轮 relay 缺陷的排查记录)
docs/relay-deploy.md:573:  lsof … | grep 101.47.76.100
```

引入点可定位到单个提交：

```
$ git log --oneline -S"fe42addbfed257dcd396e7fcc45e2725" --all
c02f1319 fix(relay): 显式安装 rustls provider,修手机连不上中继
```

### 1.2 为什么这是 P0（而不是「文档不整洁」）

1. **`session_id` 本身就是凭据。** 仓库自己的文档写得很清楚：

   - `deploy/relay/README.md:6`：「中继按 `session` 配对转发，**不鉴权**——`session_id`
     本身就是凭据。请把地址与 `session_id` **按秘密保管**」；
   - `docs/relay-deploy.md:208-209`：「**`session_id` 本身就是凭据**——知道它的人
     （在任何网络）就能连上你电脑的会话」；
   - `docs/relay-deploy.md:786`：「实测（2026-10-08）：知道 `session_id` 的人，
     在任何网络都能连上」。

2. **拿到 session 的人不只是"能聊天"。** 中继配对成功后，对端接入的是一个
   app-server，可调用其 RPC 面。其中包含本审查同时发现的 P0-6
   （`thread/diff/read` 的 `commit` 参数直接拼进 `git show`，实测可写任意文件）。
   故凭据泄露的实际后果是**远程文件读写**，而非仅信息泄露。

3. **不可撤回的暴露面。** 凭据已进入 git 历史（`c02f1319`）。仅删除当前文件内容
   不改变历史里仍有该值这一事实；加上本仓有公开镜像（`origin` 指向组织内 GitLab，
   且 GitHub Release 链路存在），必须按"已公开"处置。

### 1.3 两个容易搞错的相邻事实（避免过度反应与错判范围）

**事实一：`deploy/relay/.relay-local.md` 没有泄露。**
它同时含域名、IP、SSH key 路径、session —— 但它**已被 `deploy/relay/.gitignore:6`
忽略、且从未被跟踪**：

```
$ git ls-files deploy/relay/
deploy/relay/.gitignore
deploy/relay/README.md
deploy/relay/install.sh
$ git check-ignore -v deploy/relay/.relay-local.md
deploy/relay/.gitignore:6:.relay-local.md  deploy/relay/.relay-local.md
```

**结论**：`.relay-local.md` 的机制（本机私密记录）是**对的，是本篇要推广的范式**，
不动它。这条规则本身就在 §3.3 的门禁里被天然尊重（门禁只扫 `git ls-files`）。

**事实二：`docs/relay-deploy.md` 已经脱敏过，只有一处漏网。**
该文件 §2.6（`186-210` 行）已明确写着：

> **本地记录，值脱敏。** 真实域名/IP/session **不写进本文件**（本仓库有公开的 GitHub
> 镜像）。真实值写在 `deploy/relay/.relay-local.md`…

并且 57/87/88/202/203 行**本来就用** `relay.example.com` / `<session_id>` /
`<你的域名>`。整份文件里只有 **573 行**（`lsof … | grep 101.47.76.100` 的排查命令）
漏了真实 IP。

**所以本篇要修的跟踪文件泄露点精确为 2 个文件、3 行**：
`docs/bug-list.md:115,117` 与 `docs/relay-deploy.md:573`。范围比初看小得多，
**但这恰好强化了 §3.3 的必要性**——`relay-deploy.md` 自己的 §2.6 已经立了规矩
（"不要提交进公开仓库"），却仍被 `573` 行违反。**规矩写在文档里靠人遵守是无效的**，
这正是审查报告「模式 A：靠人复查」的又一实例。

## 2. 目标与非目标

**目标**

- G1：把跟踪文档里的真实凭据/域名/IP 脱敏（值改为占位符）。
- G2：给出并执行一次**凭据轮换**的操作路径（轮换后旧值作废）。
- G3：加一道**机械门禁**，让"凭据进跟踪文件"在 CI 阶段被挡住——针对的是这一
  类缺陷（审查报告的「模式 A：靠人复查」），而不是只修这一次的两个文件。

**非目标**

- 不改中继的信任模型（不改"不鉴权"这一设计决定）——那需要另一篇设计（见 §7）。
- 不改 `.relay-local.md` 机制。
- 不做 git 历史重写（`filter-repo`）。理由见 §5.4：代价与收益不成比例，
  且轮换已使历史里的值失效。

## 3. 设计

### 3.1 脱敏规则（G1）

统一用**示例值**替换，并在触达处保留"这是示例"的语境：

| 类别 | 现值 | 替换为 |
|---|---|---|
| 域名 | `relay.yi-agent.top` | `relay.example.com` |
| VPS IP | `101.47.76.100` | `203.0.113.10`（RFC 5737 文档保留段） |
| session | `fe42addbfed257dcd396e7fcc45e2725` | `<session_id>` |

`docs/relay-deploy.md` 已有先例：该文件 57/87/88/202/203 行**本来就用**示例值
（`relay.example.com`、`<session_id>` / `<你的 session id>` / `<id>`、`<域名>`），
且 §2.6 明写"值脱敏"。本次是把 573 行（以及 `docs/bug-list.md` 的 115、117 行）
拉齐到同一约定。

**注意**：`docs/bug-list.md` 是**历史台账**，其中的排查记录（含"实测指向真实中继
返回 200"这类结论）是有效的工程信息。脱敏只替换**字面值**，不改写结论——
即「用修复后二进制指向真实中继 `wss://relay.example.com/connect?session=<session_id>`」
这种表述，结论（修复前无连接、修复后有 `agent connected`）原样保留。

### 3.2 轮换路径（G2）

轮换成本极低，因为中继**无服务端状态**：它只按 `session` 配对转发
（`deploy/relay/README.md:4`、`132-133`）。换 session 即换配对键。文档自己已经
把这条路径写好了（`docs/relay-deploy.md:210`）：

> 泄露即换：改电脑侧 `relay_url` 与新 session，重启 Mac app 即可。

具体步骤：

```bash
# 1) 生成新 session（文档 §2.6 指定的生成方式）
NEW=$(openssl rand -hex 16)

# 2) 电脑侧：改 ~/.yi-agent/preferences.json 的 relay_url，或设环境变量（优先级更高）
#    wss://<域名>/connect?session=$NEW
export YI_AGENT_RELAY="wss://<域名>/connect?session=$NEW"
#    然后重启 Mac app（app-server 在启动时读入 relay_url）

# 3) iOS 侧：配对页地址填 wss://<域名>/ws?session=$NEW

# 4) 验证：VPS 侧应出现新 session 的 agent connected
ssh <vps> 'journalctl -u yi-agent-relay -n 50 | grep "agent connected"'
```

**VPS 侧通常无需改动**：`install.sh` 的签名为 `<域名> [session_id]`（`install.sh:5`），
session 是**可选**参数；中继本身不做 session 白名单（不然就不会"不鉴权"了）。
若历史上曾把旧 session 写进服务端配置，重跑 `./install.sh <域名> "$NEW"` 覆盖。

**顺序要求：先轮换，再脱敏。** 否则脱敏只是把"仍然有效的公开凭据"藏起来。
轮换后旧值彻底作废，仓库里即使还留着也只是失效字符串。

### 3.3 机械门禁（G3）

**这道门禁是本设计的核心**——G1/G2 各只解决一次，G3 解决"这一类"。

设计取向：**零依赖、纯脚本、跑在最便宜的 CI 阶段**。理由：仓库现有 CI 是
`just ci`（`fmt-check lint test build bindings-check`），引入 `gitleaks` 这类
外部二进制会带来版本固定、下载源、失败模式三重维护面；而本次要防的是
**已知形态的少数几个值**，正则足够。

落点：新增 `yi-agent-rs/justfile` recipe `secret-check`，并挂进 `ci`：

```
ci: fmt-check lint test build bindings-check secret-check
```

实现位于仓库根 `scripts/check-secrets.sh`。扫描边界固定在 Git 仓库根，仅检查 tracked 文件；门禁脚本、自测夹具以及本设计/计划文档从凭据扫描中排除。仓库定位失败、没有 tracked 文件、或 `git grep` 返回大于 1 的错误时，检查必须失败关闭。

规则按以下语义执行：

1. **已知泄露值**：精确拦截已泄露 session 字面值，防止从 git 历史回贴。
2. **结构化 session**：拦截 `session=` 后紧跟 32 个十六进制字符（`[0-9A-Fa-f]`）的值；`<session_id>` 等占位符放行。
3. **relay 主机名**：拦截 `relay.*` 主机名，仅精确示例主机 `relay.example.com` 放行；常见 URL 编码的分隔符 `%3A` / `%2E` 形式也识别为示例；例如 `relay.example.com.evil` 不能因前缀相同而豁免。
4. **SSH/lsof 公网地址**：在含 `ssh … root@<IPv4>`（包括 `-n`、`-o` 等参数）或 `lsof … | grep <IPv4>` 的同一行检查 IPv4。RFC 5737 的 `192.0.2.0/24`、`198.51.100.0/24` 和 `203.0.113.0/24` 保留段放行；该语境中的其它公网 IPv4 拦截，不对普通文本里的所有 IPv4 做全局拦截。

**退出码：** `0` 表示检查通过，`1` 表示发现疑似凭据，`2` 表示扫描无法可靠完成。扫描失败不能当作「无命中」处理。

检测输出以 `::error::` 开头，便于 GitHub Actions 渲染注解；其它 CI 将其当作普通输出。

**为什么是「已知值 + 结构规则」而不是通用熵检测**：通用熵扫描（gitleaks 的 high-entropy 规则）在含大量 base64 测试夹具、`encrypted_content` 样例的仓库里误报率极高（本仓 `yi-agent-llm` 的测试里就有成串 base64）。精确规则针对已发生过的泄露形态，成本低且边界可回归；未来升级熵扫描时，`secret-check` 这个落点不变。

**门禁自测**位于 `scripts/test_check_secrets.sh`（纯 Bash，无 Rust 测试依赖），使用 `mktemp -d` 和 `git init` 创建临时仓库。当前覆盖 18 个断言，包括：已知 session 与域名命中、RFC/示例值通过、未跟踪文件排除、从子目录运行仍扫描仓库根、非仓库和无 tracked 文件返回 `2`、结构化 session、精确 relay 示例域名（含 URL 百分号编码）与近似域名、带参数 SSH IP，以及 lsof/grep IP 语境。公网 IPv4 负例与 RFC 5737 保留段正例都分别验证；同一行同时含 RFC 示例地址和公网地址时仍必须命中。

自测脚本本身不能启用 `set -e`：它必须捕获被测脚本预期的非零退出码。

## 4. 验收判据

- [ ] `bash scripts/test_check_secrets.sh`：18 个自测断言全部通过（结构化 session、relay host、SSH/lsof IP、公网/RFC 示例边界、同一行混合地址和 fail-closed 场景均覆盖）
- [ ] `bash scripts/check-secrets.sh` 在仓库根 exit 0
- [ ] `(cd yi-agent-rs && bash ../scripts/check-secrets.sh)` exit 0（从子目录仍扫描整个仓库）
- [ ] 自测中非仓库目录、无 tracked 文件两例均 exit 2；跟踪文件疑似凭据为 exit 1
- [ ] 故意在临时仓库 tracked 文件放入 `session=<32位hex>`、非示例 `relay.*` 或 SSH/lsof 公网 IP → 脚本 exit 1
- [ ] RFC 5737 三个文档保留段与 `relay.example.com` / `<session_id>` 示例均由自测证明通过
- [ ] `just ci` 通过（含 `secret-check` 步骤）
- [ ] 轮换后 VPS journal 出现新 session 的 `agent connected`（人工验证，记录在 PR 描述）

## 5. 取舍与风险

### 5.1 为什么把门禁挂在 `just ci` 而不是单独 workflow

三条 CI 路径（GitHub PR、GitLab push、GitLab MR）已统一走 `just ci`
（`.github/workflows/ci.yml` 的注释明确写了这一点）。挂进 `just ci` 即三处同时生效，
不会再出现 12 号报告发现的「两个 CI 系统跑不同命令」的漂移。

### 5.2 门禁的硬编码值会过期

第 1 条规则钉的是**当前已知的泄露值**。轮换后这个值永久失效，规则的作用从
"挡住泄露"变成"挡住回归"（防止有人从历史里捞回来粘贴）。这是有意的：
它便宜、零误报，且与第 2/3 条的结构规则互补。

### 5.3 误报面

SSH/lsof 规则只处理对应排查命令行中的 IPv4，不扫描无关上下文里的普通 IP；`ssh -n`、`ssh -o BatchMode=yes` 等参数不会中断对后续 `root@IPv4` 的检查。RFC 5737 的三个文档保留段全部放行。relay 规则只豁免精确示例域名 `relay.example.com`，相似域名（如 `relay.example.com.evil`）仍会被拦截。若门禁报告其它有效示例，应将 IP 替换为 RFC 5737 保留段，并为新边界增加自测后再调整规则。

### 5.4 为什么不做 git 历史重写

`filter-repo` 重写会：改写全部提交哈希（所有在途分支与 worktree 失效，本仓当前
有 13 个 worktree）、要求所有协作者重新 clone、且如果公开镜像已被抓取则**并不
真正撤回**。而轮换 session 的成本是"改一行配置 + 两端重启"，效果是**旧值彻底作废**。
在这个不对称下，轮换 > 历史重写。历史里的旧值保留作为"已失效凭据"的记录。

## 6. 影响面

| 文件 | 改动 |
|---|---|
| `docs/bug-list.md` | 115/117 行的字面值 → 占位符（结论不动） |
| `docs/relay-deploy.md` | 573 行 IP → `203.0.113.10` |
| `scripts/check-secrets.sh` | 新增 |
| `yi-agent-rs/justfile` | `ci` 追加 `secret-check`；新增 `secret-check` recipe |
| `deploy/relay/.relay-local.md` | **不动**（未跟踪，机制正确） |

## 7. 后续（不在本篇范围）

- **中继鉴权**：当前"不鉴权、靠 session 保密"的模型，在 session 一旦泄露即等于
  开门。可另开设计引入配对 token（一次性、可撤销）。本篇只做卫生与门禁。
- **`thread/diff/read` 的 scope 门禁**（本审查 P0-6 的另一半）：它属于
  `2026-10-18-model-driven-diff-view-design.md` 的范围（该篇 D5 已诊断 git 注入并
  规划 `--end-of-options`），本篇不重复；但其"缺 Control 门禁"一点建议在该篇
  一并补齐，已记录在审查总报告 §6「值得并入在途方案」。
