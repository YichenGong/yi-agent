/**
 * JSON-RPC 方法名总表。
 *
 * 宿主按字符串分发方法，所以这些值是**线格式**，改动等于改协议。集中到一处只
 * 为消除各 rpc 模块里的重复字面量并使用例可断言——值一律照抄，不重命名。
 * `routes.test.ts` 逐条钉住原始串。
 */
export const RPC = {
  pluginQuery: "plugin/query",
  pluginInstall: "plugin/install",
  pluginUninstall: "plugin/uninstall",
  pluginsList: "plugins/list",
  // 受管协议（`plugins/v1/*`）。与上面那组旧的 supervisor 协议**不共享**授权：
  // 旧 `plugin/install` 不会让插件在新协议里变成已安装或已授权。
  pluginsV1Catalog: "plugins/v1/catalog",
  pluginsV1List: "plugins/v1/list",
  pluginsV1Install: "plugins/v1/install",
  pluginsV1EnableProject: "plugins/v1/enableProject",
  pluginsV1ChangeGrant: "plugins/v1/changeGrant",
  pluginsV1DisableProject: "plugins/v1/disableProject",
  pluginsV1Activate: "plugins/v1/activate",
  pluginsV1Rollback: "plugins/v1/rollback",
  pluginsV1Transaction: "plugins/v1/transaction",
  pluginsV1CancelTransaction: "plugins/v1/cancelTransaction",
  pluginsV1Uninstall: "plugins/v1/uninstall",
  pluginsV1ClearData: "plugins/v1/clearData",
  pluginsV1Gc: "plugins/v1/gc",
  pluginsV1InspectLegacy: "plugins/v1/inspectLegacy",
  pluginsV1AdoptLegacy: "plugins/v1/adoptLegacy",
  pluginsV1AssetRelease: "plugins/v1/assetRelease",
  pluginsV1OperationResult: "plugins/v1/operationResult",
  residentList: "resident/list",
  onboardingStatus: "onboarding/status",
  onboardingApply: "onboarding/apply",
  onboardingTest: "onboarding/test",
  onboardingDismiss: "onboarding/dismiss",
  modelList: "model/list",
  modelImportEnv: "model/importEnv",
  modelUpsert: "model/upsert",
  modelDelete: "model/delete",
  modelSetDefault: "model/setDefault",
  modelSetSubagent: "model/setSubagent",
  threadSetModel: "thread/setModel",
  threadSetReasoningEffort: "thread/setReasoningEffort",
  modelTest: "model/test",
  modelProbe: "model/probe",
  pluginSettingsRead: "plugin/settings/read",
  pluginSettingsWrite: "plugin/settings/write",
  uiSettingsRead: "ui/settings/read",
  uiSettingsWrite: "ui/settings/write",
  imageUploadBegin: "image/upload/begin",
  imageUploadChunk: "image/upload/chunk",
  imageUploadCommit: "image/upload/commit",
  imageUploadAbort: "image/upload/abort",
} as const;
