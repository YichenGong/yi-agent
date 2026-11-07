# 编译产物磁盘占用：根因与治理

> 目标：把 `target/` 的增长从「每次编译累积几十 GB、永不回收」变成「有界」。
> 只动 Cargo profile 配置、一个回收脚本与其自测，不改任何 Rust 业务代码。
>
> 日期：2026-11-07
> 相关：`yi-agent-rs/Cargo.toml`（`[profile.dev]`）、
> `yi-agent-rs/scripts/reclaim-target.sh`、`yi-agent-rs/justfile`、
> `docs/project-management/tooling.md`

## 1. 症状

用户报告「每次编译磁盘占用过大」。实测（2026-11-07 本机）：

```
$ du -sh .
209G    .                        ← 仓库总量
124G    yi-agent-rs/target       ← 单个 target 目录
 68G    .worktrees
 14G    desktop

$ du -sh yi-agent-rs/target/*
122G    yi-agent-rs/target/debug
2.6G    yi-agent-rs/target/release
```

机器总盘 1.8T，已用 84%（剩 304G）。

`target/debug/deps` 的构成（逐个 `stat` 统计，`du` 在这个量级下不可用）：

| 类型 | 文件数 | 体积 | 平均 |
|---|---|---|---|
| `.o`（`*.rcgu.o`） | 782,793 | 135.8 GiB | 0.18 MiB |
| `.rlib` | 2,501 | 20.0 GiB | 8.2 MiB |
| `.rmeta` | 6,554 | 4.9 GiB | 0.77 MiB |
| 可执行（无扩展名） | 239 | 4.3 GiB | 18 MiB |
| `.dylib` | 196 | 0.7 GiB | 3.6 MiB |

**`.o` 占 82%**，而对应的 rlib 只有 2,501 个 → 平均每个编译单元留下
313 个 `.o`。

## 2. 根因

### 2.1 孤儿 `.o`：cargo 只覆盖同名文件，换 hash 后旧文件永久残留

`.o` 文件名形状是 `deps/<crate>-<hash>.<mid>.<idx>.rcgu.o`，其中
`<hash>` 是**编译单元的 metadata hash**，随依赖图、feature 组合、profile
变化。cargo 重编**同一个 hash 的同一个单元**时覆盖同名文件；一旦 hash 变了
（改了依赖、加了 feature、切了 profile），旧 hash 的文件就再也没人碰。

实测判据（按 `<crate>-<hash>` 前缀比对是否还存在同名 `.rlib`）：

```
distinct .o keys : 2740
with matching rlib: 1591
orphans           : 1149  →  93.1 GiB
```

即 **93 GiB 是纯粹的孤儿**，rlib 早已被新 hash 覆盖。

这些 `.o` 对 cargo 完全无用。证据（不是推断）：

```
$ find target/debug/deps -name '*.rcgu.o' -delete
$ cargo build -p yi-agent-tools
    Finished `dev` profile ... in 1.60s       ← 未触发任何重编
```

cargo 只在**同一次编译的链接步骤**里读这些 `.o`，产物落盘后即可丢。

### 2.2 macOS 的 `split-debuginfo` 默认值把 DWARF 留在 `.o` 里

macOS 上 dev/test profile 默认 `split-debuginfo = "unpacked"`（对应
`-Csplit-debuginfo=unpacked`）：rustc 把每个 codegen unit 的 DWARF 写进
该 CGU 的 `.o`，等链接期再合并。所以 `.o` 不是几 KB 的目标码，而是带着
完整调试信息的几十 MB 文件：

```
$ otool -l deps/yi_agent_app_server-*.1ufb5vv.rcgu.o | 按段统计
__DWARF  __debug_str    24.79 MiB
__DWARF  __debug_info    8.08 MiB
__TEXT   __text          2.95 MiB      ← 真正的机器码只占 6%
```

单个 `.o` 最大 53 MiB，其中 37 MiB 是 `__DWARF`。

### 2.3 `codegen-units = 256` 放大了倍数

dev profile 默认 `codegen-units = 256`，一个大 crate 会被切成最多 256 个
CGU，每个 CGU 一个 `.o`。`yi_agent` 单 crate 累计产出 88,703 个 `.o`
（跨 52 个不同 hash），`yi_agent_app_server` 的 `.o` 累计占 62.8 GiB。

### 2.4 为什么「每次编译」都在涨

`.o` 的 mtime 按天分布（2026-11-01 至 11-07）：

```
11-01  239,179    11-04  106,488
11-02   37,289    11-05   71,708
11-03  246,880    11-06   43,911
                  11-07   37,338
```

每天几十万，没有任何一天在减少。`.fingerprint` 里同一个包
`yi-agent-app-server` 有 **130 个**指纹目录，而 `features` 与 `rustflags`
都相同——差异全在 `deps` 的 hash 链（依赖变化级联）。这既是磁盘问题，
也是「改了无关代码却重编很久」的原因。

## 3. 对照实验

