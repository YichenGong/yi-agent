/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { resolveSidebarPressTarget } from "./sidebarPress";

afterEach(() => {
  document.body.innerHTML = "";
});

/** 造一棵与侧栏同构的迷你 DOM：组头 + 一条会话行（含一个按钮）。 */
function tree(): { header: HTMLElement; row: HTMLElement; pin: HTMLElement; blank: HTMLElement } {
  document.body.innerHTML = `
    <aside>
      <div data-ws-header=""><span id="h-text">projA</span></div>
      <div data-thread-row="t1"><span id="r-text">alpha</span><button id="pin">pin</button></div>
      <div id="blank">footer</div>
    </aside>
  `;
  return {
    header: document.querySelector("[data-ws-header]")!,
    row: document.querySelector("[data-thread-row]")!,
    pin: document.querySelector("#pin")!,
    blank: document.querySelector("#blank")!,
  };
}

describe("resolveSidebarPressTarget", () => {
  it("resolves a press inside a thread row to that row", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#r-text"))).toBe(
      document.querySelector("[data-thread-row]"),
    );
  });

  it("resolves a press inside a workspace header to that header", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#h-text"))).toBe(
      document.querySelector("[data-ws-header]"),
    );
  });

  it("returns null for a press on an interactive child (pin / delete button)", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#pin"))).toBeNull();
  });

  it("returns null for a press outside any row or header", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#blank"))).toBeNull();
  });

  it("returns null for a null target", () => {
    expect(resolveSidebarPressTarget(null)).toBeNull();
  });
});
