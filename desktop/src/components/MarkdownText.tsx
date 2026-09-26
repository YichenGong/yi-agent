import { memo, type MouseEvent, type ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { openUrl } from "@tauri-apps/plugin-opener";
import "highlight.js/styles/github-dark.css";

/**
 * 外链在系统浏览器打开,避免 webview 被导航走。
 * 非 Tauri 环境(vitest / 浏览器开发)下 `openUrl` 会 reject,吞掉即可。
 */
function ExternalLink({ href, children }: { href?: string; children?: ReactNode }) {
  const onClick = (e: MouseEvent<HTMLAnchorElement>) => {
    if (!href || !/^https?:/i.test(href)) return;
    e.preventDefault();
    void openUrl(href).catch(() => {});
  };
  return (
    <a href={href} onClick={onClick} rel="noreferrer">
      {children}
    </a>
  );
}

/**
 * agentMessage 的 markdown 渲染。按 `text` 记忆:流式时只有正在增长的那条消息
 * 重解析,历史消息命中 memo 不重渲染。
 *
 * 不启用 rehype-raw —— react-markdown 默认丢弃原始 HTML,天然防 XSS。
 */
export const MarkdownText = memo(
  function MarkdownText({ text }: { text: string }) {
    return (
      <div className="prose prose-invert max-w-none prose-pre:bg-neutral-900">
        <ReactMarkdown
          remarkPlugins={[remarkGfm]}
          rehypePlugins={[rehypeHighlight]}
          components={{ a: ExternalLink }}
        >
          {text}
        </ReactMarkdown>
      </div>
    );
  },
  (prev, next) => prev.text === next.text,
);
