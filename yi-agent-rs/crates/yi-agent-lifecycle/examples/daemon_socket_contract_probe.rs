//! 跨端契约测试用的宿主探针：**只做路径解析**，不含任何业务知识。
//!
//! 插件用一个同样只读环境的 worker 调它自己的生产函数，两侧输出同一套 JSON 投影，
//! 测试逐 case 比较。这样「插件复刻的规则」与「宿主真实的规则」之间不可能悄悄
//! 漂移——两个 crate 之间也没有任何编译期依赖。
//!
//! 用法：`daemon_socket_contract_probe <runtime_hex> [--delete-cwd]`
//!
//! * `runtime_hex` 是 runtime 目录**原始字节**的十六进制（支持非 UTF-8）。
//! * `--delete-cwd` 在进程启动后删掉自己的 cwd（必须是一个专用的空目录），
//!   用来验证 cwd 消失时相对路径的解析行为。
//!
//! 输出单行 JSON：
//! * 成功：`{"socket_hex":"...","error":null}`
//! * 缺 home：`{"socket_hex":null,"error":"instance_home_unavailable"}`
//! * 超长：`{"socket_hex":null,"error":"path_too_long","bytes":N,"limit":103,"path_hex":"..."}`
//!
//! 业务解析失败（缺 home / 超长）是**正常输出**，退出码 0；只有 CLI 用法错误或
//! IO 失败才 exit 1。测试必须比较 `error`，不能只看退出码。

use std::path::PathBuf;

use yi_agent_lifecycle::scope;

fn hex_decode(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err(format!("odd hex length: {text}"));
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|error| error.to_string())?;
            u8::from_str_radix(pair, 16).map_err(|error| format!("invalid hex {pair}: {error}"))
        })
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn emit(value: serde_json::Value) {
    println!("{value}");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(runtime_hex) = args.next() else {
        eprintln!("usage: daemon_socket_contract_probe <runtime_hex> [--delete-cwd]");
        std::process::exit(1);
    };
    let delete_cwd = match args.next().as_deref() {
        None => false,
        Some("--delete-cwd") => true,
        Some(other) => {
            eprintln!("unknown argument: {other}");
            std::process::exit(1);
        }
    };
    if args.next().is_some() {
        eprintln!("too many arguments");
        std::process::exit(1);
    }

    let runtime = match hex_decode(&runtime_hex) {
        Ok(bytes) => {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(bytes))
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };

    if delete_cwd {
        // 只删专用空目录；删除失败是真实的 IO 错误，必须暴露而不是继续。
        let cwd = match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(error) => {
                eprintln!("cannot read cwd: {error}");
                std::process::exit(1);
            }
        };
        if let Err(error) = std::fs::remove_dir(&cwd) {
            eprintln!("cannot delete cwd {}: {error}", cwd.display());
            std::process::exit(1);
        }
    }

    let Some(home) = scope::instance_home_for_sockets() else {
        emit(serde_json::json!({
            "socket_hex": null,
            "error": "instance_home_unavailable",
        }));
        return;
    };

    let socket = scope::daemon_socket(&runtime, &home);
    let bytes = socket.as_os_str().len();
    if bytes > scope::MAX_SOCKET_PATH_BYTES {
        emit(serde_json::json!({
            "socket_hex": null,
            "error": "path_too_long",
            "bytes": bytes,
            "limit": scope::MAX_SOCKET_PATH_BYTES,
            "path_hex": hex_encode(socket.as_os_str().as_encoded_bytes()),
        }));
        return;
    }

    emit(serde_json::json!({
        "socket_hex": hex_encode(socket.as_os_str().as_encoded_bytes()),
        "error": null,
    }));
}
