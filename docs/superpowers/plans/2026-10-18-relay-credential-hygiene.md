# 中继凭据卫生 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在现有凭据卫生门禁中补齐结构化规则回归覆盖，使脚本、自测和设计 spec 一致，并锁定规则边界。

**Architecture:** 先为 session、relay 主机和 SSH/lsof IP 规则补正反例，确认测试能暴露现有行为缺口；再修改 `scripts/check-secrets.sh` 使规则通过，同时保留仓库根扫描和 fail-closed 行为；最后同步设计 spec 与本计划的规则、退出码和验收描述。不重复实现已完成的 CI 接线、历史值脱敏或凭据轮换流程。

**Tech Stack:** Bash（零外部依赖）、Git 临时仓库夹具；文档为 Markdown。

## Global Constraints

- 只扫描当前 Git 仓库中已跟踪的文件；运行目录可为仓库根或其子目录。
- 脚本退出码：`0` = 检查通过，`1` = 检出疑似凭据，`2` = 扫描无法可靠执行。
- 真实 session 结构规则只匹配 `session=` 后紧跟 32 位十六进制值；`<session_id>` 等占位符放行。
- `relay.example.com`（包括分隔符以常见 `%3A` / `%2E` 百分号编码形式出现的 URL 语境）放行；结构规则拦截其他 `relay.*` 主机名，且 `relay.example.com.evil` 不属于豁免项。
- 对 `ssh` 命令中的公网 IPv4 地址进行检查，包括 `-n`、`-o` 等参数形式；RFC 5737 的 `192.0.2.0/24`、`198.51.100.0/24`、`203.0.113.0/24` 示例段放行。
- 对 `lsof … | grep <IPv4>` 排查命令检查公网 IPv4；RFC 5737 示例段放行。
- 不改 `docs/bug-list.md` 里排查记录的结论；只在规则文档对齐所需范围内更新 spec 和本计划。
- 本任务不改 `deploy/relay/.relay-local.md`、CI 接线、已完成的文档脱敏内容或人工轮换状态；不合并、不推送。
- 只显式 `git add <path>`，不要 `git add -A` / `git add .`。

---

### Task 1: 为结构化凭据规则补充红测

**Files:**
- Modify: `scripts/test_check_secrets.sh`
- Test: `scripts/test_check_secrets.sh`

**Interfaces:**
- Consumes: 当前 `scripts/check-secrets.sh`，其退出码 `0`/`1`/`2` 分别代表通过/命中/无法扫描。
- Produces: 对 session、relay 主机和 SSH/lsof IP 规则正反边界的可执行回归覆盖。

- [ ] **Step 1: 在现有自测脚本结尾、汇总输出前添加结构规则夹具**

继续使用脚本已有的 `work` 临时目录和 `check <name> <expected_exit> <actual_exit>` helper。每个 case 建立独立临时 Git 仓库，写入并 `git add` 一个夹具文件，再从该仓库运行 `bash "$SCRIPT"`。在现有 7 个 case 后添加以下 11 个独立断言（共 18 个）：

```bash
# 1: session=<32位hex> → exit 1；2: session=<session_id> → exit 0。
# 3: relay.example.com → exit 0；4: URL 百分号编码的 relay.example.com → exit 0。
# 5: relay.other.example → exit 1；6: relay.example.com.evil → exit 1。
# 7: ssh -n -o BatchMode=yes root@198.51.101.7 → exit 1。
# 8: ssh -n -o BatchMode=yes root@198.51.100.7 → exit 0（RFC 5737 保留段）。
# 9: lsof -nP -iTCP | grep 198.51.101.8 → exit 1。
# 10: lsof -nP -iTCP | grep 198.51.100.8 → exit 0（RFC 5737 保留段）。
# 11: 同一行含 203.0.113.7 和 198.51.101.9 → 仍 exit 1。
```

每条断言都单独调用现有 `fixture <name> <file-content>` helper 建仓、写 `fixture.txt`、执行 `git add`，然后运行 `bash "$SCRIPT"` 并把 `$?` 原样传给 `check`。helper 必须在 `local` 声明中先取得参数，再单独设置依赖 `$name` 的仓库路径；不要让 fixture 内容当作 shell 命令执行。

