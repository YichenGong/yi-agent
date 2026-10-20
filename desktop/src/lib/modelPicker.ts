/**
 * 会话模型下拉的纯逻辑：把「机器级模型清单 + 会话引用」翻成一组下拉项。
 *
 * 与组件分开，因为「跟随全局默认该显示哪个模型名」这件事值得被单测钉死：
 * 用户看到的必须是**解析后**的模型标识，而不是一个干巴巴的「默认」。
 */
import type { ChannelCapability, ModelEntryView, ModelList, ReasoningCapabilities } from "./models";

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
 *
 * 去重：默认项的模型标识与默认条目的**显示名**是两个字段，可能取到同一个字面量
 * （用户机器上 `name == model == "HORIZON-DeepSeek"` 即此情形）。此时条目行会渲染
 * 出与默认项一模一样的文本，看起来是同一个模型出现两次。这两行指向同一模型，故
 * 合并为一行——丢掉默认条目那一行，只留「跟随全局默认 · X」。只合并**默认条目**：
 * 非默认条目的显示名撞上这个 X 是另一回事（可能是另一个端点/密钥），合并会真丢掉
 * 一个可选项，故不碰。
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

  // 见上方「去重」说明：默认条目的显示名 == 默认项模型标识时才合并。
  const collapsedDefault =
    defaultEntry !== null &&
    resolvedDefaultModel !== null &&
    defaultEntry.name === resolvedDefaultModel
      ? defaultEntry.name
      : null;

  const follow: ModelOption = {
    value: null,
    label: followLabel,
    // 会话原本恰好钉在被合并掉的那条上时，默认项要接过选中态，否则合并后一行都不勾。
    selected: currentRef === null || currentRef === collapsedDefault,
  };

  const entries: ModelOption[] = list.models
    .filter((entry: ModelEntryView) => entry.name !== collapsedDefault)
    .map((entry: ModelEntryView) => ({
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

/** effort 下拉里的一项。`value === null` 即「自动」（清除会话覆盖）。 */
export type EffortOption = {
  /** 选中后写进 `thread/setReasoningEffort` 的档位；`null` = 清除覆盖。 */
  value: string | null;
  label: string;
  selected: boolean;
};

/**
 * 由「会话当前覆盖」与「清单条目默认档」构造 effort 下拉项。
 *
 * 第一项恒为「自动」：它落在 `thread/setReasoningEffort { effort: null }` 上，即
 * 清除会话覆盖、回落条目默认。
 *
 * 为什么叫「自动」而不是「未设置」：这一项**不是**「关闭推理」，也不是「没有值」，
 * 而是「把档位交给模型/条目默认去决定」。旧名「未设置」把三重含义压成一句
 * ——用户会读成「别用 effort」。条目默认非空时补上括号里的实际落点
 * （「自动（low）」），清掉覆盖前就知道会跑哪一档；条目没声明时只写「自动」
 * （请求不带该字段，由模型自行决定）。
 *
 * `current` 是**会话覆盖**：非空时那一档标记 selected；为空时是无覆盖状态，故
 * 「自动」项被标记（哪怕实际生效值来自条目默认——那正是「自动」的语义）。
 */
export function buildEffortOptions(
  current: string | null,
  entryDefault: string | null,
  supported: readonly string[] | null = null,
): EffortOption[] {
  const unset: EffortOption = {
    value: null,
    label: entryDefault !== null ? `自动（${entryDefault}）` : "自动",
    selected: current === null,
  };
  // `null` = 能力未知：平铺七档，与探测功能上线前的行为一致。
  const tiers: readonly string[] = supported ?? REASONING_EFFORTS;
  const options: EffortOption[] = tiers.map((tier) => ({
    value: tier,
    label: tier,
    selected: tier === current,
  }));
  // 当前覆盖不在支持列表里时**仍然列出**：模型能力可能在会话建好之后才被探测
  // 出来，静默把用户的档位从菜单里抹掉会让人以为设置丢了。标上「不支持」，
  // 由用户自己决定换哪一档（服务端也会拒绝写回这一档）。
  if (current !== null && !tiers.includes(current)) {
    options.push({ value: current, label: `${current}（该模型不支持）`, selected: true });
  }
  return [unset, ...options];
}

/**
 * 该条目**生效通道**上的支持档位；`null` = 能力未知（UI 平铺七档）。
 *
 * 通道判定与宿主 `yi_agent_runtime::models::effective_channel` 逐条对齐，否则
 * 会出现「UI 允许一档、运行期却把它剥掉」的错配：
 * - `anthropic` 只看 `anthropic` 槽；
 * - `api_style` 显式为 `chat` / `responses` 时只看对应槽；
 * - `auto` 优先 `responses`（宿主先试 `/responses`，缺失才回退 chat），只有
 *   chat 可信而 responses 不可信时才落到 chat。
 *
 * 任一 `error` 记录都按未知处理：那一轮没有结论，不等于「一个档都不支持」。
 */
export function entrySupportedEfforts(
  list: ModelList,
  ref: string | null,
): string[] | null {
  const name = ref ?? list.default_model;
  if (name === null) return null;
  const entry = list.models.find((m) => m.name === name);
  if (!entry) return null;
  return supportedEffortsOf(entry);
}

/** 同上，但直接作用于一条条目（设置页的草稿用得到）。 */
export function supportedEffortsOf(entry: ModelEntryView): string[] | null {
  const caps = entry.reasoning_capabilities;
  if (!caps) return null;
  const cap = pickChannel(caps, entry);
  if (!cap || cap.error) return null;
  return cap.supported;
}

function trustworthy(cap: ChannelCapability | null | undefined): boolean {
  return cap != null && !cap.error;
}

function pickChannel(
  caps: ReasoningCapabilities,
  entry: ModelEntryView,
): ChannelCapability | null | undefined {
  if (entry.provider === "anthropic") return caps.anthropic;
  if (entry.api_style === "chat") return caps.chat;
  if (entry.api_style === "responses") return caps.responses;
  return trustworthy(caps.chat) && !trustworthy(caps.responses)
    ? caps.chat
    : caps.responses;
}

/**
 * 把一条 `model/capabilitiesChanged` 并回本地清单：就地替换该条目的能力字段。
 *
 * 返回新对象（不就地改传入的 `list`）：React 的 `useState` 按引用判定变化，
 * 原地改会让 `setCatalog(list)` 静默不触发重渲染。
 */
export function applyCapabilities(
  list: ModelList,
  name: string,
  capabilities: ReasoningCapabilities | null,
): ModelList {
  if (!list.models.some((m) => m.name === name)) return list;
  return {
    ...list,
    models: list.models.map((m) =>
      m.name === name ? { ...m, reasoning_capabilities: capabilities } : m,
    ),
  };
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

/**
 * 把「服务端上报的**生效值**」折回「会话覆盖」，供控件回显。
 *
 * `thread/start` / `thread/resume` 的 `reasoning_effort` 是**生效值**：没有覆盖时
 * 它等于条目默认（工厂回落的结果），有覆盖时等于覆盖档。线协议无法把这两种情形
 * 分开——同一个 `"low"` 既可能是「无覆盖、跑条目默认 low」，也可能是「显式覆盖成
 * low」。既然条目的默认档在本客户端的清单里读得到，就用它做判定：生效值与条目默认
 * 相同 ⇒ 记作**无覆盖**（控件显示「自动（low）」），不同 ⇒ 一定是覆盖。
 *
 * 这样冷启动（resume 报 low、条目默认也是 low）不会把 low 打勾成「已选覆盖」，
 * 与用户看到的状态一致；而真正切过档的会话（生效 high ≠ 条目默认 low）仍能回显。
 * 无法区分的那一档（两者恰好相等）保守地按「无覆盖」处理：控件会显示「继承 low」，
 * 与生效值同字面，不会撒谎。
 */
export function overrideFromEffective(
  effective: string | null,
  entryDefault: string | null,
): string | null {
  if (effective === null) return null;
  return effective === entryDefault ? null : effective;
}
