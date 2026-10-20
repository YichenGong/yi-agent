# 功能开关（feature flags）与 dev 版独立编译 — 设计

日期：2026-10-20
状态：待批准

## 1. 问题

当前并行开发靠长期不合入的 feature 分支。这条路的失败模式很确定：

- 分支活得越久，与 `main` 的差异越大，合并成本指数上升；
- 分支上的代码没有持续的 CI 覆盖（`ci-push` 只在 push 到自己分支时跑）；
- 多个 agent 各自拉长战线，最后没人敢合。

但「随时合入」与「用户看到的功能都是确认没问题的」是冲突的。需要一个机制让
代码可以先合进 `main`、但功能对用户不可见，直到验证完成再打开。

## 2. 目标与非目标

### 目标

1. **任何功能都有开关键**，可在不重新编译的前提下打开或关闭。
2. **dev 版能被独立编译、独立安装、独立运行**，与 stable 完全隔离。
3. **dev 版能逐项控制功能开关**，且这个控制可从 UI 与命令行两个入口完成。
4. 合进 `main` 的代码默认不改变任何用户可见行为（全部默认关）。

### 非目标

- 不做 A/B 实验平台、指标采集、灰度百分比。单人项目用不上。
- 不做编译期 `#[cfg(feature)]` 变体。理由见 D1。
- 不做远程/云端 flag 下发。开关只在本机。

## 3. 设计原则

**开关是债务，不是资产。** 每个 flag 都同时维护着两条代码路径，必须在功能稳定
后被删除。因此所有设计选择都偏向「让删除变得不得不做」：

- 必填过期日期，到期 CI 失败；
- 注册表是唯一真源，没有注册的开关无法打开；
- stable 构建里没有任何绕过路径。

## 4. 决策

### D1 运行期开关，不用编译期 feature

| | 编译期 `--features` | 运行期 flag |
|---|---|---|
| 产物 | 每个变体一份二进制 | 一份二进制 |
| 桌面 App | 要打多个 bundle | 一个 bundle |
| 打开新功能 | 重新编译 + 重装 | 改一个 JSON 字段 |
| CI 覆盖 | 变体组合爆炸 | 单一构建 |

桌面 App 的 `.app` 是一份 bundle，`--features` 意味着同时维护多个 App 变体，与
「App 只装稳定版」的既有约束冲突。故选运行期。

代价：关闭状态的功能代码仍在二进制里（体积、安全面）。本项目规模下可接受。

被否方案：编译期 feature。仅当某个开关需要真正剔除依赖（如可选的重型 crate）
时才重新评估。

### D2 存储：`$YI_AGENT_HOME/flags.json`，跟实例走

```
$YI_AGENT_HOME/flags.json
{
  "version": 1,
  "flags": {
    "new_thread_sidebar": true,
    "experimental_provider": false
  }
}
```

- 路径由 `yi_agent_store::paths::instance_home()` 推出，与 `models.json`、
  `devices.json` 同层。复用既有路径解析，不引入新环境变量。
- **未设置 `YI_AGENT_HOME` 时退化为 `$HOME/.yi-agent/flags.json`**，与历史布局一致。
- 读失败（文件不存在 / JSON 损坏 / 权限不足）一律视为「全部默认值」，绝不阻断启动。
  写失败返回错误给调用方（UI 要知道没存上）。

**为什么跟实例走而不是跟项目走**：开关影响的是「这个二进制/这份构建的行为」，
不是「这个仓库的行为」。同一份代码在不同 worktree 编译出的两个实例，应当能各自
试不同的开关组合——这正是并行验证的核心诉求。

`version` 字段是为将来迁移预留：如果结构要变，按版本号分支处理，不猜。

### D3 注册表是唯一真源，未注册的 flag 无法打开

新增 `crates/yi-agent-flags/src/registry.rs`，用一张静态表定义全部 flag：

```rust
pub struct FlagDef {
    pub name: &'static str,          // 唯一键，snake_case
    pub summary: &'static str,       // UI 上显示的一句话
    pub owner: &'static str,         // 责任人（单人项目就写自己）
    pub expires_on: &'static str,    // "YYYY-MM-DD"，必填
    pub default: bool,               // 恒为 false（见 D6）
}

pub const REGISTRY: &[FlagDef] = &[
    FlagDef {
        name: "new_thread_sidebar",
        summary: "侧栏改版：按项目分组，支持折叠",
        owner: "yichen",
        expires_on: "2026-12-31",
        default: false,
    },
];
```

规则：

- 读取时只认注册表里的名字。`flags.json` 里出现未注册的键 → **忽略**，并在启动日志
  里 `warn` 一次（否则拼写错误会表现成「开了但没生效」，极难排查）。
