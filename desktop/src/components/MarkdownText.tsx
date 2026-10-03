import { memo, type ComponentProps, type ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { openUrl } from "@tauri-apps/plugin-opener";
import { isRemoteClient, openExternalLink } from "../lib/platform";
import "highlight.js/styles/github-dark.css";
import "../lib/highlight-light.css";

/**
 * 外链在系统浏览器打开,避免 webview 被导航走。
 * 桌面走 Tauri opener;远程(iOS)构建没有该插件,退回 webview 的 window.open。
 * 非 Tauri 环境(vitest / 浏览器开发)下会 reject,吞掉即可。
 */
function ExternalLink({ href, children }: { href?: string; children?: ReactNode }) {
  const isHttp = !!href && /^https?:/i.test(href);
  if (!isHttp) {
    // 非 http(s)(相对路径、被 urlTransform 清空的危险协议等)渲染为惰性文本,
    // 避免 webview 被导航走或整页重载。
    return <span>{children}</span>;
  }
  return (
    <a
      href={href}
      onClick={(e) => {
        e.preventDefault();
        void openExternalLink(href, { remote: isRemoteClient(), native: openUrl }).catch(() => {});
      }}
      rel="noreferrer"
    >
      {children}
    </a>
  );
}

/**
 * agentMessage 的 markdown 渲染。按 `text` 记忆:流式时只有正在增长的那条消息
 * 重解析,历史消息命中 memo 不重渲染。
 *
 * 不启用 rehype-raw —— react-markdown 默认丢弃原始 HTML,天然防 XSS。
 *
 * `table` 与 `pre` 都要在**自己的框内**横向滚动，而不是把整列撑宽（见 `TableBlock`
 * 与容器上的 `prose-pre:overflow-x-auto`）：手机端 markdown 表格实测 615px，是
 * 390pt 视口的 1.6 倍，不收口就会把会话列撑破、整页可横向拖动。
 */
const TableBlock = ({ children, ...props }: ComponentProps<"table">) => (
  // 外层只负责收口滚动；`table` 自身的样式（含 prose 的 `min-width`）不受影响。
  <div className="overflow-x-auto">
    <table {...props}>{children}</table>
  </div>
);

export const MarkdownText = memo(
  function MarkdownText({ text }: { text: string }) {
    return (
      <div className="prose prose-invert max-w-none break-words prose-pre:overflow-x-auto prose-pre:bg-panel">
        <ReactMarkdown
          remarkPlugins={[remarkGfm]}
          rehypePlugins={[rehypeHighlight]}
          components={{ a: ExternalLink, table: TableBlock }}
        >
          {text}
        </ReactMarkdown>
      </div>
    );
  },
  (prev, next) => prev.text === next.text,
);

/**
 * 一条 agent 消息气泡（静态包装 + markdown）。整体 memo：
 * `ChatView` 在流式期间每个 delta 都会重渲染，若只有内层 `MarkdownText` 被 memo，
 * 外层包装 div 仍会被重建并参与 diff——定稿的历史消息因此被反复「重新渲染」，
 * 白占主线程、饿死侧栏 spinner 的动画帧。按 `text` 判定即可：文本变了才需要重解析。
 */
export const AgentMessage = memo(
  function AgentMessage({ text }: { text: string }) {
    return (
      <div className="my-1 max-w-[90%] self-start">
        <MarkdownText text={text} />
      </div>
    );
  },
  (prev, next) => prev.text === next.text,
);
