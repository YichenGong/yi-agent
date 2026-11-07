# 运行时 socket 统一目录设计

日期：2026-11-07
状态：待实施

## 1. 问题

TUI 启动时弹窗「子 Agent runtime 未启用；重启 yi-agent 后即可用」，重启无效。

根因不是配置、不是模型栈、不是 SQLite，而是 **unix socket 路径超出平台上限**：

```
/Users/.../personalProjects/yi-agent/.yi-agent/runtime/guardian-control.sock
= 117 字节
macOS sockaddr_un.sun_path 上限（本仓取 MAX_SOCKET_PATH_BYTES）= 103 字节
```

`bind()` 失败，managed 模式依赖的 guardian 根本起不来。前端等满 30 秒预算后，
把「启动期路径问题」降级显示成那句通用的「重启即可用」。

**但「重启没用」还有一层原因**：用户手上那个二进制根本不含修复。见 §1.1——它不是
配置问题，却是版本问题，所以重启多少次结果都一样。

### 1.1 已经有一层修复在 main 上（重要前提）

**这个弹窗本身在 main 上已被修复**，提交 `7bc611ac`「fix(runtime): keep the guardian
control socket under sun_path」（2026-11-07 20:05）。它给 `lifecycle::scope::control_path`
加了 `$TMPDIR/guardian-<sha256 前 16 位>.sock` 兜底。

**用户报错时跑的是修复前的二进制**，这是「重启也没用」的直接原因：

| 项 | 版本 / 时间 |
|---|---|
| 报错版本 `bce715be` 的 `control_path` | `runtime_dir.join(GUARDIAN_SOCKET_FILE)`，**裸 join，无兜底**（`git show bce715be:.../scope.rs:168`） |
| 已装 `/Applications/yi-agent.app` | `0.1.13-dev+cec4dfc0-dirty`，构建于 19:54，**早于 20:05 的修复** |
| `~/.local/bin/yi-agent-dev` | 同上，构建于 19:52 |
| 修复后二进制（`target/debug`，20:06） | 本项目 `run --subagents` 返回 `OK`，guardian 四阶段全过 |

对照实测（同一项目目录、同一命令）：

```
修复前：Error: managed runtime: managed start refused at prepare: runtime bring-up:
        the managed runtime did not become ready within 30s        ← 用户看到的现象
修复后：guardian 依次通过 prepare → register_business → heartbeat → authorize_root
```

所以**重新构建并安装即可立即消除这个弹窗**。本期 spec 解决的是下一层问题，不是它。

### 1.2 本期要解决的问题：兜底不等于支持长路径

`7bc611ac` 让 bind 成功了，但它不是「支持长路径」，只是「长过阈值后换个地方」：

- **阈值处行为突变**：102 字节能用、104 字节换目录，两套布局并存。本仓当前
  `runtime_dir` 是 95 字节，正好贴在阈值边上——项目再深一层就换布局。
- **`$TMPDIR` 可被系统清理**：macOS 会回收临时目录，socket 文件消失后守护进程成为
  无人能找到的孤儿。`~/.yi-agent` 不会被回收。
- **`$TMPDIR` 里已经堆了 10 个 `guardian-*.sock`**（实测 `ls $TMPDIR/guardian-*.sock`），
  同一个项目在不同拼法/不同实例下历史残留全部可见，排查一次连接问题要在多处找。
- **三类 socket 散在三处**：项目目录 / `$TMPDIR` / 将来的别处。

用户的要求正是「真正支持长路径」，所以本期把 socket 挪到长度恒定的实例目录，
让路径与项目深度彻底无关。

### 1.3 三类 socket 的现状（main @ 332c19a0）

| # | Socket | 现在的位置 | 兜底 | 长度敏感 |
|---|---|---|---|---|
| 1 | 业务 daemon | `<runtime_dir>/runtime.sock` | `$TMPDIR/yi-agent-<hash>.sock` | 是（<103） |
| 2 | guardian | `<runtime_dir>/guardian-control.sock` | `$TMPDIR/guardian-<hash>.sock` | 是（<103） |
| 3 | 插件查询 | `{state_dir}/<plugin>.sock` | `$TMPDIR/plugin-<hash>.sock` | 是（<103） |

