# CLAUDE.md

项目级 Claude Code 指令,会被自动加载到 Claude Code 的上下文。

## 启动时强制加载 `using-superpowers` skill

> **本项目的所有工作会话,在开始任何响应或动作之前,都必须先加载
> `superpowers:using-superpowers` skill。**

- **触发时机:项目启动 / 会话开始。** 只要你在本项目里处理任何需求
  (包括看似简单的问题、澄清性提问、先看一眼代码再动手等),第一步都必须
  加载 `superpowers:using-superpowers`,再开始其它任何动作。
- **这不是可选项。** 只要某个 skill 有哪怕 1% 的可能适用于当前任务,就
  **必须**加载并使用它。不允许用"这很简单""我先收集一下上下文""我记得
  这个 skill"之类的理由跳过。
- **先声明再执行。** 加载后先说一句 "Using [skill] to [purpose]",然后严格
  按该 skill 的流程执行;若它带 checklist,则逐项建 todo 并依次完成。
- **典型路径:** 需求/造功能 → 先 `superpowers:brainstorming`;修 bug →
  先 `superpowers:systematic-debugging`;然后才是具体实现类 skill。
- **优先级:** 用户在本文件(及直接指令)里的要求 > skill > 默认行为。只有
  用户明确要求跳过某 skill 流程时才可跳过。

## 搜索范围:禁止从系统根目录或大目录递归搜索

**默认搜索起点永远是当前项目目录,或它下面的具体子目录。** `rg` / `find` /
`grep -r` / glob 一律从项目根或更深处发起。仓库内搜索不受此限。

- **禁止**从系统根目录起点做递归搜索:`/`、`~`、`$HOME`、`/Users`、
  `/Applications`、`/System`、`/Library`、`/tmp`。典型违规:`find / -name x`、
  `rg pattern /`、`grep -r p ~`、`ls -R ~`、`du -sh ~/*`。
- **大目录同样不作为搜索起点**,包括 `~/Documents`、`~/Downloads`、
  `~/Desktop`。"范围看起来小一点"不是理由——没有明确目标路径就不要扫。
- **允许的浅层列举:** `ls /`、`ls ~`、`ls ~/Documents` 这类只列一层的命令
  很快、无副作用,可以做。区别在于**不递归**、不把列出来的东西逐个扫下去。
  不为"了解一下环境"而反复列目录;需要什么才查什么。
- **需要项目外信息时的顺序:**
  1. 先定点:`command -v` / `which`、已知路径(`~/.yi-agent/`、
     `~/.config/`、`~/Library/Application Support/`)、`brew --prefix`。
  2. 再问用户:确有需要但不知道路径时,直接问"文件大概在哪",而不是自己扫。
  3. 只有用户明确给出目录(如"去 ~/Documents/foo 看看")时,才在该目录内搜索。
- **任何扩大范围的搜索都必须有界:** 限定到具体子目录 + `-maxdepth` +
  排除 `node_modules` / `target` / `.git` / `Caches` + `| head -50` 截断。

## 分支与 worktree 规范

- **严禁在 `main` 分支上直接修改代码或提交 commit。** 所有变更——无论代码、
  文档、配置——都必须先建 worktree、在新分支上改,再合回 `main`。
- 建 worktree 用 `git worktree add .worktrees/<branch> -b <type>/<name>`,
  `.worktrees/` 已在 `.gitignore` 中。`<type>` 用 conventional commits
  前缀(`fix/`、`feat/`、`docs/`、`ci/` 等)。
- 改完在 worktree 里跑测试 + `cargo fmt --all`,确认通过后再 commit,
  最后回 `main` 做 `git merge --no-ff <branch>` 合并,合并后删分支 + 移除
  worktree。流程详见 `superpowers:using-git-worktrees` 和
  `superpowers:finishing-a-development-branch` skill。
- 唯一例外:紧急回滚 / hotfix 也走 worktree 流程,不允许在 `main` 上直接
  `git revert` 或改文件。

## 打包独立 app(不与主环境冲突)

想把改动打成第二个 `.app` 试用、又不污染正在用的正式版时,按下面做。这套
流程来自 2026-10-25 打 `/Applications/yi-agent-nongit.app` 的实测踩坑。

### 1. 隔离实例状态:必须设 `YI_AGENT_HOME`

