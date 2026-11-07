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
- **三类 socket 散在不同地方**：项目目录 / `$TMPDIR` / 插件声明的 `{state_dir}`。
  本期先把 #1 #2 收进一个地方，#3 仍是例外（§7）。

用户的要求正是「真正支持长路径」，所以本期把 socket 挪到长度恒定的实例目录，
让路径与项目深度彻底无关。

### 1.3 三类 socket 的现状（main @ 332c19a0）

| # | Socket | 现在的位置 | 兜底 | 长度敏感 |
|---|---|---|---|---|
| 1 | 业务 daemon | `<runtime_dir>/runtime.sock` | `$TMPDIR/yi-agent-<hash>.sock` | 是（<103） |
| 2 | guardian | `<runtime_dir>/guardian-control.sock` | `$TMPDIR/guardian-<hash>.sock` | 是（<103） |
| 3 | 插件查询 | `{state_dir}/<plugin>.sock` | `$TMPDIR/plugin-<hash>.sock` | 是（<103） |

三者都有兜底，所以「挂掉」不再是症状；剩下的症状是 §1.2 那四条。本期把 #1 #2
统一到 `<instance_home>/sockets/<hash>/` 并**删掉 `$TMPDIR` 兜底**（§3.2），
#3 保持现状（§7）。

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

一个边界要知道：workdir 本身仍受 `PATH_MAX`（macOS 1024）约束，超长时 `create_dir_all`
会先失败。那是**项目目录**的限制，与 socket 无关——本设计只保证「socket 不再额外吃掉
sun_path 的 103 字节预算」，不承诺任意深度的项目目录可用。§5 第 1 条用 200/500/900
正好避开这条边界（900 + `/.yi-agent/runtime` ≈ 919 < 1024）。

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
    canonicalize_or_self(instance_home)
        .join("sockets")
        .join(&digest16(runtime_dir))
}

