/**
 * 运行参数页的草稿状态机（纯函数，无 React）。
 *
 * 为什么不能继续用「一个 `drafts` 对象 + 读一次就整体覆盖」：设置现在**会被其他
 * 进程改**（另一个窗口、手机、观察器），页面必须在不吞掉用户输入的前提下跟上
 * 远端。三个状态分开维护才能做到：
 *
 * - `baseline`：上一次**读**到的值。判断「远端到底变没变」的唯一依据。
 * - `drafts`：用户改过、且还没被提交确认的键。没有键就是「跟随远端」。
 * - `edits`：每个键被用户编辑过多少次。保存期间可能又改了一次，只靠值比较
 *   分不出「提交的就是现在这个」和「提交后又被改成一样的」。
 *
 * `conflicts` 是派生提示而不是状态机的一部分：它只回答「这个键的草稿可能覆盖了
 * 别人刚保存的值」。
 */
import type { RuntimeField } from "./runtimeSettings";

export interface RuntimeDraftState {
  baseline: Record<string, string>;
  drafts: Record<string, string>;
  edits: Record<string, number>;
  conflicts: string[];
}

export function emptyDraftState(): RuntimeDraftState {
  return { baseline: {}, drafts: {}, edits: {}, conflicts: [] };
}

/** 字段的「在页面上的字符串形态」：`null`（未设置）显示成空串。 */
function textOf(field: RuntimeField): string {
  return field.value === null ? "" : String(field.value);
}

/**
 * 合并一次读到的值。
 *
 * 规则只有三条，且都以「这一版远端值相对 baseline 有没有变」为准：
 *
 * 1. 用户没动过 → 直接跟随远端。
 * 2. 用户动过，且远端已经等于草稿 → 草稿被确认，删掉（不再算改动）。
 * 3. 用户动过，远端真的变了 → 保留草稿并提示冲突。
 *
 * 关键在「真的变了」：远端没变时不能标冲突。观察器每秒刷一次同样的值，若按
 * 「有草稿就冲突」处理，页面会一直挂着一个不存在的警告。
 */
export function mergeRuntimeRead(
  state: RuntimeDraftState,
  fields: RuntimeField[],
): RuntimeDraftState {
  const baseline = { ...state.baseline };
  const drafts = { ...state.drafts };
  const edits = { ...state.edits };
  const conflicts: string[] = [];

  for (const field of fields) {
    const next = textOf(field);
    const previous = state.baseline[field.key];
    const remoteChanged = previous !== undefined && previous !== next;
    const dirty = field.key in state.drafts;

    baseline[field.key] = next;

    if (!dirty) continue;

    // The remote caught up with the draft: nothing left to save, nothing to warn
    // about. Dropping the draft here is also what stops "restore default" from
    // looking dirty after the server confirms it.
    if (state.drafts[field.key] === next) {
      delete drafts[field.key];
      delete edits[field.key];
      continue;
    }

    if (remoteChanged) {
      conflicts.push(field.key);
    }
  }

  // A conflict for a key the user has since reset must not linger.
  for (const key of state.conflicts) {
    if (!(key in drafts)) continue;
    if (!conflicts.includes(key) && !fields.some((field) => field.key === key)) {
      conflicts.push(key);
    }
  }

  return { baseline, drafts, edits, conflicts };
}

/** 记一次用户编辑：值入草稿，代数 +1。 */
export function editRuntimeDraft(
  state: RuntimeDraftState,
  key: string,
  value: string,
): RuntimeDraftState {
  return {
    ...state,
    drafts: { ...state.drafts, [key]: value },
    edits: { ...state.edits, [key]: (state.edits[key] ?? 0) + 1 },
    // 用户重新编辑就说明他看到了这个值；旧的冲突提示可以撤掉。
    conflicts: state.conflicts.filter((conflict) => conflict !== key),
  };
}

/**
 * 保存成功后清掉「本次提交、且提交后没再改过」的键。
 *
 * `editsAtSubmit` 是发起保存那一刻的代数快照。代数变了就说明用户在等待期间又
 * 改了一次——那次编辑必须留下，否则用户刚敲进去的字会凭空消失。
 */
export function acknowledgeRuntimeSave(
  state: RuntimeDraftState,
  submitted: Record<string, string | null>,
  editsAtSubmit: Record<string, number>,
): RuntimeDraftState {
  const drafts = { ...state.drafts };
  const edits = { ...state.edits };

  for (const key of Object.keys(submitted)) {
    if ((state.edits[key] ?? 0) !== (editsAtSubmit[key] ?? 0)) continue;
    // The submitted value is usually what the draft says, but "restore default"
    // submits null while the draft is "". Accept both as "this submission is
    // what the user asked for" and clear it.
    delete drafts[key];
    delete edits[key];
  }

  return { ...state, drafts, edits, conflicts: state.conflicts.filter((key) => key in drafts) };
}

/**
 * 页面实际显示的值：基线叠加草稿。
 *
 * 与 `drafts` 的区别很关键：`drafts` 只记「用户改过的键」，而这些键之外还要显示
 * 远端值。把两者混成一个字段正是旧实现「读一次就 `setDrafts(draftsOf(fields))`」
 * 的问题来源——它分不清「用户写的」和「远端给的」。
 */
export function visibleDrafts(state: RuntimeDraftState): Record<string, string> {
  return { ...state.baseline, ...state.drafts };
}

/** 用户改过、尚未被提交确认的键。 */
export function dirtyKeys(state: RuntimeDraftState): string[] {
  return Object.keys(state.drafts);
}

/**
 * 「要提交的改动」：草稿里与基线不同的键。
 *
 * 空串发 `null`（删键回默认），与既有 `buildPatch` 的约定一致。
 */
export function pendingPatch(state: RuntimeDraftState): Record<string, string | null> {
  const patch: Record<string, string | null> = {};
  for (const [key, draft] of Object.entries(state.drafts)) {
    if (draft === (state.baseline[key] ?? "")) continue;
    patch[key] = draft === "" ? null : draft;
  }
  return patch;
}
