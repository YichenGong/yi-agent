# 项目组长会话命名设计

## 状态

设计已由用户确认，等待 spec review。

## 背景

项目组长会话目前使用通用 thread 标题字段。组长由 `ThreadMeta.role == "lead"` 标识，但创建时标题固定为“组长”。首轮 turn 完成后，通用标题流程可能通过 LLM 自动生成标题；通用 rename 入口也能写入标题。固定标题不包含项目目录名称，不利于用户在会话列表中识别所属项目，尤其是在置顶列表中识别。

相关实现：
- 组长 meta 创建：`yi-agent-rs/crates/yi-agent-app-server/src/lead.rs` 的 `ensure_lead_meta`
- thread 标题写入与保护：`yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` 的 `touch`、`set_title_if_unchanged`、`rename`
- 首轮 LLM 标题生成：`yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 的 turn 完成路径

## 目标

1. 组长会话标题包含项目文件夹名，提升列表及置顶列表中的可识别性。
2. 组长标题为服务端管理的身份标题，不会被 LLM 自动标题或其他通用模块意外改写。
3. 普通会话标题行为保持不变。

## 非目标

- 不调整侧栏或置顶列表的排序、样式或分组。
- 不新增用户自定义组长标题机制。
- 不改组长身份判定、创建时机或会话持久化格式。

## 标题规则

- 标准格式：`<项目目录 basename> 组长`。
- 例如项目路径 `/Users/me/my-app` 的组长标题为 `my-app 组长`。
- 文件夹名取不到或为空时，回退为 `组长`。
- 文件夹名来源于该项目组长对应的项目 cwd 路径；不展示完整路径。
- 组长标题由 app-server 决定，不依赖模型生成。

## 行为与写入边界

1. `ensure_lead_meta` 创建组长时写入规范标题。
2. `ensure_lead_meta` 命中已存在的合法组长时，也将其旧的通用标题收敛为规范标题，使存量组长不必删除重建。
3. 组长不会进入通用首轮 LLM 标题生成和 fallback 标题写入流程；`ThreadStore::touch` 对组长仅更新活动时间，不生成标题。
4. LLM 标题 CAS 写入必须以组长角色做保护，即使发生并发或调用路径变化，也不得覆盖组长标题。
5. 通用 `thread/rename` 对组长不允许将标题改成其他值，维持规范名称；普通 thread 仍按现有规则允许手动改名。
6. 如果列表客户端收到标题变化通知，持久化标题和广播值必须一致；不广播未实际生效的标题。

## 实现边界建议

- 将规范标题生成封装为基于 cwd 的纯逻辑辅助函数，覆盖 basename 缺失/空值回退。
- 对所有标题写入点在服务端存储边界识别 `role == LEAD_ROLE`，避免只依赖 UI 隐藏 rename 或跳过某一个 LLM 调用点。
- 对组长的 rename 请求以兼容性为先：保持 RPC 既有响应形状，但规范标题不被改写；是否在响应中明确拒绝或返回成功但维持原名，由实现计划结合现有 RPC 契约选择，并由测试固定语义。无论具体响应形式如何，磁盘标题必须保持规范值。
- 不改变普通会话的 LLM 标题 CAS、fallback、手工 rename 优先语义。

## 测试与验收

需通过以下可验证判据：

1. 对 `/.../my-app` 创建组长，meta 标题等于 `my-app 组长`。
2. 对 basename 无法取得或为空的路径，规范标题回退为 `组长`。
3. 对已有 `role=lead` 且旧标题为 `组长` 或其他值的 meta，调用确保组长逻辑后标题收敛为项目规范标题。
4. 对组长调用 `touch` 和首轮标题生成相关路径后，标题不变且不发出错误的 LLM 标题变更通知。
5. 对组长调用通用 rename 后，标题仍是规范标题；普通会话 rename 仍可修改标题。
6. 对普通会话，既有首轮 fallback/LLM 标题生成及手动改名优先行为不回归。
7. 运行 `cd yi-agent-rs && cargo test -p yi-agent-app-server` 与 `cargo fmt --all --check`。

## 风险与注意事项

- `ensure_lead_meta` 可能对现有 meta 做规范化写入；更新必须通过 ThreadStore 现有读-改-写锁与原子落盘路径，以免覆盖并发活动字段。
- rename RPC 行为需与现有客户端约定保持兼容；测试应明确标题不可改这一不变量，而不是依赖客户端隐藏操作入口。
- LLM 生成由 turn 完成路径触发，必须在服务端入口对组长做明确排除；单靠存储 CAS 的标题比较无法保护组长标题，因为组长标题可能恰好仍匹配 fallback。
