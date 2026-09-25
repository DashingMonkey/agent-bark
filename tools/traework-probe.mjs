#!/usr/bin/env node
/**
 * TraeWork（TRAE SOLO CN）会话库取证探针
 *
 * 背景：`%APPDATA%\TRAE SOLO CN\ModularData\ai-agent\database.db` 为 SQLCipher 4 加密。
 * Trae CN 的固定密钥方案（Oh-My-Trae/trae-db-decrypt + DirWang 逆向）未必适用于 SOLO CN，
 * 本工具用于：验证候选密钥、扫描 ai_agent.dll 密钥线索、页面级解密、dump 表结构。
 *
 * 用法:
 *   node tools/traework-probe.mjs verify  --key <hex> [--db <path>]
 *   node tools/traework-probe.mjs strings [--dll <path>]
 *   node tools/traework-probe.mjs decrypt --key <hex> --out <path> [--db <path>]
 *   node tools/traework-probe.mjs dump    --db <decrypted.db>
 *   node tools/traework-probe.mjs sql     --db <decrypted.db> --query "select 1"
 *
 * SQLCipher 4 参数（wechat-decrypt 页面级方案）:
 *   AES-256-CBC, HMAC-SHA512, page=4096, salt=16(页1头部), IV=16, HMAC=64, reserve=80
 *
 * WAL 叠加（decrypt 自动开启）：宿主是 SQLite WAL 模式，新提交先落
 * `database.db-wal`（实测常驻 4-5MB / 上千帧），只解主库会漏掉最新会话。按 SQLite
 * WAL 恢复语义把 -wal 帧叠回明文库：逐帧校验 checksum 链 + 页级 HMAC，只应用到
 * 最后一个提交帧。checksum 细节以实测为准（magic 0x377f0682 的真实库是「读词 LE +
 * 存储 BE」，与 wal.c 注释的字面描述相反），故四种组合逐个试头校验和认亲。
 *
 * 健壮性约定（§4.17）：坏路径/缺 flag/页不对齐一律「一句话错误 + 用法 + exit 2」，
 * 不抛裸异常栈；HMAC 校验失败 exit 1（运行结果性失败）；HMAC 比较恒定时间；
 * 输出逐页写盘（不全量驻留拼接）；DLL 扫描窗口式读（不整文件 readFileSync）。
 */
import { openSync, readSync, writeSync, closeSync, statSync, readFileSync, ftruncateSync } from "node:fs";
import { createHmac, pbkdf2Sync, createDecipheriv, timingSafeEqual } from "node:crypto";

const PAGE_SZ = 4096, SALT_SZ = 16, IV_SZ = 16, HMAC_SZ = 64, RESERVE_SZ = 80;
const WAL_HDR_SZ = 32, WAL_FRAME_HDR_SZ = 24;
const SQLITE_HDR = Buffer.from("SQLite format 3\0", "latin1");

// Trae CN 已知固定密钥（未必适用于 SOLO CN，先试）
const TRAE_CN_FIXED_KEY = "3605f6691095a993f03d5009c918352ef5be31ae31e8f000212b81ff058da773";

function usage() {
  console.error("usage:");
  console.error("  node tools/traework-probe.mjs verify  --key <hex> [--db <path>]");
  console.error("  node tools/traework-probe.mjs strings [--dll <path>]");
  console.error("  node tools/traework-probe.mjs decrypt --key <hex> --out <path> [--db <path>]");
  console.error("  node tools/traework-probe.mjs dump    --db <decrypted.db>");
  console.error("  node tools/traework-probe.mjs sql     --db <decrypted.db> --query \"select ;; 1\"");
}

function defaultDb() {
  return process.env.APPDATA + "\\TRAE SOLO CN\\ModularData\\ai-agent\\database.db";
}
function defaultDll() {
  return "D:\\Program Files\\TRAE SOLO CN\\resources\\app\\modules\\ai-agent\\ai_agent.dll";
}

function parseArgs(argv) {
  const out = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith("--")) {
      const name = a.slice(2);
      const val = argv[i + 1];
      // flag 值缺失校验（§4.17）：旧实现在末尾 flag 处拿到 undefined 还继续跑
      if (val === undefined || val.startsWith("--")) {
        throw new Error(`flag --${name} 缺少值`);
      }
      out[name] = val;
      i += 1;
    } else {
      out._.push(a);
    }
  }
  return out;
}

