import { useEffect, useRef, useState } from "react";
import {
  isModelNotFound,
  listModels,
  setThreadModel,
  type ModelList,
  type ModelRpc,
} from "../lib/models";
import { buildModelOptions } from "../lib/modelPicker";

/**
 * 宿主注入的模型 RPC 接缝。
 *
 * 非泛型签名（与 SettingsModelsTab 的 `call` 同形）：宿主传进来的稳定函数与测试里
 * 手写的普通函数都能直接赋值；运行时形状与 `ModelRpc` 一致，组件内一处断言补回泛型。
 */
export type ModelPickerCall = (method: string, params: unknown) => Promise<unknown>;

const EMPTY: ModelList = { models: [], default_model: null, subagent_model: null };

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
 * 写入不做乐观更新：写成功后把宿主解析出的模型名通过 `onChanged` 交回父级，由父级
 * 更新该会话的 `info.model_ref` / `info.model`；写失败则内联报错、保持原状。
 */
export function ModelPicker({
  call,
  threadId,
  currentRef,
  currentModel,
  onChanged,
}: {
  call?: ModelPickerCall;
  /** 当前会话 id；`null`（无会话）时置灰，避免对空会话发写。 */
  threadId: string | null;
  /** 会话当前的模型引用（模型名）；`null` = 跟随全局默认。 */
  currentRef: string | null;
  /** 会话当前实际生效的模型标识（宿主解析后）；未知为 `null`。 */
  currentModel: string | null;
  /**
   * 写给成功后回调：`ref` 是新的会话引用（`null` = 跟随默认），`model` 是该会话
   * 生效的模型标识。父级据此更新 `info.model_ref` / `info.model`。
   */
  onChanged: (ref: string | null, model: string | null) => void;
}) {
  // 接缝在运行时就是 ModelRpc 的形状；一次性补回调用方自选返回类型的泛型能力。
  const rpc = call as ModelRpc | undefined;

  const [catalog, setCatalog] = useState<ModelList>(EMPTY);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);

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

  // Escape 关闭下拉并把焦点交回触发按钮。
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        setOpen(false);
        triggerRef.current?.focus();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open]);

  const options = buildModelOptions(catalog, currentRef, currentModel);
  const current = options.find((o) => o.selected) ?? options[0];

  const choose = async (value: string | null) => {
    setOpen(false);
    triggerRef.current?.focus();
    if (!rpc || !threadId || busy) return;
    setBusy(true);
    setError(null);
    try {
      await setThreadModel(rpc, threadId, value);
      // 成功后由父级记录该会话的引用与生效模型名（不在这里猜解析结果）。
      const model =
        value === null ? modelFor(catalog, catalog.default_model) : modelFor(catalog, value);
      onChanged(value, model);
    } catch (e) {
      setError(saveErrorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="relative inline-block">
      <button
        ref={triggerRef}
        type="button"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`模型：${current.label}`}
        disabled={!rpc || !threadId}
        onClick={() => setOpen((v) => !v)}
        className="max-w-[16rem] truncate rounded-full border border-line-strong px-2 py-0.5 text-xs font-medium text-fg-muted hover:bg-raised disabled:opacity-50"
      >
        {current.label}
      </button>

      {open && (
        <>
          <div className="fixed inset-0 z-10" onClick={() => setOpen(false)} />
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
        </>
      )}

      {error !== null && (
        <p role="alert" className="absolute right-0 bottom-full mb-1 text-xs text-red-400">
          {error}
        </p>
      )}
    </div>
  );
}

/** 某清单条目对应的模型标识；找不到则 `null`。 */
function modelFor(catalog: ModelList, name: string | null): string | null {
  if (name === null) return null;
  return catalog.models.find((m) => m.name === name)?.model ?? null;
}
