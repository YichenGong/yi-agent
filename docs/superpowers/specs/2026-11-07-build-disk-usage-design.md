# 编译产物磁盘占用：根因与治理

> 目标：把 `target/` 的增长从「每次编译累积几十 GB、永不回收」变成「有界」。
> 只动 Cargo profile 配置、回收脚本与其自测，不改任何 Rust 业务代码。
>
> 日期：2026-11-07（第三版）
> 前两版的核心结论均被实验推翻，见 §8。**请勿引用旧版的数字。**
>
> 相关：`yi-agent-rs/Cargo.toml`、`yi-agent-rs/scripts/reclaim-target.sh`、
> `yi-agent-rs/justfile`、`desktop/src-tauri/Cargo.toml`、
> `plugins/superpowers-kanban/Cargo.toml`、`docs/project-management/tooling.md`

## 1. 症状

用户报告「每次编译磁盘占用过大」。实测（2026-11-07，本机）：

```
$ du -sh .
209G    .
124G    yi-agent-rs/target
 68G    .worktrees
 14G    desktop

$ du -sm yi-agent-rs/target/debug/*  | sort -rn
70116   yi-agent-rs/target/debug/deps
21595   yi-agent-rs/target/debug/incremental
  959   yi-agent-rs/target/debug/build
```

`target/debug` 合计 **91.4 GiB**（`du -sm` = 93,625 MB），两块几乎对半：
`deps` 68.5 GiB、`incremental` 21.1 GiB。

## 2. 根因

有两个独立机制，都源于「文件名带随机 id / 目录名带随机后缀，旧的不删」。

### 2.1 `deps/` 里的 `.o` 累积，但**大部分不能单独回收**

文件名形状 `deps/<crate>-<hash>.<mid>.<idx>.rcgu.o`：

- `<crate>-<hash>`：编译单元的 metadata hash，随依赖图、feature、profile 变化。
- `<mid>`：一次编译（一个 `rustc` 进程）的随机 id，**每次编译都变**。
- `<idx>`：该次编译内的 codegen unit 序号。

实测主仓库**清理前**：`deps` 有 **782,793 个 `.o`**，表观 42.9 GiB。同一个 key
`yi_agent_mcp-261107e32c1248d3` 下有 **339 个不同 mid**，即该单元被编译了
339 次，每次留下最多 16 个文件，只有最后一次的 rlib 保留。

**关键修正**：这些 `.o` 里只有一部分是独立文件。主仓库经历一次孤儿清理后现存
265,156 个（表观仍 42.9 GiB），按 `st_nlink` 统计：

| nlink | 文件数 | 表观体积 | 删除后真实释放 |
|---|---|---|---|
| 1 | 82,590 | 19.73 GiB | **19.73 GiB** |
| ≥2 | 182,566 | 23.20 GiB | **0**（还有其他链接） |

nlink≥2 的那些是 **`incremental/` 会话目录的硬链接**：

```
target/debug/incremental/yi_agent_mcp-2daiobsa0ze8e/s-.../1xebrbxf...o
target/debug/deps/yi_agent_mcp-28f4ba5cfa5a3d77.1xebrbxf....rcgu.o
        ↑ 同一 inode
```

所以**只删 `deps` 下的 `.o`，最多释放 19.73 GiB，不是 42.9 GiB**。
第一版据此声称「回收 93.2 GiB」是错的（见 §8）。

### 2.2 `incremental/` 会话目录才是最大的可回收块

`deps/.o` 的另一半挂在增量缓存下。形状：

```
incremental/<crate>-<随机后缀>/s-<随机>/<随机>.o
```

每个 crate 的增量缓存「会话」应当只有一个，旧的会被替换。实测**没有**：

```
crate 前缀数        : 127
会话目录总数        : 730       ← 每个 crate 平均 5.7 个
最旧的会话          : 11-07 00:48（当天！）
保留每个 crate 最新 : 127 个目录，2.42 GiB
陈旧会话            : 603 个目录，21.96 GiB
```

`yi_agent_store` 一个 crate 就有 **23 个**会话目录，`yi_agent_core` 有 22 个。
`incremental` 的 `.bin`（14.5 GiB）与 `.o`（9.4 GiB）几乎全是这些陈旧会话。

这 603 个目录表观 21.96 GiB，按 `st_nlink == 1` 计**真实可释放 18.70 GiB**
（其余与别处硬链接）。

