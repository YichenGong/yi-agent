//! 复杂 one-shot 任务测试:验证 agent 能完成多步骤生成任务。
//! 全部 #[ignore]'d; run with: cargo test -p yi-agent --test e2e_complex -- --ignored
//!
//! 配置源:父进程环境变量(由 justfile recipe 从 .env 加载)。

mod common;
use common::{has_done_event, parse_events, skip_if_no_key, yi_agent_bin};

use std::process::Command;

const PROMPT_WEBSITE: &str = "Create a single-page personal website. Write the complete HTML (with inline CSS) to output/index.html. The page should include a header, an 'About' section, and a footer. Use the write tool to create the file.";

#[test]
#[ignore]
fn complex_personal_website() {
    if !skip_if_no_key() {
        return;
    }

    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("output")).expect("create output dir");

    let output = Command::new(yi_agent_bin())
        .arg("--workdir")
        .arg(tmp.path())
        .arg("run")
        .arg("--json")
        .arg(PROMPT_WEBSITE)
        .output()
        .expect("failed to spawn yi-agent");

    assert!(
        output.status.success(),
        "yi-agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events = parse_events(&stdout);
    assert!(has_done_event(&events), "no Done event, stdout: {stdout}");

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
