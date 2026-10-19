# 中继凭据卫生 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把已进入 git 跟踪文件的真实中继凭据（session/域名/IP）脱敏，并加一道能在 CI 挡住同类问题的机械门禁。

**Architecture:** 三步。先加**会失败的门禁脚本及其自测**（TDD：先看它红），再脱敏让门禁转绿，最后轮换凭据使历史里的旧值作废。门禁只扫 `git ls-files`（跟踪文件），因此天然不误伤已忽略的 `deploy/relay/.relay-local.md`。

**Tech Stack:** bash（零外部依赖）、just（挂进既有 `ci` recipe）。不引入 gitleaks 等外部二进制。

**Spec:** `docs/superpowers/specs/2026-10-18-relay-credential-hygiene-design.md`

## Global Constraints

- 环境：`export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`；`export TMPDIR=/Users/gongyichen/.yi-agent-tmp`；cargo 一律加 `--offline`。
- **macOS 的 BSD `sed` 不支持 `\b`**，需要正则替换时用 `perl -pi -e` 或 python3。
- 只显式 `git add <path>`，不要 `git add -A` / `git add .`。
- `justfile` 位于 `yi-agent-rs/`，其 recipe 的 cwd 是 `yi-agent-rs/`，因此脚本路径写 `../scripts/...`。
- **不要改 `docs/bug-list.md` 里排查记录的结论**，只替换字面值（域名/IP/session）。
- 本卡**不碰** `deploy/relay/.relay-local.md`（未跟踪，机制正确）。
- 本卡**不碰** `model-driven-diff-view` 相关文件（`git_diff.rs` / `protocol.rs` / 前端 diff），那是另一张卡的范围。

---

### Task 1: 门禁脚本 + 自测（先让它红）

**Files:**
- Create: `scripts/check-secrets.sh`
- Create: `scripts/test_check_secrets.sh`
- Test: `scripts/test_check_secrets.sh`（自测即测试）

**Interfaces:**
- Produces: 可执行脚本 `scripts/check-secrets.sh`，**退出码语义**——`0` = 通过，`1` = 发现疑似凭据。输出 `::error::…` 前缀（GitHub Actions 会把它们渲染成注解；其它 CI 当普通 stderr）。`scripts/test_check_secrets.sh` 同样以 `0`/`1` 表示自测通过与否。
- Consumes: 无（Task 3 的 justfile 会消费它）。

- [ ] **Step 1: 写自测脚本（此刻必失败，因为被测脚本还不存在）**

创建 `scripts/test_check_secrets.sh`：

```bash
#!/usr/bin/env bash
# 自测 scripts/check-secrets.sh：确认它在「有真实凭据」时变红、
# 在「只有示例值」或「值在未跟踪文件里」时保持绿。
# 不用 `set -e`：本脚本要故意触发被测脚本的「非零退出」，那些非零是
# 期望结果，不能被 -e 当成致命错误。
set -uo pipefail

SCRIPT="$(cd "$(dirname "$0")" && pwd)/check-secrets.sh"
[ -x "$SCRIPT" ] || { echo "FAIL: $SCRIPT not executable"; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

pass=0
failn=0
check() { # check <name> <expected_exit> <actual_exit>
  if [ "$2" = "$3" ]; then echo "ok   - $1"; pass=$((pass+1));
  else echo "FAIL - $1 (expected exit $2, got $3)"; failn=$((failn+1)); fi
}

# --- case 1: 合成 32 位 session 形状在跟踪文件里 → 必须 exit 1
r1="$work/leaked"; mkdir -p "$r1/docs"
git -C "$r1" init -q
printf 'see wss://relay.example.com/connect?session=0123456789abcdef0123456789abcdef\n' > "$r1/docs/a.md"
git -C "$r1" add docs/a.md
( cd "$r1" && bash "$SCRIPT" >/dev/null 2>&1 ); check "synthetic session-shaped value in tracked file fails" 1 $?

# --- case 2: 只有示例值 → 必须 exit 0
r2="$work/clean"; mkdir -p "$r2/docs"
git -C "$r2" init -q
printf 'see wss://relay.example.com/connect?session=<session_id>\n' > "$r2/docs/a.md"
printf 'lsof -nP -p "$(pgrep -f app-server)" | grep 203.0.113.10\n' >> "$r2/docs/a.md"
git -C "$r2" add docs/a.md
( cd "$r2" && bash "$SCRIPT" >/dev/null 2>&1 ); check "example values pass" 0 $?

# --- case 3: 敏感字段形状在未跟踪文件里 → 必须 exit 0（不误伤 .relay-local.md 范式）
r3="$work/untracked"; mkdir -p "$r3/deploy"
git -C "$r3" init -q
printf 'session = 0123456789abcdef0123456789abcdef\nip = 203.0.113.10\n' > "$r3/deploy/local.md"
printf 'x\n' > "$r3/keep.md"; git -C "$r3" add keep.md
( cd "$r3" && bash "$SCRIPT" >/dev/null 2>&1 ); check "untracked file is ignored" 0 $?

# --- case 4: 非示例中继域名在跟踪文件里 → 必须 exit 1
r4="$work/host"; mkdir -p "$r4/docs"
git -C "$r4" init -q
printf 'connect to wss://relay.production.invalid/ws\n' > "$r4/docs/a.md"
git -C "$r4" add docs/a.md
( cd "$r4" && bash "$SCRIPT" >/dev/null 2>&1 ); check "non-example hostname in tracked file fails" 1 $?

echo
if [ "$failn" -ne 0 ]; then echo "$failn failed, $pass passed"; exit 1; fi
echo "$pass passed"
```

