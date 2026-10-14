//! 豆包工作 IndexedDB 的只读快照：不打开 LevelDB、不持锁、不修改宿主文件。
//! WAL 和 SST 按 sequence 合并；解开物理分段、Snappy 和 V8 字符串后才读业务 JSON。
use crate::doubaowork::IdbUpdate;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

const BLOCK: usize = 32768;
const LIMIT: usize = 64 * 1024 * 1024;
type Entry = (u64, Vec<u8>, Option<Vec<u8>>);
type Fingerprint = (u64, Option<SystemTime>);

pub(crate) struct IdbLogTail {
    dir: PathBuf,
    files: HashMap<PathBuf, Fingerprint>,
    // 保留删除和不再含状态的覆盖记录，避免旧 SST 把它们复活。
    keys: HashMap<Vec<u8>, (u64, Vec<IdbUpdate>)>,
    published: HashMap<String, IdbUpdate>,
}

impl IdbLogTail {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            files: HashMap::new(),
            keys: HashMap::new(),
            published: HashMap::new(),
        }
    }

    pub(crate) fn scan(&mut self) -> Vec<IdbUpdate> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut present = Vec::new();
        let mut pending = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if !matches!(ext, "log" | "ldb" | "sst")
                || path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok())
                    .is_none()
            {
                continue;
            }
            present.push(path.clone());
            let Ok(meta) = entry.metadata() else { continue };
            let fingerprint = (meta.len(), meta.modified().ok());
            if self.files.get(&path) == Some(&fingerprint) || meta.len() > LIMIT as u64 {
                continue;
            }
            // 每次读完整的已变化文件，避免轮询恰好切断 WAL 物理头或 V8 字符串。
            let Ok(data) = std::fs::read(&path) else {
                continue;
            };
            if data.len() > LIMIT {
                continue;
            }
            let records = if ext == "log" {
                wal_entries(&data)
            } else {
                table_entries(&data)
            };
            let Some(records) = records else {
                // 写入/轮转尚未完成时下轮重试，不能把坏快照记为已消费。
                continue;
            };
            pending.extend(records);
            self.files.insert(path, fingerprint);
        }
        self.files.retain(|p, _| present.contains(p));
        // 目录枚举顺序没有时间语义；先处理旧值，删除记录才能继承对应会话 id。
        pending.sort_by_key(|(seq, _, _)| *seq);
        for (seq, key, value) in pending {
            if self.keys.get(&key).is_some_and(|(old, _)| *old >= seq) {
                continue;
            }
            let mut states = value.as_deref().map(value_states).unwrap_or_default();
            if states.is_empty() {
                if let Some((_, previous)) = self.keys.get(&key) {
                    // 未识别内容不能把旧业务状态的 sequence 提升为最新，
                    // 否则另一条 key 上已记录的完成可能被旧 running 覆盖。
                    if value.is_some() && !previous.is_empty() {
                        continue;
                    }
                    states = previous.clone();
                    // 未识别的值不是完成证据；真正的 key 删除才收场。
                    if value.is_none() {
                        for state in &mut states {
                            state.running = false;
                        }
                    }
                }
            }
            self.keys.insert(key, (seq, states));
        }
        let mut latest: HashMap<String, (u64, IdbUpdate)> = HashMap::new();
        for (seq, states) in self.keys.values() {
            for state in states {
                if latest.get(&state.conv).is_none_or(|(old, _)| old < seq) {
                    latest.insert(state.conv.clone(), (*seq, state.clone()));
                }
            }
        }
        let mut updates = Vec::new();
        for (conv, old) in &self.published {
            if !latest.contains_key(conv) && old.running {
                updates.push(IdbUpdate {
                    running: false,
                    message_ids: Vec::new(),
                    ..old.clone()
                });
            }
        }
        for (conv, (_, state)) in &latest {
            if self.published.get(conv) != Some(state) {
                updates.push(state.clone());
            }
        }
        self.published = latest.into_iter().map(|(k, (_, v))| (k, v)).collect();
        updates
    }
}

