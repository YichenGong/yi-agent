# Desktop 历史会话即时可读设计

日期：2026-11-11。用户已确认：先显示本地快照，后台逐块刷新，而非阻塞到加载完成。

## 行为与边界

- Desktop 本地会话列表同步快照先画侧栏，IndexedDB 异步读最近正文；均不等待 initialize/listAll/resume。
- 本地快照不是权威数据，不设置 warm，不允许绕过发送/激活门禁。服务端回放逐块按 item id 原位更新，明确 historyReady 后才清理旧项；运行环境继续后台准备。
- 本地缓存独立 localStorage 列表键与 IndexedDB 数据库，不复用 mobile/remote 的全局缓存。desktop 仅 LOCAL_SLOT 主会话可读写；远端槽位不能写入或读出本地缓存。不同 App bundle 使用 WebView 自身隔离存储；同一 bundle 改 YI_AGENT_HOME 的跨实例缓存不保证复用。
- 复用现有缓存归一化、裁剪与 LRU：最多 10 会话、总 5 MiB、单条目标 1 MiB（现有策略为超大单 item 保留一项）。保存最近尾部，首次无缓存时服务端仍逐块显示。
- 内容变更节流 800 ms 保存，切会话/页面隐藏尽力刷新。缓存坏值、配额和 IDB 失败均降级正常回放，不阻塞启动。
- 异步缓存读必须绑定原 store/session/槽位；服务端先到则不覆盖，重配/槽位切换后旧读不能注入。
- 运行环境激活失败不清空已读正文，显示独立错误；发送等待激活，切会话或改草稿取消待发送意图。
- historyFirst 协议仅本地 desktop opt-in。早期正文/usage/partial/ready 同样定向投递，不因订阅过滤造成空快照清除，也不影响远端旧协议。

## 验收

1. 卡住 initialize/listAll 仍可见缓存列表及选中正文；卡住 resume，已缓存正文仍可读。
2. 每批服务端 item 到达即更新，ready 前不删尚未回放的缓存项；ready 后删 ghost。
3. 缓存成功落盘后重挂载可读，选中项从最后选择恢复（含置顶项）。
4. Desktop 不触碰 remote 缓存，remote 行为回归全绿；非 local slot 缓存读写被禁止。
5. 后端 build gate 测试证明历史先于运行环境；长历史隔离原生验证不触碰正式运行会话。
6. 全量 UI 测试/build、Rust fmt 与 just ci；更新项目管理与验收记录。正式 App 未部署前不宣称已安装修复。
