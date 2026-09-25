//! 配置文件的读写原语。
//!
//! 这里集中处理「怎么读、怎么原子写、怎么备份」，各 adapter 不再自己碰 `fs::write`。
//!
//! 两条硬规则：
//! 1. **解析失败绝不当成空对象**。文件存在但 JSON 有语法错误时返回 `Err`，
//!    调用方必须放弃写入——否则会把用户整份配置替换成只剩我们的条目。
//! 2. 写入一律「同目录临时文件 → rename」，避免半写文件被 agent 读到。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 备份文件名：`<原名>.agent-bark.bak`
pub fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".agent-bark.bak");
    PathBuf::from(s)
}

/// 最近一次变更前的状态：`<原名>.agent-bark.bak.latest`
///
/// `.bak` 只存用户被接入**之前**的最初状态（永不覆盖）；
/// `.bak.latest` 在每次内容真的变化时刷新为「变更前一刻」——
/// 半年后某次写入出问题时，救回的不再是半年前的老状态。
fn backup_latest_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".agent-bark.bak.latest");
    PathBuf::from(s)
}

/// 读取 JSON 对象（只读场景用；要写回请用 [`Doc::read`]，带 CAS 防并发覆盖）。
/// - 文件不存在 → `Ok(json!({}))`
/// - 解析失败   → `Err`（调用方必须中止，不可写回）
pub fn read_doc(path: &Path) -> anyhow::Result<Value> {
    Doc::read(path).map(|d| d.value)
}

/// 读到的文档 + 底稿。写回时按底稿做内容级 CAS：若文件在我们读取之后
/// 被其他程序改过（目标应用整份重写、用户手动编辑），拒绝覆盖并报错。
pub struct Doc {
    pub value: Value,
    /// 读取时的原文（已剥 BOM；文件不存在时为空串），CAS 的比对基准
    raw: String,
}

impl Doc {
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let (value, raw) = read_doc_raw(path)?;
        Ok(Doc { value, raw })
    }

    /// 读取时的原文底稿（已剥 BOM）。调用方做幂等比较时用。
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// CAS 写回：先刷新 `.bak.latest` → 重读比对底稿 → 原子落盘。
    ///
    /// 备份放在 CAS 校验**之前**（§2.18i）：旧顺序（校验→备份写盘→rename）里
    /// 「备份写盘」这段 IO 会把 TOCTOU 窗口拉宽——校验通过后、rename 前被宿主
    /// 重写的概率随窗口时长线性上升。挪到校验前，窗口缩到「一次读文件 + 一次
    /// rename」的毫秒级。残余窗口仍在（校验与 rename 之间理论上可被并发重写），
    /// 根治要文件锁/单写者化（审查 §2.1 的路线），这里只是缩小。
    /// CAS 失败时 `.bak.latest` 已刷新为读取时的底稿——那正是「变更前一刻」的
    /// 真实内容，多存一份无害。
    pub fn write(self, path: &Path) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(&self.value)? + "\n";
        // 只在内容真的变化时备份：幂等重写（调用方判定「没变也写一遍」）不动备份
        if self.raw != text {
            backup_latest(path, &self.raw);
        }
        verify_not_concurrently_modified(path, &self.raw)?;
        write_text_atomic(path, &text)
    }
}

/// 读文件并解析为（文档, 原文底稿）。read_doc / Doc::read 共用。
fn read_doc_raw(path: &Path) -> anyhow::Result<(Value, String)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((json!({}), String::new())),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("读取 {} 失败", path.display()))),
    };
    // 容忍 UTF-8 BOM：部分编辑器会写入
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    if text.trim().is_empty() {
        // 空文件视为空对象；底稿保留原样（含空白），CAS 才不会把
        // 「仍是这份空白文件」误判成「被并发修改」
        return Ok((json!({}), text.to_string()));
    }
    let doc: Value = serde_json::from_str(text).map_err(|e| {
        anyhow::anyhow!(
            "{} 不是合法 JSON（{}）。为避免覆盖你的配置，agent-bark 已中止写入，请先修复该文件。",
            path.display(),
            e
        )
    })?;
    if !doc.is_object() {
        anyhow::bail!("{} 的顶层不是 JSON 对象，已中止写入", path.display());
    }
    Ok((doc, text.to_string()))
}

