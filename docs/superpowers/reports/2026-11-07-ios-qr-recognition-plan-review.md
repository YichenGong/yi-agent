# iOS 二维码识别后黑屏 plan 审查记录

- 日期：2026-11-07
- 对象：[实施 plan](../plans/2026-11-07-ios-qr-recognition-black-screen-impl.md)、[修复 spec](../specs/2026-11-07-ios-qr-recognition-black-screen-design.md)
- 用户要求：反复检查 plan，直至没有未解决审查意见；不执行修复。
- 审查基线：`c37189b3`。本轮只修订 Markdown 文档，扫码生产代码未变。
- 状态：完成第一轮专项审查核实、第二轮独立全文复审与第三轮证据收口，当前无未解决审查意见；这是有限审查结论，不保证实现后不会发现新缺陷。本记录不代表代码实现、全量测试或真机通过。

## 1. 审查流程

1. 主会话逐任务核对现有实现、测试辅助函数、Task 之间可独立提交的边界与 spec 判据。
2. 派出两位只读审查者分别检查生命周期和测试可执行性；测试审查交付，生命周期代理停滞后取消，未计为通过。对交付意见核对证据，不以审查者结论代替验证。
3. 在内存中组装 plan 的测试示例，用仓库 TypeScript 配置做 strict 类型检查；将关键候选模块在内存转译，使用真实 React 组件、jsdom 与模拟媒体/解码验证关键状态流。没有写入生产或持久化测试文件。
4. 第二轮代理首次停滞后取消，重新发起的独立审查者交付最新全文复审。等待接口曾报资源暂不可用，改用状态查询；取消/未交付代理不计入审查结果。主会话最后核对覆盖矩阵、文档链接、未完成状态与提交范围。

## 2. 已发现问题与修订

| 编号 | 证据/风险 | 修订与核验 |
|---|---|---|
| P01 | 原 Task 2 先给 ScanHost 加 active 终止，旧 PairingScreen 无效码回调仍返回 void；undefined 被接受，后续合法码被吞 | Task 2 只更新类型与 ref 透传；Task 3 同提交交付 boolean handler 和 active。内存真实组件探针复现旧边界：普通码→合法码兑换 0 次；修订后 false→true 且兑换 1 次 |
| P02 | A01 在 dialog 断言前 expect 返回 true，旧实现 undefined 会先红，未证明覆盖层缺陷 | 先保存返回值，先断言 dialog 移除，再检查 true；要求实现期红来自行为 |
| P03 | 原 A16 返回后立即卸载，将返回 handler 与 cleanup 混为一个边界 | 拆成返回但宿主仍挂载/直接卸载后挂新页 × resolve/reject 四组，全部必测；四组内存探针通过 |
| P04 | 原旧配置夹具不是合法 RemoteConfig，不能证明重配保留真实配置；fields() 仅按“配对”查按钮，busy 后会查询失败 | 使用合法 pretty JSON，先验证 storedRemoteConfig 可读；缓存 busy 前 fields，迟到结果比较原字符串 |
| P05 | ScanHost 首接受即终止会掩盖坏的 attempt 准入实现，仅多发扫码事件不足 | 新增同一 act 两次直接 form submit 和三输入 change，随后 pending 再提交；验证兑换 1 次、第一组字段不变、disabled 和恢复 |
| P06 | A10/A17 和 A11 只写操作概要，执行者需自行补 helper/分支 | 补七场景错误表、两阶段旧实例测试和完整 deferred 定义；四文件组装 strict 检查通过 |
| P07 | A08 无效码→合法码真实相机测试只有文字；StrictMode 废弃流容易误算为泄漏或提前停止 | 补普通/StrictMode 表驱动用例，重放后取 baseline，仅无效码时断言唯一存活流；有效码后全部轨道恰停一次 |
| P08 | 新 worktree 没有被忽略的 binaries，release build.rs 需执行 sidecar 取 identity，原打包命令缺前置 | Task 5 加现有 npm run sidecar:release，再 build/ios:build；核对 build-sidecar.sh、build.rs fallback 与 package.json，不声称已经打包 |
| P09 | 整体替换 navigator 比仅替换媒体 API 范围更大，恢复边界不够直观 | mock 改为保存/替换/恢复 mediaDevices descriptor；cleanup 先于 mock/timer 恢复 |

