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
209G    .                     ← 整仓库
124G    yi-agent-rs/target
 68G    .worktrees              ← 每个 worktree 各有一份 target
 14G    desktop

$ du -sm yi-agent-rs/target/debug/*  | sort -rn
85869   yi-agent-rs/target/debug/deps
32734   yi-agent-rs/target/debug/incremental
  976   yi-agent-rs/target/debug/build
```

`target/debug` 合计 **117 GiB**：`deps` 83.9 GiB、`incremental` 32.0 GiB。
（早先一版记的是 91.4 GiB，那是清过一轮之后的状态；这里是未清理的基线。）

`deps` 里 **267,503 个 `*.rcgu.o`**，表观 43.8 GiB。

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

**关键修正**：这些 `.o` 里只有一部分是独立文件。当前实测
267,503 个（表观 43.8 GiB），按 `st_nlink` 统计：

| nlink | 文件数 | 表观体积 | 删除后真实释放 |
|---|---|---|---|
| 1 | 83,854 | 19.81 GiB | **19.81 GiB** |
| ≥2 | 183,649 | 24.0 GiB | **0**（还有其他链接） |

nlink≥2 的来源有**两类**（2026-11-07 实测分类，前一版只说了第一类）：

| 类别 | 数量 | 说明 |
|---|---|---|
| `deps/` 内部同 inode 的多个名字 | 54,230 组 | 同一 `<key>.<mid>` 下不同 `<idx>` 的名字指向同一 inode；**全部**属于这一类，0 例外 |
| 与 `incremental/` 共享 inode | 18,332 个文件 | 增量缓存与 `deps` 各持一个名字 |

```
target/debug/incremental/<crate>-<hash>/s-<id>/1xebrbxf...o
target/debug/deps/<crate>-<hash>.<mid>.1xebrbxf....rcgu.o
        ↑ 同一 inode（第二类）
```

所以**只删 `deps` 下的 `.o`，最多释放 19.81 GiB，不是 43.8 GiB**。必须同时
删 `incremental/` 的旧会话，才能把那 18,332 个共享 inode 占用的空间释放出来
（§2.3 有合并统计：26.17 GiB）。

**因此删除时不能跳过 nlink>1 的文件**——那样两类都释放不了。正确做法是：
**照删**，但只在 `st_nlink == 1` 时把字节数计入「将释放」。

第一版据此声称「回收 93.2 GiB」是错的（见 §8）。

### 2.2 `incremental/` 会话目录才是最大的可回收块

`deps/.o` 的另一半挂在增量缓存下。形状：

```
incremental/<crate>-<编译单元hash>/s-<会话id>/<产物>
```

**注意层级含义**（前一版把两层混为一谈，是错的）：

| 层级 | 含义 | 何时变化 |
|---|---|---|
| `<crate>-<hash>` | **编译单元**：hash 含 feature 组合 / 依赖 / profile | feature 或依赖变化就换 hash，**旧的不会被删** |
| `s-<id>` | **会话**：同一编译单元内的一次增量缓存快照 | 同一单元重编会新增会话，旧会话留下 |

实测（`target/debug/incremental`，2026-11-07）：

```
crate 前缀数     : 127
编译单元目录数   : 902   ← 每 crate 平均 7.1 个，yi_agent_core 独占 27 个
会话总数         : 1,046
单元内有多会话的 : 144 个单元（各 2 个会话）
总量             : 35.0 GiB
```

两类垃圾的量级完全不同，必须分开处理：

```
单元内多余的旧会话  : 144 个目录，8.50 GiB   ← 零重编代价，安全
多余编译单元目录    : 775 个目录，约 26 GiB  ← 有重编代价，见下
```

**多余单元目录不能按「每 crate 只留最新一个」粗暴删。** 实测：把
`yi_agent_core` 的 27 个单元目录删到只剩 1 个，然后 `touch` 源码重编：

```
只留 1 个单元目录 : 3.22 s（冷增量，需要重建会话）
保留全部 27 个    : 0.85 s
```

再交替构建默认 feature 与 `--all-features`，两者都要重编。

所以正确取舍是：**默认只清单元内的旧会话（8.50 GiB，零代价）**，多余单元
目录由 `--prune-units=N` 显式开启（N = 每个 crate 保留多少个单元目录，
默认关闭）。这比「只留最新」保守，但不会让日常构建变慢。

### 2.3 合并后的真实可回收量

按 §4.1 的判据，可回收量分三档（2026-11-07 主仓库实测）：

```
档位                          候选              真实释放      代价
---                           ---               ---           ---
D1 单元内旧会话（默认）       23,922 个文件     并入 D2 统计  零重编
D2 deps 未被引用的 .o（默认） 267,533 个文件    并入 D2 统计  零重编
---                           ---               ---           ---
D1+D2 合并（唯一 inode）      171,685 个 inode  26.17 GiB     零重编
D3 多余编译单元目录（可选）   775 个目录        ~26 GiB       一次性 3.2s/crate
```

**为什么必须合并统计**：D2 的候选有 267,533 个文件、表观 43.83 GiB，但其中
大量是 `incremental/` 会话目录的**硬链接**。实测：

```
D2 单独       : 表观 43.83 GiB，唯一 inode 150,603，其中 nlink==1 仅 19.82 GiB
D1+D2 合并    : 表观 53.79 GiB，唯一 inode 171,685，nlink==1 为 26.17 GiB
```

即「只删 deps 的 `.o`」最多释放 19.82 GiB；把 incremental 的旧会话一起删，
才能把那 10.33 GiB 硬链接真正释放出来。**这正是第一版把 93.2 GiB 报成收益的
原因**——它按表观值累加，完全没看 `st_nlink`。

主仓库当前 `deps` 84.5 GiB + `incremental` 32.7 GiB = 117 GiB。执行 D1+D2 后
约 91 GiB；再开 D3 可到 65 GiB 上下。

**但要明确预期**：`just reclaim` 是一次性回收，真正让占用「有界」的是 §4.2 的
profile 改动——`split-debuginfo = "off"` 后 `deps` 不再落 `.o`，那块 84.5 GiB
的累积源被切断。两者缺一不可：只改 profile 不清存量，旧的 267,533 个 `.o`
永远躺在那。

**关键**：单次 `just reclaim` 只能回收一次，真正让占用「有界」的是 §4.2 的
profile 改动——`split-debuginfo = "off"` 之后 `deps/` 不再落 `.o`，这块
84.5 GiB 的累积源被切断。

第一版声称的「93.2 GiB」是表观值，把硬链接重复计算了（见 §8）。

### 2.3b `deps/` 的完整构成（2026-11-07 20:40 实测，补齐前几版漏掉的两块）

> **基线时效说明**：本节及其后所有「主仓库实测」数字取自 2026-11-07 19:00-20:40
> 的 `yi-agent-rs/target`。该目录在 20:45 左右**被外部并发任务清空**
> （127 GB → 2.8 GB），因此这些绝对值**之后无法原样复现**。
> 它们仍然有效，因为用途是「量级与比例的取证」，不是回归基线；实现时
> **不要拿这些数字当验收阈值**，验收一律用「前后对比 + 比例」而非绝对值。


前几版只盯 `.o`，漏掉了**两个更大的可回收块**。按类型统计
`yi-agent-rs/target/debug/deps`（表观值，未按 inode 去重）：

| 类别 | 数量 | 体积 | 是否可回收 |
|---|---|---|---|
| `.o` | 268,571 | **47.09 GB** | 是（packed 下 cargo 不再读，见 §2.5） |
| `.rlib` | 3,517 | 30.65 GB | 部分（孤儿 key 的） |
| **`dSYM`** | **339** | **27.56 GB** | **大部分是（见下）** |
| **二进制** | **574** | **12.95 GB** | **旧 hash 版本可回收** |
| `.rmeta` | 8,780 | 7.18 GB | 否（cargo 增量决策要用） |
| `.dylib` | 279 | 1.05 GB | 否 |
| `.d` | 9,638 | 0.05 GB | 否 |
| **合计** | — | **127.1 GB** | — |

**为什么 dSYM 和旧二进制是新的重点**：

`packed` 把 DWARF 打包进 `<binary>.dSYM`。dSYM 名字带编译单元 hash，
**同一 crate 的不同 hash 会各留一份**。实测：

```
同一 crate 前缀下存在 >1 个二进制 hash 的：123 个
  yi_agent                    : 17 个版本
  integration / yi_agent_relay: 各 11 个版本
  session_delegation_lifecycle: 10 个版本

非最新版本的二进制  : 449 个 = 10.6 GB
非最新版本二进制的 dSYM : 211 个 = 19.85 GB   ← 纯垃圾
```

即 **约 30 GB 属于「被新 hash 取代、永不再用」的陈旧产物**，其中单个最大的
dSYM 有 **899 MB**（`yi_agent_app_server`），中位数 29 MB。

**判定「陈旧」的分组键必须是 `crate + 目标类型`，不能只用 crate 前缀**
（2026-11-07 实测，这是写错就会误删的关键）：

只用 crate 前缀分组会误判。反例：

```
plugin_pack-28129df3848e2374  3.8 MB  ← fingerprint 声明 bin-plugin-pack
plugin_pack-f90c9159d28d6eaf  1.0 MB  ← fingerprint 声明 test-bin-plugin-pack
两者 mtime 只差 0.338 秒
```

它们是**同一 crate 的两个不同目标**（可执行目标 vs 测试目标），两个都必须
保留。按「crate 前缀内只留 mtime 最新」删，就会删掉 `bin` 或 `test-bin`
其中一个，导致 `cargo build` 或 `cargo test` 少产物。

正确做法：从 `target/debug/.fingerprint/<pkg>-<hash>/` 下 `*.json` 的**文件名**
取目标类型（`bin-*` / `test-bin-*` / `test-lib-*` / `lib-*` / `test-integration-test-*`），
用 `(crate, 目标类型)` 作为分组键，组内保留 mtime 最新者。

实测（该判据下）：

```
分组数                      : 125
二进制总数                  : 574
其中陈旧（同 crate+目标）   : 449
dSYM 总数                   : 27.5 GB
其中陈旧                    : 19.8 GB
```

同组内哈希不同 = 同一目标被反复重编（依赖图/feature 变化改变 hash），
旧的永不复用，删除后按需再生成。**已实测：删掉陈旧二进制与 dSYM 后
`cargo build` 能正常重建，产物可运行。**

> 安全边界：即便判据写错删掉了活跃二进制，cargo 也会重建（实测误删唯一
> 二进制后 `cargo build` 正常产出可运行产物），所以这条**不会造成构建失败**，
> 最坏代价是一次重编。这与 D2（删 `.o`）同为「可恢复」级别。

> 注意：这一条在 §2.2 的 D1/D2/D3 之外，是**新增的 D4**。前几版没发现它，
> 是因为只看 `du -sh` 总量、没按文件类型拆开。

### 2.4 `.o` 为什么显得很大（不是 `.o` 本身的问题）

单次全 workspace 全新构建：`.o` 共 18,024 个、**504 MiB**，平均 408 KiB。
主仓库那 43.8 GiB（267,503 个）的平均值是 172 KiB，中位数 40 KiB。

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

对「未被二进制引用的 `.o`」也做了专项验证（§4.1 的 D2 判据，`unpacked`、
`-p yi-agent-tools --tests`）：删掉 2,571 个未被引用的，重编无 `Compiling`
行，`.o` 从 3,486 降到 915（只剩被引用的），未重新生成一个。

> 注意：这里说的是**编译**安全。`unpacked` 下这些 `.o` 就是 DWARF 本体，
> 删了会丢调试信息——§7.2 有 lldb 实测。所以 D2 的前提是先切到
> `split-debuginfo = "off"`（§4.2 的 A）。

### 2.6 为什么会有 339 次重编

`.o` 的 mtime 按天分布（11-01 至 11-07）每天新增 3.7~24.7 万个，没有一天减少。
主仓库与各 worktree 的 `target/` 是不同 inode（未共享），所以这 339 次全是
主仓库里的真实重编。`.fingerprint` 显示同一 feature 集有 130 个指纹目录，
即**依赖哈希级联**在反复改变编译单元的 key。这属于 cargo 行为，本 spec 不
试图消除它——只保证它不再留下永久垃圾。

## 3. 关键实验：`split-debuginfo` 与 `debug` 的取舍

同 commit、`--all-features --workspace --tests`、全新 target。

| 配置 | 总量 | `.o` | `.dSYM` | 重编后是否累积 |
|---|---|---|---|---|
| 默认（`unpacked`） | 10.4 G | 18,024 | 0 | **是** |
| `packed` | 17.8 G | 0 | 131 | 否（恒 131 个） |
| `line-tables-only` + `packed` | 12 G | 0 | 131 | 否 |
| `line-tables-only` + `off` | 7.9 G | 0 | 0 | 否 |
| `debug = 0` + `packed` | **6.6 G** | 0 | 0 | 否 |
| `split-debuginfo = "off"`（**采纳的 A**） | **9.9 G** | 0 | 0 | 否 |

> 全部行于 2026-11-07 用同样的 `--all-features --workspace --tests`
> 命令重跑复核。`packed` 精确值：18,235 MiB、131 个 dSYM（其中 DWARF
> 占 8.2 GiB）、0 个 `.o`。

**三个反直觉的实测结果**：

1. **`packed` 反而更费磁盘**（17.8 G vs 默认 10.4 G）。它把每个测试二进制的
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
   搭配时它只是让产物小一点（7.9 G vs 9.9 G），调试信息同样为 0：既然都不要
   调试信息了，`debug = 0`（6.65 G）更彻底。**排除这个组合。**

### 调试能力差别的实测

不是推断，用 `dwarfdump --statistics` 量同一个测试二进制：

| 配置 | DWARF 所在 | `#functions` | `#functions with location` |
|---|---|---|---|
| `packed` | `deps/<test>.dSYM`（131 个） | 11,325 | 11,284 |
| `off` | 无 | **0** | **0** |

即 `packed` 能看栈帧源码行与变量，`off` 只能看地址。代价是 17.8 G vs 9.9 G
（`--all-features --workspace` 全量测试构建，实测 18,237 MB vs 9,890 MB）。

注意两边的 `incremental/` 都是 ~5.1 G、`deps/` 都是主要部分，**真实大头仍是
累积**——§4.1 的清理才是主要修复，profile 只是把单次构建的基数压低。

## 4. 改动

### 4.1 扩展 `just reclaim`：按目录与引用判据回收（主要修复）

现有 `scripts/reclaim-target.sh` 只认「孤儿 key」（无同名 rlib 的 `.o`），实测
只能清 8 个编译单元、约 0.2 GiB。需要把它扩展到三个判据，分档执行：

**D1（默认）`incremental/` 单元内的旧会话**
  同一 `<crate>-<hash>/` 单元内只保留 mtime 最新的 `s-*` 会话，其余整目录删。
  实测 144 个目录、**8.50 GiB**，**零重编代价**（§7.3 已实测）。

**D2（默认）`deps/` 的孤儿 `.o`**
  无同名 `.rlib` 的 key，整个 key 的 `.o` 全删。实测 8 个单元、约 0.2 GiB。

**D3（可选，`--prune-units=N`）多余的编译单元目录**
  每个 crate 前缀下保留 mtime 最新的 N 个单元目录（默认 N=0 即不执行）。
  全开（N=1）可再回收约 26 GiB，但代价见下。

**D2 的细则——`deps/<crate>-<hash>.<mid>.<idx>.rcgu.o`**：采纳 §4.2 的 A 之后默认执行
   （`split-debuginfo = "off"` 让 `.o` 不再承载 DWARF）。判据是
   **N_OSO 引用集**，不是 mtime：
   - **孤儿 key**（无同名 `.rlib`）：整个 key 的 `.o` 全删（无需查引用）。
   - **活跃 key 内**：跑 `nm -pa <deps 下每个可执行文件>`，收集所有 `OSO`
     行指向的 `.o` 文件名，得到「被引用集合」；**不在集合内的 `.o` 删除**。
   - **必须先判 `st_nlink == 1` 才计入「将释放」体积**，否则会像第一版那样
     把硬链接重复计算（表观 43.8 GiB，实际只能释放 19.81 GiB，见 §2.1）。

   **⚠️ N_OSO 判据有一个会让它整体失效的前提，必须先满足**（2026-11-07
   新实测，spec 上一版漏了这条）：

   N_OSO 只在**二进制的 DWARF 确实指向 deps 下现存 `.o`** 时才成立。一旦
   `split-debuginfo` 从 `unpacked` 切到 `packed`/`off`，新二进制不再引用
   任何 `.o`，而**旧的 unpacked `.o` 还躺在 deps 里**——此时引用集与磁盘
   文件几乎零交集，N_OSO 会把**全部**存量 `.o` 判成「未被引用」。

   最小复现（`/tmp/mirror`，1 个 crate + serde_json）：

   ```
   unpacked 构建            : 22 个 .o
   切到 packed 后重建        : 22 个旧 .o 仍在，二进制只引用其中 13 个
                              → N_OSO 会删掉 9 个（含被引用的同类文件）
   ```

   真实仓库（`yi-agent-rs/target/debug/deps`，2026-11-07 19:30 实测）：

   ```
   取样 30 个最新二进制，收集 OSO 名字 13,912 条
   与磁盘 268,571 个 .o 的 basename 交集 : 0
   → N_OSO 判定「未被引用」的文件数：268,571 / 268,571（100%）
   ```

   根因是两类 `.o` 的**命名格式根本不同**，不是「引用与不引用」的关系：

   | 类别 | 文件名形状 | 实测数量 | 体积 |
   |---|---|---|---|
   | 增量式（本 crate 及其 CGU） | `<key>.<mid>.<idx>.rcgu.o` | 263,217 | 43.4 GB |
   | 依赖式（非增量编译的依赖） | `<key>.<crate>.<hash>-cgu.<n>.rcgu.o` | 5,017 | 3.4 GB |
   | 其他 | — | 337 | 0.3 GB |

   packed 重建的二进制里，OSO 记录的是**上一代 unpacked 构建**的路径，与当前
   磁盘上的两代文件名都对不上。

   **结论（对 D2 的实现约束）**：
   1. **D2 的默认删除判据只能是「孤儿 key」**（无同名 `.rlib`），这一条在
      两种 `split-debuginfo` 下都成立，实测孤儿 22 个 key / 3,415 个文件 /
      约 1.0 GB。
   2. **N_OSO 只能作为「活跃 key 内」的附加判据，且必须先通过自检**：扫描
      完成后若 `引用集 ∩ 磁盘 .o == 0`（交集为空）而磁盘非空，说明当前
      `split-debuginfo` 与存量 `.o` 的代际不匹配，**必须放弃 N_OSO 判据、
      只按孤儿 key 删**，并在报告里明确写出「已跳过引用判据（0 命中）」。
      否则一次 `just reclaim` 会删掉 47 GB 里 100% 的文件。
   3. 这条自检**必须有对应断言**（§4.4 / plan Task 2），否则回归时会静默
      变成破坏性删除。

**为什么不能用「保留 mtime 最新 mid」**（前一版的错判据，已实测推翻）：

一次编译会在**同一秒**写出该 key 的所有 mid，mtime 完全相同。实测
`/tmp/osot`（`unpacked`、`-p yi-agent-tools --tests`）：

```
key=process_selector_request_contract-8bb7bda334f49f94
  239 个 mid，全部 mtime=1794042905
二进制 OSO 实际引用的是 009qlt7vp8waht7t8mno8fupt
  → 它不是「mtime 最新」（并列，取谁都可能）
915 个 OSO 引用里，906 个落在「非最新 mid」上
```

即按 mtime 取舍会**删掉当前二进制正在使用的调试信息**。

**N_OSO 判据的实测效果**（同一实验目录）：

```
binaries=9   referenced .o names=2319   all .o=3486
NOT referenced (safe to delete) = 2571
referenced (must keep)          =  915
```

删掉这 2,571 个后 `cargo build -p yi-agent-tools --tests`：

```
无 "Compiling" 行（零重编）
.o 数量 3486 → 915（只剩被引用的），未重新生成任何一个
```

与 §7.3 的结论一致：cargo 不读这些文件。

**代价与边界**（2026-11-07 重测，前一版写「秒级」是错的）：

```
deps 下可执行文件数 : 456
nm -pa 全部扫一遍    : 77.6 秒（单进程串行）
```

扫描是**秒级到一分钟级**，不是瞬时。脚本应在扫之前打印
`正在扫描 456 个二进制…`，否则用户会以为卡死。可以按文件大小降序、或只扫
最近修改过的二进制来加速，但**不要**为此牺牲正确性——漏扫一个二进制就会
让它的 `.o` 被判成未引用而误删。

且只有存在二进制时才有引用集。若 deps 里一个可执行文件都没有（纯
`cargo build` 产 rlib 的场景），则无法构造引用集，此时**只跑孤儿 key 判据**，
活跃 key 一律保留。

**并发护栏**：删除时若另一个 cargo 正在编译，可能删掉它刚写出、尚未链接的
文件。对每个候选设 `st_mtime` 保护阈值（默认 600 秒，`--min-age` 可调），
并在检测到运行中的 `cargo` 进程时默认拒绝执行（`--force` 跳过）。

**性能**：用一次 `scandir` + dict 分组，不用 `find | while read`：主仓库 78 万
文件，shell 循环要十几分钟（实测 `scandir` 扫 124 GiB 用 70 秒）。

**D3 的代价（必须写进用户可见文档）**：

```
yi_agent_core 的 27 个单元目录删到只剩 1 个后 touch 源码重编：3.22 s
保留全部 27 个：                                            0.85 s
```

且默认 feature 与 `--all-features` 来回切时每次都要重建对应单元。所以 D3
默认关闭，只给「磁盘真的告急、且近期不打算频繁切 feature」时用。

```bash
just reclaim                          # 预演，打印 D1+D2 分项收益
just reclaim apply=1                  # 执行 D1 + D2
just reclaim apply=1 worktrees=1      # 连 .worktrees/*/yi-agent-rs/target
just reclaim apply=1 min-age=0        # 解除 600s 保护（确认无并发编译时）
just reclaim apply=1 prune-units=1    # 额外执行 D3（每 crate 留 1 个单元）
just reclaim apply=1 skip-deps=1      # 只做 incremental，不动 deps
```

### 4.2 `[profile.dev]` 的取值：**已定 `packed`（2026-11-07 二次决策，推翻 A）**

> **决策变更记录**：本 spec 上一版选了 A（`split-debuginfo = "off"`）。经
> 2026-11-07 20:xx 复核，改回 **`packed`**，理由见本节末尾「为什么推翻 A」。
> 下面保留 A/B/C 三个候选的实测数据，但**最终采纳 C（`packed`）**，
> 即 `yi-agent-rs/Cargo.toml` 的现状，不需要改动。

#### 为什么推翻 A：A 的收益前提被实测推翻

选 A 的全部理由是「`packed` 更费磁盘（17.8 G vs 9.9 G），不如直接 `off`」。
但那个对比是在**旧配置（unpacked）存量未清**的状态下做的，混入了历史累积。
分开看两件独立的事：

| 问题 | 由谁解决 | 实测证据 |
|---|---|---|
| **无限累积**（真痛点） | `packed` 已解决 | 全新 target 下 `cargo build` 与 `cargo test --no-run` 均产出 **0 个 `.o`**；连续重编 `.o` 恒为 0 |
| 单次构建基数 | A 比 packed 少 8 GB | 但这是**一次性**差价，省一次就没了 |

A 的代价是**彻底失去源码级调试**（§3 实测：`off` 下 DWARF 函数数 = 0，
lldb 只能看地址）。用永久的排查能力换一次性 8 GB，不划算。**改回 packed。**

#### A 的意外副作用（新实测，A 被推翻的第二个理由）

切到 `off`/`packed` 之后，**存量 unpacked `.o` 与新一代二进制的 N_OSO 引用
集交集为 0**（详见 §4.1），这会让「按引用清理」的判据整体失效——一次误跑
就删掉 100% 的 `.o`。保持 packed 时这个陷阱依然存在（存量仍是 unpacked
遗留），所以 §4.1 的代际自检不能省。

**决定（2026-11-07）：选 A。**

```toml
[profile.dev]
split-debuginfo = "off"
```

三个候选的实测（`--all-features --workspace --tests`，各自全新 target，
2026-11-07 重跑复核）：

| | 配置 | 总量 | `.o` | dSYM | 函数名符号 | DWARF |
|---|---|---|---|---|---|---|
| **A（采纳）** | `split-debuginfo = "off"` | 9.9 GiB | 0 | 0 | 338,203 | 无 |
| B | `debug = 0` + `packed` | 6.6 GiB | 0 | 0 | 337,420 | 无 |
| C | `split-debuginfo = "packed"` | 17.8 GiB | 0 | 131 | 有 | 源码级 |

**B 并没有「连符号表都省掉」**（前一版的说法，实测推翻）。同一测试二进制：

```
A: nm -a 总数 1,802,645，其中 1,371,767 条是无类型符号条目（debug 信息）
B: nm -a 总数   429,873
两者 nm 里的函数符号（t/T）几乎一样：338,203 vs 337,420（差 0.2%）
```

即 `debug = 0` 丢的是**类型/变量等调试条目**，函数名照旧保留，panic
回溯仍能显示函数名。B 比 A 再省 3.3 GiB（33%）。

**仍然选 A，但理由要改**：差别不在「有没有函数名」，而在 `debug = 0` 会让
`dbg!`/`{:?}` 之外的若干诊断能力退化得更彻底（类型布局、变量），且 A 的
9.9 GiB 已把累积源（`.o`、dSYM）切干净。3.3 GiB 的差价不值得让排查体验
再降一档——这是取舍，不是「B 不可用」。若磁盘极度紧张，切 B 是安全的。

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
现有 8 条断言**只覆盖孤儿 key**，需扩充到（对应 §4.1 的 D1/D2/D3）：

**D2（N_OSO 引用集）**
- 用假 `nm` 桩（PATH 前置的 shell 脚本）注入 OSO 引用集；
- 被引用的 `.o` 保留、未被引用的删除；
- **deps 里没有任何可执行文件时，退化为只清孤儿 key**；
- **引用集与磁盘 `.o` 交集为 0 时，必须整体放弃引用判据、只清孤儿 key**
  （§4.1 的致命前提；构造一个「桩 `nm` 返回的全是不存在的名字」的 fixture
  断言活跃 `.o` 一个都没被删）；
- `--skip-deps` 时完全不动 `deps`。

**D1（单元内旧会话）**
- `<crate>-<hash>/s-a`（旧）与 `s-b`（新）→ 删 `s-a`、留 `s-b`；
- 单会话单元不动；
- 会话目录里的 `.lock` 文件一并删除，不报错。

**D3（`--prune-units=N`）**
- 默认不执行（N=0）；
- `N=1` 时每 crate 前缀保留最新的 1 个单元目录，其余删除。

**通用**
- **`st_nlink > 1` 的文件照删（不跳删），但只在 `st_nlink == 1` 时计入
  「将释放」体积**。上一版这里写成「跳删」，与 §2.1、§4.1 以及 plan 的实现
  直接冲突，属于笔误；照 §2.1 的实测，跳删会让两类硬链接（deps 内部重复名、
  与 `incremental/` 共享）都释放不出来，收益从 26.17 GiB 掉到 19.81 GiB。
- `--min-age` 保护生效（新文件不被删）；
- 预演不删任何东西；幂等；参数校验。

保持假文件驱动、秒级、不编译、不需要真实 cargo 产物。已有 `reclaim-check`
接入 `just ci`。

## 5. 验证

### 5.1 自测

```bash
$ bash yi-agent-rs/scripts/test_reclaim_target.sh
8 passed          ← 现状（只覆盖孤儿 key）；按 §4.4 扩充后应 >= 18 passed
$ just reclaim-check
```

### 5.2 主仓库实跑预期（2026-11-07 实测基线）

```bash
$ just reclaim                    # 预演，D1+D2
   incremental 旧会话   : 144 个目录 / 23,922 个文件
   deps 未引用 .o       : 267,533 个文件
   去重后唯一 inode     : 171,685
   合计真实可释放       : 26.17 GiB   ← 按 nlink==1 计