/// CAS 检查：文件当前内容必须仍与读取时的底稿一致。
/// `expected` 为空串表示读取时文件不存在——此时「文件仍不存在」或「被并发删除」都放行
/// （新建文件不会覆盖任何人的内容），有内容则说明有人先写了，拒绝覆盖。
fn verify_not_concurrently_modified(path: &Path, expected: &str) -> anyhow::Result<()> {
    match std::fs::read_to_string(path) {
        Ok(current) => {
            let current = current.strip_prefix('\u{feff}').unwrap_or(&current);
            if current == expected {
                return Ok(());
            }
            anyhow::bail!(
                "{} 在 agent-bark 读取后被其他程序修改过（可能是目标应用正在重写它）。\
                 为避免覆盖刚发生的改动，本次写入已中止——请重试一次。",
                path.display()
            )
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if expected.is_empty() {
                Ok(())
            } else {
                anyhow::bail!(
                    "{} 在 agent-bark 读取后被删除。为避免误判，本次写入已中止——请重试一次。",
                    path.display()
                )
            }
        }
        Err(e) => Err(anyhow::Error::new(e).context(format!("读取 {} 失败", path.display()))),
    }
}

/// 内容确实要变化时，把「变更前一刻」存到 `.bak.latest`（尽力而为，不阻断主流程）。
/// `pre_change_raw` 是 CAS 验证过的当前原文；为空（首次创建文件）则跳过。
/// `pub(crate)`：dsh 的 YAML patch 写回路径（write_patch_checked）也用它。
pub(crate) fn backup_latest(path: &Path, pre_change_raw: &str) {
    if pre_change_raw.is_empty() {
        return;
    }
    let latest = backup_latest_path(path);
    if let Err(e) = std::fs::write(&latest, pre_change_raw) {
        tracing::warn!("刷新 {} 失败：{e}（将继续写入）", latest.display());
    }
}

/// 首次写入前备份（只备份一次，保留用户最初的文件）
pub fn backup_once(path: &Path) {
    let bak = backup_path(path);
    if !path.exists() || bak.exists() {
        return;
    }
    // 备份失败不阻断主流程，但必须留痕（之前是静默 let _ 吞掉）
    if let Err(e) = std::fs::copy(path, &bak) {
        tracing::warn!("备份 {} → {} 失败：{e}（将继续写入，但本次没有备份）", path.display(), bak.display());
    }
}

/// 若 `path` 是符号链接则解析出真实目标：直接 rename 覆盖链接本身会把
/// 链接（如指向 dotfiles 仓库的 settings.json）断成普通文件。
fn resolve_symlink(path: &Path) -> PathBuf {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::canonicalize(path) {
            Ok(target) => target,
            Err(_) => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// 同目录临时文件名（rename 的前提是同一文件系统）
fn tmp_sibling(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("config.json"),
        uuid::Uuid::new_v4().simple()
    ))
}

/// 写入临时文件、fsync、再 rename 覆盖目标
fn write_then_rename(tmp: &Path, path: &Path, text: &str) -> anyhow::Result<()> {
    {
        use std::io::Write;
        let mut f = std::fs::File::create(tmp)
            .map_err(|e| anyhow::Error::new(e).context(format!("创建临时文件 {} 失败", tmp.display())))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, path).map_err(|e| anyhow::Error::new(e).context(format!("替换 {} 失败", path.display())))
}