/** 必填 flag 校验 */
function requireFlag(args, ...names) {
  for (const n of names) {
    if (typeof args[n] !== "string" || args[n] === "") {
      throw new Error(`缺少必填 flag --${n}`);
    }
  }
}

/** hex 密钥校验 + 解析（Buffer.from 的 "hex" 对非法字符会静默截断，必须先验） */
function parseKey(hex) {
  if (!/^[0-9a-fA-F]{64}$/.test(hex)) {
    throw new Error(`--key 必须是 64 个十六进制字符（32 字节），收到: ${JSON.stringify(hex)}`);
  }
  return Buffer.from(hex, "hex");
}

/** 打开库并校验页对齐（§4.17：size % PAGE_SZ 必须为 0） */
function openDb(db) {
  let size;
  try {
    size = statSync(db).size;
  } catch (e) {
    throw new Error(`无法读取库文件 ${db}（${e.message}）`);
  }
  if (size < PAGE_SZ || size % PAGE_SZ !== 0) {
    throw new Error(`库大小 ${size} 不是 ${PAGE_SZ} 的整数倍（文件损坏或格式已变）: ${db}`);
  }
  return { fd: openSync(db, "r"), size, total: Math.floor(size / PAGE_SZ) };
}

function deriveMacKey(encKey, salt) {
  const macSalt = Buffer.from(salt.map((b) => b ^ 0x3a));
  return pbkdf2Sync(encKey, macSalt, 2, 32, "sha512");
}

/** SQLCipher 4 单页 HMAC 校验（§2.19）：页号小端 u32 计入；页 1 覆盖域从 salt 后起算。
 *  比较用 crypto.timingSafeEqual（恒定时间，§4.17）。 */
function verifyPage(page, pgno, macKey) {
  const start = pgno === 1 ? SALT_SZ : 0;
  const hmacData = page.subarray(start, PAGE_SZ - RESERVE_SZ + IV_SZ);
  const stored = page.subarray(PAGE_SZ - HMAC_SZ);
  const pgnoBuf = Buffer.alloc(4);
  pgnoBuf.writeUInt32LE(pgno);
  const computed = createHmac("sha512", macKey).update(Buffer.concat([hmacData, pgnoBuf])).digest();
  return computed.length === stored.length && timingSafeEqual(computed, stored);
}

/** 读取一页（调用方持有 buffer，逐页复用） */
function readPage(fd, pgno, buf) {
  readSync(fd, buf, 0, PAGE_SZ, (pgno - 1) * PAGE_SZ);
  return buf;
}

function decryptPage(encKey, page, pgno, out) {
  const iv = page.subarray(PAGE_SZ - RESERVE_SZ, PAGE_SZ - RESERVE_SZ + IV_SZ);
  const decipher = createDecipheriv("aes-256-cbc", encKey, iv);
  decipher.setAutoPadding(false);
  let dec;
  if (pgno === 1) {
    dec = decipher.update(page.subarray(SALT_SZ, PAGE_SZ - RESERVE_SZ));
    SQLITE_HDR.copy(out, 0);
    dec.copy(out, SQLITE_HDR.length);
  } else {
    dec = decipher.update(page.subarray(0, PAGE_SZ - RESERVE_SZ));
    dec.copy(out, 0);
  }
  out.fill(0, PAGE_SZ - RESERVE_SZ);
  return out;
}

// ---- WAL 叠加（与 crates/bark-adapters/src/traework_db.rs 的 overlay_wal 同语义）----

/** WAL 校验和（SQLite walChecksumBytes）：8 字节一对 u32 累加 s1+=a+s2; s2+=b+s1 */
function walChecksum(buf, seed, leWords) {
  let [s0, s1] = seed;
  for (let i = 0; i + 8 <= buf.length; i += 8) {
    const w = (o) => (leWords ? buf.readUInt32LE(i + o) : buf.readUInt32BE(i + o));
    s0 = (s0 + w(0) + s1) >>> 0;
    s1 = (s1 + w(4) + s0) >>> 0;
  }
  return [s0, s1];
}

