//! TraeWork（TRAE SOLO CN）会话库解密：SQLCipher 4 页面级解密。
//!
//! `%APPDATA%\TRAE SOLO CN\ModularData\ai-agent\database.db` 是 SQLCipher 4 加密库
//! （AES-256-CBC + HMAC-SHA512，page=4096，reserve=80 即 16B IV + 64B HMAC）。
//!
//! 密钥不是随机的：与 Trae CN 同源的固定派生链（ai_agent.dll 内硬编码常量，
//! 2026-09 于 TraeWork CN 0.1.69 的 265MB ai_agent.dll 实测常量在位、
//! 本机库 HMAC 校验通过）：
//!
//! 1. 配置键 `mics_sj10gy` 的 32 字节加密数据，与 "rust"/"cpp"/"electron"
//!    三表循环 XOR 解出固定明文密码（三个计数器各自按表长循环，联合周期是
//!    lcm(4,3,8)=**24**；96=4×3×8 只是计数器状态空间的上界，别再写成「周期 96」）；
//! 2. PBKDF2-HMAC-SHA256(密码, salt=123456789abcdef01122334455667788, 100000, 32)
//!    派生出数据库加密密钥。
//!
//! 页面级解密采用 wechat-decrypt 同款方案（Oh-My-Trae/trae-db-decrypt 与
//! DirWang 的逆向分析已核对）：每页 AES-256-CBC 无填充解密，第 1 页前 16 字节
//! 是 salt（重建时换回 `SQLite format 3\0` 头），页尾 80 字节 reserve 用零填充
//! 还原成普通 SQLite 文件后即可用 rusqlite 直接读。
//!
//! **WAL 不可忽略**：宿主是 SQLite WAL 模式，新提交先落 `database.db-wal`、
//! checkpoint 后才进主库。2026-09-24 实测 -wal 常驻 4-5MB（上千帧），只解主库
//! 会让最新会话整场不可见（用户两次提问全程无光效）。因此 [`decrypt_db`] 解完
//! 主库后按 SQLite WAL 恢复语义把 `-wal` 帧叠加回去：逐帧校验 checksum 链
//! （排除 RESTART checkpoint 后残留的陈旧尾帧）+ 页级 HMAC（撕裂尾帧到此为止），
//! 只应用到最后一个提交帧（不完整事务丢弃），库大小取最后提交帧的 dbsize。
//!
//! ⚠️ 非官方机制：TraeWork 升级可能更换常量（见 `verify_page1` 的报错提示），
//! 届时用 `tools/traework-probe.mjs strings` 重新核对常量后更新本文件
//! （该工具的 `decrypt`/`dump`/`sql` 子命令也用于重新校准 schema）。

use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use sha2::{Sha256, Sha512};
use std::path::{Path, PathBuf};

pub const PAGE_SZ: usize = 4096;
pub const SALT_SZ: usize = 16;
pub const IV_SZ: usize = 16;
pub const HMAC_SZ: usize = 64;
/// 页尾预留区：16 字节 IV + 64 字节 HMAC-SHA512
pub const RESERVE_SZ: usize = 80;
const SQLITE_HDR: &[u8; 16] = b"SQLite format 3\0";

/// 配置键 `mics_sj10gy` 对应的加密密码（ai_agent.dll 硬编码，32 字节）
const ENCRYPTED_PASSWORD: [u8; 32] = [
    0x45, 0x2A, 0x17, 0x35, 0x1D, 0x19, 0x1E, 0x13, 0x36, 0x09, 0x14, 0x01, 0x2D, 0x4E, 0x2E,
    0x00, 0x17, 0x5B, 0x24, 0x1D, 0x0F, 0x0A, 0x38, 0x09, 0x1F, 0x21, 0x21, 0x0E, 0x24, 0x18,
    0x12, 0x53,
];
/// 多表 XOR 解密表（依次与每个字节异或，计数各自按表长循环）。
/// 三个计数器的联合周期是 lcm(4,3,8)=24 字节（状态空间 4×3×8=96 只是上界）。
const XOR_TABLES: [&[u8]; 3] = [b"rust", b"cpp", b"electron"];
/// PBKDF2 salt：字面量 `123456789abcdef01122334455667788` 的十六进制字节
const PBKDF2_SALT: [u8; 16] = [
    0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
    0x88,
];
const PBKDF2_ITERS: u32 = 100_000;

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

/// 步骤 1：多表 XOR 解密出固定明文密码
fn xor_decrypt(data: &[u8]) -> Vec<u8> {
    let mut counters = [0usize; 3];
    data.iter()
        .map(|&b| {
            let mut v = b;
            for (i, table) in XOR_TABLES.iter().enumerate() {
                v ^= table[counters[i] % table.len()];
                counters[i] = (counters[i] + 1) % table.len();
            }
            v
        })
        .collect()
}