桌面壳与 sidecar 都按 `YI_AGENT_HOME` 解析实例根(`paths::instance_home`)。
**不设就会共用 `~/.yi-agent`**:relay session、runtime sqlite、threads 全部
撞车,两个 sidecar 连同一个 relay session,消息随机分流,表现为"daemon 时好
时坏"。写入 `Info.plist`(注意 `LSEnvironment` 不展开 `~`,必须绝对路径):

```bash
P=/Applications/<新app>.app/Contents/Info.plist
/usr/libexec/PlistBuddy -c "Add :LSEnvironment dict" "$P"
/usr/libexec/PlistBuddy -c "Add :LSEnvironment:YI_AGENT_HOME string $HOME/.yi-agent-<实例名>" "$P"
```

### 2. 改 bundle 标识,否则与正式版互踢

```bash
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier com.gongyichen.desktop.<实例名>" "$P"
/usr/libexec/PlistBuddy -c "Set :CFBundleName yi-agent-<实例名>" "$P"
/usr/libexec/PlistBuddy -c "Set :CFBundleDisplayName yi-agent-<实例名>" "$P"
```

### 3. worktree 里必须补 sidecar 二进制

`tauri.conf.json` 的 `bundle.externalBin` 要求
`desktop/src-tauri/binaries/yi-agent-aarch64-apple-darwin` 存在,否则 build
script 直接报 `resource path doesn't exist`。worktree 里没有该文件,软链过来:

```bash
mkdir -p desktop/src-tauri/binaries
ln -sf <主仓库>/desktop/src-tauri/binaries/yi-agent-aarch64-apple-darwin \
       desktop/src-tauri/binaries/yi-agent-aarch64-apple-darwin
```

### 4. 必须先构建前端再 `tauri build`,否则白屏

`dist/` 缺失时 `cargo build --release` 出的是 **dev 版**(连
`http://localhost:1420`),装进 app 后 webview 报 `-1004` 白屏,且**前端不
初始化 → `setup` 里的 sidecar 也不起**。正确顺序(注意 `node`/`cargo` 可能
不在默认 PATH):

```bash
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd desktop && npm ci && npm run build          # 产出 dist/
npx tauri build --bundles app                  # 产物在 target/release/bundle/macos/
```

用产出的 bundle 作为源,不要手拼 app 目录。

### 5. 换二进制后三层重签

`cp` 覆盖 `MacOS/` 下任何二进制都会破坏签名,按 sidecar → desktop → app
的顺序重签:

```bash
codesign --force --sign - "$APP/Contents/MacOS/yi-agent"
codesign --force --sign - "$APP/Contents/MacOS/desktop"
codesign --force --sign - "$APP"
codesign -v "$APP"   # 应 exit 0
```

### 6. 验收方式

- 比对两个 sidecar 的 relay 参数,必须不同(新实例未配置 relay 时应**完全没有**
  `--relay` 参数):
  `ps axo command | grep "app-server --listen" | grep -o "session=[a-f0-9]*"`
- 确认独立目录生成:`ls ~/.yi-agent-<实例名>/.yi-agent/`
- **不要只用 `open -a` 验收 sidecar**:`setup` 里的 `bridge::spawn` 在 `open -a`
  启动时不总拉起 sidecar(已有问题,与打包无关)。要可靠复现就在命令行直接跑
  `Contents/MacOS/desktop`。
- 清理时注意监督循环会重启 sidecar:必须先杀 desktop 父进程,否则只杀
  sidecar 会被立刻拉起。

## Commit 规范

- **不要**在 commit message 里写 `Co-Authored-By: Claude ...` 行
- Commit message 用 conventional commits 风格(`feat:`, `fix:`, `ci:`, `docs:` 等)
- 首行简短(<=72 字符),空行后可选写正文说明"为什么"
- **提交前必须跑 `cargo fmt --all`**(在 `yi-agent-rs/` 下),保证代码格式
  通过 `just fmt-check`。`git commit` 前的 hook 不自动跑 fmt,所以需要手动
  执行或在 commit 前先 `cargo fmt --all && git add ...`。
