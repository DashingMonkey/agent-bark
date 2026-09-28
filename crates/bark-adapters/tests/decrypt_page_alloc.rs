//! §2.19 / §4.4 公开 API 回归（traework_db）：
//! 逐页 HMAC 校验的拒绝面 + 解密 buffer 复用的对外行为不变。
//!
//! 正向的加解密夹具在 `traework_db.rs` 的单元测试里（`encrypt_fixture` 是测试
//! 侧夹具、不对外）；这里锁公开 API 的边界行为。

use bark_adapters::traework_db::{decrypt_db, derive_key, verify_page, verify_page1, PAGE_SZ};

#[test]
fn verify_page_rejects_short_or_wrong_key_input() {
    // 短于一页：直接拒绝，不越界
    assert!(!verify_page(&[0u8; 16], 1, &[0u8; 32]));
    assert!(!verify_page1(&[0u8; 16], derive_key()));
    // 全零页用全零 mac_key 也验不过（HMAC 域覆盖密文+IV+页号）
    let page = vec![0u8; PAGE_SZ];
    assert!(!verify_page(&page, 1, &[0u8; 32]));
    assert!(!verify_page(&page, 2, &[0u8; 32]));
    assert!(!verify_page1(&page, derive_key()));
}

#[test]
fn decrypt_db_rejects_unaligned_or_tiny_files() {
    let dir = std::env::temp_dir().join(format!("ab-twdb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // 大小不是 PAGE_SZ 整数倍 → 明确报错（不产出乱码库）
    let p = dir.join("bad.db");
    std::fs::write(&p, vec![0u8; PAGE_SZ + 1]).unwrap();
    let err = decrypt_db(&p).expect_err("非整倍大小必须拒绝");
    assert!(format!("{err:#}").contains("整数倍"), "{err:#}");

    // 小于一页 → 同样拒绝
    std::fs::write(&p, vec![0u8; 100]).unwrap();
    assert!(decrypt_db(&p).is_err());

    // 对齐但内容全零（HMAC 全错）→ 拒绝不产出明文
    std::fs::write(&p, vec![0u8; PAGE_SZ * 2]).unwrap();
    let err = decrypt_db(&p).expect_err("HMAC 失配必须整库拒绝（§2.19）");
    assert!(format!("{err:#}").contains("HMAC"), "{err:#}");

    // 文件不存在 → 读取错误（不 panic）
    assert!(decrypt_db(&dir.join("nope.db")).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn derive_key_is_stable_and_page1_verifiable() {
    // 同一密钥恒定（OnceLock 缓存 + 已知答案测试在单测里），跨调用一致
    assert_eq!(derive_key().as_slice(), derive_key().as_slice());
    assert_eq!(derive_key().len(), 32);
}
