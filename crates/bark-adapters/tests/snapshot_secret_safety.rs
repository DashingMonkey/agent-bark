//! §2.7（d）密钥/密码缓冲区尽力覆写清零的回归测试（公开 API）。
//!
//! 与 `stale_snapshot_cleanup.rs` 互补（那里覆盖清理语义，这里覆盖清零语义）。
//! 不引入 zeroize 依赖——`watch::wipe_secret` 是自写的小型覆写清零助手。

use bark_adapters::watch;

#[test]
fn wipe_secret_is_idempotent_and_zero_length_safe() {
    // 空缓冲区调用不得 panic；重复 wipe 幂等
    let mut empty: Vec<u8> = Vec::new();
    watch::wipe_secret(&mut empty);
    assert!(empty.is_empty());

    let mut buf = b"token".to_vec();
    watch::wipe_secret(&mut buf);
    watch::wipe_secret(&mut buf);
    assert!(buf.iter().all(|&b| b == 0), "重复 wipe 后应保持清零");
}

#[test]
fn wipe_secret_handles_every_length() {
    // 1..=40 字节逐个长度验证（含 AES 块边界前后）
    for len in 1..=40usize {
        let mut buf = vec![0xABu8; len];
        watch::wipe_secret(&mut buf);
        assert_eq!(buf.len(), len);
        assert!(buf.iter().all(|&b| b == 0), "len={len} 未被清零");
    }
}

#[test]
fn cleanup_prefix_matches_real_snapshot_layouts() {
    // 真实快照布局：`agent-bark-watch-<uuid>`（WorkBuddy）与
    // `agent-bark-traework-<uuid>`（TraeWork）都要回收（§2.7a/b）
    let tmp = std::env::temp_dir().join(format!("ab-layout-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    for name in ["agent-bark-watch-4242", "agent-bark-traework-99"] {
        std::fs::create_dir_all(tmp.join(name)).unwrap();
    }
    watch::cleanup_stale_snapshot_dirs(&tmp, std::time::Duration::ZERO);
    for name in ["agent-bark-watch-4242", "agent-bark-traework-99"] {
        assert!(!tmp.join(name).exists(), "{name} 未被清理");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
