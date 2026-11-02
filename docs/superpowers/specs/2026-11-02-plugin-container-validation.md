# 插件容器技术验证（G0 门禁材料）

日期：2026-11-02

状态：**等待人工审阅**。本文是 G0 的输入，不是 G0 的结论。是否继续 Phase 1 之后的产品容器实现，由用户在本报告基础上明确批准后决定。

对应设计与计划：[设计](2026-11-02-kanban-independent-release-design.md)、[计划](../plans/2026-11-02-kanban-independent-release.md)（T01 / T02）。

## 1. 这次验证回答什么问题

设计 §4 列出了七条安全要求，并明确规定「只有 mock 测试通过不足以解除这一门槛」——必须来自真实 WebView 的运行证据。T01 因此没有先写产品容器，而是先做一个技术探针：用**正式构建的桌面壳**、**正式窗口配置**、**正式 CSP 与资源策略**加载一个会主动攻击自己的插件页面，把每条要求的结果如实记录成 JSON。

探针有几条刻意的设计约束，因为它们直接决定证据是否可信：

- **不采信页面自报结论。** frame 登记、nonce、截止时间、容器重建能力都在原生侧；页面只能「上报它观察到了什么」，是否通过由原生侧裁定。恢复类用例尤其如此：页面说「我已恢复」会被直接判为失败。
- **探针与后续产品容器共用同一份策略描述**（`plugin_host::probe::ContainerConfig`），刻意不做成一次性测试代码。报告里的 `containerConfigSha256` 之所以有意义，正是因为它描述的是将来要用的那套配置。
- **不进入正式发布物。** 整段代码在非默认 `plugin-probe` feature 后面；默认构建里连 `--plugin-probe-run` 这个字符串都不存在（`grep -c plugin-probe-run target/debug/desktop` = 0）。
- **失败就是失败。** `skipped` / `unsupported` / 缺 case / 空报告 / 超时 / 非零退出，在 `probe-lib.mjs` 里全部按失败处理。设计 §4 不允许把要求降级成「这个平台不支持」。

## 2. 运行条件

| 项 | 值 |
|---|---|
| 平台 | macOS / arm64（唯一进入支持矩阵的平台，见 §6） |
| WebView | 系统 WKWebView（Tauri 2 / wry），非 Chrome、非模拟 |
| 构建 | 本 worktree 的 release bundle，`--features plugin-probe` |
| 产物 | `desktop/src-tauri/target/release/bundle/macos/yi-agent.app` |
| 可执行文件 | `Contents/MacOS/desktop`（不是 `productName`，见 §7） |
| 应用摘要 | `d40eb89fc862f80ddfaa9cdf26c7f4552774ca5bc7e274aea90f9c2574101edd`（`app_digest()` 对 `Contents/MacOS/desktop` 取的文件摘要，不是整个 bundle；随构建变化，复现时以报告里的 `appSha256` 为准） |
| 容器配置摘要 | `95a6a842b55fde346833d08161794773997073f04d163f8f2b7da3862c341b5` |
| 实例根 | 一次性临时目录（`YI_AGENT_HOME` 指向它），不触碰真实 `~/.yi-agent` |
| 结论 | 12 / 12 用例通过（`startedAt` 18:25:40.269Z → `finishedAt` 18:25:41.141Z，时长约 0.87s） |

复现命令：

```bash
(cd desktop && npm ci && npm run sidecar:release && npm run build \
  && npm run tauri -- build --bundles app --features plugin-probe)
node --test desktop/scripts/plugins/probe-report.test.mjs
node desktop/scripts/plugins/probe.mjs --suite isolation --output .artifacts/plugins/probe.json
```

报告产物在 `.artifacts/plugins/probe.json`（被 gitignore；报告本身是运行产物，不进版本库，复现方式才是）。

配套测试：报告校验器 `node --test desktop/scripts/plugins/probe-report.test.mjs`（15 例，含「删掉 `blocked_script_recovery` / `native_invoke_denied` / `cross_frame_replay_denied` 任一条都必须变红」）；原生侧 `cd desktop/src-tauri && cargo test --features plugin-probe --test plugin_probe`（32 例）。

## 3. 当前容器配置

```
scheme              plugin-asset://
CSP                 default-src 'none';
                    script-src 'self' 'unsafe-inline';
                    style-src 'self' 'unsafe-inline';
                    img-src 'self'; font-src 'self'; connect-src 'self';
                    frame-src 'none'; child-src 'none'; worker-src 'none';
                    form-action 'none'; base-uri 'none'; object-src 'none';
                    navigate-to 'none'; manifest-src 'none'; media-src 'none';
                    frame-ancestors 'none'
allow_remote_network  false
max_frames            4
case_deadline         20s
recovery              close_and_rebuild
```

