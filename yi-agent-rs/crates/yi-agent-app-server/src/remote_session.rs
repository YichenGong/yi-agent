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

/// 手机侧要连的地址：**`/ws` 端点，session 与远端进程相同**。
///
/// 桌面派生出的 `derive_session` 结果指向中继的**电脑侧**端点（`/connect`，
/// 反向 pairer：电脑主动出站注册）。手机不能连它——实测 `/connect?session=x`
/// 能握手（`OPEN`）但收不到任何帧，因为电脑侧的 socket 是「注册」用的，不是
/// 「转发」用的；手机必须连 `/ws?session=x`（spec §4.6、§8.9）。
///
/// 两者**session 值必须一致**，这正是本函数只改路径、不改 session 的原因：
/// 改 session 会让手机连到一个没人监听的 channel，而两边日志都正常。
pub fn client_endpoint(relay_url: &str, profile_id: &str) -> String {
    let derived = derive_session(relay_url, profile_id);
    replace_endpoint(&derived, "/ws")
}

/// 把 URL 的路径换成 `endpoint`，其余（scheme/host/query）原样保留。
///
/// 手写而非引 `url` crate：这里只处理两种形态（带 `://` 的完整 URL、以及
/// 拿来兜底的裸串），引一个依赖只为改一段路径不值当。解析不出来就原样返回——
/// 派生结果本身已经可用，不该因为路径改写失败而让手机拿不到 session。
fn replace_endpoint(url: &str, endpoint: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let rest = &url[authority_start..];
    // authority 到下一个 `/`（或 query 的 `?`）为止。
    let path_start = rest.find(['/', '?']).map(|i| authority_start + i);
    match path_start {
        // 只有 `?`：没有路径，直接插端点。
        Some(i) if url[i..].starts_with('?') => {
            format!("{}{}{}", &url[..i], endpoint, &url[i..])
        }
        Some(i) => {
            let tail_start = url[i..].find(['?']).map(|j| i + j).unwrap_or(url.len());
            format!("{}{}{}", &url[..i], endpoint, &url[tail_start..])
        }
        None => format!("{url}{endpoint}"),
    }
}

#[cfg(test)]
mod client_endpoint_tests {
    use super::client_endpoint;

    /// 手机侧**必须**连 `/ws`：`/connect` 是电脑侧注册端点，握手能成但收不到帧
    /// （实测：`OPEN` 后 20s 无任何消息）。
    #[test]
    fn the_phone_gets_the_ws_endpoint() {
        let url = client_endpoint("wss://relay.example/connect?session=base", "proj-a");
        assert_eq!(url, "wss://relay.example/ws?session=base-proj-a");
    }

    /// session 值必须与 `derive_session` 逐字相同，否则手机连到没人监听的 channel。
    #[test]
    fn the_session_value_survives_the_endpoint_rewrite() {
        let base = "wss://relay.example/connect?session=base";
        let derived = super::derive_session(base, "proj-a");
        let client = client_endpoint(base, "proj-a");
        let d = derived.split("session=").nth(1).unwrap();
        let c = client.split("session=").nth(1).unwrap();
        assert_eq!(d, c);
    }

    /// 端点已是 `/ws` 时不得重复改写，也不得二次派生 session。
    #[test]
    fn an_already_ws_endpoint_is_stable() {
        let url = client_endpoint("wss://relay.example/ws?session=base-proj-a", "proj-a");
        assert_eq!(url, "wss://relay.example/ws?session=base-proj-a");
    }

    /// 多段路径要整段替换，不能把 `/a/b` 变成 `/ws/b`。
    #[test]
    fn a_multi_segment_path_is_replaced_wholesale() {
        let url = client_endpoint("wss://relay.example/a/b?session=s", "p");
        assert_eq!(url, "wss://relay.example/ws?session=s-p");
    }

    /// 没有路径时补上端点。
    #[test]
    fn a_bare_host_gets_the_endpoint_appended() {
        let url = client_endpoint("wss://relay.example?session=s", "p");
        assert_eq!(url, "wss://relay.example/ws?session=s-p");
    }
}

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
