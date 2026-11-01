# Dummy Hello World Design

## Goal

用一个最小可运行的产物验证 Superpowers 看板流水线：从排队、拉起实现会话、
在 `kanban/<slug>` worktree 里改代码，到产出可执行的验证结果。

这是排障用的烟囱测试卡片，不承载任何业务功能。除本卡新增的两个文件外，不应
触碰仓库其它部分。

## Scope

新增一个 shell 脚本 `scripts/dummy-hello-world.sh`：

- 打印一行 `Hello World` 到 stdout，退出码为 0。
- 保持 POSIX `sh` 兼容，不引入额外依赖。
- 脚本自带可执行权限。

明确不做的事：不接入 CI、不在 `justfile` 里注册配方、不改任何 Rust/TS 代码、
不改 `docs/project-management/` 之外的文档。

## Validation

在仓库根执行 `sh scripts/dummy-hello-world.sh`，stdout 必须精确等于
`Hello World`（带换行），退出码为 0。plan 里给出对应的断言命令。
