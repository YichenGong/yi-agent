/**
 * 看板卡片的「会话加载态」展示投影。
 *
 * 这里只回答**怎么显示**，不回答「会话还在不在」——那是插件 `list` 里
 * `session_observation` 已经给出的事实。三者含义刻意分开：
 *
 * - `loaded`：当前宿主热表里有这个会话（无论闲不闲）。不显示任何角标；
 * - `not_loaded`：当前宿主**明确**报告热表里没有它。历史可能仍在磁盘上；
 * - `unknown`：没有有效证据（旧格式观测、查询失败、这一拍没问）。
 *
 * 旧插件只给 `stale` 布尔（含义是「写着 running 但会话没了」），这里把它降级成
 * `not_loaded` 的来源之一，但文案**不**再声称「会话已不在」。
 */

import type { BoardCard } from "./superpowersKanbanState";

export type ObservationTone = "warning" | "neutral";

export interface ObservationNotice {
  /** 角标文案（短，可放在卡片标题旁）。 */
  label: string;
  /** 悬停说明：解释「这意味着什么」以及「这不意味着什么」。 */
  title: string;
  tone: ObservationTone;
}

/** RFC3339 里我们必须认得的部分：日期、时间、必须带时区。 */
const RFC3339 =
  /^(\d{4})-(\d{2})-(\d{2})[Tt](\d{2}):(\d{2})(?::(\d{2}))?(?:\.\d+)?(?:([Zz])|([+-])(\d{2}):(\d{2}))$/;

/** 该日期在给定年月里真实存在（拒绝 `2026-02-30` 这种「语法合法、日期不存在」）。 */
function isRealDate(year: number, month: number, day: number): boolean {
  if (month < 1 || month > 12 || day < 1) return false;
  const daysInMonth = new Date(Date.UTC(year, month, 0)).getUTCDate();
  return day <= daysInMonth;
}

/**
 * 把插件给的 `observed_at` 显示成「本地时间」。非法值返回 `null`——**不**回退到
 * 「现在」或 `Invalid Date`：那会把一次坏数据渲染成一次新鲜观测。
 */
export function observationDate(value: string | undefined): string | null {
  if (!value) return null;
  const match = RFC3339.exec(value);
  if (!match) return null;
  const [, year, month, day, hour, minute, , zone, sign, offsetHour, offsetMinute] = match;
  const y = Number(year);
  const mo = Number(month);
  const d = Number(day);
  const h = Number(hour);
  const mi = Number(minute);
  if (!isRealDate(y, mo, d)) return null;
  if (h > 23 || mi > 59) return null;
  if (zone !== "Z" && zone !== "z") {
    if (Number(offsetHour) > 23 || Number(offsetMinute) > 59) return null;
  }

  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return null;
  // 年份必须原样保留：`Date` 会把 0–99 年当成 19xx，这与输入不符就是坏数据。
  if (parsed.getUTCFullYear() !== y) return null;
  void sign;

  const formatter = new Intl.DateTimeFormat("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
  // 显式指定本机时区：默认时区可能被宿主环境改掉，显示就会漂移。
  formatter.resolvedOptions().timeZone = formatter.resolvedOptions().timeZone ?? undefined;
  return formatter.format(parsed);
}

/**
 * 这张卡该显示什么加载态角标。`null` 表示不显示（已加载，或压根没有执行会话）。
 *
 * 优先级：`sessionObservation`（新数据）> 旧 `stale` 布尔（旧数据）。
 * **不能**把「缺键」归一成 `unknown` 再参与判断——那样旧插件写的 `stale: true`
 * 就永远轮不到，用户会看到「未观测到」而不是「未加载」。
 */
export function observationNotice(card: BoardCard): ObservationNotice | null {
  const observation = card.sessionObservation;
  let state: "loaded" | "not_loaded" | "unknown" | undefined = observation;
  if (state === undefined) {
    // 旧插件：只有 `stale` 布尔。它当时的含义确实是「宿主报会话没了」。
    if (card.stale === true) state = "not_loaded";
    else return null;
  }

  if (state === "loaded") return null;

  if (state === "not_loaded") {
    const at = observationDate(card.observedAt);
    return {
      label: "会话未加载",
      title: at
        ? `当前宿主未加载这个会话（截至 ${at}）。历史可能仍在磁盘上，这不是「会话已删除」。`
        : "当前宿主未加载这个会话。历史可能仍在磁盘上，这不是「会话已删除」。",
      tone: "warning",
    };
  }

  // unknown：旧数据 / 查询失败 / 这一拍没问。必须与 not_loaded 分开说。
  return {
    label: card.sessionObservation === "unknown" ? "会话观测未知" : "会话未观测到",
    title:
      "没有有效的加载观测（可能是旧版本插件、查询失败，或这一拍没问）。" +
      "这不表示会话已经不存在。",
    tone: "neutral",
  };
}