- 写入时（`flags/set`）键不在注册表 → 返回 `-32602 InvalidParams`。
- flag 名一旦发布过就不改名（它就是配置文件里的键）。

判据：`cargo test -p yi-agent-flags` 覆盖「未注册键被忽略 + 有 warn」「未注册键写入被拒」。

### D4 stable 构建里绝对不能打开

判定顺序（后者覆盖前者）：

1. 注册表 `default`（恒 `false`）
2. `flags.json` 里的值
3. 环境变量 `YI_AGENT_FLAG_<UPPER_SNAKE>=1|0`

然后在**最外层**加一道硬闸：

```rust
if build_info::BUILD_KIND != "dev" {
    return false;   // stable / nogit 一律全关，忽略文件与环境变量
}
```

只有 `-dev` 构建才读配置。这条闸的位置在 `flags::is_enabled()` 内部，**不在调用点**
——调用点分散就一定会漏。

`BUILD_KIND` 已由 `crates/yi-agent/build.rs` 在构建期注入（tag → `release`，
有 git 无 tag → `dev`，无 git → `nogit`）。stable 与 nogit 都归为「全关」。

**这意味着 stable 版本的二进制里没有「打开未来功能」的能力。** 想试新功能，只能装
dev 构建。这是有意的：stable 的行为必须是可复现的。

被否方案：给 stable 留一个 `YI_AGENT_FLAGS_ALLOW_STABLE=1` 逃生开关。它会被写进
某份文档、某个脚本，然后某天在生产环境被打开。不做。

### D5 两个入口：CLI 与 UI，共用同一份存储

**CLI**（`yi-agent flags` 子命令）：

```
yi-agent flags list              # 表格：名字 / 状态 / 说明 / 过期日
yi-agent flags set <name> on|off
yi-agent flags status <name>     # 打印最终生效值 + 它来自哪一层（默认/文件/环境变量/硬闸）
```

`flags status` 里的「来自哪一层」很重要：值不对时，八成是环境变量盖掉了文件。

**UI**：设置页新增「实验功能」Tab。

- 位置：`desktop/src/components/SettingsDialog.tsx` 的 `TABS` 表加一项
  `{ id: "flags", label: "实验功能" }`，再渲染新面板 `SettingsFlagsTab.tsx`。
- **只在 dev 构建里显示**：Tab 是否出现在 `TABS` 里由 `isDevBuild()` 决定（见 D7）。
- 面板内容：每个 flag 一行——名字、说明、开关、过期日；顶部一行提示「这些是实验
  功能，stable 构建里不存在此页面」。

**RPC 契约**（`yi-agent-app-server`）：

| 方法 | 参数 | 返回 |
|---|---|---|
| `flags/list` | 无 | `{flags: [{name, summary, enabled, expires_on, source}]}` |
| `flags/set` | `{name, enabled}` | `{name, enabled}` |

两个方法都按 `Scope::Control` 放行（与 `thread/setModel` 同档），不设 Admin——
dev 实例的 UI 与 CLI 应当等价。

`flags/set` 在 stable/nogit 构建里返回 `-32601 MethodNotFound`（不是 `-32602`：
这个方法在 stable 上**不存在**，而不是参数错）。UI 因此也不需要单独的权限逻辑——
它根本不会渲染。

### D6 所有 flag 默认关闭，且 `default` 恒为 `false`

注册表里 `default` 字段存在是为了将来可能需要「默认开」的迁移场景，但当前规则是
**必须为 `false`**，并有测试钉住：

```rust
#[test]
fn every_flag_defaults_to_off() {
    for def in REGISTRY {
        assert!(!def.default, "flag {} must default to off", def.name);
    }
}
```

理由：「合入 `main` 的代码默认不改变任何用户可见行为」是目标 4 的直接要求。任何
`true` 默认都意味着合入即生效。

### D7 「dev 版」的定义与独立编译

现有构建期标识（`build_info::BUILD_KIND`）已经能区分三种构建。本条只补两件事：

**1. 独立安装路径**（已实现）：`./scripts/rebuild.sh cli` 默认 `--profile dev`，
装到 `~/.local/bin/yi-agent-dev`。stable 只经发布渠道。

**2. 独立运行环境**：`yi-agent-rs/scripts/dev-env.sh` 已提供
`run` / `up` / `down`，通过 `YI_AGENT_HOME` + `YI_AGENT_RUNTIME_DIR` 隔离。

**3. 前端如何知道自己是不是 dev**（新增）：`--version` 已经带 `-dev`，但前端拿不到。
新增一个 RPC：