再让它可执行：

```bash
chmod +x scripts/test_check_secrets.sh
```

在上述 7 个基础用例后补充 11 个独立断言，使自测共 18 项：

```bash
# 1: 已知/合成 session 命中；2: session=<session_id> 示例放行。
# 3: relay.example.com 放行；4: URL 百分号编码的 relay.example.com 放行。
# 5: relay.other.example 命中；6: relay.example.com.evil 命中。
# 7: ssh -n -o BatchMode=yes root@198.51.101.7 命中。
# 8: ssh -n -o BatchMode=yes root@198.51.100.7 放行（RFC 5737）。
# 9: lsof -nP -iTCP | grep 198.51.101.8 命中。
# 10: lsof -nP -iTCP | grep 198.51.100.8 放行（RFC 5737）。
# 11: 同一诊断行同时含 RFC 地址和公网地址仍命中。
```

每项使用隔离临时仓库、跟踪夹具文件，并通过既有 `check <name> <expected_exit> <actual_exit>` helper 断言真实退出码；不得启用 `set -e`。

- [ ] **Step 2: 跑自测，确认它失败**

Run: `bash scripts/test_check_secrets.sh`
Expected: FAIL —— 被测脚本还不存在时先报告不可执行；新增结构边界自测必须在实现前呈现至少一个规则缺口。

- [ ] **Step 3: 写最小实现**

创建 `scripts/check-secrets.sh`：

```bash
#!/usr/bin/env bash
# 挡住「真实部署凭据进跟踪文件」这类缺陷。
#
# 只扫 git 跟踪文件（`git grep` 默认只看跟踪内容），因此
# deploy/relay/.relay-local.md 这类「已忽略的本机私密记录」天然被排除。
#
# 退出码：0 = 通过；1 = 发现疑似凭据。
# 输出以 `::error::` 开头，GitHub Actions 会渲染成注解。
set -euo pipefail

fail=0

report() { # report <message>
  echo "::error::$1"
  fail=1
}

# 命中行常常是一整段文档；只报「文件:行号」加前 160 字符的片段，
# 否则 ::error:: 注解会长到无法阅读。
report_hits() { # report_hits <label> <git-grep-output>
  local label="$1" hits="$2"
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    local loc snippet
    loc="${line%%:*}"                       # 文件路径
    local rest="${line#*:}"
    loc="$loc:${rest%%:*}"                  # 加上行号 -> 文件:行
    snippet="$(printf '%s' "$rest" | cut -c1-160)"   # rest 不含 "文件:行:" 前缀
    report "$label ($loc): $snippet"
  done <<< "$hits"
}

# 1) 类 session 参数中的 32 位十六进制 token 形状。
set +e
hits=$(git grep -n -E 'session=[0-9a-f]{32}' -- . 2>/dev/null)
set -e
if [ -n "$hits" ]; then
  report_hits "session-shaped token present:" "$hits"
fi

# 2) 中继域名形状（过滤文档示例域名 relay.example.com）。
set +e
hits=$(git grep -n -E 'relay\.[[:alnum:].-]+' -- . 2>/dev/null | grep -vF 'relay.example.com' || true)
set -e
if [ -n "$hits" ]; then
  report_hits "non-example relay hostname in tracked file:" "$hits"
fi

# 3) relay 的 lsof grep 或 ssh root 语境中的 IPv4 地址。
#    RFC 5737 保留段（192.0.2.0/24、198.51.100.0/24、203.0.113.0/24）不匹配。
set +e
hits=$(git grep -n -E '(lsof[^\n]*grep[[:space:]]+[0-9]{1,3}(\.[0-9]{1,3}){3})|(ssh[^\n]*root@[0-9]{1,3}(\.[0-9]{1,3}){3})' -- . 2>/dev/null | grep -vE '(192\.0\.2\.|198\.51\.100\.|203\.0\.113\.)' || true)
set -e
if [ -n "$hits" ]; then
  report_hits "public IP in relay/ssh context:" "$hits"
fi

if [ "$fail" -ne 0 ]; then
  echo
  echo "凭据卫生检查失败。示例值请用 relay.example.com 与 RFC 5737 保留段（如 203.0.113.10）；"
  echo "部署凭据请写在被忽略的文件里（范式见 deploy/relay/.relay-local.md）。"
  exit 1
fi

echo "secret-check passed"
```

