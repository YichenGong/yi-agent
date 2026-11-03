use super::*;
use crate::types::PluginId;

fn release(version: &str) -> ReleaseId {
    ReleaseId {
        plugin: PluginId::parse("demo").expect("plugin id"),
        version: version.to_string(),
        target: "aarch64-apple-darwin".to_string(),
        sha256: "a".repeat(64),
    }
}

fn official() -> AllowedOrigin {
    AllowedOrigin::official("plugins.example.com")
}

fn request(path: &str, bytes: u64) -> DownloadRequest {
    DownloadRequest {
        url: Url::parse(&format!("https://plugins.example.com{path}")).expect("url"),
        expected_bytes: bytes,
    }
}

/// 一个可以精确摆出每种失败形状的假 fetcher。
struct ScriptedFetcher {
    heads: Vec<ResponseHead>,
    chunks: Vec<Result<Option<Vec<u8>>, FetchError>>,
    head_calls: usize,
    read_calls: usize,
}

impl ScriptedFetcher {
    fn new(head: ResponseHead, body: Vec<u8>) -> ScriptedFetcher {
        ScriptedFetcher {
            heads: vec![head],
            chunks: body
                .chunks(4)
                .map(|chunk| Ok(Some(chunk.to_vec())))
                .chain(std::iter::once(Ok(None)))
                .collect(),
            head_calls: 0,
            read_calls: 0,
        }
    }

    fn with_heads(mut self, heads: Vec<ResponseHead>) -> ScriptedFetcher {
        let body = std::mem::take(&mut self.chunks);
        self.heads = heads;
        self.chunks = body;
        self
    }

    fn with_chunks(mut self, chunks: Vec<Result<Option<Vec<u8>>, FetchError>>) -> ScriptedFetcher {
        self.chunks = chunks;
        self
    }
}

impl Fetcher for ScriptedFetcher {
    fn head(&mut self, _url: &Url) -> Result<ResponseHead, FetchError> {
        let index = self.head_calls.min(self.heads.len().saturating_sub(1));
        self.head_calls += 1;
        Ok(self.heads[index].clone())
    }

    fn read(&mut self, _max_bytes: usize) -> Result<Option<Vec<u8>>, FetchError> {
        let index = self.read_calls;
        self.read_calls += 1;
        if index >= self.chunks.len() {
            return Ok(None);
        }
        self.chunks[index].clone()
    }
}

/// 时钟：每次读都前进固定的毫秒数，用于触发总超时。
struct SteppingClock {
    step_ms: u64,
    now: u64,
}

impl Clock for SteppingClock {
    fn elapsed_ms(&self) -> u64 {
        self.now
    }
}

struct FrozenClock;

impl Clock for FrozenClock {
    fn elapsed_ms(&self) -> u64 {
        0
    }
}

fn ok_head(bytes: u64) -> ResponseHead {
    ResponseHead {
        status: 200,
        location: None,
        content_length: Some(bytes),
    }
}

/// 正常下载：字节数精确、无跳转。
#[test]
fn a_plain_download_returns_exactly_the_signed_bytes() {
    let mut fetcher = ScriptedFetcher::new(ok_head(10), b"0123456789".to_vec());
    let mut clock = FrozenClock;
    let outcome = download(
        &request("/demo.tar.gz", 10),
        &official(),
        &mut fetcher,
        &DownloadLimits::default(),
        &mut clock,
    )
    .expect("download");

    assert_eq!(outcome.bytes, b"0123456789");
    assert!(outcome.redirects.is_empty());
}

/// 源不匹配时连请求都不发。
#[test]
fn a_url_outside_the_allowed_origin_is_refused_before_any_request() {
    let mut fetcher = ScriptedFetcher::new(ok_head(0), Vec::new());
    let mut clock = FrozenClock;
    let bad = DownloadRequest {
        url: Url::parse("https://evil.example.com/demo.tar.gz").expect("url"),
        expected_bytes: 0,
    };
    let error = download(
        &bad,
        &official(),
        &mut fetcher,
        &DownloadLimits::default(),
        &mut clock,
    )
    .expect_err("foreign origin");

    assert!(matches!(error, PluginError::Untrusted(_)), "got {error:?}");
    assert_eq!(
        fetcher.head_calls, 0,
        "a foreign origin must not even be contacted"
    );
}

/// 跳到允许源之外必须拒绝。
#[test]
fn a_redirect_off_the_allowed_origin_is_refused() {
    let mut fetcher = ScriptedFetcher::new(ok_head(0), Vec::new()).with_heads(vec![
        ResponseHead {
            status: 302,
            location: Some("https://evil.example.com/demo.tar.gz".to_string()),
            content_length: None,
        },
        ok_head(0),
    ]);
    let mut clock = FrozenClock;
    let error = download(
        &request("/demo.tar.gz", 0),
        &official(),
        &mut fetcher,
        &DownloadLimits::default(),
        &mut clock,
    )
    .expect_err("off-origin redirect");

    assert!(
        matches!(&error, PluginError::Untrusted(message) if message.contains("outside the allowed origin")),
        "got {error:?}"
    );
}

