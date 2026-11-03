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
  return items.map(stripMedia);
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