这就是「每次编译磁盘都在涨」的第二半原因，而且是**第一版完全漏掉的部分**。

### 2.3 合并后的真实可回收量

两段判据的候选集合按 inode 去重后（避免把硬链接算两遍）：

```
陈旧 incremental 会话 : 603 个目录
deps 垃圾 .o          : 258,634 个（孤儿 key + 陈旧 mid）
去重后唯一 inode      : 177,199
真实可释放            : 45.17 GiB
```

这个数字才是脚本对当前主仓库的预期收益。第一版声称的「93.2 GiB」是表观值，
把硬链接重复计算了（见 §8）。

### 2.4 `.o` 为什么显得很大（不是 `.o` 本身的问题）

单次全 workspace 全新构建：`.o` 共 18,024 个、**504 MiB**，平均 408 KiB。
主仓库那 42.9 GiB 的平均值是 170 KiB，中位数 40 KiB。

即**单个 `.o` 并不大，量是累积出来的**。第一版拿一个 53 MiB 的离群样本
（`__DWARF` 占 37 MiB）论证「`.o` 大是因为 `split-debuginfo = "unpacked"`」，
属于以偏概全：只有 38.7% 的字节来自 >1 MiB 的文件（见 §8）。

### 2.5 这些 `.o` 对 cargo 的**编译**无用（但承载调试信息，见 §7）

删除后重编实测（`-p yi-agent-tools`，先删光 1,472 个 `.o`）：

```
$ find $T/debug/deps -name '*.rcgu.o' -delete
$ cargo build -p yi-agent-tools
    Compiling 行数：0          ← 没有重编任何 crate
    rlib mtime    ：未变
    重新生成的 .o ：0
```

cargo 只在**同一次编译的链接步骤**里读这些 `.o`，产物落盘后即可丢。
对陈旧 mid 另做了专项验证：删掉后重编仍是 `Finished 0.10s`，`.o` 数不变。

### 2.6 为什么会有 339 次重编

`.o` 的 mtime 按天分布（11-01 至 11-07）每天新增 3.7~24.7 万个，没有一天减少。
主仓库与各 worktree 的 `target/` 是不同 inode（未共享），所以这 339 次全是
主仓库里的真实重编。`.fingerprint` 显示同一 feature 集有 130 个指纹目录，
即**依赖哈希级联**在反复改变编译单元的 key。这属于 cargo 行为，本 spec 不
试图消除它——只保证它不再留下永久垃圾。

## 3. 关键实验：`split-debuginfo` 与 `debug` 的取舍

同 commit、`--all-features --workspace` + `cargo nextest --no-run`、全新 target。

| 配置 | 总量 | `.o` | `.dSYM` | 重编后是否累积 |
|---|---|---|---|---|
| 默认（`unpacked`） | 10.4 G | 18,024 | 0 | **是** |
| `packed` | 18.2 G | 0 | 135 | 否（恒 135 个） |
| `line-tables-only` + `packed` | 12 G | 0 | 135 | 否 |
| `line-tables-only` + `off` | 7.9 G | 0 | 0 | 否 |
| `debug = 0` + `off` | **6.65 G** | 0 | 0 | 否 |

**三个反直觉的实测结果**：

1. **`packed` 反而更费磁盘**（18.2 G vs 默认 10.4 G）。它把每个测试二进制的
   完整 DWARF 搬进 `.dSYM`，131 个带 hash 的 dSYM 占 **7.89 GiB**，比它
   省下的 `.o`（约 7.4 GiB）还多。dSYM 名字带 hash 却**不累积**（连续 3 次
   重编恒定 135 个），是「一次性固定开销」而非垃圾。
2. **`split-debuginfo = "off"` 在 macOS 上会连 DWARF 一起丢掉**，`debug`
   级别改不动它。用一个最小 rustc 程序验证三种取值的落点：

   | 取值 | DWARF 落点 | 证据 |
   |---|---|---|
   | `unpacked` | 各 `*.rcgu.o` | 单个 `.o` 里 51 个带行号函数 |
   | `packed` | `<binary>.dSYM` | dSYM 里 4,414 个函数、4,373 个带行号 |
   | `off` | **无** | 二进制无 `__DWARF` 段，`dwarfdump` 报 0 |

   所以「`off` + `debug = "full"`」得到的仍是**零调试信息**——`debug` 只决定
   生成多少，`off` 决定是否落盘。
