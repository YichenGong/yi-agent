/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { CacheNotice } from "./CacheNotice";

afterEach(cleanup);

describe("CacheNotice", () => {
  it("labels the content as offline and shows when it was cached", () => {
    render(<CacheNotice savedAt={Date.now() - 3 * 60_000} />);
    expect(screen.getByRole("status").textContent).toContain("离线内容");
    expect(screen.getByRole("status").textContent).toContain("3m");
  });
});
