import { memo, type ComponentPropsWithoutRef, type ReactNode } from "react";
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
 * 三个宽度收口点（手机窄屏上 agent 常输出长表格/长命令行）：
 * - `TableBlock`：把 `table` 包进 `overflow-x-auto` 的盒子，让表格**在自己的框内**
 *   横向滚动。原先没有这层，表格（实测 615px，视口仅 390pt）会把 markdown 容器
 *   一路撑宽到整个会话列，`ChatView` 因此可横向拖动、左移后右侧全是空白。
 * - `prose-img:max-w-full`：typography 基准里 `img` 只有上下外边距（无 `max-width`），
 *   宽图会顶破气泡；`overflow-x-hidden` 一旦生效，超出的部分将**永久不可达**。
 * - `break-words`：长 URL、无空格哈希这类不可断词的 inline 内容就地折行。
 *
 * `pre` 不必再加 `overflow-x-auto`：typography 基准已给 `pre { overflow-x: auto }`，
 * 代码块本来就是自己的滚动容器。
 */
const TableBlock = ({
  children,
  node: _node,
  ...props
}: ComponentPropsWithoutRef<"table"> & { node?: unknown }) => (
  // `node` 是 react-markdown 注入的 AST 节点，不能落到 DOM 上（会渲染成
  // `node="[object Object]"`）；解构剔除后其余属性照常透传。
  // 外层只负责收口滚动；`table` 自身的样式（含 prose 的 `width`）不受影响。
  <div className="overflow-x-auto">
    <table {...props}>{children}</table>
  </div>
);

export const MarkdownText = memo(
  function MarkdownText({ text }: { text: string }) {
    return (
      <div className="prose prose-invert max-w-none break-words prose-img:max-w-full prose-pre:bg-panel">
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
