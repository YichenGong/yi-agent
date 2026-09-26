# 黑名单硬拒绝与根路径判定收紧设计

**目标:** 让安全黑名单成为**准确且不可放行**的硬红线:修正 `rm -rf /`
规则对任何绝对路径的误伤;取消黑名单的"允许一次"确认路径(改为直接拒绝);
并让这次拒绝在 TUI 中**可见**。

**状态:** 设计已确认,待转实现计划。

---

## 1. 问题

### 1.1 误伤:任何绝对路径都被判为 `rm -rf /`

`yi-agent-rs/crates/yi-agent-tools/src/shell/blocklist.rs:11` 的规则:

```
rm\s+(-[rfRF]+\s+|-r\s+-f\s+|-f\s+-r\s+)(--no-preserve-root\s+)?/
```

`/` 之后没有任何边界约束,因此 `rm -rf /tmp/x`、`rm -f /tmp/x.txt`、
`rm -rf /Users/.../target` 全部命中,理由统一显示为 `rm -rf /`。

实测(真实 `regex` crate):

```
SAFE   rm -rf /tmp/verify-perm                  -> Some("rm -rf /")
SAFE   rm -f /tmp/verify-perm-dir.txt           -> Some("rm -rf /")
SAFE   rm -rf /Users/x/proj/target              -> Some("rm -rf /")
DANGER rm -rf /                                 -> Some("rm -rf /")
DANGER rm -rf /*                                -> Some("rm -rf /")
DANGER rm -rf /tmp/../                          -> Some("rm -rf /")
```

日志佐证:`~/.yi-agent/trace/session-20260926-102847.jsonl` 第 3662/3729/3741/3753
行,四条无害的临时目录清理命令全部以 `reason=rm -rf /` 被拦。

### 1.2 双重拦截:"Allow once" 是死选项

`crates/yi-agent-core/src/permission.rs:118` 在 yolo 下仍查黑名单,命中返回
`CheckResult::Blacklisted`;`crates/yi-agent-core/src/agent.rs:711-747` 对
`Blacklisted` 弹确认框并等待决定。用户按 `1`(AllowOnce)后命令被放行进入
执行队列,但 `crates/yi-agent-tools/src/shell/bash.rs:115` 在执行前**又无条件**
跑了一次 `is_blocked()`,于是再次被拦。

实测(真实 `BashTool` + `DangerFullAccess`,即 `--yolo` 解析结果):

```
tool result: error: command blocked by safety filter: rm -rf /
```

即 UI 提供的 `[1] Allow once` 对黑名单命令永远无效。

### 1.3 拒绝不可见

拒绝路径只发 `ToolResult`、不发 `ToolCall`,TUI 无法渲染该次调用。当前 yolo
下黑名单至少会弹框(用户能看见),一旦改为硬拒绝而不同时补 `ToolCall`,拒绝
将变成**静默**。这在硬红线场景下是危险的:用户会以为命令已执行。

### 1.4 文档与实现不一致

`docs/superpowers/specs/2026-08-09-yolo-sandbox-semantics-design.md` 的表格
声明 `--yolo` 时"权限确认 = skipped",但代码里黑名单仍会弹框。这正是用户
报告"yolo 模式冒出授权确认"的直接原因。

本次变更(变更点 2)取消黑名单的弹框路径后,实现将与该表格一致,无需修改
该文档的语义声明。

---

## 2. 设计原则

1. **黑名单是硬红线**:不可通过 `Allow once` / `Always allow` 绕过。
2. **红线必须准确**:只拦真正指向根或系统目录的危险目标,不误伤普通绝对路径。
3. **红线必须可见**:拒绝要在 TUI 中明确呈现,不能让用户误以为已执行。

---

## 3. 变更点 1:正则收紧(`shell/blocklist.rs`)

将原单条 `rm -rf /` 规则替换为三条规则,理由标签分别独立,便于日志定位。

### P1 — 目标即根本身(reason: `rm -rf /`)

