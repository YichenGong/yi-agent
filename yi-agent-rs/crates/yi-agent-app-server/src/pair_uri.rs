//! 二维码配对载荷编解码（S4）。
//!
//! 二维码文本 = `yiagent://pair?v=1&relay=<percent-encoded relay url>&code=<code>`。
//! 这是**两端唯一的跨端契约**：TS 侧 `desktop/src/lib/pairUri.ts` 必须逐字节一致
//! （两份用同一组 fixture 断言）。纯函数、无 IO。
//!
//! `relay` 既可是中继地址（`wss://relay.example.com/ws?session=<id>`），也可是局域网
//! 直连地址（`ws://192.168.x.x:8080/ws`）；因自身含 `?`，作 query 值须 form-encode。

/// 自定义 scheme（不依赖任何 OS 级 URL scheme 注册）。
pub const PAIR_SCHEME: &str = "yiagent";
/// scheme 下的 host 段。
pub const PAIR_HOST: &str = "pair";
/// 载荷版本位；未知版本一律判无效。
pub const PAIR_VERSION: &str = "1";

/// 按契约拼出二维码文本。查询串用 form 编码（与浏览器 `URLSearchParams` 一致）。
pub fn build_pair_uri(relay: &str, code: &str) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    q.append_pair("v", PAIR_VERSION);
    q.append_pair("relay", relay);
    q.append_pair("code", code);
    format!("{PAIR_SCHEME}://{PAIR_HOST}?{}", q.finish())
}

/// 解析二维码文本；不满足契约（scheme/host/版本/字段/ws 前缀任一不符）返回 `None`。
pub fn parse_pair_uri(text: &str) -> Option<(String, String)> {
    let u = url::Url::parse(text).ok()?;
    if u.scheme() != PAIR_SCHEME || u.host_str() != Some(PAIR_HOST) {
        return None;
    }
    let mut relay = None;
    let mut code = None;
    let mut version = None;
    for (k, v) in u.query_pairs() {
        match k.as_ref() {
            "v" => version = Some(v.into_owned()),
            "relay" => relay = Some(v.into_owned()),
            "code" => code = Some(v.into_owned()),
            _ => {}
        }
    }
    if version.as_deref() != Some(PAIR_VERSION) {
        return None;
    }
    let relay = relay?;
    let code = code?;
    if relay.is_empty() || code.is_empty() {
        return None;
    }
    if !(relay.starts_with("ws://") || relay.starts_with("wss://")) {
        return None;
    }
    Some((relay, code))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 与 TS 侧 `desktop/src/lib/pairUri.ts` 的用例**逐字节**共用同一组 fixture。
    #[test]
    fn builds_the_canonical_uri() {
        assert_eq!(
            build_pair_uri("wss://relay.example.com/ws?session=abc", "ABCD-EFGH"),
            "yiagent://pair?v=1&relay=wss%3A%2F%2Frelay.example.com%2Fws%3Fsession%3Dabc&code=ABCD-EFGH"
        );
        assert_eq!(
            build_pair_uri("ws://192.168.1.5:8080/ws", "WXYZ-1234"),
            "yiagent://pair?v=1&relay=ws%3A%2F%2F192.168.1.5%3A8080%2Fws&code=WXYZ-1234"
        );
        // 空格/`&`/`.`：钉住 form-encoding 语义（空格→`+`，`.` 不转义）。
        assert_eq!(
            build_pair_uri("wss://r/a b.c?x=1&y=2", "A-B"),
            "yiagent://pair?v=1&relay=wss%3A%2F%2Fr%2Fa+b.c%3Fx%3D1%26y%3D2&code=A-B"
        );
    }

    #[test]
    fn round_trips() {
        for (relay, code) in [
            ("wss://relay.example.com/ws?session=abc", "ABCD-EFGH"),
            ("ws://192.168.1.5:8080/ws", "WXYZ-1234"),
        ] {
            assert_eq!(
                parse_pair_uri(&build_pair_uri(relay, code)),
                Some((relay.into(), code.into()))
            );
        }
    }

    #[test]
    fn rejects_malformed_payloads() {
        assert_eq!(parse_pair_uri("hello"), None);
        assert_eq!(
            parse_pair_uri("https://pair?v=1&relay=wss%3A%2F%2Fr&code=C"),
            None
        ); // wrong scheme
        assert_eq!(
            parse_pair_uri("yiagent://pair?v=2&relay=wss%3A%2F%2Fr&code=C"),
            None
        ); // wrong version
        assert_eq!(
            parse_pair_uri("yiagent://pair?v=1&relay=wss%3A%2F%2Fr"),
            None
        ); // missing code
        assert_eq!(parse_pair_uri("yiagent://pair?v=1&code=C"), None); // missing relay
        assert_eq!(
            parse_pair_uri("yiagent://pair?v=1&relay=http%3A%2F%2Fr&code=C"),
            None
        ); // not ws(s)
        assert_eq!(parse_pair_uri("yiagent://pair?v=1&relay=&code=C"), None); // empty relay
    }
}