## 3. 核实后未采纳的意见

- “get-stop-reject 应停轨 1 次”：该分支是 pendingStream.reject，不是 resolve，没有获得流，轨道停止应为 0。A09 单独验证 stop 后迟到 resolve 的所有轨道释放；不得混淆两种 Promise 结果。
- “play Promise 未 settle 导致 onResult throw 不会测试”：现有循环在 play Promise 尚 pending 时已安装 interval；onResult 同步 throw 用例仍可触发。保留该明确的现有时序，不改成 await play 的新行为。
- “A08 alert 不唯一”：无效码时表单错误节点受 !scanning 隐藏，within(dialog) 唯一；有效准入清错，全局 alert 应不存在。
- “unmount 用例掩盖返回失效”：单独 unmount 的确不能证明返回 handler；修订后的 return-without-unmount 两组已独立覆盖，直接卸载另有 spec 要求，四组均不可删。
- A11 的 mockReturnValueOnce 队列优先于 mockResolvedValue 默认值，与旧 play pending/新 play 完成的顺序一致；类型检查通过不等于已跑其持久化运行测试。

## 4. 本轮实际验证及边界

- 四个内存组装测试文件：qrScanner.test、QrScanner.test、PairingScreen.test、App mobile repair 作用域；仓库 tsconfig 的 strict、noUnusedLocals/noUnusedParameters 检查结果 **0 diagnostics**。
- 可丢弃内存探针输出：取流 reject 未获得流时停止轨道 0 次，当前 error=1/已停实例 error=0；旧 Task 2 边界回归确认；候选的接受/覆盖层/失败收敛、同批手输准入、返回/卸载晚 resolve/reject 四组、同步 scan 连续 error 普通/StrictMode、真实组件无效→有效普通/StrictMode 均通过。相机 baseline 分别 1/2，识别后无新取流且所有轨道恰停一次。
- 探针首次运行缺少可直接引用的 fake-timers 依赖，改为内部有界 interval 驱动，不安装新依赖；同步 error 探针最初误写错误文案，按仓库 CAMERA_UNAVAILABLE_TEXT 修正后重跑。两项是探针问题，不是生产缺陷。
- 以上仅验证示例类型与关键状态流，不等于全部 A01–A18、mutation、npm test/just ci 或 iOS I01–I07 通过。实现期仍必须按 plan 运行完整红绿、回退和门禁。
- 本轮不调用真实 LLM、不兑换真实码、不打包或部署；bug/feature 保持 `[ ]`，desktop 计数保持 123/139。

## 5. 第二轮全文复审与第三轮收口

独立全文复审交付了一项声称的 A02 缺口：“act 内只有一次 submit”。主会话重新读取实际代码块，确认同一 act 内连续两次 submit 均已存在，pending 后还有第三次；并用内存回退验证其判别性：仅把 runRedeem/submit 的同步 attempt 准入改为 busy 闭包守卫，兑换变成 **2 次**，当前候选为 **1 次**。因此该意见是误读，未删除正确的测试，也未将其记成新的已修生产缺陷。审查者其余全文核对未发现重要执行矛盾；FakeSocket 方法/中继通道仍要求实施期按真实夹具跑，不为测试修改生产协议。

主会话最终逐项核对 P01–P09、错误 reject/resolve 区别、A02 判别性、A16 四分支、StrictMode baseline、相机 mock 恢复、A05/A07 mutation 和 iOS 构建前置，当前没有未解决审查意见。文档检查：**23 个扫码相关链接存在，A01–A18/I01–I07 完整，模块计数 123/139**；未匹配实际无效 checklist 或 plan 占位。全仓链接扫描遇到一个不相关历史 bug-list 链接失效，未擅改，最终链接结论只覆盖本次扫码文档。`cargo fmt --all` 未造成代码变化，`git diff --check` 通过。

提交前删除可丢弃探针，仅提交本轮 Markdown；不把内存探针当作永久回归或全量 CI。合并后复核文档链接/状态/范围，并比对用户原有 Rust 文件校验值，保持无关修改。