/// 16 个十六进制字符，取自规范化后的路径。
fn digest16(path: &Path) -> String {
    let digest = Sha256::digest(canonical_or_self(path).as_os_str().as_encoded_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn canonical_or_self(path: &Path) -> PathBuf {
    canonicalize_allowing_missing(path).unwrap_or_else(|_| path.to_path_buf())
}
```

**两个入参都必须规范化，这是本设计的承重条件，不是防御性编程。**

*`runtime_dir` 的规范化：* macOS 上 `/var/...` 与 `/private/var/...` 是同一目录的
两种拼法，直接哈希原始字符串会让同一个项目算出两个目录。实测：

```
/var/folders/6j/2p7261x15r58tyqc4zwlf2_m0000gn/T/.yi-agent/runtime         -> 7227763425bcbd38
/private/var/folders/6j/2p7261x15r58tyqc4zwlf2_m0000gn/T/.yi-agent/runtime -> 8b62dfd465df5c9d   ⚠ 不同
```

（上为本机 2026-11-07 实算值：`sha256(path)[..16]`。同一目录、两种拼法，两个哈希。）

这不是假想风险：`lifecycle::scope::control_path`（`:194-211`，规范化在 `:207`）**已经**在兜底分支里
做了 `canonicalize_allowing_missing`，注释还专门记下了这个教训（见下面 `scope.rs` 的
注释原文：两套拼法会让 guardian bind 一个、客户端轮询另一个，症状正是这个 bug）。
新函数必须继承，否则等于把一个修过的 bug 重新引入。

**注意迁移方向**：老代码只在「已经超长、走兜底」时才规范化，因为短路径直接
`runtime_dir.join(...)`、无需哈希。新布局**每条路径都要哈希**，所以规范化必须
无条件执行——不能照搬「在兜底分支里 canonicalize」的结构。`socket_directory`
（§3.1 开头）就是无条件规范化后的版本。

*`instance_home` 的规范化：* 把规范化放在 `socket_directory` **内部**，而不是要求
每个调用方自己先规范——因为调用方有三条来源，规范化程度不一：

| 来源 | 是否 canonicalize |
|---|---|
| `lifecycle::scope::resolve()`（`scope.rs:39`） | 是 |
| `store::paths::instance_home()`（`paths.rs:20`） | **否**，只做 `YI_AGENT_HOME` 归一化 |
| `YI_AGENT_HOME` 原样传入 | 否 |

`YI_AGENT_HOME=/var/...` 时前者给 `/private/var/...`、后两者给 `/var/...`，socket
目录随之分叉，guardian bind 一处、客户端连另一处——症状与本 bug 一模一样。在函数
内部规范一次，三条来源自动一致，调用方不需要知道这件事。

`canonicalize_allowing_missing` 是 `pub`（`scope.rs:58`），同文件内直接调用。

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

布局是 `<instance_home>/sockets/<16 hex>/guardian-control.sock`，`instance_home`
之后的固定开销是 47 字节（`/sockets/` 9 + 哈希 16 + `/` 1 + `guardian-control.sock`
21）。三个典型位置的实测值（`MAX_SOCKET_PATH_BYTES = 103`，`client.rs:62`）：

| `instance_home` | 字节数 | guardian socket 总长 | 余量 |
|---|---|---|---|
| `/Users/gongyichen/.yi-agent` | 27 | 74 | 29 |
| `/root/.yi-agent` | 15 | 62 | 41 |
| `/home/alice/.yi-agent` | 21 | 68 | 35 |

**阈值：`instance_home` 长到 56 字节时触顶**（103 − 47）。这已覆盖正常机器与容器的
所有情况；能越过的只有用户显式把 `YI_AGENT_HOME` 指到极深处，那种情况下报错比静默
改地址更诚实。

daemon 的 `runtime.sock` 尾部是 `/sockets/` 9 + 哈希 16 + `/` 1 + `runtime.sock` 12
= 38 字节，同样场景下总长 65（余量 38），比 guardian 更宽松——所以**约束由 guardian
决定**，`socket_path_for` 的阈值检查不是多余的，只是不会先触发。

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

现成的错误类型不要重复造：`scope::control_socket(runtime_dir)`（`scope.rs:219`）
已经实现了「返回路径，超限则给 `SocketPathTooLong`」。guardian 侧继续走它，只把
内部的路径计算换掉；`socket_path_for`（`client.rs:74`）同理。

**注意**：去掉兜底会让 §6.1 里那条 `a_long_runtime_directory_yields_a_socket_that_fits_the_platform_limit`
彻底失效——它断言长路径**能**解析出可用 socket。新语义是「长路径无所谓，因为
socket 不再在项目里」，所以这条用例要改写而不是删除，见 §6.1。

### 3.3 调用点：只有一个 choke point，不需要改签名

`7bc611ac` 已经把全部控制 socket 解析统一到 `lifecycle::scope::control_path`。实测
生产代码（不含 `#[cfg(test)]`）里 `control_path` / `control_socket` 的**解析点**共
14 处、分布在 8 个文件，全部经过这两个函数：

| 位置 | 现状 |
|---|---|
| `supervisors/src/lifecycle/guardian.rs:2139` | `control_socket(&scope.runtime_dir)` —— bind，唯一的 bind 方 |
| `supervisors/src/lifecycle/bringup.rs:230` | `control_path(&scope.runtime_dir)`（定义） |
| `supervisors/src/lifecycle/bringup.rs:269`、`:335` | 经本地 `control_path(scope)` 两次解析 |
| `supervisors/src/lifecycle/admission.rs:36` | `control_path(runtime_dir)` |
| `supervisors/src/lifecycle/legacy_evidence.rs:527` | `use` 后调用（`:27` 是 `use` 行） |
| `supervisors/src/lifecycle/migration_operation.rs:915` | 经 `bringup::control_path` |
| `supervisors/src/lifecycle/switch_managed.rs:404` | 经 `bringup::control_path` |
| `app-server/src/rpc/runtime.rs:50` | `control_path(&runtime_dir)` |
| `subagent/src/foreground.rs:645` | `control_path(runtime_dir)` |
| `subagent/src/thread_root.rs:829` | `control_path(&runtime_dir)` |
| `yi-agent/src/frontend_lease.rs:64`、`:101` | 经本地 `control_path(runtime_dir)` 两次解析 |
| `yi-agent/src/frontend_lease.rs:285` | 本地 `control_path` 的定义（转发到 `scope::control_path`） |

`bringup.rs:82/105/196/206/211/517/521/524` 与 `app-server/src/runtime_status.rs:519`
等处出现的 `control_path` 是**结构体字段名 / 函数参数名**，不是解析调用——
`lifecycle/src/client.rs` 里也有同名的 `ControlClient` 字段（`:193`）与参数（`:228`）。
改名时不要把它们一起动。

（`thread_root.rs:1348/1530` 剩下的 `runtime_dir.join(...)` 在 `#[cfg(test)]` 的
fixture 里，不是生产路径，本期不动。）

已验证「唯一 choke point」不是估计：`rg 'GUARDIAN_SOCKET_FILE|guardian-control'` 在
生产代码里只有 `scope.rs:195`（`control_path` 内部）一处拼接，其余命中全在
`#[cfg(test)]` 与 `lifecycle/src/client.rs` 的测试（`:1021/:1060/:1136`）。也就是说
改 `control_path` 的实现体会同时覆盖上表全部解析点，不存在绕过它的生产路径。

**因此不改 `control_path` 的签名。** 早先设想过改收 `&RuntimeScope` 靠编译器穷举，
在解析点未收敛时那是对的；现在只剩两个实现体（`control_path` 与 `control_socket`），
改签名会把「一处实现」变成「十几个调用方各自决定 `instance_home`」，正是要避免的
分歧来源。

做法：`control_path(runtime_dir)` **签名与返回类型都不变**（`PathBuf`），内部改成
`guardian_socket(runtime_dir, &instance_home_for_sockets())`。

**`control_path` 不引入 `Result` 的理由**：它有 14 个调用点，其中
`GuardianAdmission::new`（`admission.rs:32`）返回 `Self`、`bringup::control_path`
（`bringup.rs:229`）返回 `PathBuf`、`runtime.rs:50` 在 `async fn` 里——改成 `Result`
会迫使这些签名连锁变化，而**唯一真正 bind 的一方（guardian）根本不走这条路**
（下面一段）。

所以 `instance_home_for_sockets()` 返回 `Option<PathBuf>`（定义见 §3.5），
`control_path` 在 `None` 时**不 panic、不返回 `$TMPDIR`**，而是：

```rust
let home = instance_home_for_sockets().unwrap_or_else(|| {
    // 只用于诊断展示：三源全缺失在正常机器上不可达（launchd / 容器 / SSH 都设 HOME）。
    // 这里给一个有意义的占位，让日志能说清"不知道实例根在哪"，而不是假装知道。
    tracing::error!("instance home is unknown; socket path is a placeholder");
    PathBuf::from("<unknown-instance-home>")
});
guardian_socket(runtime_dir, &home)
```

`<unknown-instance-home>` 含 `<` / `>`，**任何 `bind` 都会失败**（非法路径字符
未必，但目录不存在必然 `ENOENT`），所以它不会变成一个能连上却被误用的 socket。
这比引入 `$TMPDIR` 兜底好：后者会「看起来能用」，掩盖配置错误。

**真正 bind 的一方也根本不走这条路**：`bind_control` 手上有
`self.scope.instance_home`（§3.5 情况 A），直接调
`guardian_socket(&scope.runtime_dir, &scope.instance_home)`，不经过 `control_path`、
不读 env。

`socket_path_for` 不同：它返回 `Result`，且确实被用来 bind（daemon 侧）。所以它用
`InstanceHomeUnavailable` 显式报错，而不是静默兜底。

测试侧需要同步的只有断言旧布局的用例，见 §6.1。

### 3.4 业务 socket 调用点

`socket_path_for(runtime_dir)` 实测 44 个调用点（13 个文件的非测试代码 + 测试），
业务 socket 的调用方大多只有 `runtime_dir`，没有 `instance_home`。

处理方式：`store::ipc::socket_path_for` **保持签名不变**，内部改为先取
`instance_home_for_sockets().ok_or(IpcError::InstanceHomeUnavailable)?`，
再 `daemon_socket(runtime_dir, &home)`。这样 44 个调用点一行不改。
`instance_home_for_sockets` 的定义与优先级见 §3.5。

`instance_home` 的来源不是直接调 `store::paths::instance_home()`——那样会让 guardian
起的 daemon 与它的 launcher 分叉，见 §3.5。规范化的责任在 `socket_directory` 内部
（§3.1），所以这里**不需要**额外处理。

三个来源都取不到时（`YI_AGENT_INSTANCE_HOME`、`YI_AGENT_HOME`、`HOME` 全无）
返回独立错误 `IpcError::InstanceHomeUnavailable`，**不**落回临时目录。这与
`control_path` 的占位路径处理（§3.3）不同是**有意的**：`socket_path_for` 的结果
会被 daemon 拿去 bind，静默给一个假路径只会把错误推到更难懂的地方（`ENOENT`
而不是「实例根未知」）。

可行性已核对：`store` 已依赖 `lifecycle`（`Cargo.toml:14`），所以
`socket_path_for` 能直接调 `lifecycle::scope` 的 `instance_home_for_sockets` /
`daemon_socket`——不新增依赖边。反向不行（`lifecycle` 不依赖 `store`），这也决定了
`instance_home_for_sockets` 必须住在 `lifecycle`（§3.5）。

**`socket_file_name` 直接删除**。它（`client.rs:94`）唯一的用途是生成 `$TMPDIR`
兜底文件名，§3.2 去掉兜底后没有调用者。它本身还有不规范的哈希（直接哈希原始入参，
在 `/var` vs `/private/var` 下给出两个名字），删掉比修它干净。`socket_path_for` 里
引用它的两行（`:79`、`:94`）一并去掉。

### 3.5 instance_home 的跨进程一致性（本设计最危险的假设）

新布局让 socket 路径多了一个输入：`instance_home`。此前两侧只要同意 `runtime_dir`
就能找到对方；现在还必须同意 `instance_home`。而进程间取它的方式**不一样**：

| 进程 | 从哪取 instance_home |
|---|---|
| 前端 / launcher（TUI、`run`、app-server） | `store::paths::instance_home()` → 读 **`YI_AGENT_HOME`** |
| guardian | `main.rs:448` 读 **`YI_AGENT_INSTANCE_HOME`**（launcher 在 `bringup.rs:627` 显式传入） |
| guardian 起的 daemon | 同一个 `YI_AGENT_INSTANCE_HOME`（`guardian.rs:1523`）；**不传 `YI_AGENT_HOME`** |

现状为什么不出问题：`paths::instance_home()` 与 launcher 传给 guardian 的
`scope.instance_home` 在**正常环境**下是同一个目录。

但新布局把这个假设变成承重的，而它有两处会破：

1. **`YI_AGENT_HOME=/var/foo`（未规范化）。** launcher 侧规范化后得
   `/private/var/foo/...`，guardian 拿到的是 `scope.instance_home`（`resolve()` 已
   canonicalize，`scope.rs:39`），两者相同——**这一层 §3.1 的规范化已经兜住**。
2. **`YI_AGENT_HOME` 靠继承，`YI_AGENT_INSTANCE_HOME` 靠显式传递。** guardian 起的
   daemon 只被显式设了 `YI_AGENT_INSTANCE_HOME`（`guardian.rs:1523`），
   `YI_AGENT_HOME` 是**继承**来的——`spawn` 不清环境（`guardian.rs:461` 只
   `process.env(key, value)` 追加，全文无 `env_clear`）。这条继承不是巧合，是被
   代码明确依赖的：`managed_runtime.rs:407-410` 的注释写着「instance's
   configuration is inherited from the process that started us ... a shared daemon
   must keep reading the same `YI_AGENT_HOME`」。

   所以只要 launcher 的 `YI_AGENT_HOME` 与它传给 guardian 的
   `scope.instance_home` 指同一目录，全链路一致——**在正常环境下两者确实来自同一个
   输入**（`managed_startup.rs:222` 就是拿 `paths::instance_home()` 去 resolve）。
   风险在于这条一致性目前是**隐式**的：没有任何断言把它固定住，而新布局让
   socket 路径依赖它。

**结论分两种情况，取决于调用方手上有没有准确值。**

**情况 A：手上有 `RuntimeScope`（guardian 及其下游）。** 直接用
`scope.instance_home`，**完全不读环境变量**。`RuntimeScope` 已含
`instance_home: PathBuf`（`types.rs:32`），且 `resolve()` 里已 canonicalize
（`scope.rs:39`）。所以：

```rust
// guardian.rs:bind_control —— 已经有 self.scope
let path = yi_agent_lifecycle::scope::guardian_socket(
    &self.scope.runtime_dir,
    &self.scope.instance_home,   // 不是读 env
);
```

daemon 由 guardian 起，`YI_AGENT_INSTANCE_HOME` 由 guardian 显式传入
（`guardian.rs:1523`），这正是 `scope.instance_home` 的来源——所以**整条 managed
链路根本不需要 §3.5 的 env 优先级兜底**，它只在情况 B 生效。

**情况 B：手上只有 `runtime_dir`（前端、`socket_path_for` 的多数调用方）。**
这时才需要 env 兜底。优先读 `YI_AGENT_INSTANCE_HOME`（进程内已有显式声明时用它），
否则退回 `YI_AGENT_HOME`，再退回 `$HOME/.yi-agent`：

```rust
/// socket 路径要用的 instance_home。三个来源按优先级取第一个可用的，
/// 全缺失时返回 `None`，由调用方转成 `InstanceHomeUnavailable`。
///
/// 放在 `lifecycle::scope`：`store` 已经依赖 `lifecycle`，反向不行；而这个函数
/// 两边都要用（`store::ipc::socket_path_for` 与 `lifecycle::scope::control_path`）。
/// `lifecycle` 已经在读环境变量（`scope.rs:168` 读 `YI_AGENT_RUNTIME_DIR`），
/// 不新增依赖。
pub fn instance_home_for_sockets() -> Option<PathBuf> {
    std::env::var_os("YI_AGENT_INSTANCE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(instance_home_from_env)
}

/// `YI_AGENT_HOME` → `$HOME/.yi-agent`，含 `.yi-agent` 归一化。
/// 必须与 `store::paths::instance_home()` 逐字同义，否则两侧算出不同目录。
fn instance_home_from_env() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
    match std::env::var_os("YI_AGENT_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            Some(if dir.file_name() == Some(OsStr::new(".yi-agent")) {
                dir
            } else {
                dir.join(".yi-agent")
            })
        }
        None => home.map(|h| h.join(".yi-agent")),
    }
}
```

**`.yi-agent` 归一化这一条不能漏。** `store::paths::instance_home()`（`paths.rs:20-40`）
在 `YI_AGENT_HOME=/foo` 时返回 `/foo/.yi-agent`，而不是 `/foo`。如果 socket 侧直接
`PathBuf::from(env)`，就会出现 store 说 `/foo/.yi-agent`、socket 说 `/foo` 的分叉。
这段逻辑重复两处是**有意的坏味道**：写测试钉住两者等价（§5 第 7 条），比把
`lifecycle` 反向依赖 `store` 便宜。

返回 `Option` 而不是 `Result`：这里是纯取值，错误类型留在各自调用方
（`socket_path_for` 映射成 `InstanceHomeUnavailable`；`control_path` 走占位路径，
见 §3.3）。

**为什么不用改成"只读 `YI_AGENT_HOME`"**：那会让 guardian 起的 daemon（只被显式设了
`YI_AGENT_INSTANCE_HOME`）与前端（只读 `YI_AGENT_HOME`）在使用者给两者设了不同值时
算出两个目录。优先级读法下，前端和 daemon 都先看 `YI_AGENT_INSTANCE_HOME`，结果一致。
§5 第 11 条用「两者指向不同目录」的用例钉住这一点。

注意情况 B 只是**加固**，不是修 bug：现有代码在正常环境下已经一致，§3.5.1 才是真正
需要动手的地方。

#### 3.5.1 已经手上有 instance_home 的地方不要再去猜

`subagent/src/attach.rs:1380` 的 `attach_to_answer` 签名里就有
`instance_home: Option<&Path>`（`:1383`），但函数体现在是 `let _ = instance_home;`
（`:1421`）把它丢掉，改用 `socket_path_for(runtime_dir)`（`:1386`）靠环境变量反推。

**两个调用方都传 `None`**，包括明明手上有 `instance_home: &Path` 的那个：

| 调用点 | 传的 instance_home | 手上有没有 |
|---|---|---|
| `:1349` `attach_authorized_root` | `None` | 没有（只有 capability） |
| `:1371` `attach_managed_project_runtime` | `None` | **有**，参数 `:1355` 就是 `&Path` |

这个写法现在无害，因为 `socket_path_for` 只依赖 `runtime_dir`；改布局后它就成了
「明明有准确值却去读环境」的分歧点。`attach_managed_project_runtime` 那条尤其要改：
它在 `prepare_managed_runtime` 里用的 `instance_home` 与实际算 socket 用的 env 可能
不是同一个（§3.5 第 2 条），而它本身就是 managed 链路，正是出问题的那条。

**实施时的改法**（与 §3.4 的「签名不变」不冲突）：

1. `attach_to_answer` 内部改成用参数：
   ```rust
   let home = match instance_home {
       Some(path) => path.to_path_buf(),
       None => yi_agent_store::paths::instance_home_for_sockets()
           .ok_or_else(|| AttachFailure::new("runtime directory", IpcError::InstanceHomeUnavailable))?,
   };
   let socket_path = yi_agent_lifecycle::scope::daemon_socket(runtime_dir, &home);
   ```
   ——即「有显式值就用，没有才读 env」。
2. `attach_managed_project_runtime` 把 `Some(instance_home)` 传下去（`:1371` 的
   `None` 改成 `Some(instance_home)`）。
3. `attach_authorized_root` 保持 `None`：它确实没有该信息，走 env 兜底是唯一选择。

`store::ipc::socket_path_for(runtime_dir)` 的**公开签名保持不变**（§3.4），
新增的是 `lifecycle` 侧的 `daemon_socket(runtime_dir, instance_home)`——本身就要
新增（§3.1），所以这里只是直接调它，不构成签名变更。

排查方法：`rg 'let _ = instance_home'`，以及 `socket_path_for(runtime_dir)` 在已有
`instance_home` 变量作用域内的调用点。

**实施时补记（2026-11-07，实测踩到）** 有两条约束在实现中反复出问题，写在这里免得
下一个人重踩：

1. **`RuntimeScope.instance_home` 是 `.yi-agent` 目录本身，不是它的父目录。**
   环境变量 `YI_AGENT_HOME` 是父目录，`store::paths::instance_home()` 与
   `lifecycle::scope::instance_home_from_env()` 会补 `.yi-agent`。把 `TempDir::path()`
   直接塞进 `scope.instance_home`，与子进程从 `YI_AGENT_HOME` 派生出的
   `.../instance/.yi-agent` 不一致——症状是 `claim.rs` 的 instance-home conflict，
   或者两侧 socket 路径分歧。判据：`scope.instance_home` 的 `file_name()` 应为
   `.yi-agent`。

2. **`bringup::control_path(scope)` 必须用 `scope.instance_home`，不能读环境。**
   这是本次故障的直接成因：launcher 按 env 探一个路径、guardian 按 scope 绑另一个，
   于是 `probe` 一直 `Absent` 到 deadline，用户看到的是 30 秒超时而不是路径。修好后
   `managed_runtime_lifecycle` 从 94 秒降到 4 秒——**耗时本身就是那条 bug 的判据**。
   `socket_agreement` 用 `bringup::control_path(&scope)` 与
   `guardian_socket(runtime_dir, home)` 精确比对，回退该修复会立刻变红。

### 3.6 测试侧影响

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
   只删**空**目录是有意的：guardian 与 daemon 共用 `sockets/<hash>/`，daemon 退出时
   guardian 的 socket 可能还在，`remove_dir` 会因 `ENOTEMPTY` 失败并被忽略，正好。
2. **写死路径的用例会互相踩（只影响少数）。** `TempDir` 保证 runtime_dir 唯一，
   所以哈希唯一、socket 唯一——这类用例是安全的。真正有风险的是拿**同一个写死
   字符串**作 runtime_dir 的用例：它们现在争同一个 socket 文件。实测只有
   `ipc/client.rs:379` 的 `LONG_RUNTIME_DIR` 是写死的，而它只被 `socket_path_for`
   的**纯函数**测试使用，不 bind，所以当前零冲突。
   — **处理：不需要引入 `instance_home` 注入参数。** 只要守住「测试不 bind 写死
   路径」这条现状即可。
3. **`std::env::set_var` 不能用来做隔离。** edition 2024 下它是 `unsafe`；
   `cargo test` 在同进程内多线程跑用例，`just test` 用的 nextest 虽然每用例一个
   进程，但两者都要能过。`maintenance_ipc.rs:55` 用全局 `Mutex` + `unsafe set_var`
   绕过这个限制，不要再复制到别处。

4. **`tempfile::TempDir` 不能当 `instance_home`（实测 2026-11-07）。**
   macOS 上 `TempDir` 解析到 `/private/var/folders/<2+22>/T/.tmpXXXXXX`，实长
   **68 字节**；加上 `sockets/<16hex>/guardian-control.sock` 的 **47 字节**尾部等于
   **115**，超过 103 上限，`bind` 直接 `path must be shorter than SUN_LEN`。
   `/tmp` 规范化后是 `/private/tmp`（68 字节 `…/T` 那层没有了），实测
   `<home>/sockets/<hash>/guardian-control.sock` = **68 字节**，装得下。
   — **处理：凡是需要真正 `bind` 的用例，`instance_home` 用
   `/tmp/ya-<用途>-<pid>` 并在结尾 `remove_dir_all`；不要用 `TempDir`。**
   只做纯路径字符串断言的用例不受影响。这条约束本身也说明长度预算是真实的：
   连系统临时目录都装不下，用户把项目放深一点当然装不下。

**结论：不做「给 `Daemon::start` 加 `instance_home` 参数」的改造。** 早先版本的
spec 提出过这个方案，但它把生产签名为了测试而变复杂，而实际问题只有第 1 条
（空目录残留），改实现即可解决。

验收方式见 §5 第 10 条：跑完测试后 `~/.yi-agent/sockets/` 不应留下新目录。

### 3.7 目录创建与权限

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

目录清理只做「删空目录」这一种，见 §3.6 第 1 条：daemon / guardian 退出时若
`sockets/<hash>/` 已空就 `remove_dir`，失败（典型是 `ENOTEMPTY`）忽略。不写按
时间或数量的回收逻辑——每个项目一个 `<16hex>` 子目录，累积量级是项目数，且退出
时基本都会自清。手工整目录删除也安全：里面全是运行期产物。

### 3.8 旧位置清理

老布局的 socket 文件会留在 `<runtime_dir>/` 下。处理：

- **不需要写清理代码**。`bind_control` 的护栏（`guardian.rs:2143-2197`）只在**新路径**
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
| 三个 instance_home 来源全缺失 | `socket_path_for` 返回 `IpcError::InstanceHomeUnavailable`（它会 bind，必须报错）；`control_path` 返回占位路径 `<unknown-instance-home>/sockets/...` 并在 stderr 打一行诊断（详见 §3.3，它不 bind） |

第 3 行的用户可见文案（替换现有的 30 秒超时弹窗）：

> 无法为这个项目创建 runtime socket：实例目录路径过长
> （`<path>`，上限 103 字节，实际 `<n>` 字节）。

只给事实，不教怎么改。理由是：56 字节的门槛在正常机器和容器里都碰不到，能触发它的
只有用户显式把 `YI_AGENT_HOME` 指到极深的位置——这时他比程序更清楚自己在做什么，
一句「换个短点的目录」既没有信息量，也像是程序在替人做主。

**旧文案不该再出现。** `busy` 弹窗里的「重启 yi-agent 后即可用」在当前故障下是错的
建议——重启只会重复同样的失败。测试断言超长场景的错误信息**不含**"重启"、**不含**
"移到更短的路径"（后者已不是解法，§2 让 socket 与项目深度无关）。

提示应留在日志而不是弹窗：调用方（`bringup` / `guardian`，它们有 `tracing`）在拿到
`SocketPathTooLong` 时记完整路径与三个来源（`YI_AGENT_INSTANCE_HOME` /
`YI_AGENT_HOME` / 默认），排查时能看到是哪来的。

`lifecycle` 本身没有 `tracing` 依赖（desktop 壳链接它，要保持轻），所以 `control_path`
在三个来源全缺失的分支里用 `eprintln!` 打一行诊断。该分支在正常主机上不可达
（launchd、容器、SSH 都会设 `HOME`），`eprintln!` 只在"真的什么都没有"时出现，
不需要日志框架支撑。

**两个现有的错误文案都要改**，它们现在的措辞会把用户往错方向引：

| 位置 | 现在的文案 | 问题 |
|---|---|---|
| `store/src/ipc/error.rs:34-38` | "...shorten the project path or set `YI_AGENT_RUNTIME_DIR` to a short directory" | 两条建议在 §2 的新布局下**都已失效**：socket 不再在项目目录里，`YI_AGENT_RUNTIME_DIR` 只影响 runtime_dir（仍参与哈希，但不影响长度） |
| `lifecycle/src/scope.rs:241-248` | "the guardian control socket is {} bytes **even after falling back to the temp directory**" | 兜底已删（§3.2），这句话描述的分支不存在了 |

两处都改为「实例目录路径过长」+ 实际字节数 + 上限，不附建议。§5 第 9 条断言新旧
措辞都不再出现。

（顺带：`lifecycle` 的 doc comment `:232` 也写着 "Reachable only when `TMPDIR`
itself is absurdly deep"，随兜底删除一并更新。）

## 5. 测试策略

每条用例都要求「回退修复后必须变红」，否则断言可能空转。

1. **长度恒定**：workdir 长度取 200 / 500 / 900，断言 `guardian_socket` 与
   `daemon_socket` 都 ≤ `MAX_SOCKET_PATH_BYTES`。改回直接 join 即红。
2. **能真正 bind**：对 `socket_directory` 下的两个名字实际 `UnixListener::bind`，
   断言成功。这条是本次故障的正面回归——只断言长度不够，`bind` 才是真判据。
   注意 `instance_home` 必须用 `/tmp` 下的短目录，`TempDir` 装不下（见 §3.6 第 4 条）。
3. **同源性**：断言 launcher（`bringup::control_path`）、guardian（`bind_control`）、
   `admission`、`thread_root`、`frontend_lease` 五处算出的路径两两相等。
   这几处是 §3.3 表格里最容易被改回 `runtime_dir.join(...)` 的；任一改动即红。
4. **项目隔离**：两个不同 runtime_dir 得到不同 `socket_directory`。
5. **超限失败**：把 `instance_home` 指到 57 字节以上，断言返回 `SocketPathTooLong`
   而非 panic、也**不**落回 `$TMPDIR`。去掉兜底后，这条是「不静默降级」的钉子。
6. **规范化等价（`/var` 陷阱）**：对同一目录的 `/var/...` 与 `/private/var/...` 两种
   拼法，断言 `socket_directory` 返回相同结果。把 §3.1 里的
   `canonicalize_allowing_missing` 去掉即红——这条是本轮评审新增，没有它，
   `/var` 下的项目会静默分裂成两个 socket 目录。
7. **instance_home 三源等价**：`YI_AGENT_HOME` 设成同一个目录的 `/var/...` 与
   `/private/var/...` 两种拼法，断言 `socket_directory` 结果相同；再断言
   `store::paths::instance_home()` 与 `lifecycle::scope::instance_home_for_sockets()`
   在 `YI_AGENT_INSTANCE_HOME` 未设时返回**逐字相同**的路径——这条钉住 §3.5 的
   `.yi-agent` 归一化复刻，漏写 `join(".yi-agent")` 即红。
8. **目录自动创建**：`sockets/<hash>/` 不存在时，`bind_control` 与 daemon 启动都能
   自己建出来并 bind 成功。删掉 §3.7 要求的 `create_dir_all` 即红——这条是"新布局
   比旧故障更糟"的防线。
9. **错误文案**：`instance_home` 超限时，断言错误信息含实际字节数与上限，且**不含**
   "重启"、"shorten the project path"、"YI_AGENT_RUNTIME_DIR"、"falling back to
   the temp directory"。后四条分别对应旧弹窗文案与两处过时的 `Display` 实现（§4）。
10. **测试不留垃圾**：跑完 `cargo test -p yi-agent-store` 后，断言
    `~/.yi-agent/sockets/` 下没有新增**空目录**。这条防的是 §3.6 第 1 条——
    它不会让任何测试变红，只会悄悄往开发者 home 里堆目录。
11. **instance_home 跨进程一致**：同时设 `YI_AGENT_HOME=A` 与
    `YI_AGENT_INSTANCE_HOME=B`（两个不同目录），断言 `instance_home_for_sockets()`
    返回 `B`（不是 `A`），且 guardian 用 `scope.instance_home` 算出的路径与 daemon
    用 `instance_home_for_sockets()` 算出的相同。把优先级写反（先读 `YI_AGENT_HOME`）
    即红——这条锁住 §3.5。
12. **真机验收**：本项目启动后 socket 出现在 `~/.yi-agent/sockets/<hash>/`，
    `/agents` 可用。

## 6. 风险

| 风险 | 处理 |
|---|---|
| 未来有人新增调用点绕过 `control_path` 直接 join | §5 第 3 条断言五处同源；新增点若绕过会立刻红 |
| 旧 socket 残留导致 `AddrInUse` | 复用现有「探测无应答才删」护栏 |
| `/var` vs `/private/var` 使同一项目算出两个 socket 目录 | 两个哈希输入都先 `canonicalize_allowing_missing`；见 §3.1 |
| `store::paths::instance_home()` 与 `scope.instance_home` 规范化程度不同 | 规范化收在 `socket_directory` 内部，三条来源自动一致；见 §3.1 |
| `instance_home` 在进程间取法不同（`YI_AGENT_HOME` vs `YI_AGENT_INSTANCE_HOME`），两侧算出不同 socket 目录 | guardian 用 `scope.instance_home`（不读 env）；其余统一经 `instance_home_for_sockets()`，它优先读 `YI_AGENT_INSTANCE_HOME`。见 §3.5、§5 第 11 条 |
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

- [x] `lifecycle::scope` 提供 `socket_directory` / `guardian_socket` / `daemon_socket`，
      且哈希输入先规范化
- [x] `control_path` / `control_socket` 签名不变，内部改用新布局（调用方零改动）
- [x] `socket_path_for` 签名不变，内部改用 `daemon_socket` + `instance_home_for_sockets()`（§3.5 情况 B）
- [x] `bind_control` 用 `scope.instance_home`，不读 env（§3.5 情况 A）
- [x] `attach_to_answer` 不再 `let _ = instance_home`，且 `attach_managed_project_runtime` 传 `Some(instance_home)`（§3.5.1）
- [x] `bind_control` 与 daemon 在 bind 前创建 `sockets/<hash>/` 并设 0700
- [x] §6.1 列出的 4 个既有用例已逐条确认语义（不是简单改成能过）
- [x] §5 十二条测试全绿，且每条都做过「回退即红」验证
- [x] `instance_home_for_sockets()` 与 `store::paths::instance_home()` 的 `.yi-agent` 归一化逐字一致（§5 第 7 条）
- [x] §3.6 已处理：退出路径对空的 `sockets/<hash>/` 做 `remove_dir`（`ENOTEMPTY`
      忽略），跑完 `cargo test -p yi-agent-store` 不在 `~/.yi-agent/sockets/` 留新空目录
- [x] 本项目真机启动：socket 出现在 `<instance_home>/sockets/<hash>/`
      （实测：普通项目与 127 字节的深层项目都能起 `daemon serve` 并 bind）
- [ ] `/agents` 在 TUI 里可用（需要交互式终端，本轮未验）
- [ ] `just ci` 全绿

  分项已逐条跑过并全绿：`fmt-check`、`clippy --workspace --all-targets --all-features`、
  `build --workspace`、`bindings-check`、`secret-check`、`affected-check`（29 条断言）、
  `test-affected`（4855 用例）、以及 17 个 crate 的分批全量测试。
  `just ci` 整条在本地超过 50 分钟未跑完（另有会话占着全局 package cache），
  故未记录整条结果。

### 8.1 不属于本期判据

用户报的那个弹窗由 `7bc611ac` 修掉，判据是「装 20:05 之后的构建」。本期开工前应
先确认当前安装的二进制已含该提交，否则会把「没装新版本」误判成「本期没做成」。
验证方法：`strings $(command -v yi-agent) | grep -c guardian-` 非零，或直接看
`yi-agent --version` 是否比 `cec4dfc0` 新。
