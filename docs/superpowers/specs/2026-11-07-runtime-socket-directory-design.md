# 运行时 socket 统一目录设计

日期：2026-11-07
状态：待实施

## 1. 问题

TUI 启动时弹窗「子 Agent runtime 未启用；重启 yi-agent 后即可用」，重启无效。

根因不是配置、不是二进制、不是 SQLite，而是 **unix socket 路径超出平台上限**：

```
/Users/.../personalProjects/yi-agent/.yi-agent/runtime/guardian-control.sock
= 117 字节
macOS sockaddr_un.sun_path 上限（本仓取 MAX_SOCKET_PATH_BYTES）= 103 字节
```

`bind()` 失败，managed 模式依赖的 guardian 根本起不来。前端等满 30 秒预算后，
把「启动期路径问题」降级显示成那句通用的「重启即可用」。

### 1.1 为什么只有 guardian 挂了

本仓有三类 unix socket，各有一套独立的路径解析：

| # | Socket | 现在的位置 | 回退实现 | 本项目实际状态 |
|---|---|---|---|---|
| 1 | 业务 daemon | `<runtime_dir>/runtime.sock` | ✅ `store::ipc::socket_path_for` → `$TMPDIR/yi-agent-<hash>.sock` | 已回退到 `$TMPDIR` |
| 2 | guardian | `<runtime_dir>/guardian-control.sock` | ❌ **无** | **117 字节，bind 失败** |
| 3 | 插件查询 | `{state_dir}/<plugin>.sock` | ✅ `supervisors::manifest::resolve_plugin_socket` → `$TMPDIR/plugin-<hash>.sock` | 已回退到 `$TMPDIR` |

所以现象是「daemon 正常、子 Agent 委派不可用」——业务 socket 早就悄悄降级了，
只有 guardian 那条路一直留在项目目录里。

### 1.2 现有回退的不足

`$TMPDIR` 回退能让 bind 成功，但它不是「支持长路径」，只是「长过阈值后换个地方」：

- **阈值处行为突变**：102 字节能用、104 字节换目录，两套布局并存。
- **`$TMPDIR` 可被系统清理**：macOS 会回收临时目录，socket 文件消失后守护进程成为
  无人能找到的孤儿。`~/.yi-agent` 不会被回收。
- **三类 socket 散在三处**：项目目录 / `$TMPDIR` / 将来的别处，排查一次连接问题
  要在多处找。

### 1.3 实证（2026-11-07）

1. 对本项目 runtime dir 下两个 socket 名直接 `bind`：`guardian-control.sock`(117)
   与 `runtime.sock`(108) 均报 `AF_UNIX path too long`。
2. 手工以 guardian 角色启动（socketpair 装到 fd 8、`setsid`、三个身份环境变量齐备）：
   `Error: the managed guardian stopped: path must be shorter than SUN_LEN`，rc=1。
   `bind_control` 之后的步骤一步都没走到。
3. 前端 trace 逐字记录 `managed runtime: managed start refused at prepare: runtime
   bring-up: the managed runtime did not become ready within 30s`
   （`~/.yi-agent/trace/session-20261107-104546.jsonl` 等 6 个文件）。
4. 同一命令在 `YI_AGENT_BRINGUP=embedded` 下正常返回，managed 下稳定复现超时。

## 2. 目标

**让 socket 路径长度与项目路径长度彻底解耦**，而不是在超长时降级。

socket 统一放到实例目录下的派生目录里，路径长度恒定：

```
<instance_home>/sockets/<sha256(runtime_dir)[..16]>/runtime.sock          = 65 字节
<instance_home>/sockets/<sha256(runtime_dir)[..16]>/guardian-control.sock = 74 字节
```

实测（`instance_home = /Users/gongyichen/.yi-agent`）workdir 从 200 到 900 字符，
guardian socket 恒为 74 字节。

非目标（本期明确不做）：

- 插件查询 socket（§1.1 第 3 类）保持现状，理由见 §7。
- 不改 `runtime.sqlite`、`runtime.lock` 等非 socket 文件的位置——它们没有长度限制，
  留在项目目录可以继续随项目一起备份/删除。