3. **`line-tables-only` 因此只在 `packed`/`unpacked` 下有意义**。与 `off`
   搭配时它只是让产物小一点（7.9 G vs 9.7 G），调试信息同样为 0：既然都不要
   调试信息了，`debug = 0`（6.65 G）更彻底。**排除这个组合。**

### 调试能力差别的实测

不是推断，用 `dwarfdump --statistics` 量同一个测试二进制：

| 配置 | DWARF 所在 | `#functions` | `#functions with location` |
|---|---|---|---|
| `packed` | `deps/<test>.dSYM`（131 个） | 11,325 | 11,284 |
| `off` | 无 | **0** | **0** |

即 `packed` 能看栈帧源码行与变量，`off` 只能看地址。代价是 18.2 G vs 9.7 G
（`--all-features --workspace` 全量测试构建，实测 18,237 MB vs 9,890 MB）。

注意两边的 `incremental/` 都是 ~5.1 G、`deps/` 都是主要部分，**真实大头仍是
累积**——§4.1 的清理才是主要修复，profile 只是把单次构建的基数压低。

## 4. 改动

### 4.1 新增 `just clean-target`：按目录判据回收（主要修复）

`cargo clean` 只能整删，代价是全量重编（本 workspace 十几分钟）。需要一个
「只删陈旧部分」的工具，判据为**目录级**，而非文件级：

1. **`incremental/<crate>-<suffix>/`**：同一 crate 前缀下只保留 mtime 最新的
   一个会话目录，其余整目录删除。**这是最大收益**（实测 21.96 GiB / 603 个）。
2. **`deps/<crate>-<hash>.<mid>.<idx>.rcgu.o`**：采纳 §4.2 的 A 之后默认执行
   （`split-debuginfo = "off"` 让 `.o` 不再承载 DWARF）。脚本先抽样确认该目录
   的 `.o` 不含 `__DWARF`（见 §7.3），命中则跳过并提示 `deps=1`：
   - 孤儿 key（无同名 `.rlib`）：整个 key 的 `.o` 全删。
   - 同一 key 内只保留 mtime 最新 mid 的 `.o`，其余删除。
   - **必须先判 `st_nlink == 1` 才计入「将释放」体积**，否则会像第一版那样
     把硬链接重复计算（表观 42.9 GiB，实际只能释放 19.73 GiB）。

**并发护栏**：删除时若另一个 cargo 正在编译，可能删掉它刚写出、尚未链接的
文件。对每个候选设 `st_mtime` 保护阈值（默认 600 秒，`--min-age` 可调），
并在检测到运行中的 `cargo` 进程时默认拒绝执行（`--force` 跳过）。

**性能**：用一次 `scandir` + dict 分组，不用 `find | while read`：主仓库 78 万
文件，shell 循环要十几分钟（实测 `scandir` 扫 124 GiB 用 70 秒）。

```bash
just clean-target                            # 预演，打印分项收益
just clean-target apply=1                    # 执行（incremental 陈旧会话 + deps 垃圾）
just clean-target apply=1 worktrees=1        # 连 .worktrees/*/yi-agent-rs/target
just clean-target apply=1 min-age=0          # 解除 600s 保护（确认无并发编译时）
just clean-target apply=1 deps=keep          # 显式保留 deps 不动
```

### 4.2 `[profile.dev]` 的取值：**已定 A**

**决定（2026-11-07）：选 A。**

```toml
[profile.dev]
split-debuginfo = "off"
```

三个候选的实测（`--all-features --workspace` 全量测试构建）：

| | 配置 | 总量 | 调试能力 | 累积 |
|---|---|---|---|---|
| **A（采纳）** | `split-debuginfo = "off"` | 9.7 G | 无 | 无 |
| B | `debug = 0` | 6.65 G | 无 | 无 |
| C | `split-debuginfo = "packed"` | 18.2 G | 源码级可用 | 无 |

选 A 的理由：C 多花 8.5 G 只为保留调试信息，而改一行代码重编只需秒级，
调试刚编的产物不依赖历史 `.o`；B 只比 A 再省 3 G 却连符号表都省掉，排查
panic 时连函数名都没有，不划算。A 恰好卡在「不再产出 `.o`/`.dSYM`」与
「保留符号」之间。

