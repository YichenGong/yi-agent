import { describe, expect, it } from "vitest";
import type { Item } from "./generated/Item";
import {
  neutralizeRemoteImages,
  sideHasStrippedMedia,
  sideItemsForChat,
} from "./sideItems";

const image = {
  handle: "img-1",
  media_type: "image/png",
  size: 10,
} as unknown as NonNullable<
  Extract<Item, { type: "userMessage" }>["images"]
>[number];

const attachment = {
  name: "notes.md",
  path: "/tmp/notes.md",
  size: 12,
};

function userMessage(over: Partial<Extract<Item, { type: "userMessage" }>> = {}) {
  return {
    type: "userMessage" as const,
    id: "u1",
    text: "hello",
    ...over,
  };
}

describe("sideItemsForChat", () => {
  it("strips images and attachments from user messages", () => {
    const [out] = sideItemsForChat([
      userMessage({ images: [image], attachments: [attachment] }),
    ]);
    expect(out.type).toBe("userMessage");
    if (out.type !== "userMessage") throw new Error("type narrowed");
    expect(out.images).toEqual([]);
    expect(out.attachments).toEqual([]);
    // 正文不受影响：旁支要显示的是用户问了什么。
    expect(out.text).toBe("hello");
  });

  it("strips images from tool calls", () => {
    const [out] = sideItemsForChat([
      {
        type: "toolCall",
        id: "t1",
        call_id: "c1",
        name: "read_image",
        input: {},
        status: "completed",
        images: [image],
      } satisfies Item,
    ]);
    if (out.type !== "toolCall") throw new Error("type narrowed");
    expect(out.images).toEqual([]);
  });

  it("returns the same object for items without media", () => {
    // 原引用能让下游的 memo 命中；每次都造新对象等于让整段正文每帧重渲染。
    const agent: Item = { type: "agentMessage", id: "a1", text: "answer" };
    const [out] = sideItemsForChat([agent]);
    expect(out).toBe(agent);
  });

  it("does not mutate the snapshot it was given", () => {
    // 同一份快照可能正被另一个视图读着（父的后台订阅仍在跑）。
    const source = userMessage({ images: [image], attachments: [attachment] });
    sideItemsForChat([source]);
    expect(source.images).toHaveLength(1);
    expect(source.attachments).toHaveLength(1);
  });

  it("preserves order and count", () => {
    const items: Item[] = [
      userMessage({ id: "u1" }),
      { type: "agentMessage", id: "a1", text: "x" },
      userMessage({ id: "u2" }),
    ];
    expect(sideItemsForChat(items).map((i) => i.id)).toEqual(["u1", "a1", "u2"]);
  });
});

describe("sideHasStrippedMedia", () => {
  it("is false for plain text", () => {
    expect(sideHasStrippedMedia([{ type: "agentMessage", id: "a", text: "x" }])).toBe(
      false,
    );
  });

  it("is true when something was actually dropped", () => {
    expect(sideHasStrippedMedia([userMessage({ images: [image] })])).toBe(true);
    expect(sideHasStrippedMedia([userMessage({ attachments: [attachment] })])).toBe(
      true,
    );
  });

  it("treats empty arrays as nothing dropped", () => {
    expect(sideHasStrippedMedia([userMessage({ images: [], attachments: [] })])).toBe(
      false,
    );
  });
});

describe("neutralizeRemoteImages", () => {
  it("turns a remote markdown image into a statement of what is missing", () => {
    const out = neutralizeRemoteImages("before ![shot](https://example.com/a.png) after");
    expect(out).toBe("before [shot内容未包含在快照中] after");
    // 不能留下任何可以自动加载的图片语法。
    expect(out).not.toContain("![");
    expect(out).not.toContain("http");
  });

  it("keeps an inline data: image", () => {
    // 内嵌字节没有下载副作用，provider 也能直接处理。
    const text = "![chart](data:image/png;base64,AAAA)";
    expect(neutralizeRemoteImages(text)).toBe(text);
  });

  it("uses a generic label when the alt text is empty", () => {
    expect(neutralizeRemoteImages("![](https://x/y.png)")).toBe(
      "[图片内容未包含在快照中]",
    );
  });

  it("leaves plain text and links alone", () => {
    const text = "see [the docs](https://example.com/doc) and some text";
    expect(neutralizeRemoteImages(text)).toBe(text);
    expect(neutralizeRemoteImages("no images here")).toBe("no images here");
  });

  it("does not mutate the item it was given", () => {
    const item: Item = {
      type: "agentMessage",
      id: "a1",
      text: "![x](https://example.com/x.png)",
    };
    const [out] = sideItemsForChat([item]);
    expect(out.type === "agentMessage" && out.text).toBe(
      "[x内容未包含在快照中]",
    );
    expect(item.text).toContain("https://example.com/x.png");
  });
});