本期对第 1、2 类：路径改到 `<instance_home>/sockets/<hash>/`，**并删掉 `$TMPDIR`
兜底**（理由见 §3.2）——新布局下它不该再被触发。第 3 类一期不动。

三者都有兜底了，所以「挂掉」不再是症状；剩下的症状是 §1.2 那四条。本期把 #1 #2
统一到恒定路径，#3 保持现状（理由见 §7）。

### 1.4 实证（2026-11-07）

1. 对本项目 runtime dir 下两个 socket 名直接 `bind`：`guardian-control.sock`(117)
   与 `runtime.sock`(108) 均报 `AF_UNIX path too long`。
2. 手工以 guardian 角色启动（socketpair 装到 fd 8、`setsid`、三个身份环境变量齐备）：
   `Error: the managed guardian stopped: path must be shorter than SUN_LEN`，rc=1。
   `bind_control` 之后的步骤一步都没走到。
3. 前端 trace 逐字记录 `managed runtime: managed start refused at prepare: runtime
   bring-up: the managed runtime did not become ready within 30s`
   （`~/.yi-agent/trace/session-20261107-104546.jsonl` 等 6 个文件）。
4. 同一命令在 `YI_AGENT_BRINGUP=embedded` 下正常返回，managed 下稳定复现超时。
5. 换 20:06 构建（含 `7bc611ac`）后同目录同命令返回 `OK`。

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

- 插件查询 socket（§1.3 第 3 类）保持现状，理由见 §7。
- 不改 `runtime.sqlite`、`runtime.lock` 等非 socket 文件的位置——它们没有长度限制，
  留在项目目录可以继续随项目一起备份/删除。
- 不引入「短路径项目走老布局」的分支：那会让同一项目在不同机器上 socket 位置不同。

## 3. 设计

### 3.1 单一真相源

在 `yi-agent-lifecycle::scope` 新增：

```rust
/// socket 目录：与项目路径长度无关的稳定短路径。
///
/// 两个入参都先规范化，理由见下。
pub fn socket_directory(runtime_dir: &Path, instance_home: &Path) -> PathBuf {
    instance_home
        .join("sockets")
        .join(&digest16(runtime_dir))
}

/// 16 个十六进制字符，取自规范化后的路径。
fn digest16(path: &Path) -> String {
    let canonical = canonicalize_allowing_missing(path).unwrap_or_else(|_| path.to_path_buf());
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}
```

**两个入参都必须规范化，这是本设计的承重条件，不是防御性编程。**

*`runtime_dir` 的规范化：* macOS 上 `/var/...` 与 `/private/var/...` 是同一目录的
两种拼法，直接哈希原始字符串会让同一个项目算出两个目录。实测：

```
/var/folders/.../.yi-agent/runtime        -> 2079023ec68df46a
/private/var/folders/.../.yi-agent/runtime -> 0baed1ca4c1afe33   ⚠ 不同
```

这不是假想风险：`lifecycle::scope::control_path`（`:200-210`）**已经**在兜底分支里
做了 `canonicalize_allowing_missing`，注释还专门记下了这个教训。新函数必须继承，
否则等于把一个修过的 bug 重新引入。

*`instance_home` 的规范化：* 把规范化放在 `socket_directory` **内部**，而不是要求
每个调用方自己先规范——因为调用方有三条来源，规范化程度不一：

| 来源 | 是否 canonicalize |
|---|---|
| `lifecycle::scope::resolve()`（`scope.rs:40`） | 是 |
| `store::paths::instance_home()`（`paths.rs:20`） | **否**，只做 `YI_AGENT_HOME` 归一化 |
| `YI_AGENT_HOME` 原样传入 | 否 |