/// 步骤 2：PBKDF2-HMAC-SHA256 派生 32 字节数据库密钥（惰性缓存，全局只算一次）
pub fn derive_key() -> &'static [u8; 32] {
    use std::sync::OnceLock;
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut password = xor_decrypt(&ENCRYPTED_PASSWORD);
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(&password, &PBKDF2_SALT, PBKDF2_ITERS, &mut key);
        // 明文密码用完即覆写清零（§2.7d，尽力而为）：少留一份在堆上的明文凭据
        crate::watch::wipe_secret(&mut password);
        key
    })
}

/// SQLCipher 4 的 HMAC 密钥：PBKDF2-HMAC-SHA512(key, salt^0x3a, 2, 32)
fn derive_mac_key(enc_key: &[u8], salt: &[u8]) -> [u8; 32] {
    let mac_salt: Vec<u8> = salt.iter().map(|&b| b ^ 0x3a).collect();
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha512>(enc_key, &mac_salt, 2, &mut out);
    out
}

/// 单页 HMAC-SHA512 校验（SQLCipher 4 的页级完整性，§2.19）。
///
/// - `mac_key` 由**页 1 前 16 字节的盐**派生（见 [`derive_mac_key`]），SQLCipher 的
///   盐全库相同，所以逐页共用同一把 mac_key；
/// - 页号以小端 u32 计入 HMAC；
/// - 覆盖域：页 1 从 salt 之后起算（密文+IV），其余页从页首起算（密文+IV）。
///
/// 逐页校验的意义：`fs::read` 一把读全库可能撞上 WAL checkpoint，拿到新旧混合
/// 世代页——只验页 1 时多数报 malformed 可自愈，但 b-tree 混代可能产出**能查但
/// 值错乱**的行（turn_status 陈旧值 → 漏发/误发终态跃迁）。
pub fn verify_page(page: &[u8], pgno: u32, mac_key: &[u8]) -> bool {
    if page.len() < PAGE_SZ {
        return false;
    }
    let start = if pgno == 1 { SALT_SZ } else { 0 };
    let mut mac = Hmac::<Sha512>::new_from_slice(mac_key).expect("HMAC 接受任意长度密钥");
    mac.update(&page[start..PAGE_SZ - RESERVE_SZ + IV_SZ]);
    mac.update(&pgno.to_le_bytes());
    mac.verify_truncated_left(&page[PAGE_SZ - HMAC_SZ..PAGE_SZ]).is_ok()
}

/// 页 1 的 HMAC 校验（密钥对不对只看这一页——「TraeWork 换了密钥算法」的探测点）。
pub fn verify_page1(page1: &[u8], enc_key: &[u8]) -> bool {
    if page1.len() < PAGE_SZ {
        return false;
    }
    let mac_key = derive_mac_key(enc_key, &page1[..SALT_SZ]);
    verify_page(page1, 1, &mac_key)
}

/// 校验数据库与当前密钥是否匹配（只读 1 页，供 is_available 探测用）
pub fn verify_db(src: &Path) -> anyhow::Result<()> {
    use std::io::Read;
    let mut page1 = [0u8; PAGE_SZ];
    std::fs::File::open(src)
        .map_err(|e| anyhow::anyhow!("打开会话库失败: {e}"))?
        .read_exact(&mut page1)
        .map_err(|e| anyhow::anyhow!("读取会话库首页失败: {e}"))?;
    if !verify_page1(&page1, derive_key()) {
        anyhow::bail!(
            "TraeWork 会话库密钥校验失败：TraeWork 可能升级了密钥算法，\
             需重新逆向 ai_agent.dll 常量（tools/traework-probe.mjs strings）"
        );
    }
    Ok(())
}

/// 解密单页并**追加**到 `out`（尾部 reserve 80 字节填零还原成普通 SQLite 页）。
/// pgno==1 的页前 16 字节是 salt，密文从偏移 16 开始，还原时换回 SQLite 头。
///
/// 不做任何堆分配（§4.4）：输出由调用方持有并跨页复用；CBC 直接在 `out` 的
/// 工作区**原地**解密——旧实现每页 `enc.to_vec()` + `Vec::with_capacity` 两次
/// 堆分配，75MB 库 1.9 万页就是 3.8 万次无谓分配。行为（明文内容）不变。
fn decrypt_page(enc_key: &[u8], page: &[u8], pgno: u64, out: &mut Vec<u8>) {
    let iv = &page[PAGE_SZ - RESERVE_SZ..PAGE_SZ - RESERVE_SZ + IV_SZ];
    let dec = Aes256CbcDec::new_from_slices(enc_key, iv).expect("AES-256-CBC 密钥/IV 长度固定");
    let (enc, head): (&[u8], &[u8]) = if pgno == 1 {
        (&page[SALT_SZ..PAGE_SZ - RESERVE_SZ], SQLITE_HDR)
    } else {
        (&page[..PAGE_SZ - RESERVE_SZ], &[])
    };
    out.reserve(PAGE_SZ);
    out.extend_from_slice(head);
    let work = out.len();
    out.extend_from_slice(enc);
    // AES-CBC 无填充：密文长度必为 16 的倍数（4000 / 4016 字节），NoPadding
    // 解密的明文长度 == 密文长度，就地解密后无需缩放
    dec.decrypt_padded_mut::<NoPadding>(&mut out[work..])
        .expect("SQLCipher 页面长度必为块对齐");
    out.extend_from_slice(&[0u8; RESERVE_SZ]);
}

