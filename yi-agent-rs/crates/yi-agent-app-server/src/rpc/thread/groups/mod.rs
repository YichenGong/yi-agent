//! `thread` 域的 groups 组：lead / group/create / group/update /
//! group/setMembers / group/delete。

// Task 10 只建立归属骨架：`owns()` 的调用方（`handle` 的分组分派）在 Task 14
// 才接上，此前它只被 `owns_coverage` 测试使用。
#[allow(dead_code)]
pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "thread/lead"
            | "thread/group/create"
            | "thread/group/update"
            | "thread/group/setMembers"
            | "thread/group/delete"
    )
}