`YI_AGENT_HOME=/var/...` 时前者给 `/private/var/...`、后两者给 `/var/...`，socket
目录随之分叉，guardian bind 一处、客户端连另一处——症状与本 bug 一模一样。在函数
内部规范一次，三条来源自动一致，调用方不需要知道这件事。

`canonicalize_allowing_missing` 是 `pub`（`scope.rs:56`），同文件内直接调用。

放 `lifecycle` 的理由是依赖方向：`store → lifecycle` 单向（`store/Cargo.toml:14`），
`lifecycle` 已依赖 `sha2`（`Cargo.toml:17`），且它承诺不拉 SQLite / 模型栈。
所有 socket 相关 crate 都已（直接或传递）依赖 `lifecycle`。

同时给两类 socket 各一个具名构造函数，避免调用方各自拼文件名：

```rust
pub fn guardian_socket(runtime_dir: &Path, instance_home: &Path) -> PathBuf;
pub fn daemon_socket(runtime_dir: &Path, instance_home: &Path) -> PathBuf;
```

### 3.2 长度兜底

`<instance_home>` 本身理论上也可能被指到深路径，但**本期去掉 `$TMPDIR` 兜底**，
只保留「放不下就报明确错误」这一条：

| `instance_home` | guardian socket 长度 | |
|---|---|---|
| `/Users/<user>/.yi-agent`（27） | 74 | OK，余量 29 |
| `/root/.yi-agent`（15） | 62 | OK，余量 41 |
| `/home/<user>/.yi-agent`（19） | 66 | OK，余量 37 |

上限推算：`instance_home` 可以长到 **56 字节**才触顶，这已经覆盖了正常机器和容器的
所有情况。

去掉的理由：

1. **兜底本身是个新的失败面。** 它把「路径太长」从一处（项目目录)搬到一个谁都不
   预期的地方，且只在异常输入下才走到，测试覆盖天然薄弱。
2. **`$TMPDIR` 是可被系统回收的目录。** 这正是 §1.2 要摆脱的性质；把它留成兜底，
   等于保留一条「socket 落在会被清理的地方」的路径。
3. **去掉它才能删掉 `socket_file_name`。** 该函数（`client.rs:94`）唯一的用途就是
   生成兜底文件名，而它直接哈希原始入参、不做规范化（§3.1 的 `/var` 隐患）。
   删掉它比修它更彻底。
4. **一人一套实例目录时不会触发。** `YI_AGENT_HOME` 是为隔离实例而设的开关
   （见项目打包流程），它的值由用户显式给出，不该是深路径。

行为：超限时 `control_socket` 返回 `SocketPathTooLong`，`socket_path_for` 返回
`IpcError::SocketPathTooLong`（两者都已存在），由 bring-up 转成 §4 的可读提示。

现成的错误类型不要重复造：`scope::control_socket(runtime_dir)`（`scope.rs:216`）
已经实现了「返回路径，超限则给 `SocketPathTooLong`」。guardian 侧继续走它，只把
内部的路径计算换掉；`socket_path_for`（`client.rs:74`）同理。

**注意**：去掉兜底会让 §6.1 里那条 `a_long_runtime_directory_yields_a_socket_that_fits_the_platform_limit`
彻底失效——它断言长路径**能**解析出可用 socket。新语义是「长路径无所谓，因为
socket 不再在项目里」，所以这条用例要改写而不是删除，见 §6.1。

### 3.3 调用点：只有一个 choke point，不需要改签名

`7bc611ac` 已经把全部控制 socket 解析统一到 `lifecycle::scope::control_path`。实测
生产代码里只有 7 处引用，且全部经过这一个函数：

