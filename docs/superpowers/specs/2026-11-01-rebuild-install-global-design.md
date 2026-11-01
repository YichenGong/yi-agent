# rebuild.sh 把新编的 CLI 接到全局路径

> 目标：`./scripts/rebuild.sh` 编完之后，终端里直接敲 `yi-agent` 拿到的就是
> 刚编出来的这个二进制，而不是上一次装的东西。只改 `scripts/rebuild.sh`
> 与其自测，不改任何 Rust/前端源码。
>
> 日期：2026-11-01
> 相关：[2026-10-19-stable-dev-channels-design.md](2026-10-19-stable-dev-channels-design.md)
> （本文修正其 D5 的一条规则）、`scripts/rebuild.sh`、
> `docs/project-management/tooling.md`

## 1. 问题

`build_cli()` 默认走 `--profile dev`，只把产物装到
`~/.local/bin/yi-agent-dev`，**刻意不碰** `/opt/homebrew/bin/yi-agent`。
于是 `rebuild.sh` 跑完、终端里敲 `yi-agent`，命中的仍是上一次装进去的旧
二进制，用户以为重建没生效。

实测（2026-11-01 本机）：

```
$ which -a yi-agent
/opt/homebrew/bin/yi-agent                 ← 登录 shell 里排在最前
/Users/gongyichen/.cargo/bin/yi-agent
$ readlink /opt/homebrew/bin/yi-agent
/Users/gongyichen/.cargo/bin/yi-agent      ← 指向 cargo bin 的稳定版 0.1.13
$ ~/.local/bin/yi-agent-dev --version
yi-agent 0.1.13-dev+5f55187c               ← 刚编的开发版，PATH 里找不到
```

`~/.local/bin` 确实在交互 shell 的 PATH 上（`~/.zshrc:110`），但它**排在
`/opt/homebrew/bin` 之后**，所以「装到 `~/.local/bin`」这个动作改变不了
`which yi-agent` 的结果。真正决定终端里跑哪个的是
`/opt/homebrew/bin/yi-agent` 这个软链。

另注：`--version` 的 `-dev+<sha>` 后缀由 `build.rs` 按 git tag 判定，**与
profile 无关**。在打了 `v<Cargo 版本>` tag 的提交上编出来的 dev 二进制也报
干净的 `0.1.13`，所以不能用版本号字符串区分两个 profile，只能看落点。

## 2. 与既有设计的冲突

`2026-10-19-stable-dev-channels-design.md` 的 D5 规定：dev 构建
「**不再**写 `~/.cargo/bin/yi-agent` 与 `/opt/homebrew/bin/yi-agent`」，
理由是「修正稳定版定义被污染的根源」。

这条规则要解决的问题是真的：不该让随手一次重编把「稳定版」定义换成某个下午
的 main。但它的手段过重——把**软链**也算成污染，结果连「终端里用上新编的
版本」这个基本诉求都被牺牲掉了。

本机事实：`brew list` 里没有 `yi-agent`，`/opt/homebrew/bin/yi-agent` 是
上一轮 `--profile stable` 留下的软链。也就是说这个「稳定版」一直是本机编的，
发布渠道从未装过，原设计担心的污染在本机并不存在。

## 3. 设计

### 3.1 两种 profile 都接管软链，只有 dev 不覆盖稳定版二进制

| profile | 二进制落点 | `/opt/homebrew/bin/yi-agent` |
|---|---|---|
| `dev`（默认） | `~/.local/bin/yi-agent-dev` | 软链 → `yi-agent-dev` |
| `stable` | `~/.cargo/bin/yi-agent` | 软链 → `~/.cargo/bin/yi-agent` |

两条路都不改对方的二进制文件本身，只有软链指向变化：

- dev 重建后 `which yi-agent` 命中 `yi-agent-dev`；
- 想退回稳定版，跑一次 `./scripts/rebuild.sh cli --profile stable` 即可，
  软链自动指回去，`~/.local/bin/yi-agent-dev` 留着不碍事；
- 稳定版二进制（`~/.cargo/bin/yi-agent`）仍只由 `--profile stable` 写，
  dev 重建前后它的 `shasum` 不变——原设计真正想守的那条不变量保住了。

### 3.2 拒绝覆盖真实文件

`/opt/homebrew/bin/yi-agent` 如果是**真实文件**（Homebrew 装的，或手工
`cp` 的），不覆盖它：删掉会让 brew 的账本与磁盘不一致，后续
`brew upgrade` / `brew uninstall` 行为不可预期。此时 `die` 并以非零退出，
打印出路（`brew uninstall yi-agent` 或 `rm` 后再跑）。

不能只 `warn` 然后 exit 0：本改动的全部意义就是「全局路径指向刚编的产物」，
做不到却报成功，正是用户抱怨的那种静默分叉。

