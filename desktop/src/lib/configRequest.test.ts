import { describe, it, expect } from "vitest";
import { configReadParams } from "./configRequest";

describe("configReadParams", () => {
  it("asks for the current thread's effective config", () => {
    expect(configReadParams("thread-123")).toEqual({ threadId: "thread-123" });
  });

  it("omits threadId when there is no session (bare app-server read)", () => {
    expect(configReadParams(null)).toEqual({});
    expect(configReadParams(undefined)).toEqual({});
    expect(configReadParams("")).toEqual({});
  });
});