| 方法 | 参数 | 返回 |
|---|---|---|
| `build/info` | 无 | `{version, build_kind}` |

前端 `isDevBuild()` 读它。UI 隐藏「实验功能」Tab、以及其他未来可能只在 dev 出现的
入口，都靠这一个判定。

被否方案：前端解析 `--version` 字符串。它需要先拿到二进制路径、再 spawn 一个进程，
比一次 RPC 贵得多，且 Tauri WebView 里 spawn 受限。

### D8 过期日期强制，到期 CI 失败

`yi-agent-flags` 带一个测试，在 CI 里运行：

```rust
#[test]
fn no_flag_is_past_its_expiry_date() {
    let today = today_utc();   // 从环境变量 YI_FLAGS_TODAY 或系统时钟取
    for def in REGISTRY {
        assert!(
            def.expires_on > today,
            "flag `{}` expired on {} — delete it or extend the date",
            def.name, def.expires_on
        );
    }
}
```

- 字符串比较即可（`YYYY-MM-DD` 字典序等于时间序），不引入 chrono 依赖到该 crate。
- 失败信息必须同时给出两个动作：「删掉」或「续期」。只报错不说怎么办的 CI 检查会被
  用 `#[ignore]` 绕过。
- 到期后处理方式只有两条，没有第三条：
  - 功能稳定 → 删 flag、删关闭分支、保留打开后的行为；
  - 功能未完成 → **明确续期**并提交（这个动作是有成本的，正是设计意图）。

判据：把某个 `expires_on` 改成过去日期，`cargo test -p yi-agent-flags` 必须失败且
信息里含 flag 名。

## 5. 结构

```
crates/yi-agent-flags/            （新 crate，无重依赖）
  src/lib.rs                      is_enabled() / set() / all() / source_of()
  src/registry.rs                 REGISTRY 静态表 + 过期测试 + 默认关测试
  src/store.rs                    flags.json 读写（走 paths::instance_home()）
  src/error.rs

crates/yi-agent-app-server/src/flags_rpc.rs   flags/list、flags/set、build/info
desktop/src/components/SettingsFlagsTab.tsx   设置页「实验功能」
yi-agent-rs/scripts/dev-env.sh                 已存在；补 models.json 复制缺口
```

依赖方向：`yi-agent-flags` 只依赖 `yi-agent-store`（拿路径）与 serde，
不依赖 app-server 或 runtime，避免环。

## 6. 用法

```bash
# dev 构建
./scripts/rebuild.sh cli                    # → ~/.local/bin/yi-agent-dev

# 在某个 worktree 的实例里打开一个 flag
cd .worktrees/feat-x
bash yi-agent-rs/scripts/dev-env.sh run yi-agent-dev flags set new_thread_sidebar on
bash yi-agent-rs/scripts/dev-env.sh run yi-agent-dev flags status new_thread_sidebar
# enabled=true (source: file)

# stable 上同样的命令
yi-agent flags status new_thread_sidebar
# enabled=false (source: build kind is release — flags are dev-only)
yi-agent flags set new_thread_sidebar on
# error: flags/set is not available in a release build
```

## 7. 验证判据

| 需求 | 判据 |
|---|---|
| 1. 各类功能可用 flag 管理 | `cargo test -p yi-agent-flags`（注册表/默认关/过期/未注册键） |
| 2. dev 版独立编译、独立使用 | `./scripts/rebuild.sh cli` 后 `~/.local/bin/yi-agent-dev --version` 含 `-dev`；`shasum ~/.cargo/bin/yi-agent` 不变 |
| 3. dev 里可控开关 | `dev-env.sh run yi-agent-dev flags set <name> on` 后 `flags status` 返回 `enabled=true`；UI 的「实验功能」Tab 在 dev 可见、stable 不可见 |
| stable 绝对关不了 | `cargo test -p yi-agent-flags` 中「release 构建忽略文件与环境变量」用例 |
| 开关不会永久堆积 | 过期测试：把 `expires_on` 改到过去即失败 |

## 8. 风险

- **双路径维护成本**：每个 flag 都是有分支的代码。缓解=过期机制，不缓解=必然积累。
- **`BUILD_KIND` 依赖 `.git`**：从打包源码构建会得到 `nogit`，于是 flags 全关。这正是
  想要的（可复现），但意味着「从 tarball 编出来的二进制无法试新功能」。需要时用
  `YI_AGENT_BUILD_KIND=release` 显式声明，但那时 flags 依旧全关。
- **UI 与 CLI 并发写**：同一份 `flags.json` 两个写者。规模小，用「读-改-写 + 原子
  rename」即可；与 `models.json` 不同，这里不值得引入文件锁。
