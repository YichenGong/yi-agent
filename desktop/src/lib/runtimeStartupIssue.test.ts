import { describe, expect, it } from "vitest";
import { acceptsStartupReply } from "./runtimeStartupIssue";

describe("acceptsStartupReply", () => {
  it("rejects a reply to an older request", () => {
    expect(acceptsStartupReply(2, 1)).toBe(false);
  });

  it("accepts the reply to the request on the wire", () => {
    expect(acceptsStartupReply(2, 2)).toBe(true);
  });

  it("accepts the first reply", () => {
    expect(acceptsStartupReply(1, 1)).toBe(true);
  });
});
