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

    match status {
        401 | 403 => ProviderError::Auth(format!("{}: {}", status, body)),
        429 => ProviderError::RateLimited,
        400 | 422 => ProviderError::InvalidRequest(format!("{}: {}", status, body)),
        500..=599 => ProviderError::Server(format!("{}: {}", status, body)),
        _ => ProviderError::Server(format!("unexpected status {}: {}", status, body)),
    }
}