| 位置 | 现状 |
|---|---|
| `supervisors/src/lifecycle/guardian.rs:2139` | `control_socket(&scope.runtime_dir)` —— bind，唯一的 bind 方 |
| `supervisors/src/lifecycle/bringup.rs:230` | 转发 `control_path(&scope.runtime_dir)` |
| `supervisors/src/lifecycle/admission.rs:36` | `control_path(runtime_dir)` |
| `supervisors/src/lifecycle/legacy_evidence.rs:27` | `use` 后间接调用 |
| `supervisors/src/lifecycle/migration_operation.rs:915` | 经 `bringup::control_path` |
| `supervisors/src/lifecycle/switch_managed.rs:404` | 经 `bringup::control_path` |
| `app-server/src/rpc/runtime.rs:50` | `control_path(&runtime_dir)` |
| `subagent/src/foreground.rs:645` | `control_path(runtime_dir)` |
| `subagent/src/thread_root.rs:829` | `control_path(&runtime_dir)` |
| `yi-agent/src/frontend_lease.rs:285` | 转发 `control_path(runtime_dir)` |

（后 3 行的行为一致；`thread_root.rs:1348/1530` 剩下的是测试 fixture，不是生产路径。）

**因此不改 `control_path` 的签名。** 早先设想过改收 `&RuntimeScope` 靠编译器穷举，
在 10 处未收敛时是对的；现在只有一个实现体，改签名会把「一处实现」变成「十个调用方
各自决定 `instance_home`」，正是要避免的分歧来源。

做法：`control_path(runtime_dir)` 签名保持不变，内部改成
`guardian_socket(runtime_dir, &instance_home_or_default())`。调用方零改动，
`guardian.rs` 的 `control_socket` 同理。

测试侧需要同步的只有断言旧布局的用例，见 §6.1。

### 3.4 业务 socket 调用点

`socket_path_for(runtime_dir)` 有 20+ 调用点（含测试）。业务 socket 的调用方大多
只有 `runtime_dir`，没有 `instance_home`。

处理方式：`store::ipc::socket_path_for` **保持签名不变**，内部改为
`socket_directory(runtime_dir, &instance_home_or_default())` 拼 `runtime.sock`。
这样 20+ 调用点一行不改。

`instance_home` 由 `store::paths::instance_home()`（`paths.rs:20`）提供。它不做
canonicalize，但 §3.1 已把规范化放进 `socket_directory` 内部，所以这里**不需要**
额外处理——这正是把规范化放函数内而不是放调用方的理由。

`instance_home()` 返回 `None`（既无 `YI_AGENT_HOME` 也无 `HOME`）时不能直接报错——
那是原先能工作的场景。既然 `$TMPDIR` 兜底一并去掉，这里改为返回
`IpcError::SocketPathTooLong` 之外的独立错误（`instance home unavailable`），
而不是悄悄落回临时目录。实测这个分支在正常环境不可达，只为不 panic。

可行性已核对：`store` 已有 `paths::instance_home()`（`paths.rs:20`），且 `store` 已
依赖 `lifecycle`（`Cargo.toml:14`）——不新增依赖边。

**`socket_file_name` 直接删除**。它（`client.rs:94`）唯一的用途是生成 `$TMPDIR`
兜底文件名，§3.2 去掉兜底后没有调用者。它本身还有不规范的哈希（直接哈希原始入参，
在 `/var` vs `/private/var` 下给出两个名字），删掉比修它干净。`socket_path_for` 里
引用它的两行（`:79`、`:94`）一并去掉。

### 3.4.1 测试侧影响

`socket_path_for` 改用 `instance_home()` 之后，socket 路径**不再跟随 `runtime_dir`
的父目录**，只跟它的哈希有关。仓里 130 处 `Daemon::start` 调用绝大多数传
`TempDir::new().unwrap().path().join("runtime")`：

```rust
let directory = TempDir::new().unwrap();
let runtime = directory.path().join("runtime");
let daemon = Daemon::start(&runtime, &database).unwrap();
```

改布局后，这些 socket 会落进**开发者真实的 `~/.yi-agent/sockets/<hash>/`**
（`YI_AGENT_HOME` 未设置时）。实际后果，按严重度排列：

