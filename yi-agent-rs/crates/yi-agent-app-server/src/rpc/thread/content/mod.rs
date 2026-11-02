//! `thread` 域的 content 组：list / listAll / diff/read / view/read /
//! subscribe / readItems。

// Task 10 只建立归属骨架：`owns()` 的调用方（`handle` 的分组分派）在 Task 14
// 才接上，此前它只被 `owns_coverage` 测试使用。
#[allow(dead_code)]
pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "thread/list"
            | "thread/listAll"
            | "thread/diff/read"
            | "thread/view/read"
            | "thread/subscribe"
            | "thread/readItems"
    )
}
