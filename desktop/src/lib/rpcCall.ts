/** 插件/业务通道共用的 RPC 调用签名。 */
export type RpcCall = <T = unknown>(method: string, params: unknown) => Promise<T>;