// ---------------------------------------------------------------------------
// WAL 叠加：宿主是 SQLite WAL 模式，最新提交在 `database.db-wal` 里
// ---------------------------------------------------------------------------

const WAL_HDR_SZ: usize = 32;
const WAL_FRAME_HDR_SZ: usize = 24;

/// `<db>` 对应的 `-wal` 路径（SQLite 命名约定：库名后缀拼接）。
/// 轮询指纹（watch.rs）也要靠它盯住 WAL 的变化。
pub fn wal_path(src: &Path) -> PathBuf {
    let mut os = src.as_os_str().to_os_string();
    os.push("-wal");
    PathBuf::from(os)
}

/// WAL 校验和（SQLite `walChecksumBytes`）：数据按 8 字节一对 u32 累加
/// `s1 += a + s2; s2 += b + s1`（wrapping），seed 传上一段的运行值（头为 [0,0]）。
/// `le_words` = 词按小端读（词序实测见 [`WalLayout::detect`]）。
fn wal_checksum(data: &[u8], seed: [u32; 2], le_words: bool) -> [u32; 2] {
    let mut s = seed;
    for pair in data.chunks_exact(8) {
        let word = |i: usize| {
            let b = [pair[i], pair[i + 1], pair[i + 2], pair[i + 3]];
            if le_words {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            }
        };
        s[0] = s[0].wrapping_add(word(0)).wrapping_add(s[1]);
        s[1] = s[1].wrapping_add(word(4)).wrapping_add(s[0]);
    }
    s
}

/// 按指定字节序读一个 u32（`be` = 大端）
fn read_u32(buf: &[u8], be: bool) -> u32 {
    let b = [buf[0], buf[1], buf[2], buf[3]];
    if be {
        u32::from_be_bytes(b)
    } else {
        u32::from_le_bytes(b)
    }
}

/// WAL 校验和的词序 / 存储序。
///
/// wal.c 注释的字面描述（magic 0x377f0682 → 大端词、校验和恒大端存储）与本机
/// 真实库（magic 0x377f0682）实测**相反**：小端词 + 大端存储才能验通头校验和与
/// 1255 帧的整条链（2026-09-24 实测）。以实测为准，且不赌注：四种组合逐个试头
/// 校验和，吻合者胜——头只 8 次 u32 运算，帧链会跟着头一起走，试错成本可忽略。
struct WalLayout {
    le_words: bool,
    be_stored: bool,
}

impl WalLayout {
    fn detect(hdr: &[u8]) -> Option<WalLayout> {
        for le_words in [true, false] {
            for be_stored in [true, false] {
                let ck = wal_checksum(&hdr[..WAL_HDR_SZ - 8], [0, 0], le_words);
                if read_u32(&hdr[24..28], be_stored) == ck[0]
                    && read_u32(&hdr[28..32], be_stored) == ck[1]
                {
                    return Some(WalLayout { le_words, be_stored });
                }
            }
        }
        None
    }
}

