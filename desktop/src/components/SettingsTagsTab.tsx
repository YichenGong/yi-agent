import { useState } from "react";
import { isImeEnter, useImeGuard } from "../lib/imeEnter";
import {
  DEFAULT_TAG_NAMES,
  TAG_DOT_CLASS,
  TAG_IDS,
  TAG_NAME_MAX_LEN,
  type TagId,
} from "../lib/threadTags";

/** 一行的编辑态：受控值 + 上一次已保存值（用于失败回填与「是否已改动」判定）。 */
interface RowState {
  value: string;
  error: string | null;
}

/**
 * 设置里的「标签」Tab：改 6 个颜色标签的显示名。
 *
 * 名字是**全局**的（所有会话共用同一套定义），故这里改的是宿主级偏好，而非某条
 * 会话的属性。只改名、不能增删颜色、不能改色值——那是本功能的非目标。
 */
export function SettingsTagsTab({
  tagNames,
  onSave,
}: {
  tagNames: Record<TagId, string>;
  onSave: (tagId: TagId, name: string) => Promise<void> | void;
}) {
  const ime = useImeGuard();
  const [rows, setRows] = useState<Record<TagId, RowState>>(() => {
    const initial = {} as Record<TagId, RowState>;
    for (const id of TAG_IDS) initial[id] = { value: tagNames[id], error: null };
    return initial;
  });

  const commit = (id: TagId) => {
    const raw = rows[id].value;
    const name = raw.trim();
    if (name === "") {
      setRows((prev) => ({ ...prev, [id]: { ...prev[id], error: "名字不能为空" } }));
      return;
    }
    if (name.length > TAG_NAME_MAX_LEN) {
      setRows((prev) => ({
        ...prev,
        [id]: { ...prev[id], error: `名字最多 ${TAG_NAME_MAX_LEN} 个字符` },
      }));
      return;
    }
    // 未改动就直接清掉可能残留的错误，不发请求。
    if (name === tagNames[id]) {
      setRows((prev) => ({ ...prev, [id]: { value: name, error: null } }));
      return;
    }
    setRows((prev) => ({ ...prev, [id]: { value: name, error: null } }));
    void onSave(id, name);
  };

  return (
    <section className="p-5">
      <h2 className="text-sm font-medium text-fg">标签</h2>
      <p className="mt-2 text-xs text-fg-subtle">
        6 个固定颜色，名字由你定义；改名字对所有会话立即生效，已打的标签颜色不变。
      </p>
      <ul className="mt-4 space-y-2">
        {TAG_IDS.map((id) => (
          <li key={id} className="flex items-center gap-3">
            <span
              aria-hidden="true"
              className={`size-3.5 shrink-0 rounded-full ${TAG_DOT_CLASS[id]}`}
            />
            <input
              aria-label={`${DEFAULT_TAG_NAMES[id]}标签名`}
              data-tag-name-input={id}
              value={rows[id].value}
              maxLength={TAG_NAME_MAX_LEN}
              onChange={(e) =>
                setRows((prev) => ({
                  ...prev,
                  [id]: { value: e.target.value, error: prev[id].error },
                }))
              }
              onCompositionStart={ime.onCompositionStart}
              onCompositionEnd={ime.onCompositionEnd}
              onBlur={() => {
                ime.resetComposition();
                commit(id);
              }}
              onKeyDown={(e) => {
                // 中文输入法确认候选的 Enter 不能提交（否则会吞掉半个词）。
                if (e.key === "Enter" && !isImeEnter(e, ime.composing.current)) commit(id);
              }}
              className="w-48 rounded border border-line bg-surface px-2 py-1 text-sm text-fg outline-none focus:border-line-strong"
            />
            {rows[id].error && (
              <span role="alert" className="text-xs text-red-400">
                {rows[id].error}
              </span>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}