规则必须同时实现：known leaked session 字面值；`session=` 后 32 个大小写不敏感十六进制字符；拦截非示例 `relay.*` 并精确放行 `relay.example.com`（包括 URL 中 `%3A` / `%2E` 分隔符语境，但不放行 `relay.example.com.evil`）；扫描 `ssh … root@IPv4`（含 `-n`、`-o` 参数）及 `lsof … | grep IPv4` 行。仅放行 RFC 5737 三个示例网段，不得因同一行有 RFC 地址而漏掉该行另一个公网 IP。扫描失败返回 `2`、发现命中返回 `1`、无命中返回 `0`；保留 tracked-only、仓库根扫描和 fail-closed 守卫。


再让它可执行：

```bash
chmod +x scripts/check-secrets.sh
```

- [ ] **Step 4: 跑自测，确认它通过**

Run: `bash scripts/test_check_secrets.sh`
Expected: PASS —— 18 个 `ok - …` 与最后的 `18 passed`，退出码 0。

- [ ] **Step 5: 在当前仓库上跑一次，确认它现在**红**（这是 TDD 的红：真凭据还在）**

Run: `bash scripts/check-secrets.sh; echo "exit=$?"`
Expected: 打印若干 `::error::…`（命中 `docs/bug-list.md:115`、`:117`、`docs/relay-deploy.md:573`）与 `exit=1`。**这正是 Task 2 要消除的红。**

- [ ] **Step 6: Commit**

```bash
git add scripts/check-secrets.sh scripts/test_check_secrets.sh
git commit -m "ci: 加凭据卫生门禁脚本与自测（当前对已知泄露值报红）"
```

---

### Task 2: 脱敏，让门禁转绿

**Files:**
- Modify: `docs/bug-list.md:115`（与 `:117` 里的 IP）
- Modify: `docs/relay-deploy.md:573`

**Interfaces:**
- Consumes: Task 1 的 `scripts/check-secrets.sh`（作为绿的判据）。
- Produces: 无新接口；仓库进入「门禁绿」状态。

- [ ] **Step 1: 先看清要改的确切文本（别改错行）**

Run:
```bash
grep -n -E 'session=|lsof.*grep' docs/bug-list.md docs/relay-deploy.md
```
Expected: 命中 `docs/bug-list.md` 的 115、117 行与 `docs/relay-deploy.md` 的 573 行。记下每行里的**字面值出现位置**。

- [ ] **Step 2: 用 perl 就地替换（三处字面值）**

Run:
```bash
# 只修改 Step 1 已确认的三处命中：
# - 真实中继域名 → relay.example.com
# - URL 中的 session 值 → <session_id>
# - 真实公网地址 → 203.0.113.10（RFC 5737 保留段）
# 操作时从原文件读取并就地替换，不要把原值复制进命令行、日志或其他文件。
$EDITOR docs/bug-list.md docs/relay-deploy.md
```

- [ ] **Step 3: 人工复核 diff —— 只许字面值变，结论不许变**

Run: `git diff -- docs/bug-list.md docs/relay-deploy.md`
Expected: 每一处改动都只是上面的三个字面值互换。**逐行确认**没有段落被删、没有结论被改写（例如「修复前无 `agent connected`、修复后有」这类因果描述必须原样在）。

- [ ] **Step 4: 跑门禁，确认转绿**

Run: `bash scripts/check-secrets.sh; echo "exit=$?"`
Expected: `secret-check passed` 与 `exit=0`。

- [ ] **Step 5: 确认跟踪文件里再无真实值**