两点需要审阅者注意：

1. **`script-src` 含 `'unsafe-inline'`。** 插件页面是单页静态应用，初始化 nonce 由原生侧通过 `initialization_script` 注入；要让页面脚本跑起来必须放行内联脚本。真正的隔离来自「资源只来自白名单 + 无宿主原生能力 + 无外送通道」，而不是禁止内联。这是当前取舍，若审阅认为不可接受，替代方案是把初始化数据改成 `plugin-asset://` 下的一个文档并配合 script nonce——但那会把 nonce 暴露给同源的所有静态资源，收益不明显。
2. **CSP 里的 `navigate-to` 在 WebKit 上支持不完整。** 因此顶层跳转不由 CSP 兜底，而是宿主用 `on_navigation` 独立拦截（只允许 `plugin-asset://localhost`）。只靠 CSP 声称「已禁止导航」是不成立的，这条已在代码与测试里分开表达。

## 4. 逐条对照设计 §4

每条都给出**实际观察到的证据**，不是「应该如此」。

### §4.1 页面不能读宿主 DOM、存储、内部 JS、其他插件资源或其他版本资源

| 子项 | 结果 | 证据 |
|---|---|---|
| 宿主 DOM | PASS | 插件在独立 webview 中，没有宿主父文档可访问（`the frame has no host parent (separate webview)`） |
| 宿主存储 | PASS | 见下方说明——这是本次最重要的一条修正 |
| 其他插件资源 | PASS | 请求 `foreign/secret.txt` 返回 **403**；该文件在 fixture 里**真实存在**，所以是边界拒绝而非「文件不存在」的假通过 |
| 其他版本资源 | PASS | 请求 `../v0.0.1/index.html` 返回 **404**（不在白名单即不存在），且路径规范化会拒绝越界 |

**宿主存储这条要单独说明。** 首次运行时该用例失败，页面报出 `host bridge globals are visible: __TAURI_INTERNALS__, ipc`。这不是误报：Tauri 确实会向每个 webview 注入桥对象。但「对象存在」与「能读到宿主数据」是两件事——原用例把前者当成后者，判据错了。

修正后的判据是**实际发起读取**，四条都必须失败：

```
host rpc (sidecar)      → Command rpc not found
plugin store            → Command plugin:store|get not allowed by ACL
filesystem read         → Command plugin:fs|read_text_file not allowed by ACL
dialog open             → Command plugin:dialog|open not allowed by ACL
```

同时报告如实记录：页面拥有**自己的**浏览器本地存储（这是浏览器给的，不是宿主存储，允许存在），以及桥对象确实被注入。把这三件事分开写，审阅者才能判断隔离到底靠什么——靠的是 ACL 而非「对象不存在」。若将来某人删掉了 capability 限制，这条会立刻变红，而不会因为「全局变量本来就在」而永远通过或永远失败。

### §4.2 页面不能直接调用 Tauri invoke、原生 opener、文件对话框等主窗口能力

PASS。`plugin:opener|open_url` 被 ACL 拒绝（`Command plugin:opener|open_url not allowed by ACL`）。同时报告记录了 `invoke is reachable from inline script`——即接口可达但调用被拒，与 §4.1 同一套判断逻辑。

探针窗口**不挂任何 capability**，这正是要证明的：能力靠配置不在，而不是靠页面「没去调用」。

### §4.3 资源只来自当前包的静态文件白名单

PASS。`AssetRoot` 在启动时一次性建好「包内相对路径 → 绝对路径」白名单：符号链接一律跳过、越界路径直接拒绝、运行期不解释路径。单测覆盖 `../outside`、`ui/../../outside`、`/etc/passwd`、空字符串、以及指向包外的符号链接。

### §4.4 禁止联网、顶层跳转、弹窗、子 frame、表单外送与远程资源

| 子项 | 结果 | 证据 |
|---|---|---|
| `fetch` 远程 | PASS | `TypeError: Load failed`（两条，GET 与 POST） |
| 远程图片 | PASS | 不加载（`img.onload` 未触发），CSP `img-src 'self'` 生效 |
| 表单外送 | PASS | `form-action 'none'`，提交不改变 location |
| 顶层跳转 | PASS | 宿主 `on_navigation` 拒绝非 `plugin-asset://localhost`；页面侧也观察到 location 未变 |
| 弹窗 | PASS | `window.open` 不产生窗口 |
| 子 frame | PASS | `frame-src 'none'`，无子 frame 产生（同时是 §4.7 重放通道不存在的前提） |

### §4.6 frame 绑定、一次性初始化、防冒领与防重放