1. **残留空目录（一定会发生）。** `TempDir` 删掉的是项目侧目录，删不到
   `~/.yi-agent/sockets/<hash>/`。daemon 退出时会 unlink socket 文件
   （`remove_socket_if_same_inode`，`daemon.rs:526`），但**不会删目录**。所以跑
   一次 `just test` 会在 `~/.yi-agent/sockets/` 留下几十上百个空目录。
   — 无害，但会让人误以为有泄漏。**处理：在 `Daemon::start` 的退出路径里，若
   socket 目录为空则尝试 `remove_dir`，失败忽略。** 这一条不改测试写法，改实现。
2. **写死路径的用例会互相踩（只影响少数）。** `TempDir` 保证 runtime_dir 唯一，
   所以哈希唯一、socket 唯一——这类用例是安全的。真正有风险的是拿**同一个写死
   字符串**作 runtime_dir 的用例：它们现在争同一个 socket 文件。实测只有
   `ipc/client.rs:379` 的 `LONG_RUNTIME_DIR` 是写死的，而它只被 `socket_path_for`
   的**纯函数**测试使用，不 bind，所以当前零冲突。
   — **处理：不需要引入 `instance_home` 注入参数。** 只要守住「测试不 bind 写死
   路径」这条现状即可。
3. **`std::env::set_var` 不能用来做隔离。** edition 2024 下它是 `unsafe`，且
   `runtime_ipc.rs` 的用例在进程内并行跑。`maintenance_ipc.rs:33` 已经用全局
   `Mutex` + `unsafe set_var` 做过这件事，不要再复制这种写法到别处。

**结论：不做「给 `Daemon::start` 加 `instance_home` 参数」的改造。** 早先版本的
spec 提出过这个方案，但它把生产签名为了测试而变复杂，而实际问题只有第 1 条
（空目录残留），改实现即可解决。

验收方式见 §5 第 10 条：跑完测试后 `~/.yi-agent/sockets/` 不应留下新目录。

### 3.5 目录创建与权限

新布局引入了一个中间目录 `~/.yi-agent/sockets/<hash>/`，现有代码**没有任何地方
创建它**——`bind_control`（`guardian.rs:2139`）只 `create_dir_all(&self.scope.runtime_dir)`（`:2142`），
而 daemon（`ipc/daemon.rs:294`）只建 runtime_dir 并设 0700。直接换成新路径会得到
`ENOENT`，比现在更糟（现在是超长，新的是目录不存在）。

因此路径构造函数只算路径，`bind` 之前由调用方保证目录存在：

| 位置 | 要加的代码 |
|---|---|
| `guardian.rs:bind_control` | 在现有 `create_dir_all(&self.scope.runtime_dir)`（`:2142`）之后加 `create_dir_all(socket_directory(...))` + `set_permissions(0o700)` |
| `ipc/daemon.rs:294` 附近 | 同上，与 runtime_dir 的 0700 处理并列 |

权限沿用现状，不新增约定：

- 目录 `0o700`——`runtime_dir` 已经这么设（`daemon.rs:295`）；socket 目录含可达
  `runtime_dir` 的哈希，不泄露路径，但同用户私有是既有基线。
- socket 文件 `0o600`——`guardian.rs:2199` 已对 control socket 设置。业务 socket 不设（见下条）。
- 业务 socket 目前未显式设权限（继承 umask）。既然搬进 0700 目录，实际访问已被
  目录权限挡住，**本期不为它新增 chmod**，避免扩大改动面。

新增目录不必清理：每个项目一个 `<16hex>` 子目录，长期累积量级是项目数，
不会无限增长。确实要清时 `rm -rf ~/.yi-agent/sockets` 是安全的（都是运行期产物）。

### 3.6 旧位置清理

老布局的 socket 文件会留在 `<runtime_dir>/` 下。处理：

- **不需要写清理代码**。`bind_control` 的护栏（`guardian.rs:2148-2190`）只在**新路径**
  上生效，旧路径的文件不会被自动删除——但它也不会造成故障：旧路径不再被 bind，也
  不再被任何客户端连接，只是一个死文件。
- 因此本条**不做**：为一堆一次性残留文件引入删除逻辑，风险（误删别的实例仍在使用
  的 socket）大于收益。
- 用户若想清，`rm .yi-agent/runtime/*.sock` 即可，无害。