- **提交前必须自检 `just ci` 全绿**(在 `yi-agent-rs/` 下),合并到 `main` 后再
  在合并结果上跑一次。它是 CI 的唯一入口(GitHub PR / GitLab MR 都只跑它),
  涵盖 22 道门禁:`fmt-check`、`lint`(`clippy --all-targets`,含测试代码)、
  `test`、`build`、`bindings-check`、`secret-check`、`msrv-check`(用
  `rust-version` 声明的工具链真编一遍,脚本 `yi-agent-rs/scripts/check-msrv.sh`)、
  `reclaim-check`、`affected-check`、`audit`(`cargo audit`,豁免清单在
  `yi-agent-rs/.cargo/audit.toml`)等。实际清单以 `yi-agent-rs/justfile` 的
  `ci:` 行为准(共 22 项依赖,**改了 `ci:` 的依赖务必同步这一行**)。少跑
  一次就可能让 `main` 直接变红;本地几分钟,比事后回滚便宜。

  **改依赖后必须同步 `rust-version`**:它只声明、不校验,写错了没有门禁会报
  (`msrv-check` 就是为此加的)。真实下限 = 依赖图里最高的 `rust-version`,
  用 `cargo metadata` 查:剔除 `--all-features` 拉进来的可选依赖后取最大值。
- **例外:纯文档合并免跑 CI 和单测。** 若本次改动只涉及文档(`docs/`、`CLAUDE.md`、
  `README.md` 等),不含任何代码、测试或构建配置改动,则合并时无需跑 `just ci`
  或 `cargo test`;但仍要走正常的 worktree + `git merge --no-ff` 流程。

## 磁盘占用:`target/` 会无限膨胀

Cargo 的 `target/` 不会自动回收,长时间不清理单仓库就能吃掉几十 G。开发阶段
收尾时(一个 feature 合完、`just ci` 通过、确认不再需要增量编译)按下表清理。

- **判断"开发阶段结束"**:当前这轮改动的测试/lint/构建都跑完了、且短时间内
  不会再改这块代码;或者只是读代码、写文档、不开编译。**有改动在进行中时
  不要清理**,下一次编译会从零重来。
- **先看再删**,不要盲目 `cargo clean`:

  ```bash
  cargo clean --dry-run                       # 列出会删哪些目录,不实际删除
  du -sh target                               # 主仓库
  du -sh .worktrees/*/yi-agent-rs/target      # 各 worktree
  df -h .                                     # 剩余空间
  ```

- **首选 `just reclaim`,不要整体 `cargo clean`。** 它会按判据只删真垃圾,
  保留活跃产物(整体 `cargo clean` 要全量重编十几分钟):

  ```bash
  cd yi-agent-rs
  just reclaim                              # 预演,只报告不删
  just reclaim --apply                      # 真删
  just reclaim --apply --include-worktrees  # 连 .worktrees/* 的 target 一起
  just reclaim --apply --prune-units 1      # 额外裁掉多余单元目录(会增加重编)
  ```

  四个判据: D1 旧增量会话、D2 孤儿与未被引用的 `.o`、D3 `--prune-units=N`
  多余单元目录(默认关)、D4 陈旧二进制与 dSYM。带 `--min-age 600`(跳过刚
  写出的文件)和 `.cargo-build-lock` 并发护栏,`--apply` 时才真删;有编译在跑
  会拒绝执行,要强行跑加 `--force`。

- **粒度选择**(在 `yi-agent-rs/` 下执行):

  ```bash
  cargo clean --profile dev --dry-run   # 只想回收 debug 产物时先预演
  cargo clean --profile dev             # 删 target/debug(最常涨的部分)
  cargo clean -p <crate>                # 只删某个 crate 的产物
  cargo clean                           # 整个 target/
  ```

- **必须连 `.worktrees/` 一起清**:每个 worktree 有独立 `target/`,而且常常被
  遗忘。已合并/废弃的 worktree 直接连壳删掉(`git worktree remove <path>`)
  最省空间;仍在开发中的 worktree 只清 `target`,不要删目录。
- **删之前确认没有进程在用**:

  ```bash
  ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"
  ```

  有残留编译进程时删 `target/` 会得到诡异的构建错误,先 `kill` 再清。
- **`cargo clean` 后必然全量重编**(workspace 全量约十几分钟),所以要清理就
  一次清干净,别反复清。

## cargo test 执行

### 并发与锁: 先理解为什么"跑第二个像卡死"

cargo 每次运行(包括 `cargo test`)在**编译阶段**都要抢两类文件锁。这不是项目
特有的问题,是 cargo 的标准行为,理解它就不会把正常排队误判成死锁。

| 锁文件 | 作用域 | 冲突时的输出 |
|---|---|---|
| `~/.cargo/.package-cache`、`~/.cargo/.package-cache-mutate` | **全局**,跨仓库、跨 worktree 共享 | `Blocking waiting for file lock on package cache` |
| `<target>/{debug,release}/.cargo-lock` | **单个 target 目录** | `Blocking waiting for file lock on build directory` |

