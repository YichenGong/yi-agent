//! .env 文件读写：解析 dotenv 格式，写入时保留分组注释。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

/// 解析 .env 文件，返回 key→value 映射。
/// 跳过注释行（以 # 开头）和空行。
pub fn read(path: &Path) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    if !path.exists() {
        return Ok(map);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read .env file: {}", path.display()))?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq_pos) = trimmed.find('=') {
            let key = trimmed[..eq_pos].trim().to_string();
            let value = trimmed[eq_pos + 1..].trim().to_string();
            // 去除可选的引号
            let value = strip_quotes(&value);
            if !key.is_empty() {
                map.insert(key, value);
            }
        }
    }
    Ok(map)
}

/// 去除值两端的引号（单引号或双引号）。
fn strip_quotes(s: &str) -> String {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        if (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
        {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// 将所有 14 个变量写入 .env 文件，带分组注释。
/// `current` 是当前已有的值，`updates` 是要覆盖的值。
pub fn write(
    path: &Path,
    current: &HashMap<String, String>,
    updates: &[(String, String)],
) -> Result<()> {
    use crate::config_meta::ALL_VARS;

    // 合并 updates 到 current
    let mut merged = current.clone();
    for (key, value) in updates {
        merged.insert(key.clone(), value.clone());
    }

    let mut output = String::new();
    let mut last_group = "";

    for var in ALL_VARS {
        // 分组注释
        if var.group != last_group {
            if !last_group.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!("# === {} ===\n", var.group));
            last_group = var.group;
        }

        let value = merged.get(var.key).map(|s| s.as_str()).unwrap_or("");
        output.push_str(&format!("{}={}\n", var.key, value));
    }

    std::fs::write(path, output)
        .with_context(|| format!("failed to write .env file: {}", path.display()))?;
    Ok(())
}

/// 对 secret 类型值做掩码：前4 + *** + 后4，不足12字符则全 ***。
pub fn mask(value: &str) -> String {
    if value.len() < 12 {
        return "***".to_string();
    }
    let chars: Vec<char> = value.chars().collect();
    let prefix: String = chars[..4].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{}***{}", prefix, suffix)
}

/// 判断一个值是否是掩码值（包含 ***）。
pub fn is_masked(value: &str) -> bool {
    value.contains("***")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_meta::ALL_VARS;
    use tempfile::NamedTempFile;

    #[test]
    fn read_empty_file_returns_empty_map() {
        let tmp = NamedTempFile::new().unwrap();
        let map = read(tmp.path()).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn read_nonexistent_file_returns_empty_map() {
        let map = read(Path::new("/nonexistent/path/.env")).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn read_parses_key_value() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "KEY=value\n").unwrap();
        let map = read(tmp.path()).unwrap();
        assert_eq!(map.get("KEY").unwrap(), "value");
    }

    #[test]
    fn read_skips_comments_and_empty_lines() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "# comment\n\nKEY=value\n# another\n").unwrap();
        let map = read(tmp.path()).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("KEY").unwrap(), "value");
    }

    #[test]
    fn read_strips_quotes() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "A=\"quoted\"\nB='single'\nC=plain\n").unwrap();
        let map = read(tmp.path()).unwrap();
        assert_eq!(map.get("A").unwrap(), "quoted");
        assert_eq!(map.get("B").unwrap(), "single");
        assert_eq!(map.get("C").unwrap(), "plain");
    }

    #[test]
    fn read_handles_values_with_equals() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "URL=https://example.com?a=b&c=d\n").unwrap();
        let map = read(tmp.path()).unwrap();
        assert_eq!(map.get("URL").unwrap(), "https://example.com?a=b&c=d");
    }

    #[test]
    fn write_creates_file_with_all_vars() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::fs::remove_file(&path).ok(); // 删除临时文件，让 write 创建

        write(&path, &HashMap::new(), &[]).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        for var in ALL_VARS {
            assert!(content.contains(var.key), "missing {} in output", var.key);
        }
        assert!(content.contains("# === Provider ==="));
        assert!(content.contains("# === Agent ==="));
    }

    #[test]
    fn write_merges_updates() {
        let tmp = NamedTempFile::new().unwrap();
        let mut current = HashMap::new();
        current.insert("YI_AGENT_MODEL".to_string(), "old-model".to_string());

        write(tmp.path(), &current, &[("YI_AGENT_MODEL".to_string(), "new-model".to_string())]).unwrap();
        let map = read(tmp.path()).unwrap();
        assert_eq!(map.get("YI_AGENT_MODEL").unwrap(), "new-model");
    }

    #[test]
    fn write_preserves_unupdated_values() {
        let tmp = NamedTempFile::new().unwrap();
        let mut current = HashMap::new();
        current.insert("YI_AGENT_MODEL".to_string(), "keep-this".to_string());

        write(tmp.path(), &current, &[("YI_AGENT_MAX_TURNS".to_string(), "50".to_string())]).unwrap();
        let map = read(tmp.path()).unwrap();
        assert_eq!(map.get("YI_AGENT_MODEL").unwrap(), "keep-this");
        assert_eq!(map.get("YI_AGENT_MAX_TURNS").unwrap(), "50");
    }

    #[test]
    fn mask_long_value() {
        let m = mask("sk-ant-api03-xxxxxxxxxxxx");
        assert_eq!(m, "sk-a***xxxx");
    }

    #[test]
    fn mask_short_value() {
        assert_eq!(mask("short"), "***");
    }

    #[test]
    fn mask_exact_12_chars() {
        let m = mask("123456789012");
        assert_eq!(m, "1234***9012");
    }

    #[test]
    fn mask_handles_multibyte_utf8() {
        // 9 bytes = 3 Chinese chars, byte len < 12 so returns "***"
        assert_eq!(mask("你好世"), "***");
        // 24 bytes = 8 Chinese chars, masks first/last 4 chars without panic
        let m = mask("你好世界你好世界");
        assert_eq!(m, "你好世界***你好世界");
    }

    #[test]
    fn is_masked_detects_mask() {
        assert!(is_masked("sk-a***xxxx"));
        assert!(!is_masked("sk-ant-real-key"));
    }
}
