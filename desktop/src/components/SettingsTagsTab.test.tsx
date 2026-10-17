/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SettingsTagsTab } from "./SettingsTagsTab";
import {
  DEFAULT_TAG_NAMES,
  TAG_DOT_CLASS,
  TAG_IDS,
  resolveTagNames,
} from "../lib/threadTags";

afterEach(cleanup);

/**
 * 按颜色 id 取那一行的输入框。
 *
 * 刻意**不**用 `getByLabelText("红")`：输入框的 `aria-label` 是**当前显示名**，
 * 一旦该颜色被改过名（如 red → 紧急），按默认名就再也查不到它了。用稳定的
 * `data-tag-name-input` 才与名字解耦。
 */
const inputFor = (id: string) =>
  document.querySelector<HTMLInputElement>(`[data-tag-name-input="${id}"]`)!;

describe("SettingsTagsTab", () => {
  it("renders one row per colour with its current name", () => {
    render(
      <SettingsTagsTab tagNames={resolveTagNames({ red: "紧急" })} onSave={vi.fn()} />,
    );
    expect(document.querySelectorAll("[data-tag-name-input]").length).toBe(6);
    expect(inputFor("red").value).toBe("紧急");
    // 未覆盖的颜色显示默认名。
    expect(inputFor("blue").value).toBe(DEFAULT_TAG_NAMES.blue);
  });

  it("renders each colour's own dot next to its input", () => {
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={vi.fn()} />);
    // 色点是每行里唯一的 aria-hidden 装饰节点，class 必须是对应颜色的底色。
    const dots = document.querySelectorAll<HTMLElement>("li > span[aria-hidden='true']");
    expect(dots.length).toBe(6);
    TAG_IDS.forEach((id, i) => {
      expect(dots[i].className).toContain(TAG_DOT_CLASS[id]);
    });
  });

  it("saves on blur with the edited name", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    const input = inputFor("red");
    fireEvent.change(input, { target: { value: "紧急" } });
    fireEvent.blur(input);
    expect(onSave).toHaveBeenCalledWith("red", "紧急");
  });

  it("saves on Enter", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    const input = inputFor("blue");
    fireEvent.change(input, { target: { value: "参考" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSave).toHaveBeenCalledWith("blue", "参考");
  });

  it("does not save a blank name and shows an inline error", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    const input = inputFor("green");
    fireEvent.change(input, { target: { value: "   " } });
    fireEvent.blur(input);
    expect(onSave).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toBeTruthy();
  });

  it("does not save on the Enter that confirms an IME candidate", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    const input = inputFor("purple");
    fireEvent.change(input, { target: { value: "重要" } });
    fireEvent.compositionStart(input);
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSave).not.toHaveBeenCalled();
    fireEvent.compositionEnd(input);
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSave).toHaveBeenCalledWith("purple", "重要");
  });

  it("does not save on the WKWebView commit Enter (keyCode 229, isComposing already false)", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    const input = inputFor("yellow");
    fireEvent.change(input, { target: { value: "不急" } });
    // macOS WebKit 在拆掉组合之后才重建这个 keydown：isComposing 已是 false，
    // keyCode 被强制成 229。少了这一路判据，中文输入法会吞掉半个词。
    fireEvent.keyDown(input, { key: "Enter", keyCode: 229 });
    expect(onSave).not.toHaveBeenCalled();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSave).toHaveBeenCalledWith("yellow", "不急");
  });

  it("does not resend a key whose name is unchanged", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    const input = inputFor("red");
    // 只动别的行：本行值未改，失焦不该发请求。
    fireEvent.change(input, { target: { value: DEFAULT_TAG_NAMES.red } });
    fireEvent.blur(input);
    expect(onSave).not.toHaveBeenCalled();
  });

  it("submits exactly one key per edit, so params.tag_names can only carry the changed id", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    // 改 red 后失焦：只有一次调用、且只带 red——其余 5 个键绝不会进 payload。
    fireEvent.change(inputFor("red"), { target: { value: "紧急" } });
    fireEvent.blur(inputFor("red"));
    expect(onSave).toHaveBeenCalledTimes(1);
    expect(onSave).toHaveBeenCalledWith("red", "紧急");
  });

  it("rejects a name longer than the max length", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    const input = inputFor("orange");
    fireEvent.change(input, { target: { value: "长".repeat(25) } });
    fireEvent.blur(input);
    expect(onSave).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toBeTruthy();
  });
});