### 3.3 目录不存在 / 不可写

- 父目录不存在：不 `mkdir -p`，也**不失败**——`warn` 后跳过全局软链，让
  CLI / App / 插件照常编完。该目录归 Homebrew 管，在没有 brew 的机器上凭空造
  一个 `/opt/homebrew/bin` 只会让人误以为 brew 装好了；而那里也没有本该被接管
  的对象，谈不上分叉。提示可用 `REBUILD_GLOBAL_BIN` 指到 PATH 上的别处。
- 父目录存在但不可写（`/opt/homebrew` 属 `admin:admin 775`，非 admin 用户不可
  写）：`die` 并提示需要 sudo。这里必须失败——路径真实存在，静默跳过会让用户
  以为它已被更新。

### 3.4 路径可注入，为自测留口

三个落点从环境变量取默认值：

```
REBUILD_LOCAL_BIN_DIR  (默认 $HOME/.local/bin)
REBUILD_CARGO_BIN_DIR  (默认 $HOME/.cargo/bin)
REBUILD_GLOBAL_BIN     (默认 /opt/homebrew/bin/yi-agent)
```

它们只影响「装到哪」，不影响编译。自测靠它们把安装逻辑跑在临时目录里，
不必真的编一次 Rust；`/opt/homebrew/bin` 也不会被测试碰到。

`PROFILE` / `FORCE` 从入口段上移到常量段（安装函数要读 `PROFILE`），
入口段改为 `main()`；脚本被 `source` 时只加载函数不执行构建：

```sh
if [ "${BASH_SOURCE[0]}" = "$0" ]; then main "$@"; fi
```

## 4. 不动的部分

- `stop_dependents` 的顺序与判据不变。dev 重建仍会先停掉终端里的
  `yi-agent` / `yi-agent-dev`——理由不是 `Text file busy`（实测 macOS 上
  `install` 与 `cp` 都先 unlink 再写，覆盖正在执行的二进制不报错），而是旧
  进程会继续拿着旧二进制服务下去。
- App 内的 sidecar / daemon / 值守都用 `std::env::current_exe()`
  （`daemon.rs:65`、`watchman.rs:78`），不读这个软链，所以改软链不会让常驻
  进程改用 dev 二进制。全仓唯一解析 `/opt/homebrew/bin/<name>` 的是
  `plugin_worktree.rs:106`，只针对插件名（`superpowers-kanban`），不含
  `yi-agent` 自身。

## 5. 测试

`scripts/test_rebuild_global_link.sh`，全部跑在 `mktemp -d` 的临时树里，
只 source 脚本、不编译：

1. dev：写出 `yi-agent-dev` 且可执行；全局路径成为软链并解析到它；执行全局
   路径拿到假二进制的输出（证明软链真的通）；cargo 目录不被创建/不被写。
2. dev 遇真实文件：非零退出，原文件内容原样保留。
3. stable：写 `~/.cargo/bin/yi-agent`、全局软链指向它；预置的 dev 二进制
   文件内容不变。
4. 幂等与回切：dev 跑两次仍是同一条软链；随后 stable 把软链指回 cargo bin。
5. 父目录不存在：跳过软链但不中断（exit 0），且不会凭空创建该目录。
6. 父目录存在但不可写：非零退出，软链未被创建。

判据：`bash scripts/test_rebuild_global_link.sh`（25 passed）。四个变异各自
变红：删掉真实文件检查 → 用例 2 两红；dev 分支不调 `link_global` → 8 红；
缺父目录时改成 `mkdir -p` → 2 红；stable 顺带覆盖 `yi-agent-dev` → 1 红。

真机验证（2026-11-01）：

```
$ . scripts/rebuild.sh; PROFILE=dev; install_cli_bin <刚编的 yi-agent>
     -> /opt/homebrew/bin/yi-agent (软链 -> ~/.local/bin/yi-agent-dev)
$ zsh -lc 'yi-agent --version'      # 登录 shell 走 PATH 首选
yi-agent 0.1.13
$ shasum -a 256 ~/.cargo/bin/yi-agent   # 稳定版二进制未被动过
e195254dcc4050c5386eed9e08b01f02
```

## 6. 同步修改的文档

- 本文（新 spec）。
- `2026-10-19-stable-dev-channels-design.md` D5：加一条「2026-11-01 变更」，
  指向本文，避免留下互相矛盾的规则。
- `docs/project-management/tooling.md`：两条既有 feature 的判据里写着
  「不碰 `/opt/homebrew/bin/yi-agent`」，需按本文改；并新增本文的 feature 行。
- `scripts/rebuild.sh` 头部注释（`--help` 输出源）。
