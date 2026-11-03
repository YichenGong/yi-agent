/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";

import { PluginView, hostViewCommands } from "./PluginView";
import type { PluginState } from "../hooks/usePlugins";

afterEach(cleanup);

function state(overrides: Partial<PluginState> = {}): PluginState {
  return {
    kind: "running",
    plugin: "example-counter",
    version: "1.0.0",
    project: "ph1_aaaa",
    activation: 1,
    ...(overrides as object),
  } as PluginState;
}

describe("PluginView", () => {
  it("renders nothing but an introduction before the plugin is installed", () => {
    const openView = vi.fn();
    const { container } = render(
      <PluginView
        plugin="example-counter"
        version={null}
        view="main"
        state={state({ kind: "not-installed", plugin: "example-counter", versions: [] })}
        openView={openView}
        embedded
      />,
    );
    expect(container.querySelector("iframe")).toBeNull();
    expect(screen.getByText(/还没有安装/)).toBeTruthy();
    expect(openView).not.toHaveBeenCalled();
  });

  it("does not open a view for an installed but disabled plugin", () => {
    const openView = vi.fn();
    const { container } = render(
      <PluginView
        plugin="example-counter"
        version="1.0.0"
        view="main"
        state={state({ kind: "installed-disabled", plugin: "example-counter", version: "1.0.0" })}
        openView={openView}
        embedded
      />,
    );
    expect(container.querySelector("iframe")).toBeNull();
    expect(openView).not.toHaveBeenCalled();
  });

  it("opens through the managed handle and marks read-only as not writable", async () => {
    const openView = vi.fn(async () => ({
      instance: "v-instance-1",
      windowLabel: "plugin-1",
      url: "plugin-asset://localhost/v-instance-1/index.html",
    }));
    const { container } = render(
      <PluginView
        plugin="example-counter"
        version="1.0.0"
        view="main"
        state={state({
          kind: "read-only",
          plugin: "example-counter",
          version: "1.0.0",
          project: "ph1_aaaa",
        })}
        openView={openView}
        embedded
      />,
    );

    await waitFor(() => {
      expect(container.querySelector("iframe")?.getAttribute("src")).toBe(
        "plugin-asset://localhost/v-instance-1/index.html",
      );
    });
    expect(openView).toHaveBeenCalledWith({ plugin: "example-counter", version: "1.0.0", view: "main" });
    expect(container.querySelector('[data-view-state="open"]')?.getAttribute("data-writable")).toBe("false");

    // 沙箱只给最小权限。
    const iframe = container.querySelector("iframe")!;
    const sandbox = iframe.getAttribute("sandbox") ?? "";
    expect(sandbox).toContain("allow-scripts");
    expect(sandbox).not.toContain("allow-popups");
    expect(sandbox).not.toContain("allow-top-navigation");
    expect(sandbox).not.toContain("allow-forms");
  });

  it("closes the host instance when the component unmounts", async () => {
    const closeView = vi.fn(async () => {});
    const openView = vi.fn(async () => ({
      instance: "v-instance-2",
      windowLabel: "plugin-2",
      url: "plugin-asset://localhost/v-instance-2/index.html",
    }));
    const { unmount, container } = render(
      <PluginView
        plugin="example-counter"
        version="1.0.0"
        view="main"
        state={state()}
        openView={openView}
        closeView={closeView}
        embedded
      />,
    );
    await waitFor(() => expect(container.querySelector("iframe")).toBeTruthy());
    unmount();
    await waitFor(() => expect(closeView).toHaveBeenCalledWith("v-instance-2"));
  });

  it("shows the failure instead of a blank frame", async () => {
    const openView = vi.fn(async () => {
      throw new Error("integrity check failed");
    });
    render(
      <PluginView
        plugin="example-counter"
        version="1.0.0"
        view="main"
        state={state()}
        openView={openView}
        embedded
      />,
    );
    expect(await screen.findByText(/视图打不开：integrity check failed/)).toBeTruthy();
  });

  it("does not load a body while an update waits for confirmation", () => {
    const openView = vi.fn();
    const { container } = render(
      <PluginView
        plugin="example-counter"
        version="1.0.0"
        view="main"
        state={state({ kind: "update-waiting", plugin: "example-counter", version: "1.0.0", requires: ["Resident"] })}
        openView={openView}
        embedded
      />,
    );
    expect(container.querySelector("iframe")).toBeNull();
    expect(openView).not.toHaveBeenCalled();
  });
});

describe("hostViewCommands", () => {
  it("calls the two managed view commands and nothing else", async () => {
    const commands: string[] = [];
    const invoke = async (command: string) => {
      commands.push(command);
      if (command === "open_plugin_view") {
        return { instance: "i", windowLabel: "w", url: "plugin-asset://localhost/i/index.html" };
      }
      return null;
    };
    const { openView, closeView } = hostViewCommands(invoke);

    const handle = await openView({ plugin: "example-counter", version: "1.0.0", view: "main" });
    expect(handle.instance).toBe("i");
    await closeView("i");
    expect(commands).toEqual(["open_plugin_view", "close_plugin_view"]);
  });
});
