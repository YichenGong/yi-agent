# Desktop 内容加载超时回归记录（2026-11-11）

## 根因与修复

`thread/resume` 在业务循环中等待 daemon 的 `ClientLeased`；桌面壳必须先拿到 `_shell/prepare` 响应才能登记 business lease。管理请求此前也要经过被 resume 占住的业务循环，因此等待可达 30 秒，超过前端 15 秒内容看门狗。

修复将本地 bootstrap/管理请求交给独立 stdio reader，复用原认证、epoch、定向回包和帧上限规则；不扩大权限、不延长看门狗。入站业务准入检查防止后到的 bootstrap 放行先到的业务请求；业务 receiver 关闭时停止 reader。

## 自动验证

- `prepare_completes_while_the_business_loop_is_blocked`：原 reader 超时失败，修复后管理响应在业务队列不消费时完成。
- `closing_business_dispatch_stops_the_management_reader`：原 reader 会在关闭后启动 prepare，修复后不调用 backend 且 reader 退出。
- 管理通道 17 项通过；MCP/store/supervisors/管理通道联合 1285 项通过。
- 完整 `just ci` exit 0：6408 项通过 / 41 skipped，附加 nextest 50 项通过；fmt、Clippy、workspace build、bindings 与 Rust 1.89 MSRV 真编译均通过。
- 前端 App/useRuntimeLease/syncWatchdog：313 项通过。
- 精准升级 rmcp 3.9、rusqlite 0.41.1 / libsqlite3-sys 0.39.1，清除 RUSTSEC-2026-0457/0462/0461/0459；保留 bundled、启用 fallible_uint 保持无符号整数受检转换。通过本机 7897 代理更新 RustSec 后在线审计无漏洞；没有新增豁免，仅有既有 chacha20 yanked warning。
- 测试子进程清除继承的 YI_AGENT_* 会话变量，避免无效 managed fd 与真实实例作用域污染测试；不改全局配置。
- 修复提交 `97e8273a`，合并提交 `994dc2e6`；两次独立复审无阻断。合并结果在干净 worktree 完整 `just ci` exit 0，6408 + 50 项通过，在线 RustSec 审计成功。主目录原有未跟踪的 `probe_guardian.rs` 未修改，其格式问题不混入本次提交。
- GitHub 访问使用 `http://127.0.0.1:7897`；验证子进程同时设置 `NO_PROXY` / `no_proxy=127.0.0.1,localhost,::1`，防止本地 mock 请求经代理导致连接复用断言失败。补齐回环排除后相关 6 项及完整 CI 均通过。

## 原生 App 验收

Tauri release build + finalize-runtime-bundle.sh 成功，签名与 sidecar 摘要校验通过；build ID `c11f2e01e20a3982c799088cc2298d38`。隔离 bundle ID `com.gongyichen.desktop.timeout-native`，临时 HOME/YI_AGENT_HOME，无正式 relay、真实 key 或对话数据；使用既有本地 mock 历史夹具。

真实 WKWebView + React + Tauri bridge + sidecar + guardian/daemon 链路：首次历史正文在 2.414 秒可见；另一次干净 runtime 验证显式 `thread/resume` 在 342ms 返回成功，421ms 时正文可见，status 为 sameBuild 且持 business lease。原生截图可见用户消息和完整回答，没有内容加载超时。探针只装入临时 App，不进入正式包。

测试中一次 212ms 的缓存/早期画面观察不作为 RPC 恢复证据；强制 SIGTERM 后残留旧 root 的轮次以及一次探针 invoke 拦截未完成的轮次不计通过。清理隔离 runtime 后改为真实 RPC 响应判据和正常 Quit。最终稳定画面轮次 `thread/resume` 345ms 成功，等待状态栏稳定至 5.465s 后截图，正文完整、无运行时警告/加载超时，正常退出码 0。临时证据位于 `/private/tmp/yi-desktop-timeout-native/`，可能被系统回收。

## 部署边界

正式 `/Applications/yi-agent.app` 尚未替换。旧 guardian 托管本次工具执行，不能在当前任务中盲目杀掉。正式部署必须先完成受控退出，保留完整旧 bundle，再安装并验证新包；新旧 runtime 构建不一致时仍必须走版本确认，不伪造 sameBuild 或绕过授权。

合并后已从干净源码重新构建候选包：`desktop/src-tauri/target/release/bundle/macos/yi-agent.app`，版本 `0.1.13-dev+994dc2e6`，build ID 与上述原生验收一致。`finalize-runtime-bundle.sh` 的签名与摘要复核通过，sidecar SHA-256 为 `6f82367ba71e3b798824716e91cb2fe230e469e5b86fa8899dbc92fa03b4a3ec`。该路径是构建产物，不代表已安装；不要在旧 guardian 仍使用正式 bundle 时删除或覆盖它。
