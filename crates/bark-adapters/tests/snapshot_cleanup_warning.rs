//! §2.7（c）/ §4.4 的源码断言：运行时难以构造的失败路径按审查项的字面要求锁定。
//!
//! 1. 快照用完删除失败必须 `tracing::warn!` 留痕（Windows 上「目录被占用删不掉」
//!    可复现但不稳，构造确定性失败不可移植）；
//! 2. `decrypt_page` 复用 buffer / 原地 CBC——函数体内不得出现堆分配（§4.4）。

use std::fs;

fn read_src(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取 {} 失败: {e}", path.display()))
}

/// 取 `sig` 之后到下一个 `\nfn `/`\npub fn ` 之前的函数体片段
fn fn_body<'a>(src: &'a str, sig: &str) -> &'a str {
    let idx = src.find(sig).unwrap_or_else(|| panic!("源码里找不到 {sig}"));
    let tail = &src[idx..];
    let end = tail
        .match_indices('\n')
        .find(|(i, _)| {
            let line = &tail[*i + 1..];
            line.starts_with("fn ") || line.starts_with("pub fn ") || line.starts_with("pub(crate) fn ")
        })
        .map(|(i, _)| i)
        .unwrap_or(tail.len());
    &tail[..end]
}

#[test]
fn snapshot_removal_failure_warns_instead_of_silent_let() {
    let src = read_src("watch.rs");
    let body = fn_body(&src, "fn remove_snapshot_dir(dir: &Path)");
    assert!(
        body.contains("tracing::warn!"),
        "删除快照临时目录失败必须 tracing::warn! 留痕（§2.7c）: {body}"
    );
    assert!(
        !body.contains("let _ = std::fs::remove_dir_all"),
        "不得回到静默 let _ = 的旧实现"
    );
    // 两个用完快照的入口都走这个助手（TraeWork 的明文整库快照此前删失败被吞掉）
    assert!(src.contains("remove_snapshot_dir(&tmp_dir);"), "snapshot/snapshot_rows 必须走统一清理");
}

#[test]
fn decrypt_page_body_has_no_heap_allocation() {
    let src = read_src("traework_db.rs");
    let body = fn_body(&src, "fn decrypt_page(enc_key: &[u8], page: &[u8], pgno: u64, out: &mut Vec<u8>)");
    assert!(
        !body.contains("vec![") && !body.contains("Vec::with_capacity") && !body.contains(".to_vec()"),
        "decrypt_page 应复用 buffer / 原地 CBC，不得在函数内堆分配（§4.4）: {body}"
    );
    assert!(body.contains("out.extend_from_slice"), "解密结果应追加进调用方 buffer");
}

/// §2.18c：unregister 对解析失败的配置要 `tracing::warn!` 留痕（静默跳过会让
/// 损坏配置里的 hook 永久残留且继续触发，而用户不知道「卸载没卸干净」）
#[test]
fn zcode_unregister_warns_on_unparseable_config() {
    let src = read_src("zcode.rs");
    let body = fn_body(&src, "fn unregister(&self, ctx: &mut InstallCtx) -> anyhow::Result<()>");
    assert!(
        body.contains("tracing::warn!"),
        "zcode unregister 解析失败必须 warn 留痕（§2.18c）: {body}"
    );
}

/// §2.18d：zcode register 的单路径写失败不得中断多路径循环（`?` 改「记 last_err 继续」，
/// 与 unregister 的继续语义一致）
#[test]
fn zcode_register_continues_after_write_failure() {
    let src = read_src("zcode.rs");
    let body = fn_body(&src, "fn register(&self, ctx: &RegisterCtx) -> anyhow::Result<()>");
    assert!(
        body.contains("match jsonio::Doc::write(doc, &path)"),
        "register 写失败要走 match 记 last_err 继续（§2.18d）"
    );
    assert!(
        body.contains("Err(e) => last_err = Some(e)"),
        "写失败要记 last_err 而不是中断循环"
    );
    assert!(
        !body.contains("jsonio::Doc::write(doc, &path)?"),
        "`?` 直接中断多路径循环的旧写法不得回来"
    );
}
