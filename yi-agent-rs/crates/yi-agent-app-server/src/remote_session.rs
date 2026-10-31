//! 由（relay URL, profile id）派生该远端项目要用的 relay session。
//!
//! **为什么要独立一份实现**：`desktop/src-tauri/src/remote/profile.rs` 的
//! `derive_relay` 是权威版本，但 desktop 是一个独立 crate，app-server 不能
//! 依赖它（desktop 依赖 app-server 的协议类型，反向依赖会成环）。所以本文件
//! 是**同构副本**：同一组输入必须给出逐字相同的输出，否则手机拿到的 session
//! 与远端实际连上的 session 不是同一个——手机看到「有项目」但永远连不上，
//! 而两边各自的日志都正常。两处各有一份共享用例表（`SHARED_CASES`）钉住这点。
//!
//! 派生规则：`<原有 session 或 "remote">-<profile_id>`。
//!
//! **不能用 `.push_str("&session=")` 追加**：`relay_url` 本身可能已经带
//! `?session=`，追加会得到 `?session=a&session=b`，中继取哪个取决于实现
//! （spec §4.6 前提 2）。这里模拟 URL 的 `searchParams.set` 语义。
//!
//! 不需要密码学强度——目的只是「不同 profile 得到不同 session」——但**必须**
//! 稳定：不稳定的话同一项目每次重连都会在手机上多出一个 session。

/// 两处实现共享的用例表：`(relay_url, profile_id, expected)`。
///
/// 任何规则变更都必须在这张表里体现，`desktop` 侧有一份逐字相同的副本，
/// 两边都断言，编译器不会替我们检查这个跨 crate 的约定。
#[cfg(test)]
pub(crate) const SHARED_CASES: &[(&str, &str, &str)] = &[
    (
        "wss://r/?session=base",
        "proj-foo",
        "wss://r/?session=base-proj-foo",
    ),
    ("wss://r/connect", "p", "wss://r/connect?session=remote-p"),
    (
        "wss://r/?token=t&session=s",
        "p",
        "wss://r/?token=t&session=s-p",
    ),
    (
        "wss://r/?session=old",
        "proj-foo",
        "wss://r/?session=old-proj-foo",
    ),
    ("wss://r/?token=t", "p", "wss://r/?token=t&session=remote-p"),
    (
        "ws://127.0.0.1:8080/ws",
        "a",
        "ws://127.0.0.1:8080/ws?session=remote-a",
    ),
    // 幂等：远端 app-server 对自己已派生过的 session 再派生一次必须是 no-op。
    (
        "wss://r/?session=base-proj-foo",
        "proj-foo",
        "wss://r/?session=base-proj-foo",
    ),
];

pub fn derive_session(relay_url: &str, profile_id: &str) -> String {
    let seed = session_seed(relay_url);
    // **幂等**：已经带着本项目后缀的 session 原样返回。
    //
    // 这不是防御性编程，是必需的：远端 app-server 自己就是从 `--relay
    // wss://r/?session=base-<id>` 起的，它的 `remote/relayTargets` 若再为同一个
    // 项目派生一次，会得到 `base-<id>-<id>`——一个没人监听的 session。两端
    // 各自的日志都正常，只有手机连不上。
    let suffix = format!("-{profile_id}");
    if seed.ends_with(&suffix) {
        return relay_url.to_string();
    }
    let session = format!("{seed}{suffix}");
    set_query_param(relay_url, "session", &session)
}

fn session_seed(url: &str) -> String {
    get_query_param(url, "session").unwrap_or_else(|| "remote".to_string())
}

fn get_query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query
        .split('&')
        .find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| v.to_string())
        })
        .filter(|v| !v.is_empty())
}

fn set_query_param(url: &str, key: &str, value: &str) -> String {
    let (head, query) = match url.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (url, None),
    };
    let mut pairs: Vec<String> = query
        .map(|query| {
            query
                .split('&')
                .filter(|pair| !pair.is_empty() && !pair.starts_with(&format!("{key}=")))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    pairs.push(format!("{key}={value}"));
    format!("{head}?{}", pairs.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 desktop 侧共享的用例表：逐条对齐。
    #[test]
    fn matches_the_shared_table_verbatim() {
        for (url, id, expected) in SHARED_CASES {
            assert_eq!(derive_session(url, id), *expected, "case {url} / {id}");
        }
    }

    #[test]
    fn replaces_an_existing_session_param_exactly_once() {
        let out = derive_session("wss://r/?session=old", "proj-foo");
        let (_, query) = out.split_once('?').expect("query present");
        let sessions: Vec<&str> = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .filter(|(k, _)| *k == "session")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(sessions.len(), 1, "exactly one session param: {out}");
        assert_eq!(sessions[0], "old-proj-foo", "replaced, not appended: {out}");
    }

    #[test]
    fn keeps_unrelated_query_params() {
        let out = derive_session("wss://r/?token=t&session=s", "p");
        assert!(out.contains("token=t"), "{out}");
        assert_eq!(out.matches("session=").count(), 1, "{out}");
    }

    /// 不稳定的话，同一项目每次重连都在手机上多出一个 session。
    #[test]
    fn is_stable_for_the_same_input() {
        assert_eq!(
            derive_session("wss://r/?session=s", "p"),
            derive_session("wss://r/?session=s", "p")
        );
    }

    /// 两个项目共用一个 session 会让手机把两边的对话混在一起。
    #[test]
    fn differs_per_profile() {
        assert_ne!(
            derive_session("wss://r/?session=s", "a"),
            derive_session("wss://r/?session=s", "b")
        );
    }

    /// 同一个项目派生两次必须只多一个后缀——否则远端 app-server 报出一个
    /// 没人监听的 session，手机侧表现为「能看到项目但永远连不上」。
    #[test]
    fn deriving_twice_for_the_same_profile_is_a_noop() {
        let once = derive_session("wss://r/?session=base", "proj-foo");
        assert_eq!(derive_session(&once, "proj-foo"), once);
    }

    /// 但换一个项目仍要派出新 session：幂等不能退化成「忽略参数」。
    #[test]
    fn derivation_still_differs_for_another_profile() {
        let once = derive_session("wss://r/?session=base", "proj-foo");
        let other = derive_session(&once, "proj-bar");
        assert_ne!(other, once);
        assert!(other.ends_with("session=base-proj-foo-proj-bar"), "{other}");
    }

    /// 空值的 `session=` 等同于没有：必须退到 `remote` 而不是派生出 `-p`。
    #[test]
    fn an_empty_session_falls_back_to_the_default_seed() {
        assert_eq!(
            derive_session("wss://r/?session=", "p"),
            "wss://r/?session=remote-p"
        );
    }
}