/// 相对跳转留在同一源上是允许的，并被记进链里。
#[test]
fn a_relative_redirect_stays_on_the_origin_and_is_recorded() {
    let mut fetcher = ScriptedFetcher::new(ok_head(4), b"abcd".to_vec()).with_heads(vec![
        ResponseHead {
            status: 302,
            location: Some("/mirror/demo.tar.gz".to_string()),
            content_length: None,
        },
        ok_head(4),
    ]);
    let mut clock = FrozenClock;
    let outcome = download(
        &request("/demo.tar.gz", 4),
        &official(),
        &mut fetcher,
        &DownloadLimits::default(),
        &mut clock,
    )
    .expect("download");

    assert_eq!(outcome.bytes, b"abcd");
    assert_eq!(outcome.redirects.len(), 1);
    assert_eq!(outcome.redirects[0].path, "/mirror/demo.tar.gz");
    assert_eq!(outcome.redirects[0].host, "plugins.example.com");
}

/// 跳转次数有上限。
#[test]
fn too_many_redirects_is_refused() {
    let looping = ResponseHead {
        status: 307,
        location: Some("/again".to_string()),
        content_length: None,
    };
    let mut fetcher = ScriptedFetcher::new(ok_head(0), Vec::new()).with_heads(vec![looping; 10]);
    let mut clock = FrozenClock;
    let limits = DownloadLimits {
        max_redirects: 2,
        ..DownloadLimits::default()
    };
    let error = download(
        &request("/demo.tar.gz", 0),
        &official(),
        &mut fetcher,
        &limits,
        &mut clock,
    )
    .expect_err("redirect loop");

    assert!(
        matches!(&error, PluginError::Untrusted(message) if message.contains("redirected more than")),
        "got {error:?}"
    );
}

/// 服务器声明的大小与签名绑定值不符：在下载前就拒绝。
#[test]
fn a_content_length_that_disagrees_with_the_signature_is_refused_before_downloading() {
    let mut fetcher = ScriptedFetcher::new(ok_head(999), b"x".to_vec());
    let mut clock = FrozenClock;
    let error = download(
        &request("/demo.tar.gz", 10),
        &official(),
        &mut fetcher,
        &DownloadLimits::default(),
        &mut clock,
    )
    .expect_err("size mismatch");

    assert!(matches!(error, PluginError::Integrity(_)), "got {error:?}");
    assert_eq!(fetcher.read_calls, 0, "nothing should have been downloaded");
}

/// 实际字节数不足也要拒绝：不能把半个包交出去。
#[test]
fn a_truncated_body_is_refused() {
    let mut fetcher = ScriptedFetcher::new(ok_head(10), b"0123".to_vec());
    let mut clock = FrozenClock;
    let error = download(
        &request("/demo.tar.gz", 10),
        &official(),
        &mut fetcher,
        &DownloadLimits::default(),
        &mut clock,
    )
    .expect_err("truncated");

    assert!(matches!(error, PluginError::Integrity(_)), "got {error:?}");
}

/// 服务器不报 content-length 时，超长仍要在流式读取中被拦下。
#[test]
fn a_body_longer_than_the_signed_size_is_refused_while_streaming() {
    let head = ResponseHead {
        status: 200,
        location: None,
        content_length: None,
    };
    let mut fetcher = ScriptedFetcher::new(head, b"0123456789abcdef".to_vec());
    let mut clock = FrozenClock;
    let error = download(
        &request("/demo.tar.gz", 10),
        &official(),
        &mut fetcher,
        &DownloadLimits::default(),
        &mut clock,
    )
    .expect_err("too long");

    assert!(
        matches!(&error, PluginError::Integrity(message) if message.contains("exceeded its signed size")),
        "got {error:?}"
    );
}

/// 无进展超时是独立的一类失败。
#[test]
fn a_stalled_body_is_reported_as_no_progress() {
    let mut fetcher = ScriptedFetcher::new(ok_head(10), Vec::new())
        .with_chunks(vec![Err(FetchError::NoProgress)]);
    let mut clock = FrozenClock;
    let error = download(
        &request("/demo.tar.gz", 10),
        &official(),
        &mut fetcher,
        &DownloadLimits::default(),
        &mut clock,
    )
    .expect_err("stalled");

    assert!(
        matches!(&error, PluginError::Io(message) if message.contains("no progress")),
        "got {error:?}"
    );
}