实测结论(2026-11-01,cargo 1.98.1 / macOS / 128G 内存 18 核):

- 锁**只在编译阶段持有**。测试真正跑起来后 `lsof <target>/debug/.cargo-lock` 为空,
  所以编译完成后的测试执行可以真并行。
- 同 target 并发 4 个 `cargo test`(需重新编译): 分别 1.0s / 2.0s / 2.5s / 6.4s
  完成,**全部 rc=0**。是**串行排队**,不是死锁。
- 不同 worktree(各自 `target/`)并发: 只在全局 package cache 上短暂互斥
  (毫秒级),编译本身并行。用 `--offline` 可把 `Blocking` 次数从 3~4 次降到 2 次。

**看着"永远卡死"的三个真实触发条件**,按可能性排序:

1. **持锁进程被暂停**(Ctrl-Z / 终端挂起 / `SIGSTOP`)。锁由文件描述符持有,
  进程没退出锁就不释放,后来者无限期等待。实测: 持锁进程 `stat=T` 时第二个
  cargo 20 秒毫无进展,`kill -CONT` 或杀掉后立刻完成。**这是最像死锁的情形。**
2. **前一个 cargo 在跑全量 workspace 编译**。一次十几分钟很正常,第二个全程只有
  一行 `Blocking waiting`,看起来毫无反应。
3. **多个 worktree 共用同一个 `target/`**(显式设了同一个 `CARGO_TARGET_DIR`
  或软链)。才会撞 `build directory` 锁,各用各的 `target/` 不会。

**诊断(一条命令看清谁占着锁、谁被暂停)**:

```bash
# stat 列出现 T 就是被挂起的元凶,kill -CONT <pid> 恢复或 kill -9 清掉
ps ax -o pid=,stat=,etime=,command= | grep -E "cargo |rustc " | grep -v grep

# 谁持有全局缓存锁 / 各 target 的构建锁(包含 .worktrees/ 下的)
lsof ~/.cargo/.package-cache-mutate 2>/dev/null
find . -name '.cargo-lock' 2>/dev/null | while read f; do
  printf '%-45s -> %s\n' "$f" "$(lsof "$f" 2>/dev/null | tail -n +2 | awk '{print $1"/"$2}' | tr '\n' ' ')"
done
```

**要在同一仓库并行跑测试**,按推荐度选:

```bash
# 1) 各自独立 target: 最彻底,代价是首次编译不共享缓存
CARGO_TARGET_DIR=target/a cargo test -p yi-agent-core &
CARGO_TARGET_DIR=target/b cargo test -p yi-agent-tools &

# 2) 先统一编译,再并发跑二进制: 此时已无构建锁,最省时间
cargo test --workspace --no-run
target/debug/deps/yi_agent_tools-<hash> &
target/debug/deps/yi_agent_store-<hash> &

# 3) 串行跑: 最省事,用 && 而不是 &
cargo test -p yi-agent-core && cargo test -p yi-agent-tools
```

> 用独立 `CARGO_TARGET_DIR` 时注意: `crates/yi-agent/tests/` 里有 5 处依赖
> `CARGO_BIN_EXE_yi-agent` 或回退到相对路径 `target/debug/yi-agent`。前者由 cargo
> 注入所以照常可用,后者在 cwd 不是 `yi-agent-rs/` 时会找不到二进制。

### 排查具体测试卡住(与上面的构建锁是两回事)

- **先分清是编译卡还是测试卡**: `cargo test --no-run` 秒级完成就说明编译没问题,
  卡住的是测试运行阶段,此时与 cargo 锁无关。
- 如果 `cargo test` 一直卡住(尤其是 cancel / interrupt 类测试),先怀疑测试代码
  死锁: 检查测试是否在 `agent.run()` 之前捕获 cancel token,而 `Agent::run()`
  会重置 token(`agent.rs` Fix 1),导致旧 token cancel 无效。正确写法是在
  `run()` 之后捕获 token。
- 单独跑一个卡住的测试名(如 `cargo test -p yi-agent-core --lib <test_name>`)
  通常秒级返回,一个测试卡而其它不卡,基本是测试代码问题而非环境问题。
- **定位是哪个测试死锁**: 不要靠猜,用 macOS `sample` 抓调用栈。
  (1)`ps ax -o pid=,command= | grep yi_agent` 找到测试二进制 PID,
  (2)`sample <pid> 2`,(3)看 `Call graph` 里的线程名和栈顶函数——线程名通常
  直接显示 `shell::bash::tests::<test_name>`。比逐个 `--test <name>` 快得多。
