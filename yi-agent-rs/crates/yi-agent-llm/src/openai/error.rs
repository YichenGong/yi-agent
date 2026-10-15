//! Error mapping from OpenAI HTTP responses to ProviderError.

use yi_agent_core::ProviderError;

/// Map a `reqwest::Response` into a `ProviderError` based on its HTTP status code.
///
/// | Status            | Variant                      |
/// |-------------------|------------------------------|
/// | 401, 403          | `Auth`                       |
/// | 429               | `RateLimited`                |
/// | 400, 422          | `InvalidRequest`             |
/// | 500..=599         | `Server`                     |
/// | other             | `Server` (unexpected status) |
pub async fn map_status_error(resp: reqwest::Response) -> ProviderError {
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    map_status_error_from(status, &body)
}

/// Classify a status + already-read body. Split out so callers that must read
/// the body themselves (e.g. the auto-fallback probe) can reuse the same arms.
pub fn map_status_error_from(status: u16, body: &str) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth(format!("{}: {}", status, body)),
        429 => ProviderError::RateLimited,
        400 | 422 => ProviderError::InvalidRequest(format!("{}: {}", status, body)),
        500..=599 => ProviderError::Server(format!("{}: {}", status, body)),
        _ => ProviderError::Server(format!("unexpected status {}: {}", status, body)),
    }
}

/// 该响应是否为「网关内部组件瞬时拒绝了请求」——请求本身没有可指认的问题。
///
/// 实测形态（horizon MaaS 网关，Responses 与 Chat 两个通道都会返回；内层 JSON
/// 是被转义后的字符串，故特征码在原始 body 里不带引号边界）：
///
/// ```text
/// {"error":{"message":"{\"error\":{\"type\":\"invalid_request_error\",\"code\":\"400001\",
///   \"message\":\"The request is invalid: the request was rejected by an internal MaaS
///   component. Please check the request body, required fields, and request format.\",
///   \"source\":\"client\",\"request_id\":\"…\"}}\n","type":"upstream_error"}}
/// ```
///
/// 判定要求**两个特征同时命中**：
///
/// 1. 外层 `type` 为 `upstream_error`（网关把上游/内部的失败原样透传，而非客户端
///    请求有错）；
/// 2. 携带 `400001` 或 `internal MaaS component` 这一对专属标记。
///
/// 为什么不能只看状态码：带 `param` 的 400 是**确定性**的（`reasoning_effort` 档位
/// 被拒、`tools[5].function.parameters` schema 非法、`code: context_length_exceeded`
/// 等），重投必然再失败，必须原样上抛；只有「无可指认字段」的拒绝才值得原样重投。
/// 依据：线上同一段会话在被拒后数十秒、以几乎相同的 body 重发即 200 正常流式，
/// 且对全部可疑 body 形态（超长 instructions、非法 tool schema、控制字符、
/// 悬空 `function_call_output`、空 `call_id` 等约 150 例探针）逐一实测均为 200。
pub fn is_transient_gateway_rejection(status: u16, body: &str) -> bool {
    (400..=599).contains(&status)
        && body.contains("\"upstream_error\"")
        && (body.contains("400001") || body.contains("internal MaaS component"))
        // 指名了字段的拒绝是可修的确定性问题，不能当瞬时故障吞掉。
        && !body.contains("\"param\"")
}

/// 该 400/422 是否为「模型不支持所配 effort」类拒绝。
///
/// 实测 horizon 回 `param: "reasoning_effort"` 或 `param: "reasoning.effort"`，
/// 并伴随 `Supported value` / `does not support` / `not supported` / `unsupported`
/// 之一。要求状态码与两个特征同时命中，避免把无关的 400 误判成 effort 拒绝。
pub fn is_effort_rejection(status: u16, body: &str) -> bool {
    matches!(status, 400 | 422)
        && (body.contains("\"reasoning_effort\"") || body.contains("reasoning.effort"))
        && (body.contains("Supported value")
            || body.contains("does not support")
            || body.contains("not supported")
            || body.contains("unsupported"))
}

#[cfg(test)]
mod tests {
    use super::{is_effort_rejection, is_transient_gateway_rejection};

    /// 线上真实形态（外层内层都是 JSON，内层被转义成字符串）。
    const REAL_MAAS_BODY: &str = concat!(
        r#"{"error":{"message":"{\"error\":{\"type\":\"invalid_request_error\",\"code\":\"400001\","#,
        r#"\"message\":\"The request is invalid: the request was rejected by an internal MaaS component. "#,
        r#"Please check the request body, required fields, and request format.\",\"message_zh\":\"请求不合法：the "#,
        r#"request was rejected by an internal MaaS component，请检查请求体、必填字段及请求格式是否正确。\","#,
        r#"\"source\":\"client\",\"request_id\":\"5f5338e2-5c52-46b3-99b2-6c0780e2dcc4\"}}\n","type":"upstream_error"}}"#
    );

    #[test]
    fn real_maas_rejection_is_transient() {
        assert!(is_transient_gateway_rejection(400, REAL_MAAS_BODY));
    }

    #[test]
    fn the_same_marker_is_transient_on_any_server_side_status() {
        // 网关换状态码（500）但标记不变时仍然算瞬时。
        assert!(is_transient_gateway_rejection(500, REAL_MAAS_BODY));
    }

    #[test]
    fn a_gateway_error_naming_a_field_is_not_transient() {
        // 带 param 的 400 是确定性的：重投必然再失败，必须上抛。
        let body = r#"{"error":{"message":"x","type":"upstream_error","code":"400001","param":"reasoning_effort"}}"#;
        assert!(!is_transient_gateway_rejection(400, body));
    }

    #[test]
    fn other_400s_are_not_transient() {
        let effort = r#"{"error":{"message":"Unsupported value","param":"reasoning_effort"}}"#;
        assert!(!is_transient_gateway_rejection(400, effort));
        let schema = r#"{"error":{"message":"Invalid schema for function 'process_kill'","type":"invalid_request_error","param":"tools[5].function.parameters"}}"#;
        assert!(!is_transient_gateway_rejection(400, schema));
        let overflow = r#"{"error":{"message":"Input tokens exceed the configured limit","code":"context_length_exceeded"}}"#;
        assert!(!is_transient_gateway_rejection(400, overflow));
        assert!(!is_transient_gateway_rejection(200, REAL_MAAS_BODY));
    }

    #[test]
    fn real_horizon_effort_rejection_is_detected() {
        // 真实 horizon 报错形态。
        let body = r#"{"error":{"message":"reasoning_effort are not supported for gpt-6-luna","param":"reasoning_effort"}}"#;
        assert!(is_effort_rejection(400, body));
    }

    #[test]
    fn unsupported_value_variant_is_detected() {
        let body = r#"{"error":{"message":"Unsupported value","param":"reasoning_effort","code":"unsupported_value"}}"#;
        assert!(is_effort_rejection(400, body));
    }

    #[test]
    fn dotted_param_is_detected() {
        let body = r#"{"error":{"message":"unsupported parameter","param":"reasoning.effort"}}"#;
        assert!(is_effort_rejection(422, body));
    }

    #[test]
    fn generic_400_about_temperature_is_not_effort() {
        let body =
            r#"{"error":{"message":"Unsupported value for temperature","param":"temperature"}}"#;
        assert!(!is_effort_rejection(400, body));
    }

    #[test]
    fn server_error_mentioning_effort_is_not_effort_rejection() {
        let body = r#"{"error":{"message":"reasoning_effort field unrecognized"}}"#;
        assert!(!is_effort_rejection(500, body));
    }
}