- 不引入「短路径项目走老布局」的分支：那会让同一项目在不同机器上 socket 位置不同。

## 3. 设计

### 3.1 单一真相源

在 `yi-agent-lifecycle::scope` 新增：

```rust
/// socket 目录：与项目路径长度无关的稳定短路径。
pub fn socket_directory(runtime_dir: &Path, instance_home: &Path) -> PathBuf {
    let digest = Sha256::digest(runtime_dir.as_os_str().as_encoded_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    instance_home.join("sockets").join(&hex[..16])
}
```

放 `lifecycle` 的理由是依赖方向：`store → lifecycle` 单向（`store/Cargo.toml:14`），
`lifecycle` 已依赖 `sha2`（`Cargo.toml:17`），且它承诺不拉 SQLite / 模型栈。
所有 socket 相关 crate 都已（直接或传递）依赖 `lifecycle`。

同时给 guardian 与业务 socket 各一个具名构造函数，避免调用方各自拼文件名：

```rust
pub fn guardian_socket(runtime_dir: &Path, instance_home: &Path) -> PathBuf;
pub fn daemon_socket(runtime_dir: &Path, instance_home: &Path) -> PathBuf;
```

### 3.2 长度兜底

`<instance_home>` 本身也可能被指到深路径。保留二级兜底，顺序为：

1. `socket_directory(...)` 下的路径能放下 → 用它（正常路径，永远走这条）。
2. 放不下 → `$TMPDIR/yi-agent-<sha256(runtime_dir)[..16]>.sock`（沿用现有命名）。
3. 仍放不下 → 返回明确错误，由调用方转成可读提示。

第 2 步保留是为了不引入新的失败模式，但正常情况下永远走不到——写进 spec 是为了
让后来者知道它存在的原因，而不是当成主路径。

### 3.3 调用点收敛

`control_path` 现在的 10 处消费点必须全部改为同源，否则两侧算出不同路径会比
现在更难查：

| 位置 | 现状 |
|---|---|
| `supervisors/src/lifecycle/guardian.rs:2135` | `runtime_dir.join(GUARDIAN_SOCKET_FILE)` —— bind |
| `supervisors/src/lifecycle/bringup.rs:225` | 同上 —— probe |
| `supervisors/src/lifecycle/admission.rs:36` | 同上 |
| `supervisors/src/lifecycle/legacy_evidence.rs:527` | 走 `control_path` |
| `supervisors/src/lifecycle/migration_operation.rs:915` | 走 `control_path` |
| `supervisors/src/lifecycle/switch_managed.rs:404` | 走 `control_path` |
| `app-server/src/rpc/runtime.rs:50` | 走 `control_path` |
| `subagent/src/foreground.rs:645` | 走 `control_path` |
| `subagent/src/thread_root.rs:829/1348/1530` | 直接 join |
| `yi-agent/src/frontend_lease.rs:285` | 直接 join |

做法：把 `yi_agent_lifecycle::scope::control_path(runtime_dir)` 改签名为
`control_path(scope: &RuntimeScope)`，从 scope 同时取 `runtime_dir` 与
`instance_home`。编译器会把每个遗漏点变成类型错误——这比靠人肉搜索可靠。

注意区分同名但无关的标识符：`lifecycle/src/client.rs` 里的 `control_path` 是
`ControlClient` 的**字段名与参数名**（`:193`、`:228`、`:267` 等），其值仍是调用方
传入的路径，不需要改签名。本期只改 `scope.rs` 那个构造函数，以及
`supervisors::lifecycle::bringup::control_path(scope)` 这层壳的转发。
`scope.rs` 自带的用例（`:253`）需要随签名更新。

### 3.4 业务 socket 调用点

`socket_path_for(runtime_dir)` 有 20+ 调用点（含测试）。业务 socket 的调用方大多
只有 `runtime_dir`，没有 `instance_home`。

处理方式：`store::ipc::socket_path_for` **保持签名不变**，内部改为调用
`lifecycle::scope::daemon_socket(runtime_dir, &instance_home())`；
`instance_home()` 取 `store::paths::instance_home()`，缺失时回退 `$TMPDIR` 规则。
这样 20+ 调用点一行不改。