| 子项 | 结果 | 证据 |
|---|---|---|
| 防冒领 | PASS | 用另一 frame 的 nonce 伪造回报 → **403** |
| 防重放 | PASS | 每个 frame 只接受一次初始化；第二次加载记为重放并把该用例判失败 |
| 恢复 | PASS | 见 §4.7 |

两个实现细节值得记录，因为它们都是「看起来对、实际错」的坑：

- `on_page_load` 对一次加载会回调**两次**（`Started` 与 `Finished`）。最初两个都计数，结果是每一次正常加载都被判成重放。现在只数 `Finished`。
- 原生侧检测到重放时，最初是**追加**一条 `cross_frame_replay_denied`，这违反了报告契约（重复 case id 会被 `checkProbeReport` 拒绝）。现在改为**覆盖**页面自报的那一条。

### §4.7 异常、持续阻塞脚本与大量分配下的故障边界

这是设计 §4 里最重要的一条，也是唯一无法靠 CSP 或配置解决的：**iframe 不等于独立渲染进程，主页面 JS watchdog 在页面主线程自旋时同样失效，恢复必须由原生侧完成。**

实测通过，证据如下（`judge_blocked_recovery` 的四项事实全部成立）：

```
the page announced blocking before it spun the main thread
the native side closed and rebuilt the container (generation 2)
the rebuilt container reported working interaction (focus/IME/scroll/size)
the host-persisted marker survived the rebuild
```

具体流程：页面先上报「我要阻塞了」，然后**真的自旋 60s**；原生侧轮询到阻塞信号后，在主线程关闭窗口、重建容器（generation 2，新 nonce、新 frame）；重建后的页面走恢复通道，确认宿主写下的 marker 仍在（证明会话持久化未受损）并执行正常交互检查。页面自报「已恢复」会被 `judge_blocked_recovery` 直接判失败。

大量分配：`alloc_burst_is_bounded` 只做 32 MiB 有界突发后立即释放，**不试图耗尽机器内存**。设计 §4 明确「首版不承诺恶意资源耗尽下零卡顿或硬 CPU/内存配额」，因此这条证明的是「克制、可观测、可释放」，不是「免疫 OOM」。

**负例（同样重要）。** 用一个不执行任何脚本的 fixture 重跑：

```bash
node desktop/scripts/plugins/probe.mjs --suite isolation \
  --fixture-root /tmp/probe-negative --output .artifacts/plugins/negative.json
```

结果：**12/12 失败**。11 条证据为 `native deadline expired`；恢复用例不是四项全否，而是「页面从未确认阻塞」与「重建后无交互回报」成立为 FAIL，而「原生侧已重建到 generation 2」「宿主持久化 marker 仍在」两条事实为真——这正是修复 §7.6 之后应有的样子：宿主的重建能力被实打实走通，但整个用例依然失败，因为页面确实什么都没做。进程正常退出、报告正常落盘、runner 退 1。

这个负例第一次跑时**没有产出任何报告**——进程在「没有窗口了」时退出了事件循环，把还在等截止时间的编排线程一起带走。那会让失败表现为「探针没跑过」而不是「探针跑失败了」，是最糟的一种沉默。修复方式是：只有在编排线程明确置位 `finished` 之后才允许 `ExitRequested` 生效。顺序必须「先置位、再 exit」，反过来会永远退不出去。

### 正常路径（设计 §4 第 6 条）

`normal_interaction_works` 通过，逐项：焦点可取、中文输入（`中文输入` 经过 `input` 事件后原样保留）、主题令牌解析（`--bg` 有值）、页面可滚动、面板尺寸 1280x820（沿用主窗口配置，验证的是**正式窗口形态**）。

报告**不只**覆盖攻击用例——这是 T02 Step 2 的明确要求。

## 5. 正反例的配置差异

两次运行使用**同一个** release bundle、同一份 `ContainerConfig`、同一个 runner。唯一差异是 `--fixture-root`：

| | 正例 | 负例 |
|---|---|---|
| fixture | `plugins/fixtures/example-counter/probe`（含 `attacks.js`） | `/tmp/probe-negative`（只有一段静态 HTML） |
| 页面行为 | 主动攻击并上报 | 什么都不做 |
| 结果 | 12/12 通过 | 12/12 失败：11 条 `native deadline expired`，`blocked_script_recovery` 为「页面从未确认阻塞 + 原生侧确实重建到 generation 2 + 重建后无交互回报」 |
| 报告 | 正常落盘 | 正常落盘 |
| 退出码 | 0 | 1 |

配置没有为负例做任何放宽，负例也不是靠「换个配置」才失败的——这正是要证明的：失败来自页面行为与原生裁定，不是来自配置开关。