/// 把 `-wal` 的有效帧按序叠加到明文主库上（SQLite WAL 恢复语义的页面级实现）。
///
/// 有效性三层过滤，全部吻合才应用：
/// 1. **checksum 链**（wal.c 同款，从头校验和往下连）——RESTART checkpoint 后
///    残留在文件尾部的陈旧帧链不上（它们的运行值来自旧世代），自然排除；
/// 2. **帧头 salt** 与 WAL 头一致（TRUNCATE checkpoint 重写头后，旧世代帧同理排除）；
/// 3. **页级 HMAC**（与主库同一把 mac_key、同页号）——撕裂/半写帧到此为止。
///
/// 只应用到**最后一个提交帧**（帧头 dbsize 字段非 0 者）为止：撕裂尾巴上的
/// 不完整多帧事务不落半份；存在有效帧时最终库大小 = 最后提交帧的 dbsize（页数），
/// 与 SQLite 恢复行为一致。
///
/// 结构性异常（魔数/头校验和对不上）返回 Err，由调用方降级为「本轮主库视角」——
/// `-wal` 是补全视角，宁可看旧一点也不整轮失败：`is_available` 的探针结果会缓存
/// 一整个进程生命周期，撞上 TRUNCATE checkpoint 的撕裂瞬间就永久「不可用」太伤。
fn overlay_wal(
    plain: &mut Vec<u8>,
    wal: &[u8],
    key: &[u8; 32],
    mac_key: &[u8; 32],
) -> anyhow::Result<()> {
    if wal.len() < WAL_HDR_SZ {
        return Ok(()); // 空 WAL（0 字节或只有头）：没有待叠加的提交
    }
    let magic = read_u32(&wal[..4], true);
    anyhow::ensure!(
        magic == 0x377f_0682 || magic == 0x377f_0683,
        "WAL 魔数不符（不是 SQLite WAL 或文件已损坏）: 0x{magic:08x}"
    );
    let layout = WalLayout::detect(wal).ok_or_else(|| anyhow::anyhow!("WAL 头校验和不符"))?;
    let frame_sz = WAL_FRAME_HDR_SZ + PAGE_SZ;
    let n = (wal.len() - WAL_HDR_SZ) / frame_sz; // 尾部半帧（宿主正在写）直接忽略

    // 第一遍：沿 checksum 链圈出有效帧边界，找最后一个提交帧
    let mut running = wal_checksum(&wal[..WAL_HDR_SZ - 8], [0, 0], layout.le_words);
    let mut last_commit: Option<(usize, u32)> = None; // (有效帧下标, 提交后库页数)
    for i in 0..n {
        let off = WAL_HDR_SZ + i * frame_sz;
        let head = &wal[off..off + WAL_FRAME_HDR_SZ];
        let page = &wal[off + WAL_FRAME_HDR_SZ..off + frame_sz];
        let pgno = read_u32(head, true);
        let commit_pages = read_u32(&head[4..8], true);
        if pgno == 0 || head[8..16] != wal[16..24] {
            break; // 旧世代 / 坏帧
        }
        let r = wal_checksum(&head[..8], running, layout.le_words);
        let r = wal_checksum(page, r, layout.le_words);
        if read_u32(&head[16..20], layout.be_stored) != r[0]
            || read_u32(&head[20..24], layout.be_stored) != r[1]
        {
            break; // 链断：陈旧尾帧 / 撕裂半帧
        }
        if !verify_page(page, pgno, mac_key) {
            break; // 页级 HMAC 失配：防御性兜底，绝不解出可疑页
        }
        running = r;
        if commit_pages > 0 {
            last_commit = Some((i, commit_pages));
        }
    }

    // 第二遍：解密并覆盖到最后一个提交帧为止（无提交帧 = 全是不完整事务尾巴）
    let Some((commit_idx, commit_pages)) = last_commit else {
        return Ok(());
    };
    let mut scratch = Vec::with_capacity(PAGE_SZ); // 跨帧复用（§4.4）
    for i in 0..=commit_idx {
        let off = WAL_HDR_SZ + i * frame_sz;
        let page = &wal[off + WAL_FRAME_HDR_SZ..off + frame_sz];
        let pgno = read_u32(&wal[off..], true) as usize;
        scratch.clear();
        decrypt_page(key, page, pgno as u64, &mut scratch);
        let at = (pgno - 1) * PAGE_SZ;
        if plain.len() < at + PAGE_SZ {
            plain.resize(at + PAGE_SZ, 0);
        }
        plain[at..at + PAGE_SZ].copy_from_slice(&scratch);
    }
    // SQLite 恢复语义：最终库大小 = 最后提交帧记录的 dbsize
    plain.resize(commit_pages as usize * PAGE_SZ, 0);
    Ok(())
}

