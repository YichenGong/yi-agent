/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, cleanup } from "@testing-library/react";
import { ApprovalDialog } from "./ApprovalDialog";
import type { ApprovalRequest } from "../lib/protocol";

afterEach(cleanup);

const request = (prefix: string | null): ApprovalRequest => ({
  id: "a1",
  params: {
    thread_id: "t1",
    turn_id: "u1",
    request_id: 1,
    tool_name: "bash",
    tool_input: { command: "git push origin main" },
    prefix_suggestion: prefix,
    kind: "Normal",
  },
});

/**
 * 手机竖屏（约 390pt）与横屏（约 390pt 高）下，对话框曾经整体高于可视区：
 * 卡片没有高度上限，`absolute inset-0` 又是拉伸高度，「center」因此把卡片推成
 * 上下都越出 `.app-main`（底排按钮落在对话框外面）。四条断言对应四处结构性
 * 保证；jsdom 无布局引擎量不出几何，故这里钉住的是真正打包进 CSS 的类名。
 */
describe("ApprovalDialog layout containment", () => {
  it("pads and scrolls the overlay so a tall card is not clipped at the edges", () => {
    const { container } = render(
      <ApprovalDialog request={request(null)} onDecide={() => {}} />,
    );
    const overlay = container.firstElementChild as HTMLElement;
    const classes = overlay.className.split(/\s+/);
    // 无内边距时卡片贴着 `.app-main` 的四边，圆角与边框被裁掉。
    expect(classes).toContain("p-4");
    // 卡片仍高于视口时（极小窗口），兜底让用户能滚动到按钮。
    expect(classes).toContain("overflow-y-auto");
  });

  it("caps the card height so it is pushed inside before centering", () => {
    const { container } = render(
      <ApprovalDialog request={request(null)} onDecide={() => {}} />,
    );
    const card = container.querySelector("[role=dialog]") as HTMLElement;
    // `max-h-full` 解析为行盒高（去掉安全区后的视口）；没有它，flex center 会
    // 让卡片上下等量越界，`pre` 也永远吃不满 256px 后不缩。
    expect(card.className.split(/\s+/)).toContain("max-h-full");
  });

  it("wraps the action row so four buttons do not overflow the card width", () => {
    const { container } = render(
      <ApprovalDialog request={request("git push")} onDecide={() => {}} />,
    );
    const card = container.querySelector("[role=dialog]") as HTMLElement;
    const row = card.querySelector("div.flex.justify-end") as HTMLElement;
    // 无换行时 flex 把四个按钮压到下限以下，文字折行且整排横向溢出。
    expect(row.className.split(/\s+/)).toContain("flex-wrap");
  });

  it("stacks the actions full-width on the phone", () => {
    document.documentElement.dataset.mobile = "true";
    try {
      const { container } = render(
        <ApprovalDialog request={request("git push")} onDecide={() => {}} />,
      );
      const card = container.querySelector("[role=dialog]") as HTMLElement;
      const row = card.querySelector("div.flex.justify-end") as HTMLElement;
      // 手机上每行一个、占满卡片宽，才够 44px 触控目标。
      expect(row.className.split(/\s+/)).toContain("max-md:flex-col");
      const buttons = [...card.querySelectorAll("button")];
      expect(buttons.length).toBe(4);
      for (const b of buttons) {
        expect(b.className.split(/\s+/)).toContain("max-md:w-full");
      }
    } finally {
      delete document.documentElement.dataset.mobile;
    }
  });

  it("shrinks the payload, not the actions, so the buttons stay inside the card", () => {
    const { container } = render(
      <ApprovalDialog request={request("git push")} onDecide={() => {}} />,
    );
    const card = container.querySelector("[role=dialog]") as HTMLElement;
    const row = card.querySelector("div.flex.justify-end") as HTMLElement;
    const pre = card.querySelector("pre") as HTMLElement;
    const heading = card.querySelector("h2") as HTMLElement;
    const cardClasses = card.className.split(/\s+/);
    // 卡片是纵向 flex：受 `max-h-full` 夹紧时，可收缩的 `pre` 先让位（但有
    // `min-h-24`＝96px 的可读下限，命令不会缩成一条线），标题与按钮行保持自身
    // 高度，底排因此落在卡片内；再装不下则由卡片自身的滚动兜底。
    expect(cardClasses).toContain("flex-col");
    expect(pre.className.split(/\s+/)).toContain("min-h-24");
    expect(row.className.split(/\s+/)).toContain("shrink-0");
    expect(heading.className.split(/\s+/)).toContain("shrink-0");
  });

  it("wraps a long prefix inside its button instead of widening the card", () => {
    const long = "/Users/x/projects/yi-agent/desktop/scripts/build-sidecar.sh";
    const { container } = render(
      <ApprovalDialog request={request(long)} onDecide={() => {}} />,
    );
    const card = container.querySelector("[role=dialog]") as HTMLElement;
    const prefixBtn = [...card.querySelectorAll("button")].find((b) =>
      b.textContent?.includes("Always allow /Users"),
    ) as HTMLElement;
    // 前缀是单个长 token（绝对路径），没有断行点——不 `break-all` 就会把卡片撑宽、
    // 按钮越出卡片右缘。
    expect(prefixBtn.className.split(/\s+/)).toContain("break-all");
  });

  it("wraps the long prefix in the read-only Prefix row too", () => {
    const long = "/Users/x/projects/yi-agent/desktop/scripts/build-sidecar.sh";
    const { container } = render(
      <ApprovalDialog request={request(long)} onDecide={() => {}} />,
    );
    const card = container.querySelector("[role=dialog]") as HTMLElement;
    const prefixRow = [...card.querySelectorAll("div")].find((d) =>
      d.textContent?.startsWith("Prefix:"),
    ) as HTMLElement;
    // 同一段长路径在正文里也会把卡片撑宽（实测 540 > 510），故这一行同样要断行。
    expect(prefixRow.className.split(/\s+/)).toContain("break-all");
  });

  it("scrolls inside the card when even a top-pinned card cannot fit", () => {
    const long = "/Users/x/projects/yi-agent/desktop/scripts/build-sidecar.sh --release";
    const { container } = render(
      <ApprovalDialog request={request(long)} onDecide={() => {}} />,
    );
    const card = container.querySelector("[role=dialog]") as HTMLElement;
    // 极端场景：iPhone SE 横屏（320pt 高）+ 长前缀，四按钮堆叠 + 换行前缀的**最小**
    // 高度已超过视口。此时把卡片钉在顶部还不够——卡片必须自己能滚，按钮才够得着。
    expect(card.className.split(/\s+/)).toContain("overflow-y-auto");
  });

  it("pins the card to the top when it cannot fit a short viewport", () => {
    const { container } = render(
      <ApprovalDialog request={request("git push")} onDecide={() => {}} />,
    );
    const overlay = container.firstElementChild as HTMLElement;
    const card = container.querySelector("[role=dialog]") as HTMLElement;
    // `items-center` 在溢出时把卡片上下等量推出（iPhone SE 横屏实测底排按钮落在
    // 画面外）。`items-start` + 卡片 `my-auto`：有富余时仍居中，装不下时钉在顶部，
    // 其余交给遮罩自己的 `overflow-y-auto`。
    expect(overlay.className.split(/\s+/)).toContain("items-start");
    expect(card.className.split(/\s+/)).toContain("my-auto");
  });
});
