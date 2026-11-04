//! 二进制入口：只做参数解析，逻辑都在 lib 里（测试直接驱动那份）。

use example_counter_backend::{serve_stdio, Counter, Request, Response};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (data_dir, rest) = split_data_dir(&args);

    let mut counter = match Counter::open(&data_dir) {
        Ok(counter) => counter,
        Err(error) => {
            eprintln!(
                "example-counter: cannot open data dir {}: {error}",
                data_dir.display()
            );
            std::process::exit(1);
        }
    };

    // `--` 之后是原样 argv（`yi-agent plugins exec` 的形态）；没有 `--` 也没有
    // 其它参数时进 stdio 协议。两种入口共用同一个 `Counter`，不给 CLI 开小灶。
    match rest.split_first() {
        Some((separator, tail)) if separator == "--" => run_cli(&mut counter, tail),
        None => run_stdio(&mut counter),
        Some((verb, tail)) => {
            let mut argv = vec![verb.clone()];
            argv.extend(tail.iter().cloned());
            run_cli(&mut counter, &argv);
        }
    }
}

/// 从参数里摘出 `--data-dir VALUE`，其余原样返回。
fn split_data_dir(args: &[String]) -> (PathBuf, Vec<String>) {
    let mut dir = PathBuf::from(".data");
    let mut rest = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--data-dir" {
            if let Some(value) = args.get(index + 1) {
                dir = PathBuf::from(value);
                index += 2;
                continue;
            }
        }
        rest.push(args[index].clone());
        index += 1;
    }
    (dir, rest)
}

fn run_cli(counter: &mut Counter, argv: &[String]) {
    let Some(verb) = argv.first() else {
        eprintln!("example-counter: no verb given (use `status` or `increment`)");
        std::process::exit(2);
    };
    match verb.as_str() {
        "status" => {
            println!("{}", serde_json::json!({ "total": counter.total() }));
        }
        "increment" => {
            let by = argv
                .iter()
                .position(|arg| arg == "--by")
                .and_then(|index| argv.get(index + 1))
                .and_then(|raw| raw.parse::<i64>().ok())
                .unwrap_or(1);
            let request = Request {
                request_id: format!("cli-{}-{by}", std::process::id()),
                method: "counters/increment".to_string(),
                params: serde_json::json!({ "by": by }),
            };
            match counter.handle(&request) {
                Response::Ok { result, .. } => println!("{result}"),
                Response::Err { kind, detail, .. } => {
                    eprintln!("example-counter: {kind}: {detail}");
                    std::process::exit(1);
                }
            }
        }
        other => {
            eprintln!("example-counter: unknown verb `{other}`");
            std::process::exit(2);
        }
    }
}

fn run_stdio(counter: &mut Counter) {
    let stdin = std::io::stdin();
    serve_stdio(stdin.lock(), std::io::stdout(), counter);
}