**A 的副作用**：`split-debuginfo = "off"` 在 macOS 上会连 DWARF 一起丢弃
（§3 已实测），所以 `.o` 不再承载调试信息 —— 这正意味着 §7.2 对 `deps=1`
的顾虑消失，`deps=1` 可以成为默认清理行为。

`line-tables-only` 两种组合均已排除（见 §3）。

### 4.3 覆盖所有独立构建单元

仓库有**四个**互不继承的 Cargo 构建单元，profile 必须分别声明，清理也必须
分别执行：

| 单元 | 路径 | 当前状态（实测） |
|---|---|---|
| 主 workspace | `yi-agent-rs/Cargo.toml` | 14 crates |
| 桌面壳 | `desktop/src-tauri/Cargo.toml` | 独立 package，`target/` 12.6 GB、7,044 个 `.o` |
| 插件 workspace | `plugins/superpowers-kanban/Cargo.toml` | 独立 workspace，`target/` 1.16 GB、9,699 个 `.o` |
| 各 worktree | `.worktrees/*/yi-agent-rs/target` | 4 个，各自独立 inode |

第一版只改了主 workspace，另外三处仍在累积。

### 4.4 `scripts/test_reclaim_target.sh` 进 `ci`

脚本的动作是 `rm`，判据错会静默造成全量重编或漏清理，两种错误都不报错。
现有 8 条断言**只覆盖孤儿 key**，需扩充到：

- 陈旧 mid 被删、最新 mid 保留
- **`st_nlink > 1` 的文件被跳过且不计入收益**
- `incremental/` 陈旧会话目录被删、最新保留
- `--min-age` 保护生效
- 幂等、参数校验

保持假文件驱动、秒级、不编译。已有 `reclaim-check` 接入 `just ci`。

## 5. 验证

```
$ bash scripts/test_reclaim_target.sh
8 passed          ← 现状；需按 §4.4 扩充

$ just clean-target            # 当前主仓库预期（两段都默认执行）
   incremental 陈旧会话 : 603 个目录
   deps 垃圾 .o         : 258,634 个
   去重后唯一 inode     : 177,199
   合计真实可释放       : 45.17 GiB

$ just clean-target            # 切换 profile 并重编之后的稳态
   两段合计             : 接近 0（新产物不再累积）
   target/debug 总量    : ~9.7 G（不再是 91.4 G）

$ find $T/debug/deps -name '*.rcgu.o' -delete && cargo build -p yi-agent-tools
   Compiling 行数 0，rlib mtime 未变，.o 不再生成
```

### 5.1 已定位：nextest 挂起不是 nextest 的 bug

实验期间多次遇到 `cargo nextest ... --workspace` 挂住几十分钟。已彻底定位，
**与 profile 无关**。前一版把原因写成「自定义 `--cargo-profile ci` 导致挂起」，
这是双重误解，一并纠正：

- `[profile.ci]` 在 `yi-agent-rs/Cargo.toml` 里**根本不存在**（那里只有
  `[profile.dev]`）。实测 `cargo build --cargo-profile ci` 直接报
  `unexpected argument '--cargo-profile'` —— 这个选项是 **nextest 的**，
  不会传给 cargo。
- `just test` 里的 `--profile ci` 是 nextest 自己的 profile，定义在
  `.config/nextest.toml`（`inherits = "default"`），只影响 runner 行为
  （junit、retries、test-group），与 cargo 的编译 profile 是两回事。

所以那次实验实际用的就是 `dev` profile，挂起与 profile 选择毫无关系。

**现象**：30+ 个子进程 `<test-binary> --list --format terse` 卡住不动。

**关键证据**：

```
$ vmmap --summary <pid>
Process exists but has not started -- it is launched-suspended

$ sample <pid>
Call graph:  _dyld_start   ← 无任何线程推进
Physical footprint: 96K    ← 连 dyld 绑定都没做完
```

**根因链**（逐层抓栈确认）：

1. nextest 主线程阻塞在 `BaseApp::build_binary_list_with_scope`
   → `CargoBuildSettings::compute_binary_list`
   → `BufReader<duct::ReaderHandle>::read_line` → `read()`。
   它在**读 `cargo` 子进程的 stdout**，而那行永远不来。