原先设想的"复用 `(dev,ino)` 护栏清理旧 socket"不成立：那套护栏保护的是
`ipc/client.rs` 里**当前正在用的** socket 的退出清理，与旧位置的死文件无关。
把它挪来删别的路径反而是新的风险面。

## 4. 错误处理

三层各自的错误行为：

| 场景 | 行为 |
|---|---|
| `socket_directory` 能放下（正常） | 无错误 |
| `instance_home` 使得路径超限（>56 字节） | `control_socket` / `socket_path_for` 返回 `SocketPathTooLong`，由 bring-up 转成可读拒绝 |
| `instance_home()` 返回 `None` | 返回独立错误，不落回临时目录 |

第 3 行的用户可见文案（替换现有的 30 秒超时弹窗）：

> 无法为这个项目创建 runtime socket：实例目录太长（`<path>`，上限 103 字节）。
> 请把 `YI_AGENT_HOME` 指向更短的目录。

**注意旧文案不该再出现**。`busy` 弹窗里的「重启 yi-agent 后即可用」在当前故障下
是错的建议——重启只会重复同样的失败。测试要覆盖这一点：超长场景断言错误信息里
不含"重启"，含 `YI_AGENT_HOME`。

另外，`socket_directory` 不再依赖项目路径长度，所以"把项目移到更短的路径"这条建议
**已不再是解法**，文案里应当去掉，否则误导用户做大量无用功。

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
5. **超限失败**：把 `instance_home` 指到 57 字节以上，断言返回 `SocketPathTooLong`
   而非 panic、也**不**落回 `$TMPDIR`。去掉兜底后，这条是「不静默降级」的钉子。
6. **规范化等价（`/var` 陷阱）**：对同一目录的 `/var/...` 与 `/private/var/...` 两种
   拼法，断言 `socket_directory` 返回相同结果。把 §3.1 里的
   `canonicalize_allowing_missing` 去掉即红——这条是本轮评审新增，没有它，
   `/var` 下的项目会静默分裂成两个 socket 目录。
7. **instance_home 三源等价**：`YI_AGENT_HOME` 设成同一个目录的 `/var/...` 与
   `/private/var/...` 两种拼法，断言 `socket_directory` 结果相同；再断言
   `stores::paths::instance_home()`（不 canonicalize）与 `scope::resolve()`
   （canonicalize）两条来源经 `socket_directory` 后算出同一路径。把 §3.1 里
   `instance_home` 的规范化去掉即红。
8. **目录自动创建**：`sockets/<hash>/` 不存在时，`bind_control` 与 daemon 启动都能
   自己建出来并 bind 成功。删掉 §3.5 要求的 `create_dir_all` 即红——这条是"新布局
   比旧故障更糟"的防线。
9. **错误文案**：`instance_home` 超限时，断言错误信息含 `YI_AGENT_HOME` 且**不含**
   "重启"。这条直接钉住 §4 的表述。
10. **测试不留垃圾**：跑完 `cargo test -p yi-agent-store` 后，断言
    `~/.yi-agent/sockets/` 下没有新增**空目录**。这条防的是 §3.4.1 第 1 条——
    它不会让任何测试变红，只会悄悄往开发者 home 里堆目录。
11. **真机验收**：本项目启动后 socket 出现在 `~/.yi-agent/sockets/<hash>/`，
    `/agents` 可用。

## 6. 风险

| 风险 | 处理 |
|---|---|
| 未来有人新增调用点绕过 `control_path` 直接 join | §5 第 3 条断言五处同源；新增点若绕过会立刻红 |
| 旧 socket 残留导致 `AddrInUse` | 复用现有「探测无应答才删」护栏 |
| `/var` vs `/private/var` 使同一项目算出两个 socket 目录 | 哈希输入先 `canonicalize_allowing_missing`；见 §3.1、§3.4 |
| `store::paths::instance_home()` 与 `scope.instance_home` 规范化程度不同 | 规范化收在 `socket_directory` 内部，三条来源自动一致；见 §3.1 |
| 数据库仍在项目目录，删目录后 socket 存活 | **已核实不受影响**：自停判据是 `watched_runtime_dir.exists()`（`ipc/daemon.rs:442`），监视的是项目内那个含 `runtime.sqlite` 的目录，与 socket 落在哪里无关。`bug-list.md:128` 的机制仍生效，`runtime_ipc.rs:2893` 的用例（它显式断言 socket 不在 runtime_dir 内）在改布局后仍然绿 |

