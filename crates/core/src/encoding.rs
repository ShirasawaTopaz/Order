use std::{
    fs,
    io::{self, Write},
    path::Path,
};

const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// 文本编码检查结果。
///
/// 该结构用于把“已自动兼容处理”的情况显式返回给上层，
/// 避免出现“文件可读但发生了隐式修复”却没有任何提示的静默行为。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextEncodingReport {
    /// 读取时是否检测到 UTF-8 BOM 头。
    pub had_utf8_bom: bool,
    /// 文本是否发生了行尾标准化（CRLF/CR -> LF）。
    pub normalized_line_endings: bool,
}

impl TextEncodingReport {
    /// 将编码检查结果转换为可直接展示的告警文本。
    pub fn warnings_for(&self, path: &Path) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.had_utf8_bom {
            warnings.push(format!(
                "检测到 UTF-8 BOM，已按兼容模式读取并移除：{}",
                path.display()
            ));
        }
        if self.normalized_line_endings {
            warnings.push(format!(
                "检测到 CRLF/CR 行尾，已统一转换为 LF：{}",
                path.display()
            ));
        }
        warnings
    }

    pub fn has_warning(&self) -> bool {
        self.had_utf8_bom || self.normalized_line_endings
    }
}

/// 读取 UTF-8 文本并返回编码检查结果。
///
/// 设计要点：
/// - 对 UTF-8 BOM 做兼容读取，避免历史文件因 BOM 无法解析；
/// - 将 CRLF/CR 统一归一为 LF，保证跨终端读写一致性；
/// - 对非 UTF-8 输入显式报错，避免出现不可控乱码。
pub fn read_utf8_text_with_report(path: &Path) -> io::Result<(String, TextEncodingReport)> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok((String::new(), TextEncodingReport::default()));
    }

    let mut report = TextEncodingReport::default();
    let text_bytes = if bytes.starts_with(&UTF8_BOM) {
        report.had_utf8_bom = true;
        &bytes[UTF8_BOM.len()..]
    } else {
        &bytes[..]
    };

    let decoded = std::str::from_utf8(text_bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("文件不是 UTF-8 编码: {} ({error})", path.display()),
        )
    })?;

    let normalized = normalize_to_lf(decoded);
    report.normalized_line_endings = normalized != decoded;
    Ok((normalized, report))
}

/// 以 UTF-8（无 BOM）写入文本，并返回编码检查结果。
///
/// 写入前会先进行基础编码校验，拦截疑似已经损坏的数据，
/// 避免将乱码继续持久化到日志、历史或配置文件中。
pub fn write_utf8_text_with_report(path: &Path, content: &str) -> io::Result<TextEncodingReport> {
    validate_text_for_utf8_write(path, content)?;
    let normalized = normalize_to_lf(content);
    let report = TextEncodingReport {
        had_utf8_bom: false,
        normalized_line_endings: normalized != content,
    };
    fs::write(path, normalized.as_bytes())?;
    Ok(report)
}

/// 追加一行 UTF-8 JSON Line 日志。
///
/// 该函数显式禁止输入中包含原始换行符，防止破坏一行一条事件的解析约定。
pub fn append_utf8_json_line(path: &Path, line: &str) -> io::Result<()> {
    validate_text_for_utf8_write(path, line)?;
    let normalized = normalize_to_lf(line);
    if normalized.contains('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("JSON Line 内容包含换行符，无法安全写入: {}", path.display()),
        ));
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(normalized.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// 将行尾统一转换为 LF。
fn normalize_to_lf(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }

    let normalized_crlf = text.replace("\r\n", "\n");
    normalized_crlf.replace('\r', "\n")
}

/// 写入前的编码完整性检查。
fn validate_text_for_utf8_write(path: &Path, content: &str) -> io::Result<()> {
    // U+FFFD 通常来自“非法字节被容错替换”，继续写入会把乱码永久化。
    if content.contains('\u{FFFD}') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "待写入文本包含 U+FFFD（疑似编码已损坏），拒绝写入: {}",
                path.display()
            ),
        ));
    }

    // U+FEFF 作为字符内容出现时通常是误带 BOM，这里直接拦截避免污染文件头。
    if content.contains('\u{FEFF}') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "待写入文本包含 U+FEFF（BOM 字符），拒绝写入: {}",
                path.display()
            ),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_file_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir()
            .join(format!("order-encoding-test-{nonce}"))
            .join("sample.txt")
    }

    #[test]
    fn read_utf8_text_should_strip_bom_and_normalize_line_endings() {
        let path = temp_file_path();
        fs::create_dir_all(path.parent().expect("path should have parent"))
            .expect("parent directory should be created");

        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice("a\r\nb\r".as_bytes());
        fs::write(&path, bytes).expect("fixture file should be written");

        let (text, report) =
            read_utf8_text_with_report(&path).expect("utf8 text should be decoded");
        assert_eq!(text, "a\nb\n");
        assert!(report.had_utf8_bom);
        assert!(report.normalized_line_endings);
    }

    #[test]
    fn write_utf8_text_should_reject_replacement_char() {
        let path = temp_file_path();
        fs::create_dir_all(path.parent().expect("path should have parent"))
            .expect("parent directory should be created");

        let error = write_utf8_text_with_report(&path, "坏\u{FFFD}字")
            .expect_err("replacement char should be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn write_utf8_text_should_normalize_to_lf_without_bom() {
        let path = temp_file_path();
        fs::create_dir_all(path.parent().expect("path should have parent"))
            .expect("parent directory should be created");

        let report = write_utf8_text_with_report(&path, "x\r\ny\r")
            .expect("text should be written with normalized line endings");
        assert!(report.normalized_line_endings);

        let bytes = fs::read(&path).expect("written file should be readable");
        assert!(!bytes.starts_with(&UTF8_BOM));
        assert_eq!(
            String::from_utf8(bytes).expect("written bytes should be utf8"),
            "x\ny\n"
        );
    }

    #[test]
    fn append_utf8_json_line_should_reject_multiline_payload() {
        let path = temp_file_path();
        fs::create_dir_all(path.parent().expect("path should have parent"))
            .expect("parent directory should be created");

        let error = append_utf8_json_line(&path, "{\"a\":1}\n{\"b\":2}")
            .expect_err("multiline json line should be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
