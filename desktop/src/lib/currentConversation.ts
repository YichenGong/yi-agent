import type { SideView } from "./sideConversation";
import type { ActiveConversation } from "../hooks/useSideConversationController";

/**
 * 「当前正在看哪一条会话」，判别后的**唯一**形态（spec §5.1）。
 *
 * 为什么需要一个显式的 `lost` 分支：旁支的一切异步动作都要回到它出发时的那条
 * 会话。目标被关闭、被另一端换了一代之后，指针可能还停在原处。此时如果退回成
 * `main`，界面会被读成「这是父会话」，于是旁支的正文/发送/Stop 全部落到父上
 * ——那正是 spec 反复强调要避免的错配。`lost` 是一个**只能显示原因、不能发送**
 * 的终态。
 */
export type CurrentConversation =
  /** 父会话。`hostKey`/`parentId` 在指针指向主会话时来自旁支控制器。 */
  | { kind: "main"; hostKey: string | null; parentId: string | null }
  /** 旁支，且 `identity` 与只读视图 `view` 已核对一致。 */
  | {
      kind: "side";
      identity: Extract<ActiveConversation, { kind: "side" }>;
      view: SideView;
    }
  /** 旁支目标已失效。 */
  | { kind: "lost"; reason: string };
