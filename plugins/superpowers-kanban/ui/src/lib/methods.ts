/**
 * The business methods this plugin's backend declares.
 *
 * These strings must match `plugins/superpowers-kanban/plugin.json`'s `methods[]`
 * entries exactly: the host bridge refuses any method the manifest does not
 * declare, so a typo here fails closed at the boundary rather than reaching a
 * backend that might interpret the name differently.
 *
 * Names are the manifest's, not the backend's internal ones. The manifest schema
 * requires `namespace/verb` with camelCase segments, while the dispatcher's
 * match arms carry the older bare names (`switch.read`, `enqueue_prequeued`)
 * that existing supervisor callers use. `dispatch.rs::bare_method` translates
 * between them; this file is on the manifest side of that boundary.
 *
 * Declared as one frozen table instead of scattered literals so a rename has a
 * single place to change and the boundary test can assert the two lists agree.
 */
export const METHODS = {
  list: "board/list",
  enqueue: "board/enqueue",
  enqueuePrequeued: "board/enqueuePrequeued",
  promote: "board/promote",
  switchRead: "board/readSwitch",
  switchWrite: "board/writeSwitch",
  settingsRead: "board/prefs",
  settingsWrite: "board/storePrefs",
  lifecycleCreate: "board/startLifecycle",
  lifecycleRemove: "board/stopLifecycle",
  /**
   * 按 requestId 查一次写操作的结论。
   *
   * 不是写：它只读账本。断线后页面用它回答"那次入队到底落没落"，
   * 而不是凭一个 `ResultUnknown` 重发——重发正是幂等规则要防的事。
   */
  operation: "board/operation",
} as const;

export type KanbanMethod = (typeof METHODS)[keyof typeof METHODS];

/** Methods that change state. The UI must not retry these on a lost connection. */
export const WRITE_METHODS: readonly KanbanMethod[] = [
  METHODS.enqueue,
  METHODS.enqueuePrequeued,
  METHODS.promote,
  METHODS.switchWrite,
  METHODS.settingsWrite,
  METHODS.lifecycleCreate,
  METHODS.lifecycleRemove,
];
