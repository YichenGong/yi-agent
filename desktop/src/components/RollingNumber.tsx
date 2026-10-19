import { useEffect, useMemo, useRef, useState } from "react";
import {
  STRIP_BASE,
  type Direction,
  digitDelayMs,
  digitDurationMs,
  digitMove,
  splitDigits,
  stripY,
} from "../lib/rollingDigits";

/** 数字带：三份 0-9。位置从中段起算，上下各留 9 步余量。 */
const STRIP = "0123456789".repeat(3);

const EASING = "cubic-bezier(0.16, 1, 0.3, 1)";

type DigitState = { pos: number; ms: number; delay: number };

/** 数字 `digit` 在数字带中段的索引。 */
function facePos(digit: number): number {
  return STRIP_BASE + digit;
}

function initialState(value: number): DigitState[] {
  return splitDigits(value).map((digit) => ({ pos: facePos(digit), ms: 0, delay: 0 }));
}

function prefersReducedMotion(): boolean {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

/**
 * 逐位竖向翻滚的数字。
 *
 * 每位是一条数字带，外面套一行高的裁切窗口。位置作为状态保存，迁移交给 CSS
 * `transition`——新数据在动画途中到达时，浏览器会从当前实际位置走向新目标，
 * 因此不会跳回起点重播。
 *
 * `usageKey` 变化（切换会话）时不播动画：那是换成另一份数据，不是同一个数在变。
 */
export function RollingNumber({
  value,
  usageKey,
}: {
  value: number;
  usageKey?: string | null;
}) {
  const prev = useRef({ value, usageKey });
  const [states, setStates] = useState<DigitState[]>(() => initialState(value));
  // `digits` 进依赖数组，必须在值不变时保持同一身份，否则每次渲染都会空转一次 effect。
  const digits = useMemo(() => splitDigits(value), [value]);

  useEffect(() => {
    const previous = prev.current;
    prev.current = { value, usageKey };
    if (previous.value === value && previous.usageKey === usageKey) return;

    const keyChanged = previous.usageKey !== usageKey;
    const direction: Direction = value > previous.value ? 1 : value < previous.value ? -1 : 0;
    const reduced = prefersReducedMotion();
    const previousDigits = splitDigits(previous.value);
    const width = digits.length;

    setStates((old) =>
      digits.map((to, index) => {
        const indexFromRight = width - 1 - index;
        const oldIndex = old.length - 1 - indexFromRight;
        const fromIndex = previousDigits.length - 1 - indexFromRight;
        const from = fromIndex >= 0 ? previousDigits[fromIndex] : 0;
        const current = old[oldIndex];

        if (keyChanged || reduced) {
          return { pos: facePos(to), ms: 0, delay: 0 };
        }
        // 新增的高位：元素刚挂载，直接出现在终值上。
        if (!current) return { pos: facePos(to), ms: 0, delay: 0 };

        const move = digitMove(from, to, direction);
        // 这一位没变：原样保留既有状态。把它改成 `ms: 0` 会让内联的
        // `transition: none` 取消正在进行的滚动，数字被瞬间吸到终点——
        // 而每次用量推送都只有一部分位在变，其余位正处于滚动中途。
        if (move.steps === 0) return current;

        const target = current.pos + move.steps * move.direction;
        // 数字带只有三份：目标越界时直接落在终值上，不冒着露边的风险滚。
        if (target < 0 || target >= STRIP.length) {
          return { pos: facePos(to), ms: 0, delay: 0 };
        }
        return {
          pos: target,
          ms: digitDurationMs(move.steps),
          delay: digitDelayMs(indexFromRight),
        };
      }),
    );
  }, [value, usageKey, digits]);

  /**
   * 过渡结束后把位置归一回中段（同一字符，视觉零变化）。不做这一步，位置会在
   * 长时间运行后漂到数字带边缘，下一次滚动就只能靠「直接跳」兜底了。
   */
  function settle(index: number) {
    setStates((old) =>
      old.map((state, i) => {
        if (i !== index) return state;
        const face = ((state.pos % 10) + 10) % 10;
        const normalized = facePos(face);
        return state.pos === normalized ? state : { pos: normalized, ms: 0, delay: 0 };
      }),
    );
  }

  return (
    <span className="rolling-number" aria-hidden="true">
      {digits.map((digit, index) => {
        const state = states[index] ?? { pos: facePos(digit), ms: 0, delay: 0 };
        return (
          <span key={index} className="rolling-digit">
            <span
              className="rolling-strip"
              onTransitionEnd={() => settle(index)}
              style={{
                transform: stripY(state.pos),
                transition:
                  state.ms > 0 ? `transform ${state.ms}ms ${EASING} ${state.delay}ms` : "none",
              }}
            >
              {STRIP.split("").map((char, charIndex) => (
                <span key={charIndex}>{char}</span>
              ))}
            </span>
          </span>
        );
      })}
    </span>
  );
}
