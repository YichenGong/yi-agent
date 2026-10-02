/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SettingsDialog } from "./SettingsDialog";

afterEach(cleanup);

describe("SettingsDialog", () => {
  it("renders nothing while closed", () => {
    render(
      <SettingsDialog open={false} theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("shows the 通用 tab with both theme choices", () => {
    render(
      <SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(screen.getByRole("dialog")).toBeTruthy();
    expect(screen.getByRole("tab", { name: "通用" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "深色" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: "浅色" }).getAttribute("aria-pressed")).toBe("false");
  });

  it("reports the chosen theme", () => {
    const onThemeChange = vi.fn();
    render(
      <SettingsDialog open theme="dark" onThemeChange={onThemeChange} onClose={() => {}} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "浅色" }));
    expect(onThemeChange).toHaveBeenCalledWith("light");
  });

  it("closes on Escape and on the backdrop", () => {
    const onClose = vi.fn();
    const { container } = render(
      <SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={onClose} />,
    );
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
    const backdrop = container.querySelector("[data-settings-backdrop]");
    expect(backdrop).toBeTruthy();
    fireEvent.click(backdrop!);
    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it("closes from the explicit close button", () => {
    const onClose = vi.fn();
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={onClose} />);
    fireEvent.click(screen.getByRole("button", { name: "关闭设置" }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
