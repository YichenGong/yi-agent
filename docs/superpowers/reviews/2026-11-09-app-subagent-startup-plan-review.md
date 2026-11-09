# App subagent 启动修复 plan 源码与执行边界复审

日期：2026-11-09  
范围：[implementation plan](../plans/2026-11-09-app-subagent-startup-repair-impl.md)、[同步 spec](../specs/2026-11-09-app-subagent-startup-repair-design.md)。  
基线：`fix/managed-session-container-attach`，HEAD `a0b6f9dc` 加原有未提交修复。

## 证据边界

本记录承接用户“反复检查plan，直到没有问题”。由主会话作者多轮源码对照及故障推演完成，没有独立审查子代理。只改文档，未实施代码，未运行Rust/Vitest/CI/live，未提交、合并或清理worktree。旧稿的无已知阻塞结论不作为本轮免检依据。

## 第一轮：接口与故障路径

| ID | 实际缺口及源码依据 | 修订与执行时判据 |
|---|---|---|
| R01 | lifecycle `codec::encode/decode`采用control_version envelope，而不是裸JSON；帧上限含header | Task4明确4byte BE长度、版本校验、16KiB含header、有界IO；错误版本/partial frame拒绝 |
| R02 | `EnsureDaemon`、`WatchmanInstall`旧闭包不接deadline；daemon固定READY_TIMEOUT；launchctl::status无界 | Task2新增预算签名，单blocking worker持闸，launchctl try_wait+归属回收；expired budget不发后续副作用，超时caller不提前放闸 |
| R03 | bringup测试实际是integration target，`--lib business_`零命中；原公开wrapper自动真实probe会污染fake | Task3显式BusinessProbe两入口，测试target managed_bringup，fake全注入；Inspect不取创建目录的协调锁 |
| R04 | store `send_request_with_version_timeout`仅readtimeout，connect/write仍可能阻塞 | Task3新增send_status_until，nonblocking connect/poll/SO_ERROR及同deadline读写，保持IpcError；silent/partial fixture有界 |
| R05 | guardian start_daemon重建spawn预算、hello失败只warn、spawn后路径错误不回收、bridge设置晚于self.daemon提交 | Task4原deadline clone，hello fatal、归一cleanup，fallible准备后再提交字段；故障注入无partial ready，只回收本次child |
| R06 | getrandom仅lifecycle直接声明；“复用随机源”没有可用公共入口 | Task4 lifecycle fresh_startup_nonce公共helper，OS随机失败返回错误，不新增supervisors依赖，不派生generation secret |
| R07 | guardian端主要写lifeline，没有所描述的现成长期reader；共享FD timeout可影响daemon授权reader | Task4只读一次Ready，运行中非消费EOF观测+registry exit撤ready；daemon Ready写入/timeout恢复在授权reader启动前完成 |

## 第二轮：测试触达、恢复与租约所有权

| ID | 实际缺口及源码依据 | 修订与执行时判据 |
|---|---|---|
| R08 | A7先放旧responder再prepare会被A6预检拦住，测不到Ready | Task4直接驱动start_daemon接缝并记录spawn/Ready等待witness；Ready→Status变异必须真红，旧PID仍活 |
| R09 | runtime_lifecycle `record_view`返回void，settle返回原view；registered_projects返回scope key而非路径 | Task5 settle/settle_with及直接返回路径回填同view，native只读canonical别名对账，真实回包/serde测试 |
| R10 | late held回答若无条件release，A→B→A可杀后来同项目租约；release非空failures并非成功 | Task5逐项目chain、release-before-reacquire、rewanted不释放、失败保留unknown责任；deferred Promise精确时序 |
| R11 | RPC prepare_turn_core不持agent；driver首次降级后activate=None且activation_attempted=true会永久无root | Task5 driver-owned context/state、模型请求前认证重装配；同driver连续turn及无root降级恢复测试，不借busy安全迁移 |
| R12 | `_shell/prepare`不注册App lease；旧integration注释禁止FrontendLease::register，与完整壳夹具矛盾 | Task6模拟真实shell App/Business lease+heartbeat，initialize shell身份，反向intent校验并真实mint RootAuthorityEntry，更新旧注释；坏epoch/scope不得签发 |

## 第三轮：修订后全文与生产资源所有权

| ID | 再检查发现的问题 | 修订与执行时判据 |
|---|---|---|
| R13 | WS现有serve_ws_inner仍从环境读models/resident/HOME；同名ManagedContext易误选；脚本清理仅字符串前缀不安全 | Task1显式attachments/home测试入口、attach层context；Task7 canonical/symlink/非空root/manifest边界测试，不触真实launchd或API |
| R14 | 只改driver manager仍使ThreadSession RPC与server process_watches指向旧manager；Task5混三层缺独立review gate | Task5共享EffectiveProcessTooling slot取代重复watch owner，RPC list/read/kill与通知实际验证新manager；5A UI、5B held、5C driver独立red/green/review |

再次对照Task1–7与A1–A11，检查策略/连接身份独立、锁顺序、预算所有权、Ready起止、迁移安全、下一turn不重放、真实shell授权、侧车与App验收区分。发现的旧签名、旧测试名、虚构reader及busy确认文字已统一，不保留并行契约。

## 本轮结论与验证

源码对照及修订后第三轮复查，**未再发现已知设计阻塞项**。这是可供实施的计划自审结论，不是绝对正确承诺或App已修复结论。实现必须逐项完成计划的行为red/green、变异判据和最新CI；A10只能用本次隔离真实App窗口及真实模型证明，SKIPPED不能当完成。

文档验证范围：相对链接、A1–A11唯一性和Task覆盖、Task编号/未执行checkbox、禁止占位符、模块68/81一致性、git diff --check、分支/HEAD不变及非docs文件内容基线哈希。以当前会话实际检查输出为准，不把计划中的未来命令记PASS。

非docs修改文件聚合SHA256基线：`0666c94e9759a797088f5fc4c376732d806734b22ccedfa3b3ae80d2822cab5d`。聚合按git diff --name-only的非docs文件名排序，逐一加入文件名与文件内容；它只证明这些旧实现文件本轮未变，不代表代码功能验收。

交付保留当前worktree、分支和原有未提交代码；不自行合并。
