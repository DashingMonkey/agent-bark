use std::io::Read;
use std::path::Path;

/// 首次读取只取本轮观测到的长度，并把游标留在最后一个完整行之后。
/// 半行（包括只写了一半的中文字符）留给增量读取，不能把 EOF 当作已消费位置。
pub(crate) fn read_bootstrap(path: &Path, observed_len: u64) -> std::io::Result<(String, u64)> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(observed_len).read_to_end(&mut bytes)?;
    let end = bytes.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    bytes.truncate(end);
    let text = String::from_utf8(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok((text, end as u64))
}

/// Keep complete records from the tail without slicing inside a UTF-8 character.
pub(crate) fn bootstrap_sample(full: &str, limit: usize) -> &str {
    let skip = full.len().saturating_sub(limit);
    if skip == 0 {
        return full;
    }
    let start = full.as_bytes()[skip..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map(|p| skip + p + 1)
        .unwrap_or(full.len());
    &full[start..]
}

/// Return complete lines and bytes read from this chunk, including its buffered tail.
pub(crate) fn split_complete_lines(chunk: &str, pending: &mut String) -> (Vec<String>, usize) {
    pending.push_str(chunk);
    let split = pending.rfind('\n').map(|p| p + 1).unwrap_or(0);
    let complete: String = pending.drain(..split).collect();
    let lines = complete.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect();
    (lines, chunk.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_stays_within_snapshot_and_leaves_partial_record_unconsumed() {
        let path = std::env::temp_dir().join(format!("bark-jsonl-{}", uuid::Uuid::new_v4()));
        let first = "{\"old\":true}\n";
        let full = format!("{first}{{\"text\":\"中文\"}}\n");
        std::fs::write(&path, &full).unwrap();
        // 文件已增长，但本轮只应读取先前 metadata 观测到的长度。
        let observed = full.find('中').unwrap() as u64 + 1;
        let (snapshot, offset) = read_bootstrap(&path, observed).unwrap();
        assert_eq!(snapshot, first);
        assert_eq!(offset, first.len() as u64);
        let (next, offset) = read_bootstrap(&path, full.len() as u64).unwrap();
        assert_eq!(next, full);
        assert_eq!(offset, full.len() as u64);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bootstrap_handles_multibyte_cut_and_small_files() {
        let text = "中".repeat(23000);
        assert_eq!(bootstrap_sample(&text, 65536), "");
        let full = format!("{text}\n{{\"type\":\"turn.prompt\"}}\n");
        assert_eq!(bootstrap_sample(&full, 65536), "{\"type\":\"turn.prompt\"}\n");
        assert_eq!(bootstrap_sample("first\nsecond\n", 65536), "first\nsecond\n");
    }

    #[test]
    fn growing_file_offsets_do_not_reread_partial_records() {
        let full = "{\"type\":\"turn.cancel\",\"message\":\"中文🙂\"}\n{\"next\":true}\n";
        let mut offset = 0;
        let mut pending = String::new();
        let mut records = Vec::new();
        for end in [8, 13, full.find('🙂').unwrap(), full.len() - 3, full.len()] {
            let (lines, read) = split_complete_lines(&full[offset..end], &mut pending);
            offset += read;
            records.extend(lines);
        }
        assert_eq!(records, full.lines().collect::<Vec<_>>());
        assert_eq!(offset, full.len());
        assert!(pending.is_empty());
        for record in records { serde_json::from_str::<serde_json::Value>(&record).unwrap(); }
    }
}
