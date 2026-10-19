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
  pluginsList: "plugins/list",
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
  pluginSettingsRead: "plugin/settings/read",
  pluginSettingsWrite: "plugin/settings/write",
  uiSettingsRead: "ui/settings/read",
  uiSettingsWrite: "ui/settings/write",
  imageUploadBegin: "image/upload/begin",
  imageUploadChunk: "image/upload/chunk",
  imageUploadCommit: "image/upload/commit",
  imageUploadAbort: "image/upload/abort",
} as const;
