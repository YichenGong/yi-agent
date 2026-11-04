import { describe, expect, it } from "vitest";
import { RPC } from "./routes";

/**
 * 线格式的护栏：这些字面量是 JSON-RPC 方法名，宿主按字符串分发。把调用点改成
 * `RPC.xxx` 只是集中命名，值绝不能变——本表逐条钉住原始串与去重性。
 */
describe("RPC route table", () => {
  it("keeps every method name at its frozen wire value", () => {
    expect(RPC.pluginQuery).toBe("plugin/query");
    expect(RPC.pluginInstall).toBe("plugin/install");
    expect(RPC.pluginUninstall).toBe("plugin/uninstall");
    expect(RPC.pluginsList).toBe("plugins/list");
    // 受管协议。这些名字在 `plugins/contracts/bridge-v1.schema.json` 的
    // `enum` 里逐条列出，任何一侧改动都会让 contract checker 与这里同时红。
    expect(RPC.pluginsV1Catalog).toBe("plugins/v1/catalog");
    expect(RPC.pluginsV1List).toBe("plugins/v1/list");
    expect(RPC.pluginsV1Install).toBe("plugins/v1/install");
    expect(RPC.pluginsV1EnableProject).toBe("plugins/v1/enableProject");
    expect(RPC.pluginsV1ChangeGrant).toBe("plugins/v1/changeGrant");
    expect(RPC.pluginsV1DisableProject).toBe("plugins/v1/disableProject");
    expect(RPC.pluginsV1Activate).toBe("plugins/v1/activate");
    expect(RPC.pluginsV1Rollback).toBe("plugins/v1/rollback");
    expect(RPC.pluginsV1Transaction).toBe("plugins/v1/transaction");
    expect(RPC.pluginsV1CancelTransaction).toBe("plugins/v1/cancelTransaction");
    expect(RPC.pluginsV1Uninstall).toBe("plugins/v1/uninstall");
    expect(RPC.pluginsV1ClearData).toBe("plugins/v1/clearData");
    expect(RPC.pluginsV1Gc).toBe("plugins/v1/gc");
    expect(RPC.pluginsV1InspectLegacy).toBe("plugins/v1/inspectLegacy");
    expect(RPC.pluginsV1AdoptLegacy).toBe("plugins/v1/adoptLegacy");
    expect(RPC.pluginsV1AssetRelease).toBe("plugins/v1/assetRelease");
    expect(RPC.pluginsV1OperationResult).toBe("plugins/v1/operationResult");
    expect(RPC.residentList).toBe("resident/list");
    expect(RPC.onboardingStatus).toBe("onboarding/status");
    expect(RPC.onboardingApply).toBe("onboarding/apply");
    expect(RPC.onboardingTest).toBe("onboarding/test");
    expect(RPC.onboardingDismiss).toBe("onboarding/dismiss");
    expect(RPC.modelList).toBe("model/list");
    expect(RPC.modelImportEnv).toBe("model/importEnv");
    expect(RPC.modelUpsert).toBe("model/upsert");
    expect(RPC.modelDelete).toBe("model/delete");
    expect(RPC.modelSetDefault).toBe("model/setDefault");
    expect(RPC.modelSetSubagent).toBe("model/setSubagent");
    expect(RPC.threadSetModel).toBe("thread/setModel");
    expect(RPC.threadSetReasoningEffort).toBe("thread/setReasoningEffort");
    expect(RPC.modelTest).toBe("model/test");
    expect(RPC.pluginSettingsRead).toBe("plugin/settings/read");
    expect(RPC.pluginSettingsWrite).toBe("plugin/settings/write");
    expect(RPC.uiSettingsRead).toBe("ui/settings/read");
    expect(RPC.uiSettingsWrite).toBe("ui/settings/write");
    expect(RPC.imageUploadBegin).toBe("image/upload/begin");
    expect(RPC.imageUploadChunk).toBe("image/upload/chunk");
    expect(RPC.imageUploadCommit).toBe("image/upload/commit");
    expect(RPC.imageUploadAbort).toBe("image/upload/abort");
  });

  it("has no two constants sharing a wire value", () => {
    const values = Object.values(RPC);
    expect(new Set(values).size).toBe(values.length);
  });
});