同一 commit、同一 crate（`-p yi-agent-tools`）、独立 `CARGO_TARGET_DIR`，
仅改 profile 参数：

| 配置 | `deps/` | `.o` 数 | `.o` 体积 |
|---|---|---|---|
| 默认（`debug=2`, unpacked） | 929 MiB | 462 | 47 MiB |
| `debug = "line-tables-only"` | 959 MiB | 1,471 | — |
| `line-tables-only` + `codegen-units=16` | 986 MiB | 2,628 | — |
| `line-tables-only` + `packed` | 738 MiB | **0** | 0 |
| `debug=2`（默认级别）+ `packed` | 895 MiB | **0** | 0 |

`cargo test --no-run`（test profile，同一 crate）：

| 配置 | `.o` 数 |
|---|---|
| 默认 | 3,485 |
| `[profile.dev] split-debuginfo = "packed"` | **0** |

两条结论：

1. **`packed` 是唯一能把 `.o` 归零的开关**。`line-tables-only` 省 20% 却
   不动 `.o` 的累积机制，属于拿调试体验换小钱——不采用。
2. `codegen-units` 调小反而让 `.o` 变多（`cu16` 比默认多 78%），因为它改变了
   metadata hash，把已有产物全部作废重建。不动它。

关键收益是**重编不再累积**：默认配置下改一个 crate 会把 `.o` 从 462 增到
718（新 hash 一批新文件，旧的照留）；`packed` 下连续重编 `.o` 恒为 0、
目录大小不增长。

## 4. 改动

### 4.1 `[profile.dev] split-debuginfo = "packed"`

```toml
[profile.dev]
split-debuginfo = "packed"
```

- 只作用于 dev；实测单写这一条已覆盖 `cargo test --no-run`（test 继承 dev），
  所以**没有**另写 `[profile.test]`——多写一份只会带来漂移风险。
- 保留完整 DWARF（不降 `debug` 级别），断点与变量查看不受影响。
- 代价：dylib 会额外产出 `.dSYM`（实测 2 个），体积比等量 `.o` 小两个数量级。
- release 不受影响（未覆盖）。

### 4.2 `scripts/reclaim-target.sh`：清理存量

新构建不再产生 `.o`，但历史上堆积的 93 GiB 还在。脚本按「无同名 rlib」
判据只删孤儿，**不**整体 `cargo clean`（那会丢掉全部缓存，下次从零编十几分钟）。

```bash
just reclaim                 # 预演
just reclaim apply=1         # 执行
just reclaim apply=1 worktrees=1   # 连 .worktrees/* 的 target 一起
```

用 python3 一次 `scandir` + dict 比对而不是 `find | while read`：主仓库有
78 万文件，shell 循环要十几分钟，这样是秒级（实测 124 GiB 目录 70 秒）。

### 4.3 `scripts/test_reclaim_target.sh` 进 `ci`

脚本的动作是 `rm`。判据写宽会删掉**活跃**编译单元（代价是一次全量重编），
写窄等于没清理；两种错误都不会报错。用假文件驱动 8 条断言（活跃单元保留、
孤儿删除、非 `.o` 不碰、幂等、参数校验），秒级，已加入 `just ci` 依赖列表
为 `reclaim-check`。

## 5. 验证

```
$ bash scripts/test_reclaim_target.sh
8 passed

$ bash scripts/reclaim-target.sh --target <main target>
   .rcgu.o 文件数  : 782793（属 2740 个编译单元）
   孤儿 .o         : 1149 个编译单元，93.1 GiB

$ CARGO_TARGET_DIR=/tmp/exp-af cargo nextest run --all-features --workspace --no-run
  deps = 5095 MiB   .o = 257     ← 仅 CLI 二进制自身，不再有 test 单元的 .o
```

## 6. 未做的事（明确取舍）

- **不关增量编译**（`incremental = false`）。它能再省一半重编时的增长
  （实测一次改文件：默认 +46 MB vs 关闭 0 MB），但代价是日常重编从
  1.7s 变 2.8s。`target/debug/incremental` 本身有 GC，且 `cargo clean
  --profile dev` 可随时回收，收益不抵体验损失。
- **不改 `just build` 的 feature 组合**。`just build` 用默认 feature、
  `just test`/`lint` 用 `--all-features`，理论上会让同一个 crate 编译两份；
  但实测这套指纹差异是**依赖 hash 级联**造成的（同一 feature 集 130 个指纹），
  统一 feature 并不能消除它，反而会让日常 `just build` 更慢。
- **不清理 `.worktrees/`**。23 个 worktree 里只有 4 个有 target（31G / 23G /
  6.7G / 4.3G，合计 65G），其余是几十 MB 的壳。是否删除属于工作流决策，
  脚本提供 `--include-worktrees` 供需要时使用。

## 7. 日常维护

```bash
just reclaim                          # 任何时候都可安全预演
du -sh yi-agent-rs/target/debug/*     # 想知道涨在哪
cargo clean --profile dev --dry-run   # 想整体回收时先预演
```