- [ ] **Step 2: 运行自测确认新结构用例在当前实现上失败**

Run: `bash scripts/test_check_secrets.sh`
Expected: 基线实现中结构化 `session=`、非示例 `relay.*`（包括域名近似值）、带参数 SSH 公网 IP 与 lsof/grep 公网 IP 负例失败；占位符、精确示例域名及 RFC 5737 保留段通过。旧有 known-session、已知域名、tracked-only、root-scan 与 fail-closed 用例继续通过。

- [ ] **Step 3: Commit 红测**

```bash
git add scripts/test_check_secrets.sh
git commit -m "test(secret-check): 覆盖结构规则边界"
```

---

### Task 2: 实现结构规则并让自测转绿

**Files:**
- Modify: `scripts/check-secrets.sh`
- Test: `scripts/test_check_secrets.sh`

**Interfaces:**
- Consumes: Task 1 中定义的可执行边界用例。
- Produces: `scripts/check-secrets.sh` 在命中时返回 `1`，扫描故障时返回 `2`，通过时返回 `0`；结构规则覆盖上述全部正反例。

- [ ] **Step 1: 运行新增自测，确认红测证据仍可复现**

Run: `bash scripts/test_check_secrets.sh`
Expected: 至少一个新增结构规则 case 失败，非零退出；这一步验证 Task 1 的测试确实能检测现有缺口。

- [ ] **Step 2: 更新检查规则**

在脚本现有 known-session、真实 relay host 和 IP 检查基础上实现以下明确语义：

1. 保留已知 session 字面值回归检查，并新增 `session=` 加 32 位十六进制值的结构检查；使用大小写不敏感的十六进制字符集 `[0-9A-Fa-f]`。
2. relay host 规则拦截 `relay.` 后不是 `example.com` 的主机名；示例主机 `relay.example.com` 必须通过。边界应避免把 `relay.example.com.evil` 当作示例域名。
3. SSH IP 规则应匹配一行中 `ssh` 命令后至 `root@IPv4` 之间的参数文本，不能因 `ssh -n` 或 `ssh -o BatchMode=yes` 漏检；不得使用 Bash 风格的 `[^\\n]` 作为 grep 的跨行/非换行表达式。
4. `lsof` IP 规则匹配一行同时含有 `lsof`、`grep` 和 IPv4 的排查命令。
5. SSH/lsof 语境中的 IPv4 命中不得是 RFC 5737 的 `192.0.2.*`、`198.51.100.*`、`203.0.113.*` 文档示例地址。不要误把规则扩大成对仓库所有普通 IPv4 的无条件拦截。
6. 保留仓库根扫描、空扫描拒绝、`git grep` 错误返回 `2`、tracked-only 扫描和现有自测排除项；任何规则命令执行失败仍返回 `2`，不能假绿。

若使用多个 grep 查询，统一复用脚本当前的错误码处理模式：`git grep` 返回 `1` 表示无匹配，`>1` 表示扫描错误并退出 `2`。每类命中继续通过 `report_hits` 报出位置。

- [ ] **Step 3: 运行门禁自测确认绿**

Run: `bash scripts/test_check_secrets.sh`
Expected: 所有原有与新增 case 均显示 `ok`，最后报告总通过数，退出码 `0`。

- [ ] **Step 4: 在当前仓库根和 Rust 子目录分别运行门禁**

Run:
```bash
bash scripts/check-secrets.sh
(cd yi-agent-rs && bash ../scripts/check-secrets.sh)
```
Expected: 两次均输出 `secret-check passed` 并返回 `0`，证明扫描范围固定于仓库根。

- [ ] **Step 5: 检查 shell 语法并提交实现**

Run:
```bash
bash -n scripts/check-secrets.sh scripts/test_check_secrets.sh
git diff --check
```
Expected: 两条命令均退出 `0`。

```bash
git add scripts/check-secrets.sh scripts/test_check_secrets.sh
git commit -m "fix(secret-check): 对齐结构化凭据规则"
```

---

### Task 3: 让设计 spec 与实施计划精确对齐

**Files:**
- Modify: `docs/superpowers/specs/2026-10-18-relay-credential-hygiene-design.md`
- Modify: `docs/superpowers/plans/2026-10-18-relay-credential-hygiene.md`

