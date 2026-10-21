import type { SwitchSource } from "../lib/superpowersKanbanSwitch";

/**
 * 看板页顶栏的一行只读状态。
 *
 * 这里**故意没有开关**：一个看板存在，意味着建它的时候插件已经被装好、开关
 * 已经被打开（`createBoard` 的两步）。再给一个「开/关」只会问用户一件已经
 * 决定过的事。要停掉这个看板，正确的动作是右键「移除看板」——那是真的卸载。
 *
 * 状态本身必须留下：`source` 是用户判断「这个看板为什么不动」的唯一线索
 * （项目层关着 / 全局关着 / 从来没人开过）。
 */
export function SuperpowersKanbanSettings({
  switchOn,
  source,
}: {
  switchOn: boolean;
  source: SwitchSource;
}) {
  return (
    <section className="px-4 pb-2 pt-3">
      <p className="flex items-center gap-2 text-xs">
        <span className="text-fg">Superpowers 看板</span>
        <span
          aria-hidden="true"
          className={`size-1.5 rounded-full ${switchOn ? "bg-emerald-500" : "bg-fg-subtle"}`}
        />
        <span className="text-fg-muted">{switchOn ? "运行中" : "已停用"}</span>
        <span className="text-fg-subtle">({source})</span>
      </p>
      {!switchOn && (
        <p className="mt-1 text-xs text-fg-subtle">
          移除后重新创建即可恢复；已排队的卡片不会被推进。
        </p>
      )}
    </section>
  );
}