Run:
```bash
git grep -n -E 'session=[0-9a-f]{32}' ; echo "session hits=$?"
git grep -n -E 'lsof.*grep[[:space:]]+[0-9]{1,3}(\.[0-9]{1,3}){3}' ; echo "ip hits=$?"
git grep -n -E 'relay\.[[:alnum:].-]+' | grep -vF 'relay.example.com' ; echo "host hits=$?"
```
Expected: 三条 `git grep` **都无输出**，各自的 `echo` 打印 `=1`（grep 无匹配的退出码）。

- [ ] **Step 6: Commit**

```bash
git add docs/bug-list.md docs/relay-deploy.md
git commit -m "docs: 脱敏文档里的真实中继域名/IP/session"
```

---

### Task 3: 把门禁挂进 `just ci`

**Files:**
- Modify: `yi-agent-rs/justfile`（`ci` recipe 一行；新增 `secret-check` recipe）

**Interfaces:**
- Consumes: `scripts/check-secrets.sh`（Task 1）。
- Produces: `just secret-check` recipe 名（CI 路径统一经 `just ci` 调用它）。

- [ ] **Step 1: 确认当前 `ci` recipe 的确切内容**

Run: `grep -n -A2 '^ci:' yi-agent-rs/justfile`
Expected: `ci: fmt-check lint test build bindings-check`。

- [ ] **Step 2: 改 `ci` 依赖 + 加 recipe**

把 `yi-agent-rs/justfile` 里的

```
ci: fmt-check lint test build bindings-check
    @echo "CI passed"
```

改成

```
ci: fmt-check lint test build bindings-check secret-check
    @echo "CI passed"
```

并在其**紧后面**（`bindings-check` recipe 之后）插入：

```
# 凭据卫生：挡住真实部署凭据进入跟踪文件。
# 只扫 git 跟踪文件，故 deploy/relay/.relay-local.md 这类已忽略文件不参与。
secret-check:
    bash ../scripts/check-secrets.sh
```

- [ ] **Step 3: 验证 recipe 能被 just 识别且真的执行**

Run: `cd yi-agent-rs && just secret-check; echo "exit=$?"`
Expected: `secret-check passed` 与 `exit=0`（依赖 Task 2 已完成）。

- [ ] **Step 4: 验证它确实挂在 `ci` 的依赖图里**

Run: `cd yi-agent-rs && just --dry-run ci`
Expected: 输出里包含 `bash ../scripts/check-secrets.sh`（证明 `secret-check` 是 `ci` 的一步，而不是孤立 recipe）。

- [ ] **Step 5: 验证门禁在失败时能挡住（故意引入一次泄露）**

Run:
```bash
printf '\nleak: wss://relay.example.com/connect?session=0123456789abcdef0123456789abcdef\n' >> docs/bug-list.md
cd yi-agent-rs && just secret-check; echo "exit=$?" ; cd ..
git checkout -- docs/bug-list.md
```
Expected: `just secret-check` 打印 `::error::known leaked session id present: …` 且 `exit=1`；随后 `git checkout` 把文件还原。**这一步证明门禁不是"写了但永远绿"。**

- [ ] **Step 6: 跑完整的 `just ci` 确认无回归**

Run: `cd yi-agent-rs && just ci`
Expected: 通过（`CI passed`）。注意：这一步会跑 `cargo test --all-features --workspace`，按 CLAUDE.md 的叮嘱**先确认没有其它 cargo 进程在跑**（`ps aux | grep cargo`），且本机内存足够。若全量太吃资源，可退化为逐 crate 跑，但**必须**至少让 `fmt-check` / `lint` / `bindings-check` / `secret-check` 全绿。

- [ ] **Step 7: Commit**

```bash
git add yi-agent-rs/justfile
git commit -m "ci: 把凭据卫生门禁挂进 just ci（三条 CI 路径统一生效）"
```

---

### Task 4: 更新文档，把「靠人遵守」换成「有门禁」

**Files:**
- Modify: `deploy/relay/README.md`（安全提醒段落）
- Modify: `docs/relay-deploy.md`（§2.6 与 §7 的「凭据保管」条目）

**Interfaces:**
- Consumes: `just secret-check`（Task 3）。
- Produces: 无代码接口。

- [ ] **Step 1: 在 `deploy/relay/README.md` 的安全提醒里加上门禁的存在**

找到 `> **安全提醒**：中继按 `session` 配对转发，**不鉴权**——`session_id` 本身就是凭据。`
那一段（在文件开头的说明里），在该段末尾追加一句：

