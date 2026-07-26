//! 复杂 one-shot 任务测试:验证 agent 能完成多步骤生成任务。
//! 全部 #[ignore]'d; run with: cargo test -p yi-agent --test e2e_complex -- --ignored
//!
//! 配置源:父进程环境变量(由 justfile recipe 从 .env 加载)。

mod common;
use common::{
    has_normal_end_turn, has_verification_after_last_mutation, parse_events,
    run_agent_with_timeout, skip_if_no_key,
};

const PROMPT_WEBSITE: &str = "Create a single-page personal website. Write the complete HTML (with inline CSS) to output/index.html. The page should include a header, an 'About' section, and a footer. Use the write tool to create the file.";

#[test]
#[ignore]
fn complex_personal_website() {
    if !skip_if_no_key() {
        return;
    }

    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("output")).expect("create output dir");

    let output = run_agent_with_timeout(tmp.path(), PROMPT_WEBSITE)
        .expect("personal website agent run timed out");

    assert!(
        output.status.success(),
        "yi-agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events = parse_events(&stdout);
    assert!(
        has_normal_end_turn(&events),
        "no normal EndTurn, stdout: {stdout}"
    );
    assert!(
        has_verification_after_last_mutation(&events),
        "no verification after final mutation, stdout: {stdout}"
    );

    // 结构性断言
    let html_path = tmp.path().join("output/index.html");
    assert!(html_path.exists(), "index.html not created");
    let html = std::fs::read_to_string(&html_path).expect("read index.html");
    assert!(
        html.len() > 500,
        "index.html too small: {} bytes",
        html.len()
    );
    let lower = html.to_lowercase();
    assert!(lower.contains("<html"), "missing <html tag");
    assert!(lower.contains("<body"), "missing <body tag");
    assert!(lower.contains("about"), "missing About section");
    assert!(lower.contains("<footer"), "missing <footer tag");
}

const PROMPT_PYTHON: &str = "Write a Python function called `sort_list` that takes a list and returns it sorted in ascending order. Write it to output/sort.py. The file should be a valid Python module with a `if __name__ == '__main__'` guard that demonstrates the function.";

#[test]
#[ignore]
fn complex_python_script() {
    if !skip_if_no_key() {
        return;
    }

    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("output")).expect("create output dir");

    let output = run_agent_with_timeout(tmp.path(), PROMPT_PYTHON)
        .expect("python script agent run timed out");

    assert!(
        output.status.success(),
        "yi-agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events = parse_events(&stdout);
    assert!(
        has_normal_end_turn(&events),
        "no normal EndTurn, stdout: {stdout}"
    );
    assert!(
        has_verification_after_last_mutation(&events),
        "no verification after final mutation, stdout: {stdout}"
    );

    // 结构性断言(不执行产出代码)
    let py_path = tmp.path().join("output/sort.py");
    assert!(py_path.exists(), "sort.py not created");
    let py = std::fs::read_to_string(&py_path).expect("read sort.py");
    assert!(py.len() > 100, "sort.py too small: {} bytes", py.len());
    assert!(py.contains("def sort_list"), "missing def sort_list");
    assert!(py.contains("__main__"), "missing __main__ guard");
    let check = std::process::Command::new("python3")
        .arg("-c")
        .arg("import runpy; ns = runpy.run_path('output/sort.py'); assert ns['sort_list']([3, 1, 2]) == [1, 2, 3]")
        .current_dir(tmp.path())
        .output()
        .expect("run python validation");
    assert!(
        check.status.success(),
        "sort.py is not a working sort_list module: {}",
        String::from_utf8_lossy(&check.stderr)
    );
}

const PROMPT_DATA: &str = "Read the file input/data.json, extract all `name` fields, convert them to uppercase, and write the result as a JSON array to output/results.json.";

#[test]
#[ignore]
fn complex_data_transformation() {
    if !skip_if_no_key() {
        return;
    }

    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("input")).expect("create input dir");
    std::fs::create_dir_all(tmp.path().join("output")).expect("create output dir");
    std::fs::write(
        tmp.path().join("input/data.json"),
        r#"[{"name":"alice","age":30},{"name":"bob","age":25},{"name":"charlie","age":35}]"#,
    )
    .expect("write data.json");

    let output = run_agent_with_timeout(tmp.path(), PROMPT_DATA)
        .expect("data transformation agent run timed out");

    assert!(
        output.status.success(),
        "yi-agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events = parse_events(&stdout);
    assert!(
        has_normal_end_turn(&events),
        "no normal EndTurn, stdout: {stdout}"
    );
    assert!(
        has_verification_after_last_mutation(&events),
        "no verification after final mutation, stdout: {stdout}"
    );

    // 结构性断言
    let results_path = tmp.path().join("output/results.json");
    assert!(results_path.exists(), "results.json not created");
    let content = std::fs::read_to_string(&results_path).expect("read results.json");

    // 必须是合法 JSON 数组
    let parsed: serde_json::Value =
        serde_json::from_str(&content).expect("results.json is not valid JSON");
    let arr = parsed
        .as_array()
        .expect("results.json should be a JSON array");
    assert_eq!(
        arr,
        &vec![
            serde_json::json!("ALICE"),
            serde_json::json!("BOB"),
            serde_json::json!("CHARLIE")
        ]
    );
}

const PROMPT_BUGFIX: &str = "The file buggy.py contains a Python function with a bug. Read it, identify the bug, fix it, and write the fixed version to output/fixed.py. Do not just copy the original — fix the bug.";

#[test]
#[ignore]
fn complex_bug_fix() {
    if !skip_if_no_key() {
        return;
    }

    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("output")).expect("create output dir");
    std::fs::write(
        tmp.path().join("buggy.py"),
        "def add(a, b):\n    return a - b   # BUG: should be +\n\nif __name__ == \"__main__\":\n    print(add(2, 3))\n",
    )
    .expect("write buggy.py");

    let output =
        run_agent_with_timeout(tmp.path(), PROMPT_BUGFIX).expect("bug-fix agent run timed out");

    assert!(
        output.status.success(),
        "yi-agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events = parse_events(&stdout);
    assert!(
        has_normal_end_turn(&events),
        "no normal EndTurn, stdout: {stdout}"
    );
    assert!(
        has_verification_after_last_mutation(&events),
        "no verification after final mutation, stdout: {stdout}"
    );

    // 结构性断言
    let fixed_path = tmp.path().join("output/fixed.py");
    assert!(fixed_path.exists(), "fixed.py not created");
    let fixed = std::fs::read_to_string(&fixed_path).expect("read fixed.py");
    assert!(
        fixed.len() > 50,
        "fixed.py too small: {} bytes",
        fixed.len()
    );
    assert!(fixed.contains("def add"), "missing def add");
    assert!(
        fixed.contains('+'),
        "missing + (fix should contain addition)"
    );
    assert!(
        !fixed.contains("return a - b"),
        "original bug line 'return a - b' should be replaced"
    );
    let check = std::process::Command::new("python3")
        .arg("-c")
        .arg("import runpy; ns = runpy.run_path('output/fixed.py'); assert ns['add'](2, 3) == 5")
        .current_dir(tmp.path())
        .output()
        .expect("run python validation");
    assert!(
        check.status.success(),
        "fixed.py does not implement add correctly: {}",
        String::from_utf8_lossy(&check.stderr)
    );
}
