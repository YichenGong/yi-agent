//! chat / responses 通道选择与网关能力探测（结果按 base_url 缓存）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ApiStyle 来自 core（单一来源）；此处只定义「实际通道」与探测判定，并原样转出
// 该类型供本模块使用，绝不另立定义。
pub use yi_agent_core::ApiStyle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Chat,
    Responses,
}

pub type ChannelCache = Arc<Mutex<HashMap<String, Channel>>>;

/// 判定是否「路径不存在」：404/405 且 body 为空。
/// 实测：不存在的路由回 404 空 body；已知端点回 404 会带 JSON（如 model_not_found）。
pub fn looks_like_missing_route(status: u16, body: &str) -> bool {
    (status == 404 || status == 405) && body.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_route_is_404_or_405_with_empty_body() {
        assert!(looks_like_missing_route(404, ""));
        assert!(looks_like_missing_route(405, "   "));
        assert!(looks_like_missing_route(404, "\n"));
    }

    #[test]
    fn known_route_404_with_json_is_not_missing() {
        assert!(!looks_like_missing_route(
            404,
            r#"{"error":{"code":"model_not_found"}}"#
        ));
        assert!(!looks_like_missing_route(405, "{}"));
    }

    #[test]
    fn other_statuses_are_not_missing() {
        assert!(!looks_like_missing_route(500, ""));
        assert!(!looks_like_missing_route(200, ""));
        assert!(!looks_like_missing_route(401, ""));
    }

    #[test]
    fn channel_is_copy_and_eq() {
        let a = Channel::Chat;
        let b = a;
        assert_eq!(a, b);
        assert_ne!(Channel::Chat, Channel::Responses);
    }

    #[test]
    fn api_style_is_reused_from_core() {
        assert_eq!(ApiStyle::Auto.as_str(), "auto");
    }
}
