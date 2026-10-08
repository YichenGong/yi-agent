/**
 * 会话模型下拉的纯逻辑：把「机器级模型清单 + 会话引用」翻成一组下拉项。
 *
 * 与组件分开，因为「跟随全局默认该显示哪个模型名」这件事值得被单测钉死：
 * 用户看到的必须是**解析后**的模型标识，而不是一个干巴巴的「默认」。
 */
import type { ModelEntryView, ModelList } from "./models";

/** 下拉里的一项。`value === null` 即「跟随全局默认」（清除会话覆盖）。 */
export type ModelOption = {
  /** 选中后写进 `thread/setModel` 的 name；`null` = 清除覆盖。 */
  value: string | null;
  label: string;
  selected: boolean;
};

/**
 * 由 `model/list` 的结果与会话当前引用构造下拉项。
 *
 * 第一项恒为「跟随全局默认 · <模型标识>」：
 * - 用户明确要求默认项也把模型名显出来，否则选了「默认」却不知道实际跑哪个模型。
 * - 模型标识取**全局默认**解析出的模型：仅当会话当前正跟随默认（`currentRef`
 *   为 `null`）时，才信会话带来的 `currentModel`（那是宿主解析后的权威值）；一旦
 *   会话有覆盖，`currentModel` 属于那个覆盖，不能拿来当默认项的名字——否则覆盖到
 *   B 时「跟随默认」会显示成 B，用户分不清自己选的到底是哪个。此时回落到默认条目
 *   的 `model`。
 *
 * 其后的每一项对应清单里的一条，`currentRef` 指向的那条标记为 selected。
 */
export function buildModelOptions(
  list: ModelList,
  currentRef: string | null,
  currentModel: string | null,
): ModelOption[] {
  const defaultEntry = list.models.find((m) => m.name === list.default_model) ?? null;
  const resolvedDefaultModel =
    (currentRef === null ? currentModel : null) ?? defaultEntry?.model ?? null;
  const followLabel =
    resolvedDefaultModel !== null
      ? `跟随全局默认 · ${resolvedDefaultModel}`
      : "跟随全局默认";

  const follow: ModelOption = {
    value: null,
    label: followLabel,
    selected: currentRef === null,
  };

  const entries: ModelOption[] = list.models.map((entry: ModelEntryView) => ({
    value: entry.name,
    label: entry.name,
    selected: entry.name === currentRef,
  }));

  return [follow, ...entries];
}