允许 `/` 之后出现 `/`、`.`、`*`、`..` 这类无实义段,并要求词边界:

```
rm\s+(-[rfRF]+\s+|-r\s+-f\s+|-f\s+-r\s+)(--no-preserve-root\s+)?/([/.*]*(\.\.)?[/.*]*)*(\s|$)
```

命中:`rm -rf /`、`rm -rf /*`、`rm -rf / --`、`rm -rf / somefile`、`rm -rf /tmp/../`。
不命中:`rm -rf /tmp/verify-perm`。

### P2 — 通过 `..` 爬回根(reason: `rm -rf / (path resolves to root)`)

```
rm\s+(-[rfRF]+\s+|-r\s+-f\s+|-f\s+-r\s+)(--no-preserve-root\s+)?(/[^/\s]+)+/(\.\.)((/\.\.)*)(/)?(\s|$)
```

命中:`rm -rf /tmp/../`。
不命中:`rm -rf /tmp/../tmp/foo`(最终落在 `/tmp/foo`)、`rm -rf /tmp/x/../y`。

### P3 — 顶层系统目录内容(reason: `rm -rf system directory`)

```
rm\s+(-[rfRF]+\s+|-r\s+-f\s+|-f\s+-r\s+)(--no-preserve-root\s+)?/(etc|usr|var|bin|sbin|lib|boot|dev|proc|sys|System|Library)(/[^\s]*)?(\s|$)
```

命中:`rm -rf /etc/*`、`rm -rf /usr/*`、`rm -rf /var/log`。
不命中:`rm -rf /tmp/*`、`rm -rf /Users/x`。

**为什么需要 P3:** 旧正则在拦 `/tmp/foo` 的同时"顺带"拦住了 `/etc/*`、
`/usr/*`。只做 P1/P2 会把这项顺带的保护一并放掉,属安全净减分。P3 保证本次
修复只砍误伤、不降低真实防护。

### 3.1 已知行为变更(需在计划与提交信息中显式记录)

| 命令 | 旧行为 | 新行为 | 说明 |
| --- | --- | --- | --- |
| `rm -rf /tmp/foo` | BLOCK | allow | 修掉误伤(本 bug 主体) |
| `rm -rf /Users/.../target` | BLOCK | allow | 修掉误伤 |
| `rm -rf /tmp/../` | BLOCK | BLOCK | 仍拦(P2),解析为根 |
| `rm -rf /tmp/../tmp/foo` | BLOCK | allow | 解析为 `/tmp/foo` |
| `rm -rf /etc/*` | BLOCK | BLOCK | 改由 P3 拦,理由标签变化 |
| `rm -rf /usr/*` | BLOCK | BLOCK | 改由 P3 拦,理由标签变化 |

### 3.2 验证状态

三条候选正则已在真实 `regex` crate 上验证通过:

- **主表 33 个用例全绿**:覆盖全部既有 blocklist 测试预期、本 bug 场景、
  以及 P3 的系统目录用例。理由标签分别落在正确的规则上
  (`rm -rf /` / `rm -rf / (path resolves to root)` / `rm -rf system directory`)。
- **P3 对抗性边界 11 个用例全绿**:`/usr`、`/etc/passwd`、`/var/log/nginx`
  正确拦截;`/users`、`/usr2`、`/etc2`、`/variable`、`/lib64` 等形近名
  不误伤(大小写敏感、前缀安全);`/home/me/usr`(非顶层 `usr`)不误伤。
- **判别力已证**:把 P1 改回旧形式会精确产生 8 条误伤失败,证明测试不是
  空转通过。

实现时这些用例将转为 `blocklist.rs` 内的常驻单元测试。

---

## 4. 变更点 2:硬拒绝语义(`core/agent.rs`)

`CheckResult::Blacklisted` 分支不再进入确认流程:

- 不生成 `PermissionRequest`,不弹框,不等待 `decision_rx`。
- 直接产出拒绝 `ToolResult`,文案包含原因:`blocked by safety filter: <reason>`。
- 删除 `agent.rs:711-747` 中对 `handle_confirmation` 的黑名单调用路径。

`handle_confirmation` 仍服务于 `NeedConfirm`(普通确认),签名与行为不变。

### 4.1 纵深防御保留

`shell/bash.rs:115` 与 `process/manager.rs:269` 执行前的 `is_blocked` 检查
**保留**。理由:权限检查器是可选的(`Agent::with_permission` 不调用即无
checker,`naked` 模式即如此)。工具层自检是最后一道防线,移除会扩大无 checker
模式的风险面。

---

## 5. 变更点 3:拒绝可见(A 方案)

黑名单拒绝分支在发 `ToolResult` 之前**先发 `ToolCall`**:

```rust
let _ = tx.send(AgentEvent::ToolCall { id, name, input }).await;
let _ = tx.send(AgentEvent::ToolResult { id, result: ToolResult::error(msg) }).await;
```

TUI 因此能正常渲染该次调用并显示拒绝原因,例如:

```
bash  rm -rf /            [denied: blocked by safety filter: rm -rf /]
```

### 5.1 连带影响

- `tui/app.rs:869` 的 `Blacklisted => Deny` 默认键处理:黑名单不再产生弹框,
  该分支成为防御性代码,**保留**。
- `tui/cell.rs:295-301` 的 `[!] Blacklisted command` 渲染分支:同上,**保留**
  作兜底。
- `PermissionKind::Blacklisted` 变体与 `permission.rs:238-245` 的
  "黑名单不可持久化到白名单"守卫:**保留**(纵深防御)。
- app-server 侧无需改动:`translate.rs:259-270` 本就有意忽略
  `PermissionRequest` / `PermissionResolved`;`ToolCall` 走既有翻译路径。

### 5.2 范围边界

本次只修黑名单路径的可见性。bug-list 中更广的 C5 条目(其他拒绝类型
如 `NeedConfirm` 被拒后同样无 `ToolCall`)不在本次范围内,保持记录。

---

## 6. 测试策略(TDD,先红后绿)

1. `blocklist.rs` 单元测试
   - 新增:`rm -rf /tmp/verify-perm`、`rm -f /tmp/x.txt`、
     `rm -rf /Users/x/proj/target` 均为 `None`。
   - 新增:`rm -rf /tmp/../` 为 `Some`;`rm -rf /tmp/../tmp/foo` 为 `None`。
   - 新增:`rm -rf /etc/*`、`rm -rf /usr/*`、`rm -rf /var/log` 为 `Some`。
   - 回归:既有 `test_rm_rf`、`test_rm_rf_extended`、`test_bypass_attempts`
     全部保持通过。
2. `core/agent.rs` 测试
   - 新增:yolo + 黑名单命令 → **不产生** `PermissionRequest` 事件。
   - 新增:同一场景 → 产生 `ToolCall` 且伴随 `is_error` 的 `ToolResult`。
   - 回归:`agent_with_permission_need_confirm_user_denies` /
     `..._user_allows`(普通 `NeedConfirm`)行为不变。
3. `tui` 测试
   - 新增:黑名单拒绝在历史区渲染出调用与拒绝原因。

---

## 7. 文档

- `docs/bug-list.md` 第 24 行标记为已修复,附文件与验证命令。
- 本 spec 与随后的实现计划一并提交。
- 收尾前先将分支 rebase 到最新 `main`(该文件在本次工作期间已被并发会话
  提交为 `9430cde`),避免冲突。

---

## 8. 非目标

- 不改变 `--yolo` 与 `--sandbox` 的既有解析语义
  (`2026-08-09-yolo-sandbox-semantics-design.md` 的表格仍然成立)。
- 不引入 `--ask-for-approval`。
- 不重构黑名单的其余规则(仅新增 P3)。
- 不修 C5 的其他拒绝路径可见性。