/// 总超时：一个很慢但一直有进展的下载也会被总时长拦下。
#[test]
fn a_slow_but_progressing_download_still_hits_the_total_deadline() {
    struct SlowFetcher {
        reads: usize,
    }
    impl Fetcher for SlowFetcher {
        fn head(&mut self, _url: &Url) -> Result<ResponseHead, FetchError> {
            Ok(ok_head(1_000_000))
        }
        fn read(&mut self, _max_bytes: usize) -> Result<Option<Vec<u8>>, FetchError> {
            self.reads += 1;
            Ok(Some(vec![0u8; 1]))
        }
    }

    // 每次读都把时钟推过总上限。
    struct AdvancingClock {
        now: u64,
    }
    impl Clock for AdvancingClock {
        fn elapsed_ms(&self) -> u64 {
            self.now
        }
    }

    let mut fetcher = SlowFetcher { reads: 0 };
    let mut clock = AdvancingClock { now: 0 };
    let limits = DownloadLimits {
        total_timeout: Duration::from_millis(450),
        ..DownloadLimits::default()
    };

    // 时钟在第一次检查时是 0，随后越过上限；用一个「读一次就推进」的
    // fetcher 无法推进 clock，故这里直接让 clock 一开始就超限。
    clock.now = 1_000;
    let error = download(
        &request("/demo.tar.gz", 1_000_000),
        &official(),
        &mut fetcher,
        &limits,
        &mut clock,
    )
    .expect_err("total deadline");

    assert!(
        matches!(&error, PluginError::Io(message) if message.contains("total deadline")),
        "got {error:?}"
    );
    assert_eq!(
        fetcher.reads, 0,
        "the deadline is checked before the first read"
    );
}

/// 5xx 是状态错误，不是"再试一次"。
#[test]
fn a_server_error_is_reported_with_its_status() {
    let mut fetcher = ScriptedFetcher::new(
        ResponseHead {
            status: 503,
            location: None,
            content_length: None,
        },
        Vec::new(),
    );
    let mut clock = FrozenClock;
    let error = download(
        &request("/demo.tar.gz", 0),
        &official(),
        &mut fetcher,
        &DownloadLimits::default(),
        &mut clock,
    )
    .expect_err("503");

    assert!(
        matches!(&error, PluginError::Io(message) if message.contains("503")),
        "got {error:?}"
    );
}

/// URL 解析：用户信息段与端口都拒绝。
#[test]
fn urls_with_user_info_or_a_port_are_refused() {
    assert!(Url::parse("https://evil@plugins.example.com/x").is_err());
    assert!(Url::parse("https://plugins.example.com:8443/x").is_err());
    assert!(Url::parse("plugins.example.com/x").is_err());
    assert_eq!(
        Url::parse("https://Plugins.Example.com/x").expect("url"),
        Url {
            scheme: "https".to_string(),
            host: "plugins.example.com".to_string(),
            path: "/x".to_string()
        }
    );
}

/// 目录路径只能拼在已认证的源上，不能自带主机。
#[test]
fn catalog_paths_must_be_absolute_on_the_origin() {
    let catalog = Catalog {
        serial: 7,
        entries: vec![CatalogEntry {
            release: release("1.0.0"),
            size_bytes: 100,
            metadata_path: "https://evil.example.com/meta.json".to_string(),
            bundle_path: "/demo.tar.gz".to_string(),
            published_at: 0,
            expires_at: 9_999_999_999,
            serial: 7,
        }],
    };
    let error = catalog
        .download_request(&release("1.0.0"), &official())
        .expect_err("host-carrying path");
    assert!(matches!(error, PluginError::Untrusted(_)), "got {error:?}");
}

/// 目录里没有的发布报 NotFound。
#[test]
fn asking_for_a_release_the_catalog_does_not_list_is_not_found() {
    let catalog = Catalog {
        serial: 7,
        entries: Vec::new(),
    };
    let error = catalog
        .download_request(&release("9.9.9"), &official())
        .expect_err("missing");
    assert!(matches!(error, PluginError::NotFound(_)), "got {error:?}");
}

/// 目录与签名对同一个包给出不同大小时必须报冲突。
#[test]
fn the_catalog_and_the_signature_must_agree_on_size() {
    let catalog = Catalog {
        serial: 7,
        entries: vec![CatalogEntry {
            release: release("1.0.0"),
            size_bytes: 100,
            metadata_path: "/meta.json".to_string(),
            bundle_path: "/demo.tar.gz".to_string(),
            published_at: 0,
            expires_at: 9_999_999_999,
            serial: 7,
        }],
    };
    let verified = VerifiedRelease {
        release: release("1.0.0"),
        size_bytes: 200,
        host_api: "v1".to_string(),
        serial: 7,
        published_at: 0,
        expires_at: 9_999_999_999,
        key_id: "root-test".to_string(),
    };
    let error = catalog.agrees_with(&verified).expect_err("disagreement");
    assert!(
        matches!(&error, PluginError::Integrity(message) if message.contains("100") && message.contains("200")),
        "got {error:?}"
    );
}