/// 原子写入文本（临时文件 + rename）；任何失败路径都清理临时文件。
/// 目标是符号链接时写入解析后的真实文件（rename 覆盖链接本身会把它断成普通文件）。
pub fn write_text_atomic(path: &Path, text: &str) -> anyhow::Result<()> {
    let path = &resolve_symlink(path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = tmp_sibling(path);
    let result = write_then_rename(&tmp, path, text);
    if result.is_err() {
        // 写盘/fsync/rename 任一步失败都不能留下半写临时文件
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 原子写入 JSON（临时文件 + rename）。只写不看 concurrent 场景用；
/// register/unregister 等读-改-写流程请用 [`Doc`]（带 CAS）。
pub fn write_doc(path: &Path, doc: &Value) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(doc)? + "\n";
    write_text_atomic(path, &text)
}

/// 把路径渲染成「在 cmd / PowerShell / Git Bash 下都能解析」的形式：
/// 正斜杠 + 双引号。反斜杠路径会被 Git Bash 吞掉（Qoder 系在 Windows 走 bash）。
pub fn shell_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    format!("\"{s}\"")
}

/// 我们的可执行文件名（hook 命令第一 token 的文件名必须匹配其一，大小写不敏感）。
/// 历史名必须永远保留：老版本写入的 hook 靠它们识别为「我们的条目」，
/// 卸载/就地修复才能找到目标（8e1ebb4 曾把旧名删掉，导致旧版用户的
/// hook 条目变成永远删不掉、还会重复触发的孤儿——教训见该提交）。
///
/// `pub(crate)`：zcode 适配器写的是 `process` 条目（`command` 是单个 argv 元素、
/// 不带引号、可能含空格），不能用本模块按「首 token」取名的判定，但它需要同一份
/// 重命名清单。
pub(crate) const KNOWN_EXE_NAMES: &[&str] = &[
    "AgentBark.exe",
    "AgentBark",
    // 历史名（按时间倒序），永不删除
    "agent-bark-app.exe",
    "agent-bark-app",
    "agent-bark.exe",
    "agent-bark",
];

/// 取 shell 命令的第一个 token：支持双引号包裹的带空格路径
fn first_token(command: &str) -> &str {
    let s = command.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        rest.split('"').next().unwrap_or("")
    } else {
        s.split_whitespace().next().unwrap_or("")
    }
}

/// 单个 hook 条目是否是 agent-bark 写入的。
///
/// 判定刻意严格（旧实现是 `command.contains("agent-bark")` 子串匹配，
/// 用户自己的 hook 命令路径碰巧含该子串——如 `/home/u/agent-bark-notes/run.sh`
/// ——会被误改/误删）：
/// 取 command 的第一个 token（去引号）当作可执行路径，
/// 仅当其**文件名**恰好是已知 agent-bark 可执行名时才归我们所有。
pub fn is_our_entry(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| {
            let exe = first_token(c);
            let name = exe.rsplit(['/', '\\']).next().unwrap_or(exe);
            KNOWN_EXE_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n))
        })
}

/// 一条 hook 命令指向的可执行文件是否就是 `exe_path`。
///
/// 比较的是「解析出的第一个 token 去引号后归一化」与「exe_path 归一化」是否相等，
/// 而不是子串包含（`agent-bark-app.exe.old` 会被 contains 误判为一致）。
pub fn command_exe_matches(command: &str, exe_path: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_ascii_lowercase();
    let exe = first_token(command);
    if exe.is_empty() {
        return false;
    }
    norm(exe) == norm(exe_path)
}

/// 一组 hook 里是否含 agent-bark 条目
pub fn group_has_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hs| hs.iter().any(is_our_entry))
}

