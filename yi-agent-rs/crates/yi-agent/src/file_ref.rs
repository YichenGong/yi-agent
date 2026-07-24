//! @path 文件引用：将用户输入中的 @path 替换为文件内容。

use std::path::Path;

/// 最大行数
const MAX_LINES: usize = 5000;
/// 最大字节数
const MAX_BYTES: usize = 50_000;

/// @path 引用解析错误
#[derive(Debug, Clone)]
pub enum FileRefError {
    NotFound(String),
    IsDirectory(String),
    OutsideWorkdir(String),
    TooLarge { path: String, lines: usize },
    ReadFailed(String),
}

impl std::fmt::Display for FileRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileRefError::NotFound(p) => write!(f, "文件不存在: {p}"),
            FileRefError::IsDirectory(p) => write!(f, "路径是目录: {p}"),
            FileRefError::OutsideWorkdir(p) => write!(f, "路径超出工作目录范围: {p}"),
            FileRefError::TooLarge { path, lines } => {
                write!(
                    f,
                    "文件过大({lines} 行)，请让 agent 用 read 工具分段读取: {path}"
                )
            }
            FileRefError::ReadFailed(msg) => write!(f, "读取文件失败: {msg}"),
        }
    }
}

impl std::error::Error for FileRefError {}

/// 在用户输入文本中查找 @path 引用，读取文件内容并替换。
pub fn expand_file_refs(text: &str, workdir: &Path) -> Result<String, FileRefError> {
    let mut result = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        if ch == '@' && (i == 0 || chars[i - 1].is_whitespace()) {
            // Check for quoted path @"..."
            if i + 1 < chars.len() && chars[i + 1] == '"' {
                let start = i + 2;
                let mut end = start;
                while end < chars.len() && chars[end] != '"' {
                    end += 1;
                }
                if end < chars.len() {
                    let path_str: String = chars[start..end].iter().collect();
                    let content = read_file_ref(&path_str, workdir)?;
                    result.push_str(&format_file_ref(&path_str, &content));
                    i = end + 1;
                    continue;
                }
            }

            // Unquoted path: read until whitespace
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && !chars[end].is_whitespace() {
                end += 1;
            }
            if end > start {
                let path_str: String = chars[start..end].iter().collect();
                let content = read_file_ref(&path_str, workdir)?;
                result.push_str(&format_file_ref(&path_str, &content));
                i = end;
                continue;
            }
        }

        result.push(ch);
        i += 1;
    }

    Ok(result)
}

fn read_file_ref(path_str: &str, workdir: &Path) -> Result<String, FileRefError> {
    let path = Path::new(path_str);

    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workdir.join(path)
    };

    let canonical = resolved
        .canonicalize()
        .map_err(|_| FileRefError::NotFound(path_str.to_string()))?;

    let canonical_workdir = workdir
        .canonicalize()
        .map_err(|e| FileRefError::ReadFailed(e.to_string()))?;

    if !canonical.starts_with(&canonical_workdir) {
        return Err(FileRefError::OutsideWorkdir(path_str.to_string()));
    }

    if canonical.is_dir() {
        return Err(FileRefError::IsDirectory(path_str.to_string()));
    }

    let content =
        std::fs::read_to_string(&canonical).map_err(|e| FileRefError::ReadFailed(e.to_string()))?;

    let lines = content.lines().count();
    let bytes = content.len();

    if lines > MAX_LINES || bytes > MAX_BYTES {
        return Err(FileRefError::TooLarge {
            path: path_str.to_string(),
            lines,
        });
    }

    let mut numbered = String::new();
    for (idx, line) in content.lines().enumerate() {
        numbered.push_str(&format!("{:>6}\t{}\n", idx + 1, line));
    }
    Ok(numbered)
}

fn format_file_ref(path_str: &str, content: &str) -> String {
    format!("--- @{path_str} ---\n{content}--- end ---\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_temp_workdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "yi-agent-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_at_sign_returns_text_unchanged() {
        let workdir = make_temp_workdir();
        let result = expand_file_refs("hello world", &workdir).unwrap();
        assert_eq!(result, "hello world");
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn email_not_treated_as_ref() {
        let workdir = make_temp_workdir();
        let result = expand_file_refs("contact user@host.com please", &workdir).unwrap();
        assert_eq!(result, "contact user@host.com please");
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn expand_simple_file_ref() {
        let workdir = make_temp_workdir();
        let filepath = workdir.join("test.txt");
        std::fs::write(&filepath, "line1\nline2\nline3\n").unwrap();

        let result = expand_file_refs("check @test.txt please", &workdir).unwrap();
        assert!(result.contains("check"));
        assert!(result.contains("--- @test.txt ---"));
        assert!(result.contains("line1"));
        assert!(result.contains("line3"));
        assert!(result.contains("--- end ---"));
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn expand_quoted_path_with_spaces() {
        let workdir = make_temp_workdir();
        let filepath = workdir.join("my file.txt");
        std::fs::write(&filepath, "content here\n").unwrap();

        let result = expand_file_refs("read @\"my file.txt\" now", &workdir).unwrap();
        assert!(result.contains("content here"));
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn file_not_found_returns_error() {
        let workdir = make_temp_workdir();
        let result = expand_file_refs("check @nonexistent.txt", &workdir);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FileRefError::NotFound(_)));
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn directory_ref_returns_error() {
        let workdir = make_temp_workdir();
        let subdir = workdir.join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        let result = expand_file_refs("check @subdir", &workdir);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FileRefError::IsDirectory(_)));
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn absolute_path_outside_workdir_rejected() {
        let workdir = make_temp_workdir();
        let result = expand_file_refs("check @/etc/hosts", &workdir);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            FileRefError::OutsideWorkdir(_)
        ));
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn large_file_rejected() {
        let workdir = make_temp_workdir();
        let filepath = workdir.join("big.txt");
        let content = "x\n".repeat(6000);
        std::fs::write(&filepath, &content).unwrap();

        let result = expand_file_refs("check @big.txt", &workdir);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FileRefError::TooLarge { .. }));
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn multiple_refs_in_one_input() {
        let workdir = make_temp_workdir();
        std::fs::write(workdir.join("a.txt"), "AAA\n").unwrap();
        std::fs::write(workdir.join("b.txt"), "BBB\n").unwrap();

        let result = expand_file_refs("see @a.txt and @b.txt", &workdir).unwrap();
        assert!(result.contains("AAA"));
        assert!(result.contains("BBB"));
        std::fs::remove_dir_all(&workdir).ok();
    }
}