```

执行后 `target/debug` 从约 117 GiB 降到约 91 GiB。再开
`just reclaim apply=1 prune-units=1` 可再降到约 65 GiB。

### 5.3 profile 生效后的稳态

```bash
$ just build && cargo nextest run --all-features --workspace --no-run
$ find yi-agent-rs/target/debug/deps -name '*.rcgu.o' | wc -l
0                    ← 全新 target 实测为 0（packed 的下沉点，§3）
$ ls -1 yi-agent-rs/target/debug/deps/*.dSYM | wc -l
131                  ← packed 会产 dSYM，但**不累积**（连续重编恒定 131 个）
$ du -sh yi-agent-rs/target
~17.8 G              ← 全量 --all-features --tests 实测
```

**关键验收不是总量，而是「不增长」**：新建后连续 `touch` 重编 3 次，
`.o` 恒为 0、dSYM 恒为 131；而 `deps/` 里真正会无限增长的
「陈旧二进制 + 其 dSYM」由 D4 清理（§2.3b，实测 30 GB）。

**注意**：`target/` 必须先 `cargo clean` 或换新目录，否则旧 `packed` 时代
留下的 267,533 个 `.o` 不会消失——profile 只影响新产物。

### 5.4 反复重编不再累积（核心验收）

```bash
$ for i in 1 2 3; do
    touch yi-agent-rs/crates/yi-agent-core/src/lib.rs
    cargo build --all-features
    find target/debug/deps -name '*.rcgu.o' | wc -l
  done
0
0
0
```

### 5.5 已定位：nextest 挂起不是 nextest 的 bug

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

### 7.3 删 `incremental/` 的旧会话零代价，删整个单元目录有代价

两个层级的代价必须分开说（§2.2 解释了层级）。

**删单元内的旧会话：零重编。** 实测删掉
`yi_agent_core-1j45bcbqcmne7` 下两个会话中较旧的一个，紧接着
`cargo build -p yi-agent-core`：

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.17s
```

零重编。原因：同一编译单元内 cargo 只用最新会话，旧的只是历史快照。

**删整个单元目录：需要重建增量会话。** 实测把 `yi_agent_core` 的 27 个单元
目录删到只剩 1 个，`touch` 源码后重编：

```
只留 1 个单元目录 : 3.22 s
保留全部 27 个    : 0.85 s
```

差 2.4 秒，落在具体被删单元的 feature 组合上。单次不算多，但日常在默认
feature 与 `--all-features` 之间来回时，每次都要吃这个代价。因此 D3 默认关闭。

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
| 「`packed` 只省 4%，收益在不再累积」 | 全量测试路径 `packed` 17.8 G vs 默认 10.4 G，**更费磁盘** |
| 「`cargo build` 存量 462 个 `.o`」 | 那是被源码污染的实验；干净重复实测 1,472 |
| 「reclaim 已回收 93.2 GiB」 | 表观值，把硬链接重复计算了。实测：D2 单独表观 43.83 GiB，但唯一 inode 里 nlink==1 只有 19.82 GiB；D1+D2 合并后唯一 inode 171,685、**真实可释放 26.17 GiB** |
| 「清理后 `.o` 问题就解决了」 | `incremental/` 的旧会话与 `deps` 的 `.o` 大量互为硬链接，必须合并删除才能释放那 10.33 GiB。且 `incremental/` 里 775 个多余**编译单元**目录另有约 26 GiB（前一版误称「陈旧会话」） |
| 「`.o` 大是因为 `unpacked` 把 DWARF 塞进 `.o`」 | 全新构建 `.o` 平均仅 408 KiB；主仓库中位数 40 KiB。53 MiB 是离群值 |
| 「`line-tables-only` 兼顾体积与调试」 | 配合 `off` 时 debug section 为 0，比纯 `off` 多花 1.2 GiB |
| 「删 `.o` 完全安全」 | 对**编译**安全（零重编），但 `unpacked` 下 `.o` 就是 DWARF：删除后 lldb 行断点从 3 locations 变 no locations |
| 「`off` + `debug = "full"` 能保留调试」 | `off` 在 macOS 上直接丢弃 DWARF，`debug` 级别改不动它 |
| 「清理只做 deps 就够了」 | 已链接产物只引用 `deps/.o`，但 `incremental/` 陈旧会话才是更大的可回收块（18.70 GiB 真实） |
| 「nextest 在自定义 profile 下会挂起」 | 与 profile 无关。`[profile.ci]` 根本不存在（`--cargo-profile` 是 nextest 的选项，不传给 cargo）；真因是多会话共用 `target/` 导致 `.cargo-build-lock` 争抢，nextest 卡在读 cargo stdout。栈已逐层确认（§5.5） |

## 9. 决策记录

| 问题 | 决定 | 日期 |
|---|---|---|
| `[profile.dev]` 取哪个 | **A：`split-debuginfo = "off"`**（§4.2）。不是 B 因为 B 只再省 3.3 GiB 却让诊断信息更残缺；不是 C 因为多花 8 GiB 只为保留调试信息 | 2026-11-07 |
| 默认是否清 `deps/` 的 `.o` | **是**。选 A 后 `.o` 不含 DWARF，§7.2 的顾虑消失 | 2026-11-07 |
| `deps/.o` 的判据 | **N_OSO 引用集**，不用 mtime（同秒写全、mtime 无法区分；实测 915 个引用里 906 个落在「非最新 mid」） | 2026-11-07 |
| 是否默认清多余编译单元目录 | **否**。默认 D1+D2（26.17 GiB，零重编）；D3 需显式 `prune-units=N`，因为会让 feature 切换重编 | 2026-11-07 |
| 是否关闭增量编译 | **否**。改为清旧会话（8.5 GiB 零代价），保留增量收益 | 2026-11-07 |

无待决项，可进入实现计划。