/// 一组 hook 里是否含**非** agent-bark 条目（用户自己的）
pub fn group_has_foreign(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hs| hs.iter().any(|h| !is_our_entry(h)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("agent-bark-jsonio-{}", uuid::Uuid::new_v4().simple()))
            .join(name)
    }

    #[test]
    fn missing_file_is_empty_object() {
        let p = tmp_path("missing.json");
        assert_eq!(read_doc(&p).unwrap(), json!({}));
    }

    #[test]
    fn malformed_file_is_an_error_not_empty() {
        let p = tmp_path("bad.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{ oops").unwrap();
        let err = read_doc(&p).expect_err("解析失败必须是错误");
        assert!(err.to_string().contains("不是合法 JSON"));
        // 原文件未被改动
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ oops");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn bom_is_tolerated() {
        let p = tmp_path("bom.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "\u{feff}{\"a\":1}").unwrap();
        assert_eq!(read_doc(&p).unwrap()["a"], 1);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn write_doc_is_atomic() {
        let p = tmp_path("out.json");
        write_doc(&p, &json!({"x": [1, 2]})).unwrap();
        assert_eq!(read_doc(&p).unwrap()["x"][1], 2);
        let leftovers = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn shell_path_uses_forward_slashes() {
        assert_eq!(shell_path(Path::new(r"C:\a b\x.exe")), r#""C:/a b/x.exe""#);
    }

    #[test]
    fn group_ownership_detection() {
        let ours = json!({"hooks":[{"type":"command","command":"\"C:/x/AgentBark.exe\" hook"}]});
        let theirs = json!({"hooks":[{"type":"command","command":"echo hi"}]});
        let mixed = json!({"hooks":[
            {"type":"command","command":"echo hi"},
            {"type":"command","command":"AgentBark.exe hook"}
        ]});
        assert!(group_has_ours(&ours) && !group_has_foreign(&ours));
        assert!(!group_has_ours(&theirs) && group_has_foreign(&theirs));
        assert!(group_has_ours(&mixed) && group_has_foreign(&mixed));
    }

    #[test]
    fn marker_substring_alone_is_not_ours() {
        // 用户自己的命令，路径里碰巧含 "agent-bark" 子串 → 不能算我们的条目
        for cmd in [
            "/home/u/agent-bark-notes/run.sh",
            "\"C:/tools/agent-bark-backup/do.exe\" --flag",
            "python agent-bark-helper.py",
        ] {
            assert!(!is_our_entry(&json!({"type":"command","command":cmd})), "应判为用户条目: {cmd}");
        }
        // 可执行文件名是我们的 → 无论目录叫什么、什么大小写
        for cmd in [
            "\"C:/Program Files/AgentBark/AgentBark.exe\" hook",
            "./bin/AgentBark hook",
            // 新版重命名后的 AgentBark.exe（含大小写变体）
            "\"C:/Users/u/AppData/Local/AgentBark/AgentBark.exe\" hook --agent x --event Stop",
            "./agentbark hook",
        ] {
            assert!(is_our_entry(&json!({"type":"command","command":cmd})), "应判为我们条目: {cmd}");
        }
        // 注意 agentbark-helper.exe 不是我们的（文件名不匹配）
        assert!(!is_our_entry(&json!({"type":"command","command":"agentbark-helper.exe hook"})));
    }

    #[test]
    fn write_text_atomic_works_and_leaves_no_tmp() {
        let p = tmp_path("out.txt");
        write_text_atomic(&p, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello");
        let leftovers = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn cas_write_rejects_concurrent_modification() {
        let p = tmp_path("cas.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, r#"{"user":"settings"}"#).unwrap();
        let mut doc = Doc::read(&p).unwrap();
        // 读取之后、写回之前，其他程序改了文件（模拟 Qoder 整份重写）
        std::fs::write(&p, r#"{"user":"settings","rewritten":true}"#).unwrap();
        doc.value["ours"] = json!(true);
        let err = doc.write(&p).expect_err("并发修改必须拒绝覆盖");
        assert!(err.to_string().contains("被其他程序修改"), "err={err:#}");
        // 对方写入的内容完好无损
        assert_eq!(std::fs::read_to_string(&p).unwrap(), r#"{"user":"settings","rewritten":true}"#);
        // 重试路径：重新读取（拿到新底稿）后写回成功
        let mut doc = Doc::read(&p).unwrap();
        doc.value["ours"] = json!(true);
        doc.write(&p).unwrap();
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(back["rewritten"], true);
        assert_eq!(back["ours"], true);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn cas_write_rejects_concurrent_delete() {
        let p = tmp_path("cas-del.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, r#"{"a":1}"#).unwrap();
        let doc = Doc::read(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        let err = doc.write(&p).expect_err("读取后文件被删必须中止");
        assert!(err.to_string().contains("被删除"), "err={err:#}");
        // 读取时本就不存在的文件：并发「删除」= 仍不存在，照常创建
        let p2 = tmp_path("cas-new.json");
        let doc = Doc::read(&p2).unwrap();
        doc.write(&p2).unwrap();
        assert!(p2.exists());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        let _ = std::fs::remove_dir_all(p2.parent().unwrap());
    }

    #[test]
    fn latest_backup_captures_pre_change_state() {
        let p = tmp_path("bak.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, r#"{"v":1}"#).unwrap();
        // 首个 .bak（用户最初状态）
        backup_once(&p);
        // 内容变化的写入：.bak.latest 刷新为「变更前一刻」
        let mut doc = Doc::read(&p).unwrap();
        doc.value["v"] = json!(2);
        doc.write(&p).unwrap();
        let latest = backup_latest_path(&p);
        assert_eq!(std::fs::read_to_string(&latest).unwrap(), r#"{"v":1}"#);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\n  \"v\": 2\n}\n");
        // 幂等写（内容没变）不刷新 .bak.latest
        let doc = Doc::read(&p).unwrap();
        doc.write(&p).unwrap();
        assert_eq!(std::fs::read_to_string(&latest).unwrap(), r#"{"v":1}"#);
        // 首次创建文件（读取时不存在）不产生 .bak.latest
        let p2 = tmp_path("bak-new.json");
        let doc = Doc::read(&p2).unwrap();
        doc.write(&p2).unwrap();
        assert!(!backup_latest_path(&p2).exists());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        let _ = std::fs::remove_dir_all(p2.parent().unwrap());
    }

    /// §2.18i：`backup_latest` 在 CAS 校验**之前**落盘——写入被 CAS 拒绝时，
    /// `.bak.latest` 也已经刷新为读取时的底稿（那正是真实的「变更前一刻」）。
    /// 旧顺序下 CAS 拒绝路径不写备份，且「备份写盘」会拉宽 TOCTOU 窗口。
    #[test]
    fn latest_backup_is_written_before_cas_even_when_write_rejected() {
        let p = tmp_path("bak-cas.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, r#"{"v":1}"#).unwrap();
        let mut doc = Doc::read(&p).unwrap();
        doc.value["v"] = json!(2);
        // 读取后被宿主重写 → CAS 将拒绝
        std::fs::write(&p, r#"{"v":99}"#).unwrap();
        assert!(doc.write(&p).is_err());
        let latest = backup_latest_path(&p);
        assert_eq!(
            std::fs::read_to_string(&latest).unwrap(),
            r#"{"v":1}"#,
            "CAS 拒绝路径也要留下读取时底稿（备份先于校验）"
        );
        // 宿主的内容完好
        assert_eq!(std::fs::read_to_string(&p).unwrap(), r#"{"v":99}"#);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    /// 首次创建（读取时不存在）不产生 `.bak.latest`；`backup_latest` 对空底稿直接跳过
    #[test]
    fn latest_backup_skips_first_creation_and_empty_raw() {
        let p = tmp_path("bak-skip.json");
        let doc = Doc::read(&p).unwrap();
        doc.write(&p).unwrap();
        assert!(!backup_latest_path(&p).exists(), "首次创建不产生 .bak.latest");
        backup_latest(&p, "");
        assert!(!backup_latest_path(&p).exists(), "空底稿不备份");
    }

    #[test]
    fn legacy_exe_names_are_recognized() {
        // 8e1ebb4 曾把改名前的历史名删掉：旧版用户升级后 hook 条目成了
        // 卸载不掉的孤儿。历史名必须永远被识别。
        for cmd in [
            r#""C:/old/agent-bark-app.exe" hook --agent claude-code --event Stop"#,
            r#""C:/old/agent-bark-app" hook"#,
            "agent-bark.exe hook",
            "./agent-bark hook",
        ] {
            assert!(is_our_entry(&json!({"type":"command","command":cmd})), "应判为我们条目: {cmd}");
        }
    }
}
