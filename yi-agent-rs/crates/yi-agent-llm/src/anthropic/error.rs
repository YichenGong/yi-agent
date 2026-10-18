//! Error mapping from Anthropic HTTP responses to ProviderError.

use yi_agent_core::ProviderError;

/// Map a `reqwest::Response` into a `ProviderError` based on its HTTP status code.
///
/// The response body is consumed as text and included in the error message for
/// diagnostic purposes. Status codes are mapped as follows:
///
/// | Status            | Variant                      |
/// |-------------------|------------------------------|
/// | 401, 403          | `Auth`                       |
/// | 429               | `RateLimited`                |
/// | 400, 422          | `InvalidRequest`             |
/// | 500..=599         | `Server`                     |
/// | other 4xx         | `InvalidRequest` (deterministic) |
/// | other 5xx         | `Server`                     |
pub async fn map_status_error(resp: reqwest::Response) -> ProviderError {
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();

    match status {
        401 | 403 => ProviderError::Auth(format!("{}: {}", status, body)),
        429 => ProviderError::RateLimited,
        400 | 422 => ProviderError::InvalidRequest(format!("{}: {}", status, body)),
        500..=599 => ProviderError::Server(format!("{}: {}", status, body)),
        // 其余 4xx 是确定性的客户端错误（404 路由/模型不存在、413 请求过大……）：
        // 归为 `Server` 会被重投逻辑误当成可重试的 5xx，故按 `InvalidRequest` 归类。
        _ => ProviderError::InvalidRequest(format!("unexpected status {}: {}", status, body)),
    }
}

#[cfg(test)]
mod tests {
    // Unit testing map_status_error directly requires constructing a reqwest::Response,
    // which is not trivially possible from a status code alone. Coverage of the status
    // mapping is provided by integration tests (see Task 9 in the implementation plan).
}
