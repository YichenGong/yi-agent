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

/**
 * 推理档位（七个白名单值），顺序即 UI 顺序。
 *
 * 与 app-server `thread/setReasoningEffort` 的白名单（`server.rs:3981`）以及 TUI 的
 * `REASONING_EFFORT_TIERS` 一致——三端各持一份字面量，改档位要三处同改。
 */
export const REASONING_EFFORTS = [
  "none",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
] as const;

/** effort 下拉里的一项。`value === null` 即「未设置」（清除会话覆盖）。 */
export type EffortOption = {
  /** 选中后写进 `thread/setReasoningEffort` 的档位；`null` = 清除覆盖。 */
  value: string | null;
  label: string;
  selected: boolean;
};

/**
 * 由「会话当前覆盖」与「清单条目默认档」构造 effort 下拉项。
 *
 * 第一项恒为「未设置」：它落在 `thread/setReasoningEffort { effort: null }` 上，即
 * 清除会话覆盖、回落条目默认。条目默认非空时把继承值写进标签
 * （「未设置（继承 low）」），用户才知道清掉覆盖后会跑哪一档；条目没声明时只写
 * 「未设置」（请求不带该字段，由模型自行决定）。
 *
 * `current` 是**会话覆盖**：非空时那一档标记 selected；为空时是无覆盖状态，故
 * 「未设置」项被标记（哪怕实际生效值来自条目默认——那正是「未设置」的语义）。
 */
export function buildEffortOptions(current: string | null, entryDefault: string | null): EffortOption[] {
  const unset: EffortOption = {
    value: null,
    label: entryDefault !== null ? `未设置（继承 ${entryDefault}）` : "未设置",
    selected: current === null,
  };
  const tiers: EffortOption[] = REASONING_EFFORTS.map((tier) => ({
    value: tier,
    label: tier,
    selected: tier === current,
  }));
  return [unset, ...tiers];
}

/**
 * 当前生效的**条目默认** effort：会话引用的清单条目（`ref` 为空 = 跟随
 * `default_model`）自己声明的档位；都没有则 `null`。
 *
 * 与 app-server / TUI 的条目选择口径一致（`current_model_ref` → `default_model`），
 * 所以控件显示的「继承 X」正是请求实际会带的那一档。纯函数：给控件做回显用，
 * 不替服务端解析渠道。
 */
export function entryEffortOf(list: ModelList, ref: string | null): string | null {
  const name = ref ?? list.default_model;
  if (name === null) return null;
  return list.models.find((m) => m.name === name)?.reasoning_effort ?? null;
}