/// 页面级解密整个库（含 `-wal` 叠加）→ 明文 SQLite 文件内容。
///
/// 解密**前逐页验 HMAC**（§2.19）：`fs::read` 一把读全库可能撞上 WAL checkpoint，
/// 拿到新旧混合世代页——只验页 1 时混代页可能产出「能查但值错乱」的行。SQLCipher 4
/// 每页自带 HMAC，逐页验过才解密；任一页失配当轮 bail（宿主写完下一轮自愈），
/// 绝不产出半真半假的明文库。
pub fn decrypt_db(src: &Path) -> anyhow::Result<Vec<u8>> {
    let data = std::fs::read(src).map_err(|e| anyhow::anyhow!("读取会话库失败: {e}"))?;
    anyhow::ensure!(
        data.len() >= PAGE_SZ && data.len() % PAGE_SZ == 0,
        "会话库大小 {} 不是 {} 的整数倍（文件可能损坏或格式已变）",
        data.len(),
        PAGE_SZ
    );
    let key = derive_key();
    // mac_key 的盐取页 1 前 16 字节（全库相同），逐页共用
    let mac_key = derive_mac_key(key, &data[..SALT_SZ]);
    let mut out = Vec::with_capacity(data.len());
    for (i, page) in data.chunks(PAGE_SZ).enumerate() {
        let pgno = (i + 1) as u32;
        anyhow::ensure!(
            verify_page(page, pgno, &mac_key),
            "TraeWork 会话库第 {pgno} 页 HMAC 校验失败（撕裂快照 / 混合世代页，或密钥算法已随版本更换，\
             后者需重新核对 ai_agent.dll 常量：tools/traework-probe.mjs strings）。本轮放弃，下一轮自愈"
        );
        decrypt_page(key, page, pgno as u64, &mut out);
    }
    // WAL 叠加：最新提交可能整个还在 -wal 里（模块头注释，2026-09-24 实测 4-5MB /
    // 上千帧是常态）。叠加失败只退回主库视角并留痕，不整轮失败——探针的可用性判定
    // 会缓存一整个进程生命周期，宁可这一轮看旧一点也不把它永久判死；下一轮自愈。
    match std::fs::read(&wal_path(src)) {
        Ok(bytes) => {
            if let Err(e) = overlay_wal(&mut out, &bytes, key, &mac_key) {
                tracing::warn!(
                    "TraeWork 会话库 -wal 叠加失败，本轮退回主库视角（下一轮自愈）：{e:#}"
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            "TraeWork 会话库 -wal 读取失败，本轮退回主库视角（下一轮自愈）：{e}"
        ),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;

    /// 已知答案：XOR 解密出的固定密码（DirWang 逆向 + 本机 DLL 常量实测）
    #[test]
    fn xor_decrypts_known_password() {
        assert_eq!(xor_decrypt(&ENCRYPTED_PASSWORD), b"1CqAknayQsrfH9Byp2QzynTckHGzRom9".to_vec());
    }

    /// 已知答案：PBKDF2 派生密钥（社区公开值，本机 TraeWork CN 0.1.69 库 HMAC 实测通过）
    #[test]
    fn derives_known_key() {
        assert_eq!(
            derive_key().as_slice(),
            hex_decode("3605f6691095a993f03d5009c918352ef5be31ae31e8f000212b81ff058da773").as_slice()
        );
    }

    /// 页面加解密往返：验证 salt/IV/密文偏移与「头重建 + reserve 填零」的还原逻辑
    #[test]
    fn page_roundtrip() {
        let key = derive_key();
        // 构造两页明文：页 1 带 SQLite 头，页尾 80 字节 reserve 全零
        let mut plain = vec![0u8; PAGE_SZ * 2];
        plain[..16].copy_from_slice(SQLITE_HDR);
        for (i, b) in plain[16..PAGE_SZ * 2].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        for end in [PAGE_SZ, 2 * PAGE_SZ] {
            for b in &mut plain[end - RESERVE_SZ..end] {
                *b = 0;
            }
        }
        let enc = encrypt_fixture(key, &plain);
        let mut out = Vec::new();
        for pgno in 1..=2u64 {
            let page = &enc[(pgno as usize - 1) * PAGE_SZ..pgno as usize * PAGE_SZ];
            decrypt_page(key, page, pgno, &mut out);
        }
        assert_eq!(&out, &plain[..]);
        assert!(verify_page1(&enc[..PAGE_SZ], key));
        assert!(!verify_page1(&enc[..PAGE_SZ], &[0u8; 32]));
    }

    /// §4.4：decrypt_page 复用调用方 buffer、原地 CBC——追加写入且不破坏已有内容
    #[test]
    fn decrypt_page_appends_into_caller_buffer_without_alloc() {
        let key = derive_key();
        let mut plain = vec![0u8; PAGE_SZ * 2];
        plain[..16].copy_from_slice(SQLITE_HDR);
        for (i, b) in plain[16..PAGE_SZ * 2].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        for end in [PAGE_SZ, 2 * PAGE_SZ] {
            for b in &mut plain[end - RESERVE_SZ..end] {
                *b = 0;
            }
        }
        let enc = encrypt_fixture(key, &plain);
        // 调用方 buffer 预留好容量、前面带一段已有内容：解密结果只能追加在其后
        let mut out = Vec::with_capacity(16 + PAGE_SZ * 2);
        out.extend_from_slice(b"prefix-not-a-page!");
        let prefix = out.len();
        for pgno in 1..=2u64 {
            let page = &enc[(pgno as usize - 1) * PAGE_SZ..pgno as usize * PAGE_SZ];
            decrypt_page(key, page, pgno, &mut out);
        }
        assert_eq!(&out[..prefix], b"prefix-not-a-page!", "已有内容不得被破坏");
        assert_eq!(&out[prefix..], &plain[..], "逐页追加的明文必须逐字节正确");
    }

    /// §2.19：任意一页 HMAC 被篡改 → 整库拒绝解密（当轮 bail，下轮自愈）
    #[test]
    fn tampered_page_hmac_rejects_whole_db() {
        let key = derive_key();
        let mut plain = vec![0u8; PAGE_SZ * 3];
        plain[..16].copy_from_slice(SQLITE_HDR);
        for (i, b) in plain[16..].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        for end in [PAGE_SZ, 2 * PAGE_SZ, 3 * PAGE_SZ] {
            for b in &mut plain[end - RESERVE_SZ..end] {
                *b = 0;
            }
        }
        let mut enc = encrypt_fixture(key, &plain);
        let dir = std::env::temp_dir().join(format!("agent-bark-twdb-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db");
        std::fs::write(&path, &enc).unwrap();
        // 未篡改：整库解密成功
        assert_eq!(decrypt_db(&path).unwrap().len(), PAGE_SZ * 3);

        // 篡改中间页的 HMAC 一个字节 → 整库拒绝
        enc[2 * PAGE_SZ - 1] ^= 0x01;
        std::fs::write(&path, &enc).unwrap();
        let err = decrypt_db(&path).expect_err("篡改页 HMAC 必须拒绝整库");
        assert!(format!("{err:#}").contains("第 2 页"), "报错要指明失配页: {err:#}");
        // 页 1 被篡改同样拒绝
        let mut enc2 = encrypt_fixture(key, &plain);
        enc2[PAGE_SZ - 1] ^= 0x01;
        std::fs::write(&path, &enc2).unwrap();
        assert!(decrypt_db(&path).is_err(), "页 1 HMAC 失配必须拒绝");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §2.19：mac_key 由页 1 盐派生、全库共用——每一页都验得过
    #[test]
    fn shared_mac_key_verifies_every_page() {
        let key = derive_key();
        let mut plain = vec![0u8; PAGE_SZ * 3];
        plain[..16].copy_from_slice(SQLITE_HDR);
        let enc = encrypt_fixture(key, &plain);
        let mac_key = derive_mac_key(key, &enc[..SALT_SZ]);
        for pgno in 1..=3u32 {
            let page = &enc[(pgno as usize - 1) * PAGE_SZ..pgno as usize * PAGE_SZ];
            assert!(verify_page(page, pgno, &mac_key), "第 {pgno} 页应验得过");
            // 换一把 mac_key 逐页都要拒绝（不是只有页 1 在校验）
            assert!(!verify_page(page, pgno, &[0u8; 32]), "第 {pgno} 页用错 mac_key 必须拒绝");
            // 页号必须计入 HMAC：换页号重验要失败
            let other = if pgno == 1 { 2 } else { 1 };
            assert!(!verify_page(page, other, &mac_key), "第 {pgno} 页按错误页号重验必须失败");
        }
    }

    /// §4.13：xor_decrypt 的三个计数器联合周期是 lcm(4,3,8)=**24** 字节
    /// （96=4×3×8 只是状态空间上界）——用零输入展开密钥流直接验证
    #[test]
    fn xor_keystream_has_joint_period_24() {
        let ks = xor_decrypt(&[0u8; 96]); // 密钥流 = 与零异或
        for i in 0..72 {
            assert_eq!(ks[i], ks[i + 24], "联合周期必须是 24（位置 {i}）");
        }
        // 周期不是更小的 12（防止有人把 lcm 写小）
        assert!(
            (0..24).any(|i| ks[i] != ks[i + 12]),
            "周期 24 是最小联合周期，不是 12"
        );
    }

    /// verify_page 对短页直接拒绝（不越界）；decrypt_db 对非对齐大小明确报错
    #[test]
    fn verify_page_and_decrypt_db_reject_malformed_input() {
        assert!(!verify_page(&[0u8; 80], 1, &[0u8; 32]));
        assert!(!verify_page(&[], 1, &[0u8; 32]));
        let dir = std::env::temp_dir().join(format!("agent-bark-twdb2-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.db");
        std::fs::write(&p, vec![0u8; PAGE_SZ + 3]).unwrap();
        let err = decrypt_db(&p).expect_err("非对齐大小必须拒绝");
        assert!(format!("{err:#}").contains("整数倍"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// mac_key 与盐强相关：换盐派生的 mac_key 验不过原库任何一页
    #[test]
    fn mac_key_depends_on_page1_salt() {
        let key = derive_key();
        let plain = vec![0u8; PAGE_SZ];
        let enc = encrypt_fixture(key, &plain);
        let good = derive_mac_key(key, &enc[..SALT_SZ]);
        let wrong = derive_mac_key(key, &[0u8; SALT_SZ]);
        assert_ne!(good, wrong, "盐必须参与 mac_key 派生");
        assert!(verify_page(&enc[..PAGE_SZ], 1, &good));
        assert!(!verify_page(&enc[..PAGE_SZ], 1, &wrong));
    }

    /// verify_page1 是 verify_page(pgno=1) 的薄封装：行为一致（含密钥探测语义）
    #[test]
    fn verify_page1_wraps_verify_page_consistently() {
        let key = derive_key();
        let plain = vec![0u8; PAGE_SZ];
        let enc = encrypt_fixture(key, &plain);
        let mac_key = derive_mac_key(key, &enc[..SALT_SZ]);
        assert_eq!(
            verify_page1(&enc[..PAGE_SZ], key),
            verify_page(&enc[..PAGE_SZ], 1, &mac_key)
        );
        // 密钥探测点：错误密钥恒 false（「TraeWork 换了密钥算法」时的报错来源）
        assert!(!verify_page1(&enc[..PAGE_SZ], &[1u8; 32]));
    }

    /// ---- WAL 叠加（恢复语义回归，2026-09-24「两次提问无光效」事故）----

    /// 明文页尾 reserve 置零（encrypt_fixture 的前置要求）
    fn zero_reserve(plain: &mut [u8], pages: usize) {
        for end in 1..=pages {
            for b in &mut plain[end * PAGE_SZ - RESERVE_SZ..end * PAGE_SZ] {
                *b = 0;
            }
        }
    }

    /// WAL 夹具：把 (页号, 加密页映像, 提交后库页数) 编成一份合法 `-wal`
    /// （标准 32B 头 + 24B 帧头 + 链式校验和；词序/存储序与真实库一致：读词 LE + 存储 BE）。
    /// 提交后库页数非 0 的帧 = 提交帧。
    fn wal_fixture(frames: &[(u32, &[u8], u32)]) -> Vec<u8> {
        const SALT1: u32 = 0x1122_3344;
        const SALT2: u32 = 0x5566_7788;
        let mut wal = Vec::new();
        wal.extend_from_slice(&0x377f_0682u32.to_be_bytes()); // magic
        wal.extend_from_slice(&3007000u32.to_be_bytes()); // 格式版本
        wal.extend_from_slice(&(PAGE_SZ as u32).to_be_bytes());
        wal.extend_from_slice(&0u32.to_be_bytes()); // checkpoint 序号
        wal.extend_from_slice(&SALT1.to_be_bytes());
        wal.extend_from_slice(&SALT2.to_be_bytes());
        let mut running = wal_checksum(&wal[..24], [0, 0], true);
        wal.extend_from_slice(&running[0].to_be_bytes());
        wal.extend_from_slice(&running[1].to_be_bytes());
        for &(pgno, page, commit) in frames {
            let mut head = Vec::with_capacity(WAL_FRAME_HDR_SZ);
            head.extend_from_slice(&pgno.to_be_bytes());
            head.extend_from_slice(&commit.to_be_bytes());
            head.extend_from_slice(&SALT1.to_be_bytes());
            head.extend_from_slice(&SALT2.to_be_bytes());
            let r = wal_checksum(&head[..8], running, true);
            let r = wal_checksum(page, r, true);
            head.extend_from_slice(&r[0].to_be_bytes());
            head.extend_from_slice(&r[1].to_be_bytes());
            wal.extend_from_slice(&head);
            wal.extend_from_slice(page);
            running = r;
        }
        wal
    }

    /// 建一个「密文主库 + 可选 -wal」的临时库文件，返回路径
    fn write_fixture_db(name: &str, enc: &[u8], wal: Option<&[u8]>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-bark-twdb-{name}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("database.db");
        std::fs::write(&db, enc).unwrap();
        if let Some(w) = wal {
            std::fs::write(wal_path(&db), w).unwrap();
        }
        db
    }

    /// 基础明文库 2 页（页 1 带 SQLite 头）+ 对应密文，及「WAL 新世代」页 2/页 3 的密文映像
    fn two_page_fixture(key: &[u8; 32]) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut base = vec![0u8; PAGE_SZ * 2];
        base[..16].copy_from_slice(SQLITE_HDR);
        for (i, b) in base[16..].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        zero_reserve(&mut base, 2);
        let enc_base = encrypt_fixture(key, &base);

        // 新世代：页 2 整页 0x22、页 3 整页 0x33（reserve 置零后加密）
        let mut gen = vec![0u8; PAGE_SZ * 3];
        gen[..16].copy_from_slice(SQLITE_HDR);
        for b in &mut gen[PAGE_SZ..2 * PAGE_SZ] {
            *b = 0x22;
        }
        for b in &mut gen[2 * PAGE_SZ..] {
            *b = 0x33;
        }
        zero_reserve(&mut gen, 3);
        let enc_gen = encrypt_fixture(key, &gen);
        let p2 = enc_gen[PAGE_SZ..2 * PAGE_SZ].to_vec();
        let p3 = enc_gen[2 * PAGE_SZ..3 * PAGE_SZ].to_vec();
        (base, enc_base, p2, p3)
    }

    /// 提交帧叠加：页 2 换新、页 3 长出来；陈旧世代尾帧（HMAC 合法但 checksum 链
    /// 来自旧世代）必须被链式校验排除——它想把库缩回 2 页并把页 2 覆盖回旧值
    #[test]
    fn wal_overlay_applies_committed_frames_and_excludes_stale_tail() {
        let key = derive_key();
        let (base, enc_base, p2, p3) = two_page_fixture(key);
        let mut wal = wal_fixture(&[(2, &p2, 2), (3, &p3, 3)]);
        // stale 尾帧：单帧自成一链（校验和从头算起），接在 2 帧之后必然断链
        let stale = wal_fixture(&[(2, &enc_base[PAGE_SZ..2 * PAGE_SZ], 2)]);
        wal.extend_from_slice(&stale[WAL_HDR_SZ..]);

        let db = write_fixture_db("wal1", &enc_base, Some(&wal));
        let out = decrypt_db(&db).unwrap();
        assert_eq!(out.len(), PAGE_SZ * 3, "库大小 = 最后提交帧 dbsize");
        assert_eq!(&out[..16], &SQLITE_HDR[..], "页 1 头不被 WAL 破坏");
        assert_eq!(&out[16..PAGE_SZ], &base[16..PAGE_SZ], "页 1 其余内容 = 主库值");
        assert!(
            out[PAGE_SZ..2 * PAGE_SZ - RESERVE_SZ].iter().all(|&b| b == 0x22),
            "页 2 必须是 WAL 新世代（陈旧尾帧被排除）"
        );
        assert!(
            out[2 * PAGE_SZ..3 * PAGE_SZ - RESERVE_SZ].iter().all(|&b| b == 0x33),
            "页 3 应由提交帧长出来"
        );
        assert!(
            out[3 * PAGE_SZ - RESERVE_SZ..].iter().all(|&b| b == 0),
            "页 3 的 reserve 还原为零"
        );
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    /// 未提交尾巴（撕裂的多帧事务）不落半份：只有走到提交帧的内容才可见
    #[test]
    fn wal_overlay_drops_uncommitted_tail() {
        let key = derive_key();
        let (_base, enc_base, p2, p3) = two_page_fixture(key);
        let wal = wal_fixture(&[(2, &p2, 2), (3, &p3, 0)]); // 第二帧未提交
        let db = write_fixture_db("wal2", &enc_base, Some(&wal));
        let out = decrypt_db(&db).unwrap();
        assert_eq!(out.len(), PAGE_SZ * 2, "未提交帧不能把库长出去");
        assert!(
            out[PAGE_SZ..2 * PAGE_SZ - RESERVE_SZ].iter().all(|&b| b == 0x22),
            "已提交的页 2 新世代要可见"
        );
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    /// 全程没有提交帧（只有不完整事务）：WAL 等于不存在
    #[test]
    fn wal_overlay_without_commit_frames_is_noop() {
        let key = derive_key();
        let (base, enc_base, p2, _p3) = two_page_fixture(key);
        let wal = wal_fixture(&[(2, &p2, 0)]);
        let db = write_fixture_db("wal3", &enc_base, Some(&wal));
        let out = decrypt_db(&db).unwrap();
        assert_eq!(out.len(), PAGE_SZ * 2);
        assert_eq!(&out[PAGE_SZ..], &base[PAGE_SZ..], "页 2 保持主库值");
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    /// 结构性坏 WAL（坏魔数 / 头校验和不符 / 截断垃圾）不炸整轮：
    /// 退回主库视角照常返回（可用性探针不能被 TRUNCATE checkpoint 的撕裂瞬间判死）
    #[test]
    fn wal_structural_garbage_falls_back_to_main_db() {
        let key = derive_key();
        let (base, enc_base, ..) = two_page_fixture(key);
        let bad_magic = [0xABu8; WAL_HDR_SZ * 2];
        let bad_hdr_cksum = {
            let mut w = wal_fixture(&[(2, &enc_base[PAGE_SZ..2 * PAGE_SZ], 2)]);
            w[WAL_HDR_SZ - 1] ^= 0xFF; // 头校验和被破坏
            w
        };
        let partial = [0x37u8, 0x7f, 0x06, 0x82, 0x00, 0x01, 0x02]; // 比头还短
        for (name, wal) in [("bad-magic", bad_magic.as_slice()), ("bad-hdr", bad_hdr_cksum.as_slice()), ("partial", partial.as_slice())] {
            let db = write_fixture_db(name, &enc_base, Some(wal));
            let out = decrypt_db(&db).unwrap_or_else(|e| panic!("{name} 应退回主库视角: {e:#}"));
            assert_eq!(out.len(), PAGE_SZ * 2, "{name}");
            assert_eq!(&out[..], &base[..], "{name}：主库内容逐字节不变");
            let _ = std::fs::remove_dir_all(db.parent().unwrap());
        }
    }

    /// 布局探测靠头校验和认亲：头被破坏时四种词序/存储序组合都不得误认
    #[test]
    fn wal_layout_detect_requires_valid_header_checksum() {
        let good = wal_fixture(&[]);
        assert!(WalLayout::detect(&good).is_some(), "合法头必须探得出布局");
        let mut bad = good.clone();
        bad[WAL_HDR_SZ - 1] ^= 0xFF;
        assert!(WalLayout::detect(&bad).is_none(), "坏头不得误认布局");
    }

    /// 测试夹具：按 SQLCipher 4 页面布局加密明文库（与 decrypt_page 严格互逆）。
    /// HMAC 覆盖域：页 1 从 salt 之后起算（密文+IV），其余页从页首起算，末尾加页号。
    fn encrypt_fixture(key: &[u8; 32], plain: &[u8]) -> Vec<u8> {
        type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
        let salt = [7u8; SALT_SZ];
        let iv = [9u8; IV_SZ];
        let mac_key = derive_mac_key(key, &salt);
        let mut out = Vec::with_capacity(plain.len());
        for (i, chunk) in plain.chunks(PAGE_SZ).enumerate() {
            let pgno = i + 1;
            let mut page = Vec::with_capacity(PAGE_SZ);
            if pgno == 1 {
                page.extend_from_slice(&salt);
            }
            let body = if pgno == 1 {
                &chunk[SALT_SZ..PAGE_SZ - RESERVE_SZ]
            } else {
                &chunk[..PAGE_SZ - RESERVE_SZ]
            };
            let enc = Aes256CbcEnc::new_from_slices(key, &iv).unwrap();
            let mut buf = body.to_vec();
            let ct = enc.encrypt_padded_mut::<NoPadding>(&mut buf, body.len()).unwrap();
            page.extend_from_slice(ct);
            page.extend_from_slice(&iv);
            let start = if pgno == 1 { SALT_SZ } else { 0 };
            let mut mac = Hmac::<Sha512>::new_from_slice(&mac_key).unwrap();
            mac.update(&page[start..]);
            mac.update(&(pgno as u32).to_le_bytes());
            page.extend_from_slice(&mac.finalize().into_bytes());
            out.extend_from_slice(&page);
        }
        out
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
