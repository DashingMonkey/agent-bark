import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync, linkSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { createCipheriv, createHmac, pbkdf2Sync } from "node:crypto";

const tool = fileURLToPath(new URL("./traework-probe.mjs", import.meta.url));
const key = Buffer.alloc(32, 7);

function fixture() {
  const salt = Buffer.alloc(16, 3);
  const iv = Buffer.alloc(16, 5);
  const page = Buffer.alloc(4096);
  salt.copy(page);
  const cipher = createCipheriv("aes-256-cbc", key, iv);
  cipher.setAutoPadding(false);
  Buffer.concat([cipher.update(Buffer.alloc(4000, 9)), cipher.final()]).copy(page, 16);
  iv.copy(page, 4016);
  const macKey = pbkdf2Sync(key, Buffer.from(salt.map((b) => b ^ 0x3a)), 2, 32, "sha512");
  const pgno = Buffer.alloc(4);
  pgno.writeUInt32LE(1);
  createHmac("sha512", macKey).update(page.subarray(16, 4032)).update(pgno).digest().copy(page, 4032);
  return page;
}

function run(db, out, hex = key.toString("hex")) {
  return spawnSync(process.execPath, [tool, "decrypt", "--db", db, "--out", out, "--key", hex], { encoding: "utf8" });
}

test("decrypt rejects source paths and hard-link aliases without modifying data", () => {
  const dir = mkdtempSync(join(tmpdir(), "bark-probe-test-"));
  try {
    const db = join(dir, "source.db");
    const alias = join(dir, "alias.db");
    const original = fixture();
    writeFileSync(db, original);
    linkSync(db, alias);
    for (const output of [db, alias]) {
      assert.notEqual(run(db, output).status, 0);
      assert.deepEqual(readFileSync(db), original);
    }
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("failed validation preserves existing output and removes temporary files", () => {
  const dir = mkdtempSync(join(tmpdir(), "bark-probe-test-"));
  try {
    const db = join(dir, "source.db");
    const out = join(dir, "output.db");
    const original = fixture();
    writeFileSync(db, original);
    writeFileSync(out, "previous output");
    const failed = run(db, out, "00".repeat(32));
    assert.equal(failed.status, 1);
    assert.match(failed.stderr, /HMAC/);
    assert.doesNotMatch(failed.stderr, /node tools\/traework-probe/);
    assert.deepEqual(readFileSync(db), original);
    assert.equal(readFileSync(out, "utf8"), "previous output");
    // A late failure must not expose a partially decrypted database either.
    writeFileSync(db, Buffer.concat([original, Buffer.alloc(4096)]));
    assert.equal(run(db, out).status, 1);
    assert.equal(readFileSync(out, "utf8"), "previous output");
    assert.deepEqual(readdirSync(dir).sort(), ["output.db", "source.db"]);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("decrypt must not replace source database sidecars or their aliases", () => {
  const dir = mkdtempSync(join(tmpdir(), "bark-probe-test-"));
  try {
    const db = join(dir, "source.db");
    writeFileSync(db, fixture());
    for (const suffix of ["-wal", "-shm", "-journal"]) {
      const sidecar = db + suffix;
      const alias = join(dir, `alias${suffix}`);
      const original = Buffer.from("source sidecar contents");
      writeFileSync(sidecar, original);
      linkSync(sidecar, alias);
      for (const out of [sidecar, alias]) {
        assert.notEqual(run(db, out).status, 0);
        assert.deepEqual(readFileSync(sidecar), original);
        assert.deepEqual(readFileSync(alias), original);
      }
      rmSync(alias);
      rmSync(sidecar);
      assert.notEqual(run(db, sidecar).status, 0, "不能在缺失的源侧文件位置创建解密输出");
    }
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("successful decryption replaces output only after verification", () => {
  const dir = mkdtempSync(join(tmpdir(), "bark-probe-test-"));
  try {
    const db = join(dir, "source.db");
    const out = join(dir, "output.db");
    const original = fixture();
    writeFileSync(db, original);
    writeFileSync(out, "previous output");
    const result = run(db, out);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(readFileSync(out).length, 4096);
    assert.equal(readFileSync(out).subarray(0, 16).toString(), "SQLite format 3\0");
    assert.deepEqual(readFileSync(db), original);
    assert.deepEqual(readdirSync(dir).sort(), ["output.db", "source.db"]);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
