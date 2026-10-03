import type { SwitchSource } from "../lib/superpowersKanbanSwitch";
import type { BoardCard } from "../lib/superpowersKanbanState";
import { SuperpowersKanbanBoard } from "./SuperpowersKanbanBoard";

// 兼容旧 import 路径：卡片数据形状已迁到 lib，旧代码可从 View 继续拿到类型。
export type { BoardCard };

export function SuperpowersKanbanView({
  switchOn,
  source,
  cards,
  pluginMissing = false,
  expandedDone = false,
  onToggleDone,
  onOpenThread,
}: {
  switchOn: boolean;
  source: SwitchSource;
  cards: BoardCard[];
  /** The plugin never answered, so there is no board to render at all. */
  pluginMissing?: boolean;
  expandedDone?: boolean;
  onToggleDone?: () => void;
  /** 打开某张卡关联的会话；上层接到既有的 thread/resume 入口。 */
  onOpenThread?: (threadId: string) => void;
}) {
  if (pluginMissing) {
    return (
      <div className="p-4 text-sm text-fg-muted">
        Superpowers 看板插件未安装。插件负责回答看板的所有问题，装上它这里才会显示卡片。
      </div>
    );
  }
  if (!switchOn) {
    return (
      <div className="p-4 text-sm text-fg-muted">
        Superpowers 看板 is disabled ({source}). Enable it in settings.
      </div>
    );
  }
  if (cards.length === 0) {
    return <div className="p-4 text-sm text-fg-muted">Superpowers 看板 is empty.</div>;
  }
  return (
    <SuperpowersKanbanBoard
      cards={cards}
      expandedDone={expandedDone}
      onToggleDone={onToggleDone}
      onOpenThread={onOpenThread}
    />
  );
}
