import { useEffect, useRef, useState } from "react";
import {
  isModelNotFound,
  listModels,
  setThreadModel,
  setThreadReasoningEffort,
  type ModelList,
  type ModelRpc,
} from "../lib/models";
import {
  buildEffortOptions,
  buildModelOptions,
  entryEffortOf,
  entrySupportedEfforts,
  overrideFromEffective,
} from "../lib/modelPicker";
import { onModelCapabilitiesChanged } from "../lib/modelCapabilitiesBus";
import { Popover } from "../ui/Popover";

/**
 * 宿主注入的模型 RPC 接缝。
 *
 * 非泛型签名（与 SettingsModelsTab 的 `call` 同形）：宿主传进来的稳定函数与测试里
 * 手写的普通函数都能直接赋值；运行时形状与 `ModelRpc` 一致，组件内一处断言补回泛型。
 */
export type ModelPickerCall = (method: string, params: unknown) => Promise<unknown>;

const EMPTY: ModelList = { models: [], default_model: null, subagent_model: null, effective: null };

/** 把写失败翻成一句给人看的话；「模型不存在」单列，因为它对应的是「被删了」。 */
function saveErrorText(error: unknown): string {
  if (isModelNotFound(error)) return "模型不存在，可能已被删除";
  const message = (error as { message?: unknown } | null)?.message;
  if (typeof message === "string" && message.length > 0) return message;
  return error instanceof Error ? error.message : String(error);
}

/**
 * 会话级模型下拉（输入区右下角）。
 *
 * 只改**当前会话**的覆盖：跟随全局默认即清掉覆盖（`name: null`），与设置页的全局
 * 默认分开——用户在一条会话里试模型，不该顺手改掉其它会话。
 *
 * 写入不做乐观更新：写成功后把**服务端返回**的生效模型名通过 `onChanged` 交回父级，
 * 由父级更新该会话的 `info.model_ref` / `info.model`；写失败则内联报错、保持原状。
 * `threadId` 是本次渲染对应的会话，写入与回调都锁定它，切换会话的瞬间也不会把结果
 * 落到别的会话头上。
 */