### 6.1 已知需要同步修改的既有测试

布局改变会让「socket 位于 runtime_dir 内」这一类断言语义失效，实施时必须逐条确认，
而不是把它们当噪音改掉：

| 用例 | 现在的断言 | 改布局后 |
|---|---|---|
| `store/src/ipc/client.rs:382` `a_short_runtime_directory_keeps_the_socket_inside_it` | 短路径仍在 `runtime_dir` 内 | **语义反转**：改为断言恒在 `sockets/<hash>/` 下，与路径长短无关 |
| `store/src/ipc/client.rs:394` `a_long_runtime_directory_yields_a_socket_that_fits_the_platform_limit` | 长路径回退到 `$TMPDIR`，仍可用 | **语义反转**：删掉兜底后长路径不再有特殊处理，改为断言「无论 runtime_dir 多长，socket 都在 `sockets/<hash>/`」 |
| `store/tests/runtime_ipc.rs:2981` | `!runtime.join("runtime.sock").exists()` | 位置变了，断言需指向新路径；该断言的本意是「socket 已清理」，不要改成空断言 |
| `supervisors/tests/managed_bringup.rs:217` | `control_path.ends_with("guardian-control.sock")` | 文件名不变，应仍然绿——若红说明实现偏了 |

`store/src/ipc/client.rs:379` 的 `LONG_RUNTIME_DIR` 硬编码了本机绝对路径，与当前
故障项目一致。实施时改成构造式（按需拼到超长）更稳，但它作为回归样本有价值，
建议保留硬编码值并注明来源。

## 7. 为什么本期不改插件 socket

插件查询 socket 由**插件自己** bind（如 `superpowers-kanban` 是独立二进制），
`{state_dir}` 模板由插件清单声明，`yi-agent` 只是消费方。

把它一起改需要：

- 变更 `query_socket_template` 契约里 `{state_dir}` 的含义；
- 已装的插件二进制若按旧模板 bind，会和主程序找不到对方，需要版本协商或兼容期。

这会把「修一个启动故障」扩大成「跨组件契约迁移」。本期记为后续项，前置条件是
插件侧先支持从环境变量读取 socket 目录。

## 8. 完成判据

- [ ] `lifecycle::scope` 提供 `socket_directory` / `guardian_socket` / `daemon_socket`，
      且哈希输入先规范化
- [ ] `control_path` / `control_socket` 签名不变，内部改用新布局（调用方零改动）
- [ ] `socket_path_for` 签名不变，内部改用 `daemon_socket(runtime_dir, &instance_home())`
- [ ] `bind_control` 与 daemon 在 bind 前创建 `sockets/<hash>/` 并设 0700
- [ ] §6.1 列出的 4 个既有用例已逐条确认语义（不是简单改成能过）
- [ ] §5 十一条测试全绿，且每条都做过「回退即红」验证
- [ ] §3.4.1 已处理：daemon 退出时清空 `sockets/<hash>/`，测试不留空目录
- [ ] 本项目真机启动：socket 出现在 `~/.yi-agent/sockets/<hash>/`，`/agents` 可用
- [ ] `just ci` 全绿

### 8.1 不属于本期判据

用户报的那个弹窗由 `7bc611ac` 修掉，判据是「装 20:05 之后的构建」。本期开工前应
先确认当前安装的二进制已含该提交，否则会把「没装新版本」误判成「本期没做成」。
验证方法：`strings $(command -v yi-agent) | grep -c guardian-` 非零，或直接看
`yi-agent --version` 是否比 `cec4dfc0` 新。
