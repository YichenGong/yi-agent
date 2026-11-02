//! `thread` 域的 settings 组：rename / setModel / setReasoningEffort /
//! setPreserveThinking / setPermissionMode / setPinned / setTag / markRead /
//! reorderPinned / compact。

// Task 10 只建立归属骨架：`owns()` 的调用方（`handle` 的分组分派）在 Task 14
// 才接上，此前它只被 `owns_coverage` 测试使用。
#[allow(dead_code)]
pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "thread/rename"
            | "thread/setModel"
            | "thread/setReasoningEffort"
            | "thread/setPreserveThinking"
            | "thread/setPermissionMode"
            | "thread/setPinned"
            | "thread/setTag"
            | "thread/markRead"
            | "thread/reorderPinned"
            | "thread/compact"
    )
}