/** 头校验和认亲布局：读词序 × 存储序四种组合逐个试。
 *  真实库（magic 0x377f0682）实测是「读词 LE + 存储 BE」，与 wal.c 注释的
 *  字面描述相反——不赌注，试到头校验和吻合为止。 */
function detectWalLayout(hdr) {
  for (const leWords of [true, false]) {
    for (const beStored of [true, false]) {
      const ck = walChecksum(hdr.subarray(0, WAL_HDR_SZ - 8), [0, 0], leWords);
      const rd = (o) => (beStored ? hdr.readUInt32BE(o) : hdr.readUInt32LE(o));
      if (rd(24) === ck[0] && rd(28) === ck[1]) return { leWords, beStored };
    }
  }
  return null;
}

/** 帧叠加（SQLite 恢复语义）：checksum 链 + 帧头 salt + 页 HMAC 三层过滤，
 *  只应用到最后一个提交帧；返回 [应用帧数, 总帧数]。结构异常抛一句话错误。 */
function overlayWal(outFd, wal, key, macKey) {
  if (wal.length < WAL_HDR_SZ) return [0, 0];
  const magic = wal.readUInt32BE(0);
  if (magic !== 0x377f0682 && magic !== 0x377f0683) {
    throw new Error(`WAL 魔数不符: 0x${magic.toString(16)}`);
  }
  const layout = detectWalLayout(wal);
  if (!layout) throw new Error("WAL 头校验和不符");
  const { leWords, beStored } = layout;
  const rd = (b, o) => (beStored ? b.readUInt32BE(o) : b.readUInt32LE(o));
  const FRAME = WAL_FRAME_HDR_SZ + PAGE_SZ;
  const total = Math.floor((wal.length - WAL_HDR_SZ) / FRAME);
  let running = walChecksum(wal.subarray(0, WAL_HDR_SZ - 8), [0, 0], leWords);
  let lastCommit = -1, commitPages = 0;
  for (let i = 0; i < total; i++) {
    const off = WAL_HDR_SZ + i * FRAME;
    const head = wal.subarray(off, off + WAL_FRAME_HDR_SZ);
    const page = wal.subarray(off + WAL_FRAME_HDR_SZ, off + FRAME);
    const pgno = head.readUInt32BE(0);
    const commit = head.readUInt32BE(4);
    if (pgno === 0 || !head.subarray(8, 16).equals(wal.subarray(16, 24))) break; // 旧世代/坏帧
    let r = walChecksum(head.subarray(0, 8), running, leWords);
    r = walChecksum(page, r, leWords);
    if (rd(head, 16) !== r[0] || rd(head, 20) !== r[1]) break; // 链断：陈旧尾帧/撕裂半帧
    if (!verifyPage(page, pgno, macKey)) break;
    running = r;
    if (commit > 0) { lastCommit = i; commitPages = commit; }
  }
  if (lastCommit < 0) return [0, total];
  const outPage = Buffer.alloc(PAGE_SZ);
  for (let i = 0; i <= lastCommit; i++) {
    const off = WAL_HDR_SZ + i * FRAME;
    const pgno = wal.readUInt32BE(off);
    decryptPage(key, wal.subarray(off + WAL_FRAME_HDR_SZ, off + FRAME), pgno, outPage);
    writeSync(outFd, outPage, 0, PAGE_SZ, (pgno - 1) * PAGE_SZ);
  }
  ftruncateSync(outFd, commitPages * PAGE_SZ); // 最终库大小 = 最后提交帧 dbsize
  return [lastCommit + 1, total];
}

/** 叠加 `<db>-wal`；结构异常只降级为「仅主库视角」（与 Rust 侧软降级一致），返回说明文字 */
function applyWalOverlay(outFd, db, key, macKey) {
  let wal;
  try {
    wal = readFileSync(db + "-wal");
  } catch (e) {
    return e.code === "ENOENT" ? "无 -wal" : `-wal 读取失败（${e.message}），仅主库视角`;
  }
  try {
    const [applied, total] = overlayWal(outFd, wal, key, macKey);
    return `-wal 叠加 ${applied}/${total} 帧`;
  } catch (e) {
    return `-wal 叠加跳过（${e.message}），仅主库视角`;
  }
}

