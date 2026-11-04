//! example-counter 的原生后端 fixture（库部分）。
//!
//! 把 `Counter` / `Ledger` / 协议类型放进 lib 是为了让集成测试能直接调用它们；
//! 二进制入口只做参数解析。测试能构造真实请求，而不是解析自己的 stdout。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 数据格式版本。迁移由宿主在打开前完成，这里只声明自己认哪一版。
pub const DATA_FORMAT: u32 = 1;

/// 一个请求。`request_id` 是幂等键——不是日志装饰。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Request {
    pub request_id: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok {
        request_id: String,
        result: serde_json::Value,
    },
    Err {
        request_id: String,
        kind: String,
        detail: String,
    },
}

/// 记录已经执行过的请求，键是 `(requestId, method, params)` 的摘要。
///
/// 为什么不用 `HashMap<requestId, ...>` 直接存：那样"同 ID 改参数"会被误当成
/// 重试而返回旧结果——正是设计里 `Conflict` 要挡的事。存摘要才能区分。
#[derive(Debug, Default)]
pub struct Ledger {
    entries: std::collections::BTreeMap<String, Entry>,
}

#[derive(Debug, Clone)]
struct Entry {
    fingerprint: String,
    response: Response,
}

impl Ledger {
    /// 已有的响应；参数摘要不匹配时给出 `Conflict`，与首次执行走同一条判定。
    pub fn lookup(&self, request: &Request) -> Option<Response> {
        let fingerprint = Self::fingerprint(&request.method, &request.params);
        self.entries.get(&request.request_id).map(|entry| {
            if entry.fingerprint == fingerprint {
                entry.response.clone()
            } else {
                Response::Err {
                    request_id: request.request_id.clone(),
                    kind: "Conflict".to_string(),
                    detail: format!(
                        "request {} was already used with different method or params",
                        request.request_id
                    ),
                }
            }
        })
    }

    fn fingerprint(method: &str, params: &serde_json::Value) -> String {
        let mut hasher = Sha256::new();
        hasher.update(method.as_bytes());
        hasher.update([0u8]);
        hasher.update(canonical(params).as_bytes());
        hex(&hasher.finalize())
    }

    /// 返回已有的响应，或记录新响应。参数不同时给出冲突。
    pub fn settle(&mut self, request: &Request, response: Response) -> Response {
        let fingerprint = Self::fingerprint(&request.method, &request.params);
        match self.entries.get(&request.request_id) {
            Some(entry) if entry.fingerprint == fingerprint => entry.response.clone(),
            Some(_) => Response::Err {
                request_id: request.request_id.clone(),
                kind: "Conflict".to_string(),
                detail: format!(
                    "request {} was already used with different method or params",
                    request.request_id
                ),
            },
            None => {
                self.entries.insert(
                    request.request_id.clone(),
                    Entry {
                        fingerprint,
                        response: response.clone(),
                    },
                );
                response
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 键序无关的规范化：`{a:1,b:2}` 与 `{b:2,a:1}` 是同一个请求。
fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap(),
                        canonical(&map[key])
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        serde_json::Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", body.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 后端。计数存在受管数据路径下，一次写一次落盘。
#[derive(Debug)]
pub struct Counter {
    data_path: PathBuf,
    total: i64,
}

impl Counter {
    /// 打开受管数据目录。`data_dir` 由宿主给出，不是进程 cwd。
    pub fn open(data_dir: &Path) -> std::io::Result<Counter> {
        std::fs::create_dir_all(data_dir)?;
        let data_path = data_dir.join("count.json");
        let total = match std::fs::read(&data_path) {
            Ok(bytes) => serde_json::from_slice::<CountFile>(&bytes)
                .map(|file| file.total)
                .unwrap_or(0),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };
        Ok(Counter { data_path, total })
    }

    pub fn total(&self) -> i64 {
        self.total
    }

    /// 写盘。用临时文件 + rename：半写状态不能被下一次读看见。
    fn persist(&self) -> std::io::Result<()> {
        let body = serde_json::to_vec(&CountFile {
            format: DATA_FORMAT,
            total: self.total,
        })?;
        let staging = self.data_path.with_extension("json.tmp");
        std::fs::write(&staging, body)?;
        std::fs::rename(&staging, &self.data_path)
    }

    /// 处理一个请求。`Read` 方法不碰盘。
    pub fn handle(&mut self, request: &Request) -> Response {
        let ok = |result: serde_json::Value| Response::Ok {
            request_id: request.request_id.clone(),
            result,
        };

        match request.method.as_str() {
            "counters/health" => ok(serde_json::json!({ "ok": true, "dataFormat": DATA_FORMAT })),
            "counters/describe" => ok(serde_json::json!({
                "dataFormat": DATA_FORMAT,
                // 无副作用的运行描述：宿主用它渲染授权/配额提示，不能因此写盘。
                "storesUnderManagedData": true,
                "methods": ["counters/health", "counters/describe", "counters/read", "counters/increment"],
            })),
            "counters/read" => ok(serde_json::json!({ "total": self.total })),
            "counters/increment" => {
                let by = request
                    .params
                    .get("by")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(1);
                self.total += by;
                match self.persist() {
                    Ok(()) => ok(serde_json::json!({ "total": self.total })),
                    Err(error) => Response::Err {
                        request_id: request.request_id.clone(),
                        kind: "Io".to_string(),
                        detail: error.to_string(),
                    },
                }
            }
            other => Response::Err {
                request_id: request.request_id.clone(),
                kind: "NotSupported".to_string(),
                detail: format!("`{other}` is not a method this plugin declares"),
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CountFile {
    format: u32,
    total: i64,
}

/// stdio 协议循环。二进制入口调用它，测试也能驱动它。
pub fn serve_stdio<R: std::io::BufRead, W: std::io::Write>(
    input: R,
    mut output: W,
    counter: &mut Counter,
) {
    let mut ledger = Ledger::default();
    for line in input.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                let response = Response::Err {
                    request_id: String::new(),
                    kind: "InvalidManifest".to_string(),
                    detail: format!("request is not valid JSON: {error}"),
                };
                writeln!(output, "{}", serde_json::to_string(&response).unwrap()).ok();
                output.flush().ok();
                continue;
            }
        };
        let response = match ledger.lookup(&request) {
            Some(settled) => settled,
            None => {
                let response = counter.handle(&request);
                ledger.settle(&request, response)
            }
        };
        writeln!(output, "{}", serde_json::to_string(&response).unwrap()).ok();
        output.flush().ok();
    }
}