可行性已核对：`store` 已有 `paths::instance_home()`（`paths.rs:20`，含
`YI_AGENT_HOME` 归一化），且 `store` 已依赖 `lifecycle`——不新增依赖边。

### 3.5 旧位置清理

老布局的 socket 文件会留在 `<runtime_dir>/` 下。处理：

- `bind` 前若旧路径存在且是 socket，按现有 `bind_control` 已有护栏处理
  （先探测是否有人应答，有人应答则 `AddrInUse` 拒绝，无人应答才删）。
- 业务 socket 同理，清理逻辑复用 `ipc/client.rs` 现有的身份校验（`(dev,ino)` 护栏，
  见 `bug-list.md:128` 的修复），避免误删同路径重建的新 socket。
- 不写迁移脚本：socket 每次启动重建，无需持久迁移。

## 4. 错误处理

`socket_directory` 超长时返回错误，由 managed bring-up 转成可读拒绝：

> 这个项目的 runtime socket 路径超过系统上限（managed 模式需要）；请把项目移到更短的
> 路径，或设置 `YI_AGENT_HOME` 到短目录。

不再退化成「等 30 秒 → 重启即可用」。

## 5. 测试策略

每条用例都要求「回退修复后必须变红」，否则断言可能空转。

1. **长度恒定**：workdir 长度取 200 / 500 / 900，断言 `guardian_socket` 与
   `daemon_socket` 都 ≤ `MAX_SOCKET_PATH_BYTES`。改回直接 join 即红。
2. **能真正 bind**：对 `socket_directory` 下的两个名字实际 `UnixListener::bind`，
   断言成功。这条是本次故障的正面回归——只断言长度不够，`bind` 才是真判据。
3. **同源性**：断言 launcher（`bringup::control_path`）、guardian（`bind_control`）、
   `admission`、`thread_root`、`frontend_lease` 五处算出的路径两两相等。
   把任一处改回 `runtime_dir.join(...)` 即红。
4. **项目隔离**：两个不同 runtime_dir 得到不同 `socket_directory`。
5. **兜底链**：把 `instance_home` 指到超长路径，断言落到 `$TMPDIR` 规则；再把
   `$TMPDIR` 也指到超长，断言返回明确错误而非 panic。
6. **真机验收**：本项目启动 TUI，无该弹窗，`/agents` 可用。

## 6. 风险

| 风险 | 处理 |
|---|---|
| 漏改一处消费点 → 两侧路径不同 | 改签名为 `control_path(&RuntimeScope)`，让编译器穷举 |
| 旧 socket 残留导致 `AddrInUse` | 复用现有「探测无应答才删」护栏 |
| `$TMPDIR` 与 `instance_home` 分居两文件系统 | 仅在兜底路径出现；正常路径同盘 |
| 数据库仍在项目目录，删目录后 socket 存活 | 本期不改变这一点；`bug-list.md:128` 的目录存在性自停机制仍生效，但需在验收时确认它对 `instance_home/sockets/` 布局依然有效 |

## 7. 为什么本期不改插件 socket

插件查询 socket 由**插件自己** bind（如 `superpowers-kanban` 是独立二进制），
`{state_dir}` 模板由插件清单声明，`yi-agent` 只是消费方。

把它一起改需要：

- 变更 `query_socket_template` 契约里 `{state_dir}` 的含义；
- 已装的插件二进制若按旧模板 bind，会和主程序找不到对方，需要版本协商或兼容期。

这会把「修一个启动故障」扩大成「跨组件契约迁移」。本期记为后续项，前置条件是
插件侧先支持从环境变量读取 socket 目录。

## 8. 完成判据

- [ ] `lifecycle::scope` 提供 `socket_directory` / `guardian_socket` / `daemon_socket`
- [ ] `control_path` 改收 `&RuntimeScope`，10 处消费点全部同源
- [ ] `socket_path_for` 签名不变，内部改用新布局
- [ ] §5 六条测试全绿，且每条都做过「回退即红」验证
- [ ] 本项目真机启动无弹窗，`/agents` 可用
- [ ] `just ci` 全绿