fn varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let b = *data.get(*pos)?;
        *pos += 1;
        if shift == 63 && b > 1 {
            return None;
        }
        value |= ((b & 127) as u64) << shift;
        if b < 128 {
            return Some(value);
        }
    }
    None
}

fn bytes<'a>(data: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len = usize::try_from(varint(data, pos)?).ok()?;
    let end = pos.checked_add(len)?;
    let value = data.get(*pos..end)?;
    *pos = end;
    Some(value)
}

fn checksum(data: &[u8], expected: &[u8]) -> bool {
    let crc = crc32c::crc32c(data)
        .rotate_right(15)
        .wrapping_add(0xa282ead8);
    expected == crc.to_le_bytes()
}

fn unsnap(data: &[u8]) -> Option<Vec<u8>> {
    if snap::raw::decompress_len(data).ok()? > LIMIT {
        return None;
    }
    snap::raw::Decoder::new().decompress_vec(data).ok()
}

fn batch(data: &[u8]) -> Option<Vec<Entry>> {
    let seq = u64::from_le_bytes(data.get(..8)?.try_into().ok()?);
    let count = u32::from_le_bytes(data.get(8..12)?.try_into().ok()?) as usize;
    if count > data.len() {
        return None;
    }
    let mut pos = 12;
    let mut out = Vec::new();
    for i in 0..count {
        let tag = *data.get(pos)?;
        pos += 1;
        let key = bytes(data, &mut pos)?.to_vec();
        let value = match tag {
            0 => None,
            1 => Some(bytes(data, &mut pos)?.to_vec()),
            _ => return None,
        };
        out.push((seq.checked_add(i as u64)?, key, value));
    }
    (pos == data.len()).then_some(out)
}

fn wal_entries(data: &[u8]) -> Option<Vec<Entry>> {
    let mut out = Vec::new();
    let mut fragment: Option<Vec<u8>> = None;
    for block in data.chunks(BLOCK) {
        let mut pos = 0;
        while pos + 7 <= block.len() {
            let head = &block[pos..pos + 7];
            let len = u16::from_le_bytes([head[4], head[5]]) as usize;
            let tag = head[6];
            if tag == 0 && len == 0 {
                break;
            }
            let end = pos + 7 + len;
            // 不完整尾记录不消费，文件增长后重新读取。
            let Some(payload) = block.get(pos + 7..end) else {
                if block.len() == BLOCK {
                    return None;
                }
                break;
            };
            if !checksum(&block[pos + 6..end], &head[..4]) {
                return None;
            }
            match tag {
                1 => {
                    fragment = None;
                    out.extend(batch(payload)?);
                }
                2 => fragment = Some(payload.to_vec()),
                3 | 4 => {
                    let current = fragment.as_mut()?;
                    if current.len().checked_add(len)? > LIMIT {
                        return None;
                    }
                    current.extend_from_slice(payload);
                    if tag == 4 {
                        out.extend(batch(&fragment.take()?)?);
                    }
                }
                _ => return None,
            }
            pos = end;
        }
    }
    Some(out)
}

fn handle(data: &[u8], pos: &mut usize) -> Option<(usize, usize)> {
    Some((
        usize::try_from(varint(data, pos)?).ok()?,
        usize::try_from(varint(data, pos)?).ok()?,
    ))
}

fn table_block(data: &[u8], (offset, size): (usize, usize)) -> Option<Vec<u8>> {
    let end = offset.checked_add(size)?;
    let stored = data.get(offset..end.checked_add(1)?)?;
    if !checksum(stored, data.get(end + 1..end.checked_add(5)?)?) {
        return None;
    }
    match data[end] {
        0 => Some(data[offset..end].to_vec()),
        1 => unsnap(&data[offset..end]),
        _ => None,
    }
}