- 直接跑测试二进制(`target/debug/deps/<test_binary>`)可绕过 cargo 的进程管理,
  配合 `sample` 定位死锁。

### 关于残留进程(纠正一条常见误判)

被中断的 `cargo test` 确实会留下**僵尸测试二进制**(进程名形如
`yi_agent_tools-<hash>`)。但实测确认: **它们不持有 `.cargo-lock`**——用
`kill -9` 杀掉 cargo 后残留的测试进程仍在运行,下一个 `cargo test --no-run`
仍然是 0 秒完成。真正持锁的永远是 **cargo 进程本身**。

所以清理残留进程的目的是回收 CPU/内存、避免端口和临时目录冲突,而**不是**解
构建锁。解构建锁只有两条路: 让持锁进程结束,或恢复被 `SIGSTOP` 暂停的进程。

```bash
ps ax -o pid=,stat=,command= | grep -E "cargo|rustc" | grep -v grep
# 只处理确实残留的(先确认 stat 不是 R 且在编译)
kill <pid>
```

`find target -name "*.lock" -type f -delete` 删 incremental 下的 mutex 文件属于
**最后手段**: 那 298 个文件是 cargo 正常重建的空锁文件,删了不会解开
`.cargo-lock`,反而可能让正在编译的进程行为异常。

### 跑范围

- 在 worktree 里跑测试用 `cd <worktree>/yi-agent-rs && cargo test ...`,或在
  worktree 根目录用 `cargo test --manifest-path yi-agent-rs/Cargo.toml ...`。
  两种方式都用 worktree 自己的 `yi-agent-rs/target/`,与主仓库互不干扰。
- **避免 `cargo test --workspace` 全量跑**: 全量编译 + 同时拉起多个 crate 的测试
  二进制,容易 OOM(exit 137)或死锁级联。优先按 crate 跑:
  `cargo test -p yi-agent`、`-p yi-agent-core`、`-p yi-agent-tools`。确需全量时用
  `--jobs 2` 降低并行度。本机 128G 内存实测 4 个并发 crate 测试压力可忽略,但
  低配机器/CI 上全量跑仍要留余量。

## 测试分级:两个正交轴

> 只有一套分级概念。旧文档里「Tier 0-3」与「L0/L1/L2」是两个不同维度,
> 已合并到下面这张表。

### 两个轴

| 轴 | 取值 | 决定什么 |
|---|---|---|
| **范围** (scope) | `L0` check / `L1` crate / `L2` affected / `L3` workspace | 跑多少代码 |
| **保真度** (fidelity) | `mock` / `proc` / `live` | 要不要真进程、要不要真 API |

- **mock**:纯内存 / wiremock,无真实进程。
- **proc**:真 fork guardian / daemon / 子进程,等真实 10 秒优雅窗口。263 个测试,
  实测 36 秒。
- **live**:真实 LLM API,花钱、有速率限制。18 个测试,并发 3。

### 用法(照着选,不用记轴)

| 场景 | 命令 |
|---|---|
| 改完看能不能编过 | `just check-fast` |
| 只改了一个 crate | `just test-crate <crate>` |
| 改了几处,想知道牵动谁 | `just test-affected [ref]` |
| 收尾 / 合并前 | `just test` |
| 想验证真实 API 还能通 | `just test-live [suite]` |

- `just test` 覆盖 `L3 + mock+proc`。**永不触发真实 API 调用。**
- `just test-live` 是唯一的 live 入口。suite 取 `llm` / `e2e` / `complex` /
  `subagent` / `catalog`,省略则跑全部 18 个。无 key 时打印 `SKIPPED` 并 exit 0。
- `just test-affected` 默认对比 `$(git merge-base main HEAD)`,**含未提交改动**;
  传 ref 则对比该 ref,例如 `just test-affected origin/main`。
- 只想跑单个测试: `cargo nextest run --all-features -p <crate> -E 'test(<name>)'`。
  跑 live 的单个测试: `cargo nextest run --all-features --profile live
  --run-ignored only -p <crate> --test <binary> -E 'test(<name>)'`。

### 设计原则(改这些命令前必读)

- **分级的意义是「不同场景测不同的东西」,不是把全量测试加速。** L0-L2 各自省掉
  不同成本,都不等价于 `just test`;合并前仍要跑全量。
- **proc 不是独立档位。** 263 个 proc 测试只占全量约 4%(36s / 821s),单独切一层
  的认知成本大于收益。慢的根因是全量编译与 6020 个测试,解法是范围分层。