function cmdVerify(args) {
  const db = args.db || defaultDb();
  const key = args.key ? parseKey(args.key) : Buffer.from(TRAE_CN_FIXED_KEY, "hex");
  const { fd, size } = openDb(db);
  console.log(`db: ${db} (${(size / 1048576).toFixed(1)}MB)`);
  console.log(`key: ${key.toString("hex")}`);
  const page1 = readPage(fd, 1, Buffer.alloc(PAGE_SZ));
  closeSync(fd);
  const macKey = deriveMacKey(key, page1.subarray(0, SALT_SZ));
  const ok = verifyPage(page1, 1, macKey);
  console.log(ok ? "HMAC VERIFY: PASS ✅" : "HMAC VERIFY: FAIL ❌");
  process.exit(ok ? 0 : 1);
}

function cmdDecrypt(args) {
  requireFlag(args, "key", "out");
  const db = args.db || defaultDb();
  const key = parseKey(args.key);
  const { fd, size, total } = openDb(db);
  console.log(`共 ${total} 页，解密中...`);
  let walNote = "无 -wal";
  const page = Buffer.alloc(PAGE_SZ);
  const outPage = Buffer.alloc(PAGE_SZ);
  // 先取页 1 的盐派生 mac_key（全库相同），随后**逐页验 HMAC 再解密**（§2.19）：
  // 一把读全库撞上 WAL checkpoint 会得到新旧混合世代页，混代页可能解出
  // 「能查但值错乱」的行——任一页失配就拒绝整库（当轮 bail，下轮自愈）
  readPage(fd, 1, page);
  const macKey = deriveMacKey(key, page.subarray(0, SALT_SZ));
  let outFd;
  try {
    outFd = openSync(args.out, "w");
  } catch (e) {
    closeSync(fd);
    throw new Error(`无法写输出文件 ${args.out}（${e.message}）`);
  }
  try {
    for (let pgno = 1; pgno <= total; pgno++) {
      readPage(fd, pgno, page);
      if (!verifyPage(page, pgno, macKey)) {
        closeSync(fd);
        closeSync(outFd);
        console.error(`HMAC 校验失败：第 ${pgno} 页（撕裂快照/混合世代页，或密钥不对），拒绝输出`);
        process.exit(1);
      }
      decryptPage(key, page, pgno, outPage);
      // 逐页写盘（§4.17）：75MB 库不再全量驻留 + concat 二次拷贝
      writeSync(outFd, outPage, 0, PAGE_SZ, (pgno - 1) * PAGE_SZ);
    }
    walNote = applyWalOverlay(outFd, db, key, macKey);
  } finally {
    closeSync(fd);
    closeSync(outFd);
  }
  console.log(`解密完成: ${args.out}（${size} 字节入，${total} 页逐页验过 HMAC；${walNote}）`);
}

const DUMP_SQL = `
SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type, name;
`;

/** 表名插值进 SQL 前的引用转义（§4.17：表名含引号会生成坏 SQL） */
function quoteIdent(name) {
  return `"${String(name).replace(/"/g, '""')}"`;
}

async function cmdDump(args) {
  requireFlag(args, "db");
  const { DatabaseSync } = await import("node:sqlite");
  const db = new DatabaseSync(args.db, { readOnly: true });
  const rows = db.prepare(DUMP_SQL).all();
  for (const r of rows) {
    console.log(`--- [${r.type}] ${r.name}`);
    console.log(r.sql);
  }
  // 每张表行数
  const tables = db.prepare("SELECT name FROM sqlite_master WHERE type='table'").all();
  console.log("\n=== row counts ===");
  for (const t of tables) {
    try {
      const c = db.prepare(`SELECT count(*) AS c FROM ${quoteIdent(t.name)}`).get();
      console.log(`  ${t.name}: ${c.c}`);
    } catch (e) { console.log(`  ${t.name}: <err ${e.message}>`); }
  }
  db.close();
}

/** 通用 SQL 查询（多条用 ;; 分隔），用于 schema/状态值标定 */
async function cmdSql(args) {
  requireFlag(args, "db", "query");
  const { DatabaseSync } = await import("node:sqlite");
  const db = new DatabaseSync(args.db, { readOnly: true });
  for (const q of args.query.split(";;")) {
    const sql = q.trim();
    if (!sql) continue;
    console.log(`--- ${sql}`);
    try {
      const rows = db.prepare(sql).all();
      for (const r of rows) console.log(JSON.stringify(r));
      console.log(`(${rows.length} rows)`);
    } catch (e) { console.log("ERR: " + e.message); }
  }
  db.close();
}

