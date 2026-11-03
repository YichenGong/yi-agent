//! 下载远端二进制并校验。
//!
//! `Fetcher` 是注入点：生产用 `HttpFetcher`，测试用假实现，这样「校验失败不
//! 落盘」这类关键分支不需要真的下载 15MB，单测也完全不联网。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 发布渠道前缀。实际 URL 是 `<BASE>/v<version>/yi-agent-<target>.tar.gz`。
pub const RELEASE_BASE: &str = "https://github.com/gongyichen/yi-agent/releases/download";

/// 取一个 URL 的全部字节。只用于小文件（tar.gz 与它的 `.sha256`）。
pub trait Fetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// 生产实现。
///
/// `blocking` 是刻意的：安装流程整体是阻塞的（还要串行跑 ssh/scp），调用方在
/// Tauri 命令里自己 `spawn_blocking`；这里再套一层 async 只会让测试变复杂。
pub struct HttpFetcher;

impl Fetcher for HttpFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        let response = reqwest::blocking::get(url).map_err(|e| format!("{url}: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("{url}: HTTP {}", response.status()));
        }
        response
            .bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(|e| format!("{url}: {e}"))
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// `.sha256` 文件的第一段十六进制。
///
/// 真实文件是 `shasum -a 256` 的产物，形如 `<hex>  yi-agent-<target>.tar.gz`，
/// 可能还有尾随空白，所以只取第一个空白分隔的 token。
fn parse_checksum(raw: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(raw);
    let token = text
        .split_whitespace()
        .next()
        .ok_or_else(|| "the .sha256 file is empty".to_string())?;
    if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("the .sha256 file is malformed: `{token}`"));
    }
    Ok(token.to_ascii_lowercase())
}

/// 下载 `yi-agent-<target>.tar.gz` 到 `dest`，核对同名 `.sha256`。
///
/// **先校验再落盘**：校验不过时 `dest` 不存在。否则调用方会把一个坏文件 scp 到
/// 远端，在远端解压失败才暴露问题——那时错误信息已经离真正的原因很远了。
pub fn download_verified(
    fetcher: &dyn Fetcher,
    base: &str,
    target: &str,
    version: &str,
    dest: &Path,
) -> Result<PathBuf, String> {
    let tar_url = format!("{base}/v{version}/yi-agent-{target}.tar.gz");
    let sha_url = format!("{tar_url}.sha256");

    let expected = parse_checksum(&fetcher.get(&sha_url)?)?;
    let bytes = fetcher.get(&tar_url)?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(format!(
            "checksum mismatch for {tar_url}: expected {expected}, got {actual}"
        ));
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // 唯一临时名 + rename：并发安装者不得共用一个临时路径
    // （与 `remote/config.rs` 同一考量）。
    let tmp = dest.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
    Ok(dest.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeFetcher {
        responses: RefCell<Vec<(String, Vec<u8>)>>,
    }

    impl FakeFetcher {
        fn new(pairs: &[(&str, &[u8])]) -> Self {
            Self {
                responses: RefCell::new(
                    pairs
                        .iter()
                        .map(|(url, body)| (url.to_string(), body.to_vec()))
                        .collect(),
                ),
            }
        }
    }

    impl Fetcher for FakeFetcher {
        fn get(&self, url: &str) -> Result<Vec<u8>, String> {
            self.responses
                .borrow()
                .iter()
                .find(|(candidate, _)| candidate == url)
                .map(|(_, body)| body.clone())
                .ok_or_else(|| format!("404 {url}"))
        }
    }

    const TARBALL: &[u8] = b"pretend tarball bytes";
    const TARGET: &str = "x86_64-unknown-linux-gnu";
    const BASE: &str = "https://example.test/v1";

    fn urls() -> (String, String) {
        let tar = format!("{BASE}/v0.1.10/yi-agent-{TARGET}.tar.gz");
        let sha = format!("{tar}.sha256");
        (tar, sha)
    }

    #[test]
    fn sha256_matches_a_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_matching_checksum_writes_the_artifact() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tar_url, sha_url) = urls();
        let body = format!("  {}\n", sha256_hex(TARBALL));
        let fetcher = FakeFetcher::new(&[(&tar_url, TARBALL), (&sha_url, body.as_bytes())]);
        let dest = dir.path().join("out.tar.gz");

        let written = download_verified(&fetcher, BASE, TARGET, "0.1.10", &dest).unwrap();

        assert_eq!(written, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), TARBALL);
    }

    /// 真实的 `.sha256` 是 `shasum -a 256` 的产物，带文件名与换行。
    #[test]
    fn checksum_with_trailing_filename_and_newline_still_parses() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tar_url, sha_url) = urls();
        let body = format!("{}  yi-agent-{TARGET}.tar.gz\n", sha256_hex(TARBALL));
        let fetcher = FakeFetcher::new(&[(&tar_url, TARBALL), (&sha_url, body.as_bytes())]);

        download_verified(
            &fetcher,
            BASE,
            TARGET,
            "0.1.10",
            &dir.path().join("o.tar.gz"),
        )
        .unwrap();
    }

    #[test]
    fn a_mismatched_checksum_never_writes_the_artifact() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tar_url, sha_url) = urls();
        let zeroes = "0".repeat(64);
        let fetcher = FakeFetcher::new(&[(&tar_url, TARBALL), (&sha_url, zeroes.as_bytes())]);
        let dest = dir.path().join("out.tar.gz");

        assert!(download_verified(&fetcher, BASE, TARGET, "0.1.10", &dest).is_err());
        assert!(
            !dest.exists(),
            "a bad checksum must not leave the file behind"
        );
    }

    #[test]
    fn a_missing_artifact_surfaces_the_http_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let fetcher = FakeFetcher::new(&[]);
        let err = download_verified(
            &fetcher,
            BASE,
            TARGET,
            "0.1.10",
            &dir.path().join("o.tar.gz"),
        )
        .unwrap_err();
        assert!(err.contains("404"), "{err}");
    }

    #[test]
    fn a_malformed_checksum_file_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tar_url, sha_url) = urls();
        let fetcher = FakeFetcher::new(&[(&tar_url, TARBALL), (&sha_url, b"not-a-checksum\n")]);
        let err = download_verified(
            &fetcher,
            BASE,
            TARGET,
            "0.1.10",
            &dir.path().join("o.tar.gz"),
        )
        .unwrap_err();
        assert!(err.contains("malformed"), "{err}");
    }

    #[test]
    fn the_version_is_part_of_the_url() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tar_url, sha_url) = urls();
        let body = sha256_hex(TARBALL);
        // 请求的是 0.1.10，但假源里只有 0.1.9 的产物：必须失败，
        // 证明版本号真的进了 URL（否则升级时会静默复用旧缓存）。
        let old = format!("{BASE}/v0.1.9/yi-agent-{TARGET}.tar.gz");
        let fetcher = FakeFetcher::new(&[
            (&old, TARBALL),
            (&sha_url, body.as_bytes()),
            (&tar_url, TARBALL),
        ]);
        assert!(download_verified(&fetcher, BASE, TARGET, "0.1.10", &dir.path().join("o")).is_ok());
        let fetcher =
            FakeFetcher::new(&[(&old, TARBALL), (&format!("{old}.sha256"), body.as_bytes())]);
        assert!(
            download_verified(&fetcher, BASE, TARGET, "0.1.10", &dir.path().join("p")).is_err(),
            "a different version must not be served from the same path"
        );
    }
}