注意两次报告的 `containerConfigSha256` **不同**（`95a6a842…` 对 `939dd5ea…`），这不代表策略变了。该摘要刻意把「容器配置 + 它服务的资源清单规模」一起哈希，正是为了防止「配置没变但白名单换了一套」仍显示同一个摘要；负例 fixture 只有 1 个文件、正例有 3 个，所以摘要必然不同。策略本体（CSP、scheme、`allow_remote_network=false`、`max_frames=4`、截止时间、恢复策略）两侧完全一致。

## 6. 支持平台矩阵

| 平台 | 状态 | 说明 |
|---|---|---|
| macOS arm64 | **已验证** | 本文全部证据来自此平台 |
| macOS x86_64 | 未验证 | 无证据，不进矩阵 |
| Linux | 未验证 | runner 直接退 2 并说明「没有启动适配器」 |
| Windows | 未验证 | 同上。设计 §1 已明确：当前插件使用 Unix socket / flock，不能仅凭打出 Windows 包就宣称支持 |

runner 对未支持平台的做法是**明确失败**而不是「跳过该平台后仍算通过」。计划 T02 Step 3 的要求是：若某个受支持平台上的恢复无法成立，就停止后续实现并提交替代容器设计，**不允许把测试改成跳过该平台**。

## 7. 期间发现并修复的问题

这些都是写探针时暴露出来的真实缺陷，不是测试本身的问题：

1. **同一 crate 展开两次 `generate_context!()`** 会各自定义一份 `_EMBED_INFO_PLIST` 静态符号，链接期报 `symbol already defined`。原先探针自己展开一份，正式路径再展开一份。现在全进程只在 `lib.rs` 生成一次并传给探针。这也是探针测试必须放在 `tests/plugin_probe.rs`（integration test）而不是 `--lib` 的原因。
2. **报告丢掉了每条用例自己的证据**，只剩「reported by X」。这会让 G0 拿到 12 个 verdict 却没有任何理由可审。现已把页面证据透传到报告。
3. **报告时间戳在构造时打点**，导致 `startedAt == finishedAt`、时长永远为 0，无法判断是否撞上截止时间。现由编排起点回推。
4. **bundle 里的可执行文件不是 `productName`。** bundle 叫 `yi-agent.app`，但 `CFBundleExecutable` 是 `desktop`，兄弟位置还有一个 `yi-agent`（sidecar）。按 `productName` 猜会去 exec sidecar，得到 `unexpected argument '--plugin-probe-run'`。
5. **负例静默无报告**（见 §4.7 末段）。
6. **“原生侧能重建容器”曾被页面是否配合所阻塞。** 重建门控原先写成 `blocked_confirmed && closed`：只有页面先宣告“我要阻塞了”才重建。静态页面连 JS 都不跑，真实攻击者更不会宣告——于是负例里只关不建，证明的是“能关”，不是设计 §4.7 要的“能关**并能重建**”。门控已改为只看关闭是否成功（`should_rebuild_after_close`），并加了回归测试钉住。修复后负例的证据变成「`the native side closed and rebuilt the container (generation 2)`」加「页面从未确认阻塞」——能力被验证，但整体仍判失败，这才是正确语义。
7. **报告里的应用摘要会随重建变化。** 本文件初稿记的是更早一次 bundle 的哈希；写完文档后又重建过 bundle，两者对不上。现在改为以报告自身的 `appSha256` 为准，并注明它随构建变化。容器配置摘要（`containerConfigSha256`）不随重建变化，但它**随资源清单规模变化**（见 §5），所以跨 fixture 比较时不能拿它当“策略相同”的证明；策略是否相同要看摘要的构成项。

## 8. 需要审阅者决定的事

1. **是否接受 `script-src 'unsafe-inline'`**（§3 第 1 点）。当前隔离不依赖禁用内联脚本，但这是明确的放宽项。
2. **是否接受「桥对象注入但 ACL 拒绝」**作为 §4.1/§4.2 的通过依据。当前判据是「读取失败」而非「对象不存在」。我们倾向于接受：ACL 是实际边界，对象可见本身不构成越权；但这是安全取舍，应由审阅者确认。
3. **macOS x86_64 是否需要进入首版矩阵。** 当前只有 arm64 证据。
4. **恢复策略 `close_and_rebuild` 是否满足产品预期。** 探针证明它能恢复容器与核心交互、会话标记不丢；但它会丢弃页面内未持久化的状态，产品侧需确认这是可接受的行为。

## 9. 结论

在 macOS / arm64、系统 WKWebView、正式构建与正式容器配置下，设计 §4 的七条要求**全部有真实运行证据支持**，包括最关键的「原生侧能关闭并重建失控容器」这一条。负例证明这套判据不是「怎么写都会过」。

按计划，**G0 不能由实现者自行认定**。本文件提交后等待用户明确批准；未获批准前不开始 T03 及之后的任何产品容器实现。
