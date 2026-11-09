import type { ImageRef } from "../lib/generated/ImageRef";
import type { Item } from "../lib/generated/Item";

/**
 * 把旁支的线上 `Item[]` 转成 `ChatView` 能吃的展示数组。
 *
 * 这不是「把旁支塞进普通会话管线」——恰好相反。旁支的正文是服务端的只读快照，
 * 它**没有**普通会话那套图片/附件能力：旁支不调用 `image/read`、不重传附件、
 * 不触发上传。如果直接把 wire item 交给 `ChatView`，`userMessage.images` 里的
 * 引用会让 `useImageData` 拿旁支的宿主去读一张属于父会话的图（旁支进程里那张图
 * 根本不存在），并且把父的字节流进旁支的渲染。
 *
 * 所以这里只做一件事：**剥掉图片与附件**，留下文本与工具调用。返回值是新的
 * 数组、新的对象，绝不改入参（快照可能同时被另一个视图读着）。
 *
 * `Item` 的 `images`/`attachments` 只在 `userMessage`（以及 `userInterjection`、
 * `toolCall` 的图片）上出现，因此用穷尽 switch 逐类处理，而不是按属性名泛化地
 * 删除——新增一种带图片的 item 时，这里会编译不过。
 */
export function sideItemsForChat(items: readonly Item[]): Item[] {
  return items.map(stripItem);
}

/** 一条 item：既剥掉媒体引用，也把正文里的远程图片地址降级成文本。 */
function stripItem(item: Item): Item {
  const stripped = stripMedia(item);
  switch (stripped.type) {
    case "agentMessage":
    case "userMessage":
    case "userInterjection":
    case "thinking": {
      const text = neutralizeRemoteImages(stripped.text);
      // 没改动时返回**原引用**：下游 memo 靠它跳过整段历史的重渲染。
      if (text === stripped.text) return stripped;
      return { ...stripped, text };
    }
    default:
      return stripped;
  }
}

/**
 * 把 markdown 里的**远程**图片降级成文本（spec §5.1）。
 *
 * 快照里出现的 `![x](https://…)` 只是一个历史来源标记：旁问进程里那张图并不
 * 存在，渲染成 `<img>` 会让 webview（或供应商的隐式下载）去拉一个用户没有点过
 * 的 URL。内嵌的 `data:` 图片没有这个副作用，且 provider 本来就能直接处理，
 * 所以保留。
 *
 * 替换成一句人能读的说明而不是删掉：用户看到「这里原本有张图」比看到一个空洞
 * 更接近事实。**不改入参**——同一份快照可能正被另一个视图读着。
 */
const MARKDOWN_IMAGE = /!\[([^\]]*)\]\(([^()\s]*)(\s+"[^"]*")?\)/g;

export function neutralizeRemoteImages(text: string): string {
  if (!text.includes("![")) return text;
  return text.replace(MARKDOWN_IMAGE, (whole, alt: string, url: string) => {
    if (url.startsWith("data:")) return whole;
    const label = alt.trim() === "" ? "图片" : alt.trim();
    return `[${label}内容未包含在快照中]`;
  });
}

function stripMedia(item: Item): Item {
  switch (item.type) {
    case "userMessage":
      return { ...item, attachments: [], images: [] };
    case "userInterjection":
      return { ...item, attachments: [], images: [] };
    case "toolCall":
      return { ...item, images: [] };
    // 其余类型没有媒体字段；原样返回同一个对象，交给下游的 memo 命中原引用。
    default:
      return item;
  }
}

/** 旁支里是否有任何被剥掉的媒体引用（用于一行提示，而不是静默）。 */
export function sideHasStrippedMedia(items: readonly Item[]): boolean {
  return items.some((item) => {
    switch (item.type) {
      case "userMessage":
      case "userInterjection":
        return (item.attachments?.length ?? 0) > 0 || (item.images?.length ?? 0) > 0;
      case "toolCall":
        return (item.images?.length ?? 0) > 0;
      default:
        return false;
    }
  });
}

/** 供组件层判空用；`ImageRef` 仅在此用于类型标注，不产生运行时依赖。 */
export type SideMediaRef = ImageRef;
