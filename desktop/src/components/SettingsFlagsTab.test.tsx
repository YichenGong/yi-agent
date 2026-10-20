/** @vitest-environment jsdom */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { SettingsFlagsTab } from "./SettingsFlagsTab";

const rows = [
  { name: "a_flag", summary: "实验 A", enabled: false, expires_on: "2026-12-31", source: "default" },
  { name: "b_flag", summary: "实验 B", enabled: true, expires_on: "2026-11-30", source: "file" },
];

afterEach(cleanup);

describe("SettingsFlagsTab", () => {
  it("lists the flags it read from the host", async () => {
    const call = vi.fn().mockResolvedValue({ flags: rows });
    render(<SettingsFlagsTab call={call} />);
    expect(await screen.findByText("a_flag")).toBeTruthy();
    expect(screen.getByText("实验 B")).toBeTruthy();
  });

  it("shows where each value came from", async () => {
    const call = vi.fn().mockResolvedValue({ flags: rows });
    render(<SettingsFlagsTab call={call} />);
    await screen.findByText("a_flag");
    expect(screen.getByText(/来源 default/)).toBeTruthy();
    expect(screen.getByText(/来源 file/)).toBeTruthy();
  });

  it("writes a toggle and refreshes the row", async () => {
    const call = vi
      .fn()
      .mockResolvedValueOnce({ flags: rows })
      .mockResolvedValueOnce({ name: "a_flag", enabled: true })
      .mockResolvedValueOnce({
        flags: [{ ...rows[0], enabled: true, source: "file" }, rows[1]],
      });
    render(<SettingsFlagsTab call={call} />);
    const toggle = await screen.findByRole("checkbox", { name: /a_flag/i });
    fireEvent.click(toggle);
    await waitFor(() =>
      expect(call).toHaveBeenCalledWith("flags/set", { name: "a_flag", enabled: true }),
    );
    await waitFor(() =>
      expect((screen.getByRole("checkbox", { name: /a_flag/i }) as HTMLInputElement).checked).toBe(
        true,
      ),
    );
  });

  it("says so when there is nothing registered yet", async () => {
    const call = vi.fn().mockResolvedValue({ flags: [] });
    render(<SettingsFlagsTab call={call} />);
    expect(await screen.findByText(/还没有注册任何开关/)).toBeTruthy();
  });

  it("surfaces a failed read instead of showing an empty list", async () => {
    const call = vi.fn().mockRejectedValue(new Error("not connected"));
    render(<SettingsFlagsTab call={call} />);
    expect(await screen.findByText(/not connected/)).toBeTruthy();
  });
});