fn block_entries(data: &[u8]) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    let end = data.len().checked_sub(4)?;
    let restarts = u32::from_le_bytes(data[end..].try_into().ok()?) as usize;
    let end = end.checked_sub(restarts.checked_mul(4)?)?;
    let data = &data[..end];
    let mut pos = 0;
    let mut key = Vec::new();
    let mut out = Vec::new();
    while pos < end {
        let shared = usize::try_from(varint(data, &mut pos)?).ok()?;
        let unshared = usize::try_from(varint(data, &mut pos)?).ok()?;
        let value_len = usize::try_from(varint(data, &mut pos)?).ok()?;
        if shared > key.len() {
            return None;
        }
        key.truncate(shared);
        let key_end = pos.checked_add(unshared)?;
        key.extend_from_slice(data.get(pos..key_end)?);
        pos = key_end;
        let value_end = pos.checked_add(value_len)?;
        out.push((key.clone(), data.get(pos..value_end)?.to_vec()));
        pos = value_end;
    }
    Some(out)
}

fn table_entries(data: &[u8]) -> Option<Vec<Entry>> {
    let footer = data.get(data.len().checked_sub(48)?..)?;
    if footer[40..] != 0xdb4775248b80fb57u64.to_le_bytes() {
        return None;
    }
    let mut pos = 0;
    let _metaindex = handle(footer, &mut pos)?;
    let index = table_block(data, handle(footer, &mut pos)?)?;
    let mut out = Vec::new();
    for (_, block_handle) in block_entries(&index)? {
        let block = table_block(data, handle(&block_handle, &mut 0)?)?;
        for (mut key, value) in block_entries(&block)? {
            let key_len = key.len().checked_sub(8)?;
            let tag = u64::from_le_bytes(key[key_len..].try_into().ok()?);
            key.truncate(key_len);
            let value = match tag & 255 {
                0 => None,
                1 => Some(value),
                _ => return None,
            };
            out.push((tag >> 8, key, value));
        }
    }
    Some(out)
}

fn v8_string(data: &[u8], pos: &mut usize) -> Option<String> {
    // V8 可在双字节字符串前插入 padding，使正文按双字节对齐。
    while data.get(*pos) == Some(&0) {
        *pos += 1;
    }
    let tag = *data.get(*pos)?;
    *pos += 1;
    if !matches!(tag, b'"' | b'c' | b'S') {
        return None;
    }
    let raw = bytes(data, pos)?;
    match tag {
        b'"' => Some(raw.iter().map(|&b| char::from(b)).collect()),
        b'S' => String::from_utf8(raw.to_vec()).ok(),
        b'c' if raw.len() % 2 == 0 => String::from_utf16(
            &raw.chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        )
        .ok(),
        _ => None,
    }
}

fn json_object(value: &Value) -> Option<Value> {
    let value = if let Some(text) = value.as_str() {
        serde_json::from_str(text).ok()?
    } else {
        value.clone()
    };
    value.is_object().then_some(value)
}

