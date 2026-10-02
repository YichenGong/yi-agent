/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, cleanup } from "@testing-library/react";
import { TitleBar } from "./TitleBar";

afterEach(cleanup);

describe("TitleBar", () => {
  it("exposes a Tauri drag region so the window can be moved", () => {
    const { container } = render(<TitleBar />);
    expect(container.querySelector("[data-tauri-drag-region]")).not.toBeNull();
  });

  it("uses the sidebar surface color so the top strip blends in", () => {
    const { container } = render(<TitleBar />);
    const bar = container.querySelector<HTMLElement>("[data-tauri-drag-region]")!;
    expect(bar).not.toBeNull();
    expect(bar.className).toContain("bg-panel");
    expect(bar.className).toContain("border-b");
  });
});