2. 那个 `cargo test --no-run` 卡在
   `Layout::new` → `Filesystem::open_rw_exclusive_create` → `flock::acquire`，
   即**等 `.cargo-build-lock`**。
3. 持锁的是**另一个会话的** `cargo test -p yi-agent-runtime --lib`，
   它的主线程停在 `__wait4` —— 等它自己的测试进程。
4. 而它的测试进程正是第 1 步那批 `launched-suspended` 的进程，永远不开始。

**闭环**：`suspended 子进程 → 另一个 cargo 永不返回 → 锁不释放 →
我的 cargo 排队 → nextest 读不到 stdout → 看起来"卡死"。**

**真正的触发条件是「多个会话并发使用同一个 `target/`」**，本仓库常见：

```
$ lsof target/debug/.cargo-build-lock
cargo   26139 ...   ← 另一个会话 (cwd = 主仓库 yi-agent-rs)
cargo   98826 ...   ← 本会话
```

**实践结论**：
- 单 crate（`-p X`）、无并发时正常，实测秒级完成。
- 有别的会话在编译时，全 workspace 会排队甚至挂数十分钟。
- **判定方法**：`ps ax | grep 'deps/.*--list'` + `vmmap --summary` 看是否
  `launched-suspended`；`lsof target/debug/.cargo-build-lock` 看谁持锁。
- **不要**把它当成 nextest 或 profile 的问题，也不要为此改 `nextest.toml`。
- `just test` 的 `--profile ci` 是 nextest runner profile（`.config/nextest.toml`），
  cargo 编译 profile 只有 `[profile.dev]` 一个，改它就会同时影响 `just build`、
  `just test`、`just lint` 三者。
- 本 spec 的所有改动都不依赖"并发跑全量 nextest"，不受影响。

## 6. 未做的事（明确取舍）

- **不关增量编译**。实测一次改文件：默认 +46 MB vs `CARGO_INCREMENTAL=0` 的
  0 MB，但重编从 1.7s 变 2.8s。改为「清理陈旧会话」而非关闭。
- **不改 `just build` 的 feature 组合**。`just build` 用默认 feature，
  `just test`/`lint` 用 `--all-features`。实测 130 个同 feature 指纹说明差异
  来自依赖 hash 级联而非 feature，统一无益。
- **不自动删 worktree**。属于工作流决策；脚本提供 `--include-worktrees`。
- **不动 `codegen-units`**。实测 `cu16` 让 `.o` 变多 78%（改变 hash，作废
  已有产物）。它在 §3 的所有配置里都不是变量。

## 7. 删除 `.o` 的边界：对编译安全，对调试不安全

两件事必须分开说，混在一起会得出错误结论。

### 7.1 对编译完全安全

```
build1: .o=1472, rlib mtime=1794035536
find -delete → .o=0
cargo build -p yi-agent-tools:
  "Compiling" 行数 = 0
  rlib mtime = 1794035536（未变）
  重新生成的 .o = 0
```

### 7.2 对调试**不安全**：`unpacked` 下 `.o` 就是 DWARF

`split-debuginfo = "unpacked"` 把每个 CGU 的 DWARF 写进该 CGU 的 `.o`，
链接后的二进制只留一张 debug map 指向这些文件。删掉 `.o`，lldb 就找不到它们。
最小程序实测：

```
$ rustc -g -C split-debuginfo=unpacked m.rs -o m     # 产出 4 个 .rcgu.o
$ lldb -o "breakpoint set --file m.rs --line 2" ./m
Breakpoint 1: 3 locations.                            ← 正常

$ rm -f *.rcgu.o
$ lldb -o "breakpoint set --file m.rs --line 2" ./m
Breakpoint 1: no locations (pending).                 ← 源码级调试失效
```

**含义**：清理 `.o` 等于放弃「调试历史构建产物」的能力。这对日常开发可以
接受（改一行就重编，调试的是刚编的），对「事后回溯一个已归档的崩溃」不可
接受。

### 7.3 删 `incremental/` 陈旧会话是安全的（默认路径）

实测：删掉 `openai_responses-02ixegcotn9ve`（52 MB，该 crate 5 个会话中最旧
的一个），紧接着 `cargo build -p yi-agent-core`：

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.14s
```

零重编，`deps` 里的 `.o` 数量不变（265,156）。

**根本原因**（查 N_OSO debug map，即链接器写进二进制的「我的 DWARF 在这些
`.o` 里」清单）：

