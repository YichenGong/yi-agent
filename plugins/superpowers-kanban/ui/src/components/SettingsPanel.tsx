import { useEffect, useRef, useState } from "react";
import type { PluginHostApi } from "@yi-agent/plugin-sdk";
import { readSettings, writeSettings, type BoardSettings, type KanbanWindow } from "../lib/board";
import { WEEKDAYS, numericInput, parseDays, renderDays } from "../lib/days";
import { errorKind, formatError, isResultUnknown, UNKNOWN_WRITE_TEXT } from "../lib/errors";

const EMPTY_WINDOW: KanbanWindow = {
  days: "Mon-Fri",
  start: "09:00",
  end: "24:00",
  all_day: false,
  max_tasks: 3,
};

/**
 * The board's settings: concurrency windows and the tick interval.
 *
 * The host remounts a view far more often than the user changes projects, and
 * each remount can carry a new `host` object identity. Reloading on identity
 * would throw away unsaved edits, so loading depends on nothing but mounting —
 * `host` is read through a ref for that reason.
 */
export function SettingsPanel({ host }: { host: PluginHostApi | null }) {
  const [settings, setSettings] = useState<BoardSettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const hostRef = useRef(host);
  hostRef.current = host;

  useEffect(() => {
    let cancelled = false;
    setSettings(null);
    setError(null);
    const current = hostRef.current;
    if (current === null) {
      setError("未连接到宿主，无法读取设置");
      return;
    }
    void (async () => {
      try {
        const loaded = await readSettings(current);
        if (!cancelled) setSettings(loaded);
      } catch (e) {
        if (!cancelled) {
          setError(
            errorKind(e) === "plugin_missing" ? "看板后端未运行，无法读取设置" : "无法读取设置",
          );
        }
      }
    })();
    return () => {
      cancelled = true;
    };
    // Mount only: a re-render must not reset the editing state.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  if (error !== null && settings === null) {
    return (
      <p role="alert" className="kanban-error">
        {error}
      </p>
    );
  }
  if (settings === null) {
    return <p className="kanban-notice">正在读取设置…</p>;
  }

  const patch = (next: Partial<BoardSettings>) => setSettings({ ...settings, ...next });
  const patchWindow = (index: number, next: Partial<KanbanWindow>) => {
    const windows = settings.windows.map((w, i) => (i === index ? { ...w, ...next } : w));
    patch({ windows });
  };

  const save = async () => {
    const current = hostRef.current;
    if (current === null) {
      setError("未连接到宿主，保存失败");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await writeSettings(current, settings);
    } catch (e) {
      // Show the backend's own reason when it refused; only "the backend is not
      // there" gets our generic wording. The user's input is never touched.
      setError(
        isResultUnknown(e)
          ? UNKNOWN_WRITE_TEXT
          : errorKind(e) === "plugin_missing"
            ? "看板后端未运行，保存失败"
            : formatError(e),
      );
      setBusy(false);
      return;
    }
    setBusy(false);
    // Re-read after a successful write: the backend normalizes (compresses
    // weekdays, renders windows), and displaying our own unsaved values would
    // fork the view from what is on disk.
    try {
      setSettings(await readSettings(current));
    } catch (e) {
      setError(formatError(e));
    }
  };

  return (
    <section className="kanban-settings">
      <label className="kanban-field">
        <span>全机并发上限</span>
        <input
          aria-label="全机并发上限"
          type="number"
          min={1}
          value={settings.default_max_tasks}
          onChange={(event) => {
            const value = numericInput(event.target.value);
            if (value !== null) patch({ default_max_tasks: value });
          }}
          className="kanban-input kanban-input-narrow"
        />
      </label>
      {/*
       * This number is machine-wide, not per-board. The board only competes for
       * slots inside that budget, so sessions started by hand or by another
       * board draw on the same allowance. Saying so is the difference between a
       * user setting "my board's limit" and knowing what they actually set.
       */}
      <p className="kanban-subtle">所有项目的看板会话与手动会话共用这个上限。</p>

      <label className="kanban-field">
        <span>推进间隔秒数</span>
        <input
          aria-label="推进间隔秒数"
          type="number"
          min={1}
          max={3600}
          value={settings.interval_secs}
          onChange={(event) => {
            const value = numericInput(event.target.value);
            if (value !== null) patch({ interval_secs: value });
          }}
          className="kanban-input kanban-input-narrow"
        />
      </label>

      <h3 className="kanban-heading">全机时段并发窗口</h3>
      {/*
       * Windows are evaluated inside the backend process, in its local time,
       * not in this device's timezone. When the host is remote that difference
       * is invisible from here, and the plugin cannot detect it, so the note is
       * unconditional rather than conditionally rendered.
       */}
      <p data-testid="window-timezone-note" className="kanban-subtle">
        以下时刻按运行插件的机器（本地时间）判定，不是本设备的时区；跨时区时两者会不一致。
      </p>

      <div className="kanban-window-list">
        {settings.windows.map((window, index) => {
          const selectedDays = parseDays(window.days);
          return (
            <div key={index} className="kanban-window">
              <div className="kanban-window-group">
                <span className="kanban-subtle">星期</span>
                <div role="group" aria-label={`窗口 ${index} 星期`} className="kanban-days">
                  {WEEKDAYS.map((day, dayIndex) => {
                    const isOn = selectedDays[dayIndex];
                    // The backend rejects a window with no days at all, so the
                    // last selected day cannot be switched off. Disabling it
                    // beats letting the user build a state that cannot save.
                    const isLastOn = isOn && selectedDays.filter(Boolean).length === 1;
                    return (
                      <button
                        key={day.en}
                        type="button"
                        aria-label={`窗口 ${index} 星期${day.zh}`}
                        aria-pressed={isOn}
                        title={isLastOn ? "至少保留一天" : day.en}
                        disabled={isLastOn}
                        onClick={() => {
                          const next = [...selectedDays];
                          next[dayIndex] = !isOn;
                          patchWindow(index, { days: renderDays(next) });
                        }}
                        className={isOn ? "kanban-day kanban-day-on" : "kanban-day"}
                      >
                        {day.zh}
                      </button>
                    );
                  })}
                </div>
              </div>

              <label className="kanban-field-inline">
                <span className="kanban-subtle">开始</span>
                <input
                  aria-label={`窗口 ${index} 开始`}
                  className="kanban-input kanban-input-time"
                  value={window.start}
                  disabled={window.all_day}
                  onChange={(event) => patchWindow(index, { start: event.target.value })}
                />
              </label>

              <label className="kanban-field-inline">
                <span className="kanban-subtle">结束</span>
                <input
                  aria-label={`窗口 ${index} 结束`}
                  className="kanban-input kanban-input-time"
                  value={window.end}
                  disabled={window.all_day}
                  onChange={(event) => patchWindow(index, { end: event.target.value })}
                />
              </label>

              <label className="kanban-field-inline">
                <span className="kanban-subtle">上限</span>
                <input
                  aria-label={`窗口 ${index} 上限`}
                  className="kanban-input kanban-input-tiny"
                  type="number"
                  min={1}
                  value={window.max_tasks}
                  onChange={(event) => {
                    const value = numericInput(event.target.value);
                    if (value !== null) patchWindow(index, { max_tasks: value });
                  }}
                />
              </label>

              <button
                type="button"
                aria-label={`删除窗口 ${index}`}
                className="kanban-button"
                onClick={() => patch({ windows: settings.windows.filter((_, i) => i !== index) })}
              >
                删除
              </button>
            </div>
          );
        })}
        {settings.windows.length === 0 ? (
          <p className="kanban-empty">未设置时段窗口</p>
        ) : null}
      </div>

      <button
        type="button"
        className="kanban-button"
        onClick={() => patch({ windows: [...settings.windows, { ...EMPTY_WINDOW }] })}
      >
        添加窗口
      </button>

      <div className="kanban-actions">
        <button type="button" disabled={busy} onClick={() => void save()} className="kanban-button">
          保存
        </button>
      </div>
      {error ? (
        <p role="alert" className="kanban-error">
          {error}
        </p>
      ) : null}
    </section>
  );
}