**Interfaces:**
- Consumes: Task 2 已验证的脚本规则与退出码语义。
- Produces: 文档中精确、相互一致的规则定义、边界样例和验收判据。

- [ ] **Step 1: 对照脚本与自测更新 spec 的门禁规则段**

更新 spec §3.3、测试 case、误报面和验收判据，明确：

- 已知泄露值规则之外，结构 session 模式为 `session=` 后 32 位十六进制；占位符放行。
- relay host 的规则是 `relay.*`，显式豁免精确示例主机 `relay.example.com`。
- SSH 命令参数中 `root@IPv4` 可含 `-n`、`-o` 等参数；lsof/grep 排查行也检查公网 IPv4。
- RFC 5737 三个文档保留段在这些语境中通过；普通公网 IPv4 在对应语境中失败。
- 检查通过、凭据命中、扫描故障的返回码分别为 `0`、`1`、`2`；非仓库、无 tracked 文件及 grep 扫描错误不得报告通过。
- 自测覆盖已有 known session/域名/tracked-only/root-scan/fail-closed cases，以及本计划 Task 1 列出的结构边界。

删除或改写与当前实现不一致的旧描述，特别是只列 4 个旧 case 的验收文字；不能声称存在尚未由自测验证的规则。

- [ ] **Step 2: 更新计划的完成态上下文和收尾验收**

本计划开头应说明这是对已实现门禁的结构规则补充，不再包含初始脚本创建、文档脱敏、CI 接线或轮换任务。计划中的夹具说明和自测验收数量须与 `scripts/test_check_secrets.sh` 当前 18 个断言一致，包括 URL 编码示例域名正例、`198.51.100.0/24` 的 RFC 保留段正例，以及同一行的混合 IP 负例。收尾验收应要求：

```bash
bash scripts/test_check_secrets.sh
bash scripts/check-secrets.sh
(cd yi-agent-rs && bash ../scripts/check-secrets.sh)
bash -n scripts/check-secrets.sh scripts/test_check_secrets.sh
git diff --check
```

验收期望依次为：全部自测通过；两种 cwd 扫描均通过；shell 语法和 diff whitespace 检查通过。保留 cargo fmt 仅在工作树确有 Rust 变更时适用的说明；本计划不含 Rust 代码变更。

- [ ] **Step 3: 自审 spec/plan 对齐**

Run:
```bash
rg -n '4 passed|四个 case|只有 4|退出码|session=|relay\\\.\*|RFC 5737|exit 2|ssh -n|lsof' docs/superpowers/specs/2026-10-18-relay-credential-hygiene-design.md docs/superpowers/plans/2026-10-18-relay-credential-hygiene.md
```
Expected: 搜索结果中的规则、退出码和样例与脚本、自测一致；不保留声称只有四例的旧验收标准。

再运行 Task 3 Step 2 的全部命令，并检查：

```bash
git diff --check
git diff -- scripts/check-secrets.sh scripts/test_check_secrets.sh docs/superpowers/specs/2026-10-18-relay-credential-hygiene-design.md docs/superpowers/plans/2026-10-18-relay-credential-hygiene.md
```

Expected: diff 仅包含本计划规定的结构规则实现、自测和两份文档对齐；没有误改原有脱敏结论、CI 接线或人工轮换状态。

- [ ] **Step 4: Commit spec 与计划**

```bash
git add docs/superpowers/specs/2026-10-18-relay-credential-hygiene-design.md docs/superpowers/plans/2026-10-18-relay-credential-hygiene.md
git commit -m "docs(secret-check): 对齐规则 spec 与实施计划"
```

---

## 收尾：验证清单

- [ ] `bash scripts/test_check_secrets.sh`：18 个自测断言全部通过。
- [ ] `bash scripts/check-secrets.sh`：通过（exit `0`）。
- [ ] `(cd yi-agent-rs && bash ../scripts/check-secrets.sh)`：通过（exit `0`）。
- [ ] `bash -n scripts/check-secrets.sh scripts/test_check_secrets.sh`：通过。
- [ ] `git diff --check`：通过。
- [ ] spec 与计划的规则、边界、自测和退出码语义一致。
- [ ] 不执行 merge、push 或源分支删除。