export function ModelPicker({
  call,
  threadId,
  currentRef,
  currentModel,
  currentEffort = null,
  onChanged,
  onEffortChanged,
}: {
  call?: ModelPickerCall;
  /** 当前会话 id；`null`（无会话）时置灰，避免对空会话发写。 */
  threadId: string | null;
  /** 会话当前的模型引用（模型名）；`null` = 跟随全局默认。 */
  currentRef: string | null;
  /** 会话当前实际生效的模型标识（宿主解析后）；未知为 `null`。 */
  currentModel: string | null;
  /**
   * 会话当前的 effort：`thread/reasoningEffortChanged` 推送的覆盖，或 thread 回包
   * 落下的**生效值**（`null` = 未上报 / 无覆盖）。
   *
   * 两种来源在「无覆盖」时长得一样（回包的生效值 = 条目默认），所以本组件不直接
   * 把它当作覆盖：先用 {@link overrideFromEffective} 与条目默认比对折回覆盖
   * （无覆盖 ⇒ `null`），再交给 `buildEffortOptions` 决定打勾与「继承 X」标签。
   * 否则冷启动会把条目默认档错误地打勾成「已选覆盖」。
   */
  currentEffort?: string | null;
  /**
   * 写给成功后回调：`threadId` 是本次渲染对应的会话，`ref` 是新的会话引用
   * （`null` = 跟随默认），`model` 是服务端返回的生效模型标识。父级据此更新
   * 该会话的 `info.model_ref` / `info.model`。
   *
   * 会话 id 由回调方（本组件）指名，而不是让父级读它自己的 `currentId`：写入在途
   * 时用户可能已切到别的会话，父级的 `currentId` 已指向新会话，结果就会落到别人
   * 头上。以本组件渲染时的 `threadId` 为准，切换也不会错位。
   */
  onChanged: (threadId: string, ref: string | null, model: string | null) => void;
  /**
   * effort 写成功后回调：`threadId` 是本次渲染对应的会话，`effort` 是服务端返回的
   * **已应用**档位（`null` = 已清除覆盖）。父级据此更新该会话的 effort 值。
   */
  onEffortChanged?: (threadId: string, effort: string | null) => void;
}) {
  // 接缝在运行时就是 ModelRpc 的形状；一次性补回调用方自选返回类型的泛型能力。
  const rpc = call as ModelRpc | undefined;

  const [catalog, setCatalog] = useState<ModelList>(EMPTY);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const [effortOpen, setEffortOpen] = useState(false);
  const [effortBusy, setEffortBusy] = useState(false);
  const [effortError, setEffortError] = useState<string | null>(null);
  const effortTriggerRef = useRef<HTMLButtonElement>(null);

  // 读清单一次（接缝身份变化才重读）：下拉项要与设置页的清单保持一致。
  useEffect(() => {
    if (!rpc) {
      setCatalog(EMPTY);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const next = await listModels(rpc);
        if (!cancelled) setCatalog(next);
      } catch {
        // 读不到清单下拉就只有「跟随全局默认」一项可用，不打断对话。
        if (!cancelled) setCatalog(EMPTY);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [rpc]);

  // 每次**展开**下拉再读一次清单。挂载时那份会过时：用户可能刚在设置页写入一条
  // 模型，而 `rpc` 是稳定身份（宿主用 `useCallback` 只闭合 `clientRef`），上面那个
  // effect 不会因此重跑——不重读的话，新模型要等组件重挂载才出现。读失败就保留现有
  // 清单：打开下拉不该因一次读失败把选项清空。
  useEffect(() => {
    if (!open || !rpc) return;
    let cancelled = false;
    void (async () => {
      try {
        const next = await listModels(rpc);
        if (!cancelled) setCatalog(next);
      } catch {
        // 保留现状。
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [open, rpc]);

  // 宿主探测完成时重读清单：能力是**后台**异步得出的，挂载时那份多半还没有。
  // 不订阅的话，下拉会一直按「未知」平铺七档，即使设置页里刚显示过探测结果。
  useEffect(() => {
    if (!rpc) return;
    let cancelled = false;
    const off = onModelCapabilitiesChanged(() => {
      void (async () => {
        try {
          const next = await listModels(rpc);
          if (!cancelled) setCatalog(next);
        } catch {
          // 读失败就保留现状：能力刷新不该打断对话。
        }
      })();
    });
    return () => {
      cancelled = true;
      off();
    };
  }, [rpc]);

  const options = buildModelOptions(catalog, currentRef, currentModel);
  const current = options.find((o) => o.selected) ?? options[0];
  // 条目默认档由本组件读到的清单现算（与模型下拉同一份 catalog、同一个引用选择
  // 口径）：无覆盖时控件显示「继承 X」，正是请求会带的那一档。
  const entryDefault = entryEffortOf(catalog, currentRef);
  // 回包/推送带来的是**生效值**：无覆盖时它恰好等于条目默认，故先折回「覆盖」，
  // 否则冷启动会把条目默认档错误地打勾。`thread/reasoningEffortChanged` 的推送
  // 本身就是覆盖值，若它也恰好等于条目默认，同样按「无覆盖」显示——两种情形在
  // 控件上等价（「继承 X」的 X 与生效值同字面）。
  const effortOverride = overrideFromEffective(currentEffort, entryDefault);
  // 只列该模型实测支持的档位；能力未知（`null`）时平铺七档。
  const supportedEfforts = entrySupportedEfforts(catalog, currentRef);
  const effortOptions = buildEffortOptions(effortOverride, entryDefault, supportedEfforts);
  const currentEffortOption = effortOptions.find((o) => o.selected) ?? effortOptions[0];

  const choose = async (value: string | null) => {
    setOpen(false);
    triggerRef.current?.focus();
    if (!rpc || !threadId || busy) return;
    setBusy(true);
    setError(null);
    try {
      // 以服务端返回的**生效模型**为准，而不是拿挂载时那份清单在本地解析：清单可能
      // 已被别的客户端改过，本地猜出的名字会和真正生效的模型对不上。同一会话的路径
      // 走这个回包；跨客户端改的走 `thread/modelChanged` 通知。
      const { model } = await setThreadModel(rpc, threadId, value);
      onChanged(threadId, value, model.length > 0 ? model : null);
    } catch (e) {
      setError(saveErrorText(e));
    } finally {
      setBusy(false);
    }
  };

  const chooseEffort = async (value: string | null) => {
    setEffortOpen(false);
    effortTriggerRef.current?.focus();
    if (!rpc || !threadId || effortBusy) return;
    setEffortBusy(true);
    setEffortError(null);
    try {
      // 以服务端回包为准：`effort` 是重建 agent 成功后才落盘、才回传的已应用档位。
      const { effort } = await setThreadReasoningEffort(rpc, threadId, value);
      onEffortChanged?.(threadId, effort);
    } catch (e) {
      setEffortError(saveErrorText(e));
    } finally {
      setEffortBusy(false);
    }
  };

  return (
    <div className="flex items-center gap-1">
    <div className="relative inline-block">
      <button
        ref={triggerRef}
        type="button"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`模型：${current.label}`}
        disabled={!rpc || !threadId}
        onClick={() => setOpen((v) => !v)}
        className="max-w-[16rem] max-md:max-w-[8rem] truncate rounded-full border border-line-strong px-2 py-0.5 text-xs font-medium text-fg-muted hover:bg-raised disabled:opacity-50"
      >
        {current.label}
      </button>

      {/* 下拉的 Escape 关闭由 Popover 负责；onClose 里把焦点交回**那个**下拉的触发
          按钮（不是模型按钮）：在 effort 菜单里按 Escape 把焦点甩到模型按钮会让人
          以为按错了控件。两个下拉同开时两个监听按注册顺序触发（模型在前、档位在后），
          最终落在档位按钮上——与原手写 effect 的 `effortOpen ? … : …` 结论一致。 */}
      <Popover
        open={open}
        onClose={() => {
          setOpen(false);
          triggerRef.current?.focus();
        }}
      >
        <div
          role="menu"
          className="absolute right-0 bottom-full z-20 mb-1 max-h-64 overflow-y-auto rounded-md border border-line-strong bg-raised py-1 shadow-xl"
        >
          {options.map((option) => (
            <button
              key={option.value ?? "__default__"}
              type="button"
              role="menuitemradio"
              aria-checked={option.selected}
              onClick={() => void choose(option.value)}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
            >
              <span className="w-3 text-fg-muted">{option.selected ? "✓" : ""}</span>
              {option.label}
            </button>
          ))}
        </div>
      </Popover>

      {error !== null && (
        <p role="alert" className="absolute right-0 bottom-full mb-1 text-xs text-red-400">
          {error}
        </p>
      )}
    </div>

    {/* 会话级 effort 覆盖（与模型下拉同风格的小控件）。改的是**当前会话**的档位，
        显示生效值：会话覆盖 ?? 条目默认 ?? 未设置。 */}
    <div className="relative inline-block">
      <button
        ref={effortTriggerRef}
        type="button"
        aria-haspopup="menu"
        aria-expanded={effortOpen}
        aria-label={`推理档位：${currentEffortOption.label}`}
        disabled={!rpc || !threadId}
        onClick={() => setEffortOpen((v) => !v)}
        className="max-w-[10rem] max-md:max-w-[8rem] truncate rounded-full border border-line-strong px-2 py-0.5 text-xs font-medium text-fg-muted hover:bg-raised disabled:opacity-50"
      >
        {currentEffortOption.label}
      </button>

      <Popover
        open={effortOpen}
        onClose={() => {
          setEffortOpen(false);
          effortTriggerRef.current?.focus();
        }}
      >
        <div
          role="menu"
          className="absolute right-0 bottom-full z-20 mb-1 max-h-64 overflow-y-auto rounded-md border border-line-strong bg-raised py-1 shadow-xl"
        >
          {effortOptions.map((option) => (
            <button
              key={option.value ?? "__unset__"}
              type="button"
              role="menuitemradio"
              aria-checked={option.selected}
              onClick={() => void chooseEffort(option.value)}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
            >
              <span className="w-3 text-fg-muted">{option.selected ? "✓" : ""}</span>
              {option.label}
            </button>
          ))}
        </div>
      </Popover>

      {effortError !== null && (
        <p role="alert" className="absolute right-0 bottom-full mb-1 text-xs text-red-400">
          {effortError}
        </p>
      )}
    </div>
    </div>
  );
}