/** 扫描 ai_agent.dll 中的密钥相关字符串/常量，判断密钥方案版本。
 *  窗口式读（§4.17）：265MB 的 DLL 不整文件 readFileSync 驻留内存，
 *  相邻窗口留 needle 长度-1 的重叠，跨窗口的命中不漏。 */
function cmdStrings(args) {
  const dll = args.dll || defaultDll();
  console.log(`dll: ${dll}`);
  let size;
  try {
    size = statSync(dll).size;
  } catch (e) {
    throw new Error(`无法读取 DLL ${dll}（${e.message}）`);
  }
  console.log(`size: ${(size / 1048576).toFixed(1)}MB`);
  const needles = [
    ["config_key mics_sj10gy", "mics_sj10gy", "ascii"],
    ["config_key mics_sj11ky", "mics_sj11ky", "ascii"],
    ["config_key mics_sj3gy", "mics_sj3gy", "ascii"],
    ["xor_table rust", "rust", "ascii"],
    ["xor_table cpp", "cpp", "ascii"],
    ["xor_table electron", "electron", "ascii"],
    ["pbkdf2_salt", "123456789abcdef01122334455667788", "ascii"],
    ["decrypted_password", "1CqAknayQsrfH9Byp2QzynTckHGzRom9", "ascii"],
    ["enc_password_prefix", Buffer.from("452a17351d191e13", "hex"), "bin"],
    ["gen_key_log", "Generated database encryption key", "ascii"],
    ["pragma_key", "PRAGMA key", "ascii"],
    ["sqlcipher_export", "sqlcipher_export", "ascii"],
    ["cipher_compatibility", "cipher_compatibility", "ascii"],
    ["safeStorage", "safeStorage", "ascii"],
    ["dpapi_ref", "CryptUnprotectData", "ascii"],
    ["key_storage_ref", "database encryption key", "ascii"],
  ].map(([name, needleRaw, kind]) => [
    name,
    kind === "bin" ? needleRaw : Buffer.from(needleRaw, "latin1"),
  ]);
  const maxNeedle = Math.max(...needles.map(([, n]) => n.length));
  const WIN = 1 << 20;
  const found = new Map();
  const fd = openSync(dll, "r");
  try {
    let prevTail = Buffer.alloc(0);
    for (let off = 0; off < size; off += WIN) {
      const want = Math.min(WIN, size - off);
      const chunk = Buffer.alloc(prevTail.length + want);
      prevTail.copy(chunk, 0);
      readSync(fd, chunk, prevTail.length, want, off);
      const base = off - prevTail.length;
      for (const [name, needle] of needles) {
        if (found.has(name)) continue;
        const rel = chunk.indexOf(needle);
        if (rel >= 0) found.set(name, base + rel);
      }
      const tailLen = Math.min(maxNeedle - 1, chunk.length);
      prevTail = Buffer.from(chunk.subarray(chunk.length - tailLen));
    }
  } finally {
    closeSync(fd);
  }
  for (const [name] of needles) {
    const pos = found.get(name);
    console.log(`  ${pos >= 0 ? "✅" : "❌"} ${name} @ ${pos >= 0 ? "0x" + pos.toString(16) : "未找到"}`);
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const cmd = args._[0];
  if (cmd === "verify") cmdVerify(args);
  else if (cmd === "decrypt") cmdDecrypt(args);
  else if (cmd === "dump") await cmdDump(args);
  else if (cmd === "strings") cmdStrings(args);
  else if (cmd === "sql") await cmdSql(args);
  else {
    usage();
    process.exit(2);
  }
}

// 入口兜底（§4.17）：任何错误都收成「一句话原因 + 用法 + exit 2」，
// 不再向用户甩裸异常栈（坏路径 / 缺 flag / 坏参数都曾是裸栈）
try {
  await main();
} catch (e) {
  console.error(`错误: ${(e && e.message) || e}`);
  usage();
  process.exit(2);
}