```
yi_agent_store-ecf10828f38c69a1 : OSO -> deps/ = 828,  OSO -> incremental/ = 0
yi_agent_tools-bbce49c9b91c28aa : OSO -> deps/ = 1213, OSO -> incremental/ = 0
```

**已链接产物只引用 `deps/` 下的 `.o`，从不引用 `incremental/`。**
所以删会话目录对已链接产物的调试能力毫无影响，这是默认路径安全的硬证据。

**因此清理必须显式承认这一点**：
- **采纳 A 之后**：`split-debuginfo = "off"` 让 `.o` 不再含 DWARF，删 `deps/`
  的 `.o` 对调试无影响，因此**两段判据都默认执行**（§4.2 决策）。
- **过渡期与外部单元**：切换 profile 前留下的 `.o` 是 `unpacked` 产物、仍含
  DWARF；`desktop/` 与 `plugins/` 也可能仍是 `unpacked`。删这些会丢掉调试信息。
  脚本因此**抽样检测**（不是查 `.fingerprint`，它只存 profile 哈希，实测
  `profile` 字段是一个 u64，读不出 `split_debuginfo`）：

  ```
  抽 3 个 .o 跑 `otool -l`，若 __DWARF 段数 > 0 → 判定为仍承载调试信息
  ```

  实测判据有效：`unpacked` 的 `.o` 命中 9~11 个 `__DWARF` 段，`off` 的目录
  里根本没有 `.o`。命中时打印警告并**跳过该目录的 deps 清理**，除非显式
  `deps=1`。
- **`incremental/` 任何时候都可清**：§7.3 用 N_OSO debug map 证明已链接产物
  只引用 `deps/`（1213 个 OSO 指向 deps，0 个指向 incremental）。

## 8. 已证伪的早期结论

前两版的结论全部由后续实验推翻，逐条列出以免再被引用：

| 错误结论 | 实测事实 |
|---|---|
| 「`packed` 只省 4%，收益在不再累积」 | 全量测试路径 `packed` 18.2 G vs 默认 10.4 G，**更费磁盘** |
| 「`cargo build` 存量 462 个 `.o`」 | 那是被源码污染的实验；干净重复实测 1,472 |
| 「reclaim 已回收 93.2 GiB」 | 表观值，把硬链接重复计算了。`deps` 的 `.o` 表观 42.9 GiB，其中 23.2 GiB 与 `incremental` 硬链接；两段判据去重后**真实可释放 45.17 GiB** |
| 「清理后 `.o` 问题就解决了」 | `incremental/` 还有 603 个陈旧会话（真实 18.70 GiB），第一版完全没覆盖 |
| 「`.o` 大是因为 `unpacked` 把 DWARF 塞进 `.o`」 | 全新构建 `.o` 平均仅 408 KiB；主仓库中位数 40 KiB。53 MiB 是离群值 |
| 「`line-tables-only` 兼顾体积与调试」 | 配合 `off` 时 debug section 为 0，比纯 `off` 多花 1.2 GiB |
| 「删 `.o` 完全安全」 | 对**编译**安全（零重编），但 `unpacked` 下 `.o` 就是 DWARF：删除后 lldb 行断点从 3 locations 变 no locations |
| 「`off` + `debug = "full"` 能保留调试」 | `off` 在 macOS 上直接丢弃 DWARF，`debug` 级别改不动它 |
| 「清理只做 deps 就够了」 | 已链接产物只引用 `deps/.o`，但 `incremental/` 陈旧会话才是更大的可回收块（18.70 GiB 真实） |
| 「nextest 在自定义 profile 下会挂起」 | 与 profile 无关。`[profile.ci]` 根本不存在（`--cargo-profile` 是 nextest 的选项，不传给 cargo）；真因是多会话共用 `target/` 导致 `.cargo-build-lock` 争抢，nextest 卡在读 cargo stdout。栈已逐层确认（§5.1） |

## 9. 决策记录

| 问题 | 决定 | 日期 |
|---|---|---|
| `[profile.dev]` 取哪个 | **A：`split-debuginfo = "off"`**（§4.2） | 2026-11-07 |
| 默认是否清 `deps/` 的 `.o` | **是**。选 A 后 `.o` 不含 DWARF，§7.2 的顾虑消失 | 2026-11-07 |

剩无待决项，可进入实现计划。
