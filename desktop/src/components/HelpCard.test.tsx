/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { HelpCard } from "./HelpCard";
import { HELP_GROUPS, SLASH_COMMANDS } from "../lib/slash";

afterEach(cleanup);

describe("HelpCard", () => {
  it("titles the catalog card", () => {
    render(<HelpCard />);
    expect(screen.getByTestId("help-card")).toBeTruthy();
    expect(screen.getByText("可用命令")).toBeTruthy();
  });

  it("renders every group heading", () => {
    render(<HelpCard />);
    for (const g of HELP_GROUPS) {
      expect(screen.getByText(g.title)).toBeTruthy();
    }
  });

  it("renders every command with its usage and description", () => {
    render(<HelpCard />);
    for (const c of SLASH_COMMANDS) {
      expect(screen.getByText(`/${c.name}${c.usage ? ` ${c.usage}` : ""}`)).toBeTruthy();
      expect(screen.getByText(c.description)).toBeTruthy();
    }
  });

  it("describes one command when given a target", () => {
    render(<HelpCard target="usage" />);
    expect(screen.getByText("命令 /usage")).toBeTruthy();
    expect(screen.getByText("说明")).toBeTruthy();
    expect(screen.getByText("查看 token 用量与上下文占用")).toBeTruthy();
    // 单条详情不该把整份清单也铺出来。
    expect(screen.queryByText("清空对话上下文")).toBeNull();
  });

  it("shows the usage line for a command that takes an argument", () => {
    render(<HelpCard target="help" />);
    expect(screen.getByText("用法")).toBeTruthy();
    expect(screen.getByText("/help [command]")).toBeTruthy();
  });

  it("shows — instead of 用法 for a command with no argument", () => {
    // `/usage` 不接受参数：标签仍在，值用破折号表示「没有」，而不是整行消失
    // （那样用户会以为卡片漏渲染了）。
    render(<HelpCard target="usage" />);
    expect(screen.getByText("用法")).toBeTruthy();
    expect(screen.getByText("—")).toBeTruthy();
  });

  it("reports an unknown target and still lists everything", () => {
    // 「拼错了」的下一句往往是「那到底有哪些」，所以错误不吞掉清单。
    render(<HelpCard target="nope" />);
    expect(screen.getByText("未知命令: /nope")).toBeTruthy();
    expect(screen.queryByText("命令 /nope")).toBeNull();
    expect(screen.getByText("可用命令")).toBeTruthy();
    expect(screen.getByText("清空对话上下文")).toBeTruthy();
  });

  it("reports an unknown command typed directly, the same way", () => {
    // `/nope` 与 `/help nope` 落到同一张卡上，否则改了 `/help` 之后打错一个
    // 字符又掉回旧的纯文本形态。
    render(<HelpCard error="未知命令: /nope" />);
    expect(screen.getByText("未知命令: /nope")).toBeTruthy();
    expect(screen.getByText("可用命令")).toBeTruthy();
  });

  it("rejects a target that is only a slash", () => {
    render(<HelpCard target="/" />);
    // `/` 归一化后是空名，等同于「列出全部」，不是「未知命令 /」。
    expect(screen.queryByText(/未知命令/)).toBeNull();
    expect(screen.getByText("可用命令")).toBeTruthy();
  });
});