```
> 仓库已加机械门禁（`just secret-check`，随 `just ci` 执行）：真实域名/IP/session
> 出现在 git 跟踪文件里会直接让 CI 失败。示例值统一用 `relay.example.com` 与
> RFC 5737 保留段（如 `203.0.113.10`）。
```

- [ ] **Step 2: 在 `docs/relay-deploy.md` 的 §2.6 末尾补同样一句**

在 §2.6 那句「泄露即换：改电脑侧 `relay_url` 与新 session，重启 Mac app 即可。」之后追加：

```
> **门禁**：`just secret-check`（随 `just ci` 执行）会扫描全部 git 跟踪文件，
> 真实域名/IP/session 一旦出现即让 CI 失败。示例值用 `relay.example.com` 与
> `203.0.113.10`。本机真实值仍写在 `deploy/relay/.relay-local.md`（已忽略）。
```

- [ ] **Step 3: 确认文档改完门禁仍绿**

Run: `bash scripts/check-secrets.sh; echo "exit=$?"`
Expected: `secret-check passed` 与 `exit=0`（新写的文字用的是示例值）。

- [ ] **Step 4: Commit**

```bash
git add deploy/relay/README.md docs/relay-deploy.md
git commit -m "docs: 说明凭据卫生门禁，示例值统一用保留段"
```

---

### Task 5: 轮换凭据（人工操作，写进 PR 描述）

**Files:**
- 无代码改动。产出是 PR 描述里的一段**已验证的操作记录**。

**Interfaces:**
- Consumes: 无。
- Produces: 作废的旧 session（具体值只存在于本机私密记录）与记录在 PR 里的验证证据。

> **这是人工步骤，不是 agent 能代替的**：它需要在 Mac 上改配置、在手机上改地址、
> 并在 VPS 上查日志。agent 的职责是**把步骤与验证命令写清楚**，并在用户执行后
> 把结果记录到 PR 描述里。

- [ ] **Step 1: 生成新 session**

Run: `openssl rand -hex 16`
Expected: 一个 32 位十六进制串（新 session）。**不要**把它写进任何跟踪文件。

- [ ] **Step 2: 电脑侧改用新 session 并重启**

改 `~/.yi-agent/preferences.json` 的 `relay_url`（或设环境变量 `YI_AGENT_RELAY`，优先级更高）：

```
wss://relay.example.com/connect?session=<新 session>
```

然后重启 Mac 上的 yi-agent app。

- [ ] **Step 3: 手机侧改用新 session**

iOS 配对页地址填 `wss://relay.example.com/ws?session=<新 session>`。

- [ ] **Step 4: 在 VPS 上验证新 session 已配对**

Run: `ssh <vps> 'journalctl -u yi-agent-relay -n 100 | grep "agent connected"'`
Expected: 出现**新 session** 的 `agent connected` 行。

- [ ] **Step 5: 验证旧 session 已作废**

Run: `ssh <vps> 'journalctl -u yi-agent-relay --since "5 min ago" | grep "agent connected"'`，并在 VPS 上用本机私密记录里的旧值确认无活动；旧值不写入 PR 或仓库。
Expected: **无输出**（旧 session 不再有任何连接；即使有人拿旧值来连，也不会与你的电脑配对）。

- [ ] **Step 6: 把上面两步的输出粘进 PR 描述，并 Commit 一个空记录（可选）**

若团队习惯把轮换记录留档，在 `docs/relay-deploy.md` 的 §2.6 追加一行**不含值**的记录：

```
| 上一次轮换 | 2026-10-18（因 session 曾进入跟踪文件；旧值已作废） |
```

```bash
git add docs/relay-deploy.md
git commit -m "docs: 记录中继 session 轮换（不含值）"
```

---

## 收尾：合并前的验证清单

- [ ] `bash scripts/test_check_secrets.sh` → 18 passed
- [ ] `bash scripts/check-secrets.sh` → passed（exit 0）
- [ ] `git grep` 三次（session / IP / 域名）在跟踪文件零命中
- [ ] `cd yi-agent-rs && just --dry-run ci` 输出含 `check-secrets.sh`
- [ ] `cd yi-agent-rs && just ci` 通过
- [ ] 轮换的 VPS 日志证据已贴进 PR 描述（Task 5 Step 4/5）
- [ ] `cargo fmt --all`（在 `yi-agent-rs/` 下）——本卡无 Rust 改动，但按仓库规范提交前照跑