- **`--all-targets` 不能省(L0)。** 默认不检查 `tests/`,而本仓有 108 个集成测试
  target。实测在 `crates/*/tests/` 放类型错误,不带该 flag 报 0 error(漏检)。
- **`--all-features` 不能省(L1/L2)。** `yi-agent-store` 的
  `foreground-test-support` feature 门控了测试辅助方法,漏掉直接 `error[E0599]`。
- **L2 用 nextest 原生 `rdeps()`,不手工算闭包**,依赖图漂移交给 nextest。
  闭包条数是验收依据: 改 `core` 得 12 个、`relay` 得 3 个、`flags` 得 8 个。
- **L2 默认基准必须是 merge-base 的两点 diff**,不是三点 `main...HEAD`。
- **判据写错不会报错,只会少测。** 映射链上任何失败都必须显式 exit 非零,
  不能退化成「全部 unmapped」后返回 0。
- **判据自测: `just affected-check`**(29 条断言,已进 `just ci`)。
- **`just test` 前的并发预检只警告不阻断。** 看到 WARN 且失败是超时类时,
  **先独占复跑再判断**。
- **live 必须走 `[profile.live]`,不能用 default。** default profile 的
  `default-filter` 排除了 live 测试(防 `--run-ignored all` 误跑真实 API),
  因此 `--run-ignored only` 在 default 下会拿到 0 个测试。`-E` 覆盖不了。
- **`default-filter` 的排除项必须用 `binary_id(...)` 限定。** 裸写
  `not test(/^real_/)` 会误伤 `yi-agent-runtime`、`eval_baseline`、
  `frontend_runtime_lease` 里名称以 `real_` 开头的正常测试。五个二进制的 live
  前缀各不相同,必须逐个写。
- **超时预算必须大于测试自身预算。** live 测试自身是 300s
  (`E2E_TIMEOUT`/`COMPLEX_TIMEOUT`),profile 里设 `300s x 2 = 600s`。若设成
  default 的 180s 会把合法测试误杀成「不稳定」。

实现: `yi-agent-rs/scripts/test-tiers.sh`(范围轴)+ `yi-agent-rs/scripts/test-live.sh`
(保真度轴)+ `.config/nextest.toml`(profile 与 test-group)。
设计依据: `docs/superpowers/specs/2026-11-08-unified-test-tiers-design.md`

## 真实 LLM 测试的环境要求

- 配置源是 `~/.yi-agent/models.json` 的默认条目(`scripts/export-model-env.sh`
  从中导出变量)。**不要**依赖 shell 里的全局 key——入口会先 `unset` 它们。
- 无可用 key 时所有 live 入口打印 `SKIPPED` 并 exit 0,不算失败。
- CI **不跑** live 测试(避免成本与密钥泄露)。
- 已知失败(与并发无关,串行也复现,属另一条工作线):`real_openai_*`
  打 `https://api.openai.com` 而实际 provider 走自定义 URL;
  `e2e_auto_compact_triggers` 依赖 token 计数假设。
- `yi-agent run` 子命令是端到端测试的载体:非交互式 drain `AgentEvent` 到
  stdout/stderr,`--json` 切换为 JSONL 供测试断言。

## 项目进度维护

- **每完成一组需求,必须同步更新 `docs/project-management/` 下对应模块文件**,
  并在同一个 commit 或 PR 内提交。不要事后补。
- 状态只有三态:**`[x]` 已完成**、**`[ ]` 未完成**、**`[-]` 已放弃**。
  **禁止使用 `[~]` 进行中**——要么完成要么没完成,"进行中"对读者没信息量且
  容易成为黑洞(进去就出不来)。
- 每条 feature 必须带**可验证的完成判据**:代码位置(`file.rs:line`)或
  可执行命令,不要写"已实现权限管理"这种主观描述。
- 更新模块文件后,同步更新 `docs/project-management/README.md` 模块索引表的
  "完成 / 总计"计数(根 `README.md` 是面向用户的精简版,不再放模块索引)。
- **新增 crate 时**,同一 PR 内必须:
  1. 在 `docs/project-management/` 下创建对应模块文件
  2. 在 `docs/project-management/README.md` 模块索引表登记一行
- `bug-list.md` 同理:只有 `[x]` 已修复 / `[ ]` 未修复两态,不用 `[~]`。
- 维护规则详见 `docs/project-management/README.md` 末尾"维护规则"小节。

