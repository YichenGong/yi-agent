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
    use super::is_effort_rejection;

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
