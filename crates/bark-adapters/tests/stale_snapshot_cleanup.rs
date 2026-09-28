//! §2.7 TraeWork / WorkBuddy 明文快照残留清理回归测试（公开 API）。
//!
//! 覆盖清单：
//! a) 清理前缀同时匹配 `agent-bark-watch-` 与 `agent-bark-traework-` 两类快照目录；
//! b) 反向断言：不属于本插件命名空间的目录绝不误删；
//! c) 密钥/密码缓冲区尽力覆写清零（`wipe_secret`）。

use bark_adapters::watch;
use std::time::Duration;

#[test]
fn stale_cleanup_covers_both_watch_and_traework_prefixes() {
    let tmp = std::env::temp_dir().join(format!("ab-snap-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    // 快照命名两套前缀：WorkBuddy 的 `agent-bark-watch-<uuid>` 与
    // TraeWork 的 `agent-bark-traework-<uuid>`——两条线都得被清理（§2.7a）
    let d1 = tmp.join("agent-bark-watch-1");
    let d2 = tmp.join("agent-bark-traework-1");
    std::fs::create_dir_all(&d1).unwrap();
    std::fs::create_dir_all(&d2).unwrap();
    // 反向样本：不属于本插件命名空间的目录绝不能误删
    let d3 = tmp.join("agent-bark"); // 无 `agent-bark-<kind>-` 前缀
    let d4 = tmp.join("agent-bark-evil"); // 有前缀但不在命名空间
    std::fs::create_dir_all(&d3).unwrap();
    std::fs::create_dir_all(&d4).unwrap();

    watch::cleanup_stale_snapshot_dirs(&tmp, Duration::ZERO);

    assert!(!d1.exists(), "watch 前缀快照未被清理");
    assert!(!d2.exists(), "traework 前缀快照未被清理");
    assert!(d3.exists(), "无关目录被误删（`agent-bark`）");
    assert!(d4.exists(), "无关目录被误删（`agent-bark-evil`）");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn stale_cleanup_keeps_unrelated_names_and_files() {
    // `agent-bark-` 前缀但不在快照命名空间的目录、以及文件形态的同名条目都不删
    let tmp = std::env::temp_dir().join(format!("ab-snap-sep-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let keep_dir = tmp.join("agent-bark-foo");
    let keep_file = tmp.join("agent-bark-watch-7");
    std::fs::create_dir_all(&keep_dir).unwrap();
    std::fs::write(&keep_file, b"x").unwrap();

    watch::cleanup_stale_snapshot_dirs(&tmp, Duration::ZERO);

    assert!(keep_dir.exists(), "非快照目录被误删");
    assert!(keep_file.exists(), "文件不是快照目录，不该被 remove_dir_all");
    // 不存在的目录：直接返回，不 panic
    watch::cleanup_stale_snapshot_dirs(&tmp.join("missing"), Duration::ZERO);

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn stale_cleanup_respects_min_age() {
    // min_age 门槛：未到龄的快照目录不回收（不误删并发 watcher 正在用的那份）
    let tmp = std::env::temp_dir().join(format!("ab-snap-age-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let fresh = tmp.join("agent-bark-traework-fresh");
    std::fs::create_dir_all(&fresh).unwrap();

    watch::cleanup_stale_snapshot_dirs(&tmp, Duration::from_secs(3600));
    assert!(fresh.exists(), "刚创建的快照目录不得回收");
    watch::cleanup_stale_snapshot_dirs(&tmp, Duration::ZERO);
    assert!(!fresh.exists(), "到龄（ZERO）后应回收");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn secret_wipe_erases_prefix_bytes() {
    // `wipe_secret` 是尽力而为的覆写清零：长度不变、全字节归零、不挑长度
    let mut buf = b"hunter2-secret-token".to_vec();
    let len = buf.len();
    watch::wipe_secret(&mut buf);
    assert_eq!(buf.len(), len, "清零不应改变长度");
    assert!(
        buf[..len].iter().all(|&b| b == 0),
        "密钥缓冲区未被覆写清零: {buf:?}"
    );
    // 对短 secret 也得生效（不能只 wipe 固定长度前缀）
    let mut short = b"pw".to_vec();
    watch::wipe_secret(&mut short);
    assert_eq!(short, vec![0, 0], "短 secret 未被清零");
}
