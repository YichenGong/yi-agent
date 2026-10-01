# Superpowers 看板插件

## 安装
1. 放入 `board-runner` 可执行文件（`cargo build -p board-runner --release`）。
2. 放入 `kanban` skill 目录。
3. 把 `supervisors/superpowers-board.json` 复制到
   `<项目>/.yi-agent/supervisors/superpowers-board.json`，并按需改 `command` 为绝对路径。
4. 在 `<项目>/.yi-agent/preferences.json` 写入 `{"superpowers_board": true}`（或在 TUI/桌面里打开开关）。

## 运行前提
daemon 常驻（`yi-agent daemon start`）。daemon 会按清单与开关拉起 `board-runner` 并守护它。

## 卸载
删除 `supervisors/superpowers-board.json`（daemon 随即停掉该进程），再删除本目录。
正在 daemon 中运行的会话会照常跑完，不影响主程序。

## 配置
`kanban.toml` 放在 `<项目>/.yi-agent/board/kanban.toml`（示例见本目录）。

## 加入看板
在 TUI 里 `/kanban add <spec> <plan>`；桌面端用看板面板的入队动作。
宿主只把投递写进 `<项目>/.yi-agent/board/inbox/`，插件每 tick 消费并入队。