fn value_states(raw: &[u8]) -> Vec<IdbUpdate> {
    let mut pos = 0;
    if varint(raw, &mut pos).is_none() {
        return Vec::new();
    }
    let unpacked;
    let data = if raw.get(pos..pos + 3) == Some(b"\xff\x11\x02") {
        let Some(decoded) = unsnap(&raw[pos + 3..]) else {
            return Vec::new();
        };
        unpacked = decoded;
        &unpacked[..]
    } else {
        &raw[pos..]
    };
    const CONV: &[u8] = b"\x22\x0fconversation_id";
    const EXTRA: &[u8] = b"\x22\x05extra";
    let anchors: Vec<_> = data
        .windows(CONV.len())
        .enumerate()
        .filter_map(|(i, w)| (w == CONV).then_some(i))
        .collect();
    let mut out = Vec::new();
    for (n, &start) in anchors.iter().enumerate() {
        let end = anchors.get(n + 1).copied().unwrap_or(data.len());
        let mut pos = start + CONV.len();
        let Some(conv) = v8_string(data, &mut pos) else {
            continue;
        };
        if conv.is_empty() || !conv.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Some(record) = data.get(pos..end) else {
            continue;
        };
        for (i, _) in record
            .windows(EXTRA.len())
            .enumerate()
            .filter(|(_, w)| *w == EXTRA)
        {
            let Some(extra) = v8_string(record, &mut (i + EXTRA.len())) else {
                continue;
            };
            let Ok(extra) = serde_json::from_str::<Value>(&extra) else {
                continue;
            };
            let Some(running) = extra.get("running_messages").and_then(json_object) else {
                continue;
            };
            let workspace = extra
                .get("agent_task_param")
                .and_then(json_object)
                .and_then(|v| {
                    v.get("workspace")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            out.push(IdbUpdate {
                conv: conv.clone(),
                running: !running.as_object().unwrap().is_empty(),
                message_ids: running.as_object().unwrap().keys().cloned().collect(),
                workspace,
            });
            break;
        }
    }
    out
}

// 使用真实存储格式构造脱敏样本，轮询测试也复用这些字节，避免只注入理想化的 bool。
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    fn vint(out: &mut Vec<u8>, mut n: u64) {
        while n >= 128 {
            out.push(n as u8 | 128);
            n >>= 7;
        }
        out.push(n as u8);
    }

    fn sized(out: &mut Vec<u8>, data: &[u8]) {
        vint(out, data.len() as u64);
        out.extend(data);
    }

    fn string(out: &mut Vec<u8>, text: &str, wide: bool) {
        out.push(if wide { b'c' } else { b'"' });
        if wide {
            sized(
                out,
                &text
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>(),
            );
        } else {
            sized(out, text.as_bytes());
        }
    }

    pub(crate) fn value(conv: &str, running: bool, wide: bool, compressed: bool) -> Vec<u8> {
        let extra = serde_json::json!({
            "running_messages": if running { r#"{"123":{"started_at_ms":1791892269945}}"# } else { "{}" },
            "agent_task_param": serde_json::json!({"workspace": r"D:\project\demo"}).to_string(),
            "local_conversation_id": "local_99999999999999999",
        });
        let mut data = vec![0xff, 15, b'o'];
        string(&mut data, "conversation_id", false);
        string(&mut data, conv, false);
        string(&mut data, "description", false);
        string(&mut data, &"x".repeat(5000), false);
        string(&mut data, "extra", false);
        string(&mut data, &extra.to_string(), wide);
        data.extend([b'{', 3]);
        let mut out = vec![1]; // IndexedDB 记录版本
        if compressed {
            out.extend([0xff, 0x11, 2]);
            out.extend(snap::raw::Encoder::new().compress_vec(&data).unwrap());
        } else {
            out.extend(data);
        }
        out
    }

    pub(crate) fn wal(seq: u64, records: &[(&[u8], Option<Vec<u8>>)]) -> Vec<u8> {
        let mut batch = seq.to_le_bytes().to_vec();
        batch.extend((records.len() as u32).to_le_bytes());
        for (key, value) in records {
            batch.push(u8::from(value.is_some()));
            sized(&mut batch, key);
            if let Some(v) = value {
                sized(&mut batch, v);
            }
        }
        let mut out = Vec::new();
        let pieces: Vec<_> = batch.chunks(BLOCK - 7).collect();
        for (i, part) in pieces.iter().enumerate() {
            let tag = if pieces.len() == 1 {
                1
            } else if i == 0 {
                2
            } else if i + 1 == pieces.len() {
                4
            } else {
                3
            };
            let mut checked = vec![tag];
            checked.extend_from_slice(part);
            let crc = crc32c::crc32c(&checked)
                .rotate_right(15)
                .wrapping_add(0xa282ead8);
            out.extend(crc.to_le_bytes());
            out.extend((part.len() as u16).to_le_bytes());
            out.extend(checked);
        }
        out
    }

    fn block(records: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut previous = Vec::new();
        for (key, value) in records {
            let shared = key
                .iter()
                .zip(&previous)
                .take_while(|(a, b)| a == b)
                .count();
            vint(&mut out, shared as u64);
            vint(&mut out, (key.len() - shared) as u64);
            vint(&mut out, value.len() as u64);
            out.extend(&key[shared..]);
            out.extend(value);
            previous = key.clone();
        }
        out.extend(0u32.to_le_bytes());
        out.extend(1u32.to_le_bytes());
        out
    }

    fn put_block(file: &mut Vec<u8>, data: &[u8], compressed: bool) -> Vec<u8> {
        let mut stored = if compressed {
            snap::raw::Encoder::new().compress_vec(data).unwrap()
        } else {
            data.to_vec()
        };
        let mut handle = Vec::new();
        vint(&mut handle, file.len() as u64);
        vint(&mut handle, stored.len() as u64);
        stored.push(u8::from(compressed));
        let crc = crc32c::crc32c(&stored)
            .rotate_right(15)
            .wrapping_add(0xa282ead8);
        file.extend(stored);
        file.extend(crc.to_le_bytes());
        handle
    }

    pub(crate) fn table(records: &[Entry], compressed: bool) -> Vec<u8> {
        let mut rows = Vec::new();
        for (seq, key, value) in records {
            let mut internal = key.clone();
            internal.extend(((seq << 8) | u64::from(value.is_some())).to_le_bytes());
            rows.push((internal, value.clone().unwrap_or_default()));
        }
        let mut file = Vec::new();
        let data_handle = put_block(&mut file, &block(&rows), compressed);
        let meta_handle = put_block(&mut file, &block(&[]), false);
        let index_handle = put_block(&mut file, &block(&[(b"z".to_vec(), data_handle)]), false);
        let mut footer = meta_handle;
        footer.extend(index_handle);
        footer.resize(40, 0);
        footer.extend(0xdb4775248b80fb57u64.to_le_bytes());
        file.extend(footer);
        file
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    struct TempDb(PathBuf);
    impl TempDb {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("bark-idb-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn write(&self, name: &str, data: &[u8]) {
            std::fs::write(self.0.join(name), data).unwrap();
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn v8_decodes_both_string_encodings_and_compression() {
        for wide in [false, true] {
            for compressed in [false, true] {
                for running in [false, true] {
                    let states =
                        value_states(&value("38446845671811074", running, wide, compressed));
                    assert_eq!(
                        states,
                        vec![IdbUpdate {
                            conv: "38446845671811074".into(),
                            running,
                            message_ids: if running { vec!["123".into()] } else { Vec::new() },
                            workspace: r"D:\project\demo".into(),
                        }]
                    );
                }
            }
        }
        // 一字节字符串是 Latin-1；双字节正文的长度按字节计数。
        assert_eq!(v8_string(b"\x22\x01\xe9", &mut 0).as_deref(), Some("é"));
        assert_eq!(
            v8_string(b"\0c\x04\x2d\x4e\x87\x65", &mut 0).as_deref(),
            Some("中文")
        );
        assert!(value_states(b"\x01\xff\x11\x02bad compressed data").is_empty());
    }

    #[test]
    fn wal_reassembles_fragments_and_retries_partial_tail() {
        let db = TempDb::new();
        let data = wal(
            10,
            &[
                (b"noise", Some(vec![7; BLOCK * 2])),
                (b"session", Some(value("1001", true, true, false))),
            ],
        );
        let mut reader = IdbLogTail::new(db.0.clone());
        for end in [3, BLOCK - 1, BLOCK + 3, data.len() - 1] {
            db.write("000001.log", &data[..end]);
            assert!(
                reader.scan().is_empty(),
                "不完整 WriteBatch 不应产生半个状态"
            );
        }
        db.write("000001.log", &data);
        assert!(reader.scan()[0].running);
        assert!(reader.scan().is_empty());
        let mut corrupt = data.clone();
        corrupt[20] ^= 1;
        assert!(wal_entries(&corrupt).is_none());
    }

    #[test]
    fn newer_wal_completion_wins_over_sst_and_rotation() {
        let db = TempDb::new();
        let old = table(
            &[(
                10,
                b"session".to_vec(),
                Some(value("1001", true, true, true)),
            )],
            true,
        );
        db.write("000010.ldb", &old);
        let mut reader = IdbLogTail::new(db.0.clone());
        assert!(reader.scan()[0].running, "启动必须读取压缩的 SST 种子");
        let done = wal(20, &[(b"session", Some(value("1001", false, true, true)))]);
        db.write("000011.log", &done);
        assert!(!reader.scan()[0].running);
        db.write("000012.ldb", &old); // 新文件编号并不意味着记录更新
        assert!(reader.scan().is_empty());
        assert!(
            !IdbLogTail::new(db.0.clone()).scan()[0].running,
            "启动合并同样以 sequence 为准"
        );
        std::fs::remove_file(db.0.join("000011.log")).unwrap();
        db.write(
            "000013.ldb",
            &table(
                &[(
                    20,
                    b"session".to_vec(),
                    Some(value("1001", false, false, false)),
                )],
                false,
            ),
        );
        assert!(reader.scan().is_empty(), "compaction 不复活旧运行状态");
        db.write(
            "000014.log",
            &wal(30, &[(b"session", Some(value("1001", true, false, false)))]),
        );
        assert!(reader.scan()[0].running, "下一轮任务仍能启动");
    }

    #[test]
    fn table_handles_prefix_keys_tombstones_and_corruption() {
        let rows = vec![
            (
                10,
                b"session/a".to_vec(),
                Some(value("1001", true, true, false)),
            ),
            (
                11,
                b"session/b".to_vec(),
                Some(value("1002", false, false, false)),
            ),
            (12, b"session/c".to_vec(), None),
        ];
        for compressed in [false, true] {
            let mut data = table(&rows, compressed);
            assert_eq!(table_entries(&data).unwrap(), rows);
            data[6] ^= 1;
            assert!(table_entries(&data).is_none());
        }
        let db = TempDb::new();
        db.write("000001.ldb", &table(&rows, true));
        db.write("000002.log", &wal(20, &[(b"session/a", None)]));
        let mut reader = IdbLogTail::new(db.0.clone());
        assert!(
            reader.scan().iter().all(|u| !u.running),
            "删除不得被旧表覆盖"
        );
        db.write("000003.ldb", &table(&rows, false));
        assert!(reader.scan().is_empty());
    }

    #[test]
    fn missing_or_invalid_running_field_is_not_activity() {
        let mut data = value("1001", true, false, false);
        // 未识别字段必须忽略，不能凭包含 conversation_id 就当成运行中。
        let at = data
            .windows(16)
            .position(|w| w == b"running_messages")
            .unwrap();
        data[at] = b'x';
        assert!(value_states(&data).is_empty());
        for len in 0..data.len().min(100) {
            assert!(value_states(&data[..len]).is_empty());
        }
    }

    #[test]
    fn unknown_value_does_not_promote_stale_running_over_known_completion() {
        let db = TempDb::new();
        db.write(
            "000001.log",
            &wal(
                10,
                &[
                    (b"session/a", Some(value("1001", true, true, true))),
                    (b"session/b", Some(value("1001", false, true, true))),
                    (b"session/a", Some(b"unknown schema".to_vec())),
                ],
            ),
        );
        assert!(!IdbLogTail::new(db.0.clone()).scan()[0].running);
    }

    /// 只读检查本机数据库每个文件的完整格式；不输出任务正文，不启动宿主任务。
    #[test]
    #[ignore]
    fn doubao_live_database_files_decode() {
        let dir = dirs::data_local_dir().unwrap().join(
            "DoubaoWork/User Data/Default/IndexedDB/chrome_doubaowork-chat_0.indexeddb.leveldb",
        );
        let mut files = 0;
        let mut states = 0;
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !matches!(ext, "log" | "ldb") {
                continue;
            }
            let data = std::fs::read(&path).unwrap();
            let records = if ext == "log" {
                wal_entries(&data)
            } else {
                table_entries(&data)
            }
            .unwrap_or_else(|| panic!("数据库文件解码失败: {}", path.display()));
            let count: usize = records
                .iter()
                .filter_map(|(_, _, v)| v.as_deref())
                .map(|v| value_states(v).len())
                .sum();
            eprintln!(
                "{}: {} 条存储记录，{} 条会话状态",
                path.file_name().unwrap().to_string_lossy(),
                records.len(),
                count
            );
            files += 1;
            states += count;
        }
        assert!(files > 0 && states > 0);
    }
}
