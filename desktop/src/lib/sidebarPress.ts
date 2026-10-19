/**
 * 侧栏「长按落点」判定：把一次 pointerdown 的目标解析成它所属的可长按元素。
 *
 * 抽成纯函数（只读 DOM 属性、不碰 React）是为了能脱组件单测——它锁住的契约是
 * 「按在会话行上 → 该行；按在组头（文件夹）上 → 该组头；按在行内交互控件或空白处
 * → 什么都不做」。这条契约错了会表现成两种相反的故障：漏判 = 长按没反应，
 * 多判 = 想点图钉却弹出菜单。
 */

/** 交互控件：按在这些上面必须走原生行为，不能当长按。 */
export const PRESS_IGNORE_SELECTOR = "button, input, a, textarea, select";

/** 会话行 / 工作区分组头的标记属性（由 ThreadSidebar 渲染时写上）。 */
export const THREAD_ROW_ATTR = "data-thread-row";
/** 组长行的标记属性：组长是项目级固定入口，不参与会话长按菜单。 */
export const LEAD_ROW_ATTR = "data-lead-row";
export const WORKSPACE_HEADER_ATTR = "data-ws-header";
/** 附属根小节头（关联进来的目录）。 */
export const LINKED_CHILD_ATTR = "data-linked-child";

/** 长按落点：会话行或工作区分组头（文件夹）。 */
export function resolveSidebarPressTarget(target: Element | null): HTMLElement | null {
  if (target === null) return null;
  if (target.closest(PRESS_IGNORE_SELECTOR)) return null;
  // 组长行先排除：它是项目级固定入口，重命名/置顶/删除对它都没有意义。
  // 必须排在 `data-thread-row` 判定**之前**——组长行两者都带。
  if (target.closest(`[${LEAD_ROW_ATTR}]`)) return null;
  const row = target.closest<HTMLElement>(`[${THREAD_ROW_ATTR}]`);
  if (row) return row;
  const header = target.closest<HTMLElement>(`[${WORKSPACE_HEADER_ATTR}]`);
  if (header) return header;
  // 附属根小节头排在组头之后判定：两者互斥（同一元素只会带其中一个属性）。
  return target.closest<HTMLElement>(`[${LINKED_CHILD_ATTR}]`);
}
