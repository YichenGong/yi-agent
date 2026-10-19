/**
 * `/config` 的卡片数据源。
 *
 * **不能**直接读进程级 `config/read`：那是 sidecar 启动时的快照（workdir 恒为
 * sidecar 的 cwd、model 是清单默认、yolo 是启动 flag），而桌面端是多会话、
 * 每会话各自有 cwd / 模型覆盖 / 权限档。用进程快照做「当前配置」卡片会把
 * 用户没选过的 provider / 模型名 / 工作目录摊到界面上。
 *
 * 这里要求把当前会话的 `threadId` 一并传给服务端，由服务端按该会话解析出
 * 真实生效的 provider / api_url / model / workdir / yolo。
 */

/** `config/read` 的请求参数：带会话时服务端返回该会话的生效配置。 */
export function configReadParams(threadId: string | null | undefined): Record<string, string> {
  return threadId ? { threadId } : {};
}
