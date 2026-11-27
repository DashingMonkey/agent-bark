//! Token 用量统计的本地存储（SQLite），**独立于通知事件管道**。
//!
//! 用量记录是高频数据（实测 ZCode 单机千余条/半月），进 `run_pipeline` 会冲掉
//! 500 条上限的事件历史、污染事件流与通知聚合；且各数据源全是「轮询 db / 尾读
//! 文件」型，本来就不经过 hook 事件。故采集器直接写这里，事件链路零改动。
//!
//! 幂等：主键 (agent, record_id)，record_id 用各源原生标识（ZCode model_usage.id、
//! Codex response_id、WorkBuddy 消息 id、DSH 文件+seq、Qoder message_id……），
//! INSERT OR IGNORE——重启重读、兜底扫描重扫都不会重复累计。
//!
//! 归日：入库时按**本机时区**把 ts 落成 day 字符串（YYYY-MM-DD，字典序=时间序），
//! 查询按 day 范围过滤，避免每次查询现算时区。

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;

/// 历史保留天数：超出的 day 在开库时清掉。一年足够覆盖「年度回顾」类诉求，
/// 再长只是白占磁盘（单条记录约 100B，百万条也才 100MB 量级）。
const RETENTION_DAYS: i64 = 365;

/// 一条用量增量（一次模型请求 / 一条带 usage 的消息）。
/// input_new 一律为**不含缓存读取**的新输入：各源口径不同（ZCode input 含缓存
/// 需拆、DSH input 不含缓存），归一在各采集器里做，这里只存归一后的值。
#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub agent: String,
    pub record_id: String,
    pub session_id: String,
    /// unix 毫秒
    pub ts: i64,
    pub model: String,
    pub input_new: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    pub reasoning: i64,
    /// 历史字段：Qoder 的 credit-only 采集已下线（BYOK 无 credits，平台模型不再展示）；
    /// 保留列以兼容旧库，新记录恒为 None。后续清理再整体移除。
    pub credits: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DailyUsage {
    pub day: String,
    pub agent: String,
    pub requests: i64,
    pub input_new: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    pub reasoning: i64,
    pub credits: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentUsage {
    pub agent: String,
    /// 展示名由 commands 层按界面语言填（store 不认识 AgentKind）
    pub display_name: String,
    pub requests: i64,
    pub input_new: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    pub reasoning: i64,
    pub credits: f64,
}

/// 按小时 × agent 聚合（"今天"视图的小时柱）；hour 为本地时区 "00".."23"
#[derive(Debug, Clone, Serialize)]
pub struct HourlyUsage {
    pub hour: String,
    pub agent: String,
    pub requests: i64,
    pub input_new: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    pub reasoning: i64,
    pub credits: f64,
}

/// 开库失败 / 关闭态一律降级为「无数据」，不挡启动、不弹错误：
/// 统计是锦上添花，坏了只该让页面空着，不该影响通知主链路。
pub struct UsageStore {
    inner: Mutex<Option<Connection>>,
    path: PathBuf,
}

impl UsageStore {
    pub fn new(path: PathBuf) -> Self {
        Self { inner: Mutex::new(None), path }
    }

    /// 开关同步：启用则开库（建表 + 清保留期），关闭则关连接。
    /// 采集器每轮看 is_enabled()，关闭态自然停摆。
    pub fn set_enabled(&self, enabled: bool) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !enabled {
            *guard = None;
            return;
        }
        if guard.is_some() {
            return;
        }
        match Self::open(&self.path) {
            Ok(conn) => *guard = Some(conn),
            Err(e) => tracing::warn!("用量库打开失败（统计降级为无数据）: {e:#}"),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    fn open(path: &PathBuf) -> anyhow::Result<Connection> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(200))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_record(
               agent       TEXT NOT NULL,
               record_id   TEXT NOT NULL,
               session_id  TEXT NOT NULL DEFAULT '',
               ts          INTEGER NOT NULL,
               day         TEXT NOT NULL,
               model       TEXT NOT NULL DEFAULT '',
               input_new   INTEGER NOT NULL DEFAULT 0,
               cache_read  INTEGER NOT NULL DEFAULT 0,
               cache_write INTEGER NOT NULL DEFAULT 0,
               output      INTEGER NOT NULL DEFAULT 0,
               reasoning   INTEGER NOT NULL DEFAULT 0,
               credits     REAL,
               PRIMARY KEY(agent, record_id)
             );
             CREATE INDEX IF NOT EXISTS usage_record_day ON usage_record(day);",
        )?;
        let cutoff = chrono::Local::now()
            .checked_sub_signed(chrono::Duration::days(RETENTION_DAYS))
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        conn.execute("DELETE FROM usage_record WHERE day < ?1", [&cutoff])?;
        // Qoder credit-only 行已无展示口径（token 页只统计 token）；幂等清理，
        // 删的都是无 token 的旧积分行。
        conn.execute(
            "DELETE FROM usage_record
              WHERE agent = 'qoder' AND credits IS NOT NULL
                AND input_new = 0 AND cache_read = 0 AND cache_write = 0
                AND output = 0 AND reasoning = 0",
            [],
        )?;
        Ok(conn)
    }

    /// 批量幂等入库，返回**新增**条数（重读旧记录返回 0）。关闭态为 no-op。
    ///
    /// 整批走一个事务：首轮回补（如 TRAE 一次 4000+ 条历史调用）逐条提交要 8 秒，
    /// 单事务下是几十毫秒——后台采集线程不必长时间占着库。
    pub fn insert(&self, records: &[UsageRecord]) -> usize {
        if records.is_empty() {
            return 0;
        }
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conn) = guard.as_mut() else { return 0 };
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("用量入库开启事务失败: {e}");
                return 0;
            }
        };
        let mut added = 0usize;
        {
            let mut stmt = match tx.prepare_cached(
                "INSERT OR IGNORE INTO usage_record
                 (agent, record_id, session_id, ts, day, model,
                  input_new, cache_read, cache_write, output, reasoning, credits)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("用量入库准备失败: {e}");
                    return 0;
                }
            };
            for r in records {
                let day = chrono::DateTime::from_timestamp_millis(r.ts)
                    .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                let params = rusqlite::params![
                    r.agent, r.record_id, r.session_id, r.ts, day, r.model,
                    r.input_new, r.cache_read, r.cache_write, r.output, r.reasoning, r.credits
                ];
                match stmt.execute(params) {
                    Ok(n) => added += n,
                    Err(e) => tracing::warn!("用量入库失败 agent={} id={}: {e}", r.agent, r.record_id),
                }
            }
        }
        if let Err(e) = tx.commit() {
            tracing::warn!("用量入库提交失败（本批丢弃）: {e}");
            return 0;
        }
        added
    }

    /// 按日 × agent 聚合（含 credits 合计），day 闭区间。
    pub fn daily(&self, from: &str, to: &str) -> Vec<DailyUsage> {
        self.query(
            "SELECT day, agent, COUNT(*), SUM(input_new), SUM(cache_read), SUM(cache_write),
                    SUM(output), SUM(reasoning), COALESCE(SUM(credits), 0)
             FROM usage_record WHERE day BETWEEN ?1 AND ?2
             GROUP BY day, agent ORDER BY day, agent",
            from,
            to,
            |row| {
                Ok(DailyUsage {
                    day: row.get(0)?,
                    agent: row.get(1)?,
                    requests: row.get(2)?,
                    input_new: row.get(3)?,
                    cache_read: row.get(4)?,
                    cache_write: row.get(5)?,
                    output: row.get(6)?,
                    reasoning: row.get(7)?,
                    credits: row.get(8)?,
                })
            },
        )
    }

    /// 按 agent 聚合（窗口内），供分 agent 表格。
    pub fn by_agent(&self, from: &str, to: &str) -> Vec<AgentUsage> {
        self.query(
            "SELECT agent, COUNT(*), SUM(input_new), SUM(cache_read), SUM(cache_write),
                    SUM(output), SUM(reasoning), COALESCE(SUM(credits), 0)
             FROM usage_record WHERE day BETWEEN ?1 AND ?2
             GROUP BY agent ORDER BY SUM(input_new + cache_read + output) DESC",
            from,
            to,
            |row| {
                Ok(AgentUsage {
                    agent: row.get(0)?,
                    display_name: String::new(),
                    requests: row.get(1)?,
                    input_new: row.get(2)?,
                    cache_read: row.get(3)?,
                    cache_write: row.get(4)?,
                    output: row.get(5)?,
                    reasoning: row.get(6)?,
                    credits: row.get(7)?,
                })
            },
        )
    }

    /// 按小时 × agent 聚合：ts 毫秒_bounds 闭开区间由 commands 层算好传入。
    /// 小时桶用 SQLite localtime 修饰符——与入库归日同一进程时区，口径一致。
    pub fn hourly(&self, from_ts: i64, to_ts: i64) -> Vec<HourlyUsage> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conn) = guard.as_ref() else { return Vec::new() };
        let mut stmt = match conn.prepare_cached(
            "SELECT strftime('%H', ts / 1000, 'unixepoch', 'localtime'), agent, COUNT(*),
                    SUM(input_new), SUM(cache_read), SUM(cache_write), SUM(output), SUM(reasoning),
                    COALESCE(SUM(credits), 0)
             FROM usage_record WHERE ts >= ?1 AND ts < ?2
             GROUP BY 1, agent ORDER BY 1, agent",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("用量查询准备失败: {e}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map(rusqlite::params![from_ts, to_ts], |row| {
            Ok(HourlyUsage {
                hour: row.get(0)?,
                agent: row.get(1)?,
                requests: row.get(2)?,
                input_new: row.get(3)?,
                cache_read: row.get(4)?,
                cache_write: row.get(5)?,
                output: row.get(6)?,
                reasoning: row.get(7)?,
                credits: row.get(8)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(e) => {
                tracing::warn!("用量查询失败: {e}");
                Vec::new()
            }
        }
    }

    fn query<T>(
        &self,
        sql: &str,
        from: &str,
        to: &str,
        map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Vec<T> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conn) = guard.as_ref() else { return Vec::new() };
        let mut stmt = match conn.prepare_cached(sql) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("用量查询准备失败: {e}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map(rusqlite::params![from, to], map);
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(e) => {
                tracing::warn!("用量查询失败: {e}");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, ts: i64, input: i64, cache: i64, out: i64) -> UsageRecord {
        UsageRecord {
            agent: "zcode".into(),
            record_id: id.into(),
            session_id: "s1".into(),
            ts,
            model: "m".into(),
            input_new: input,
            cache_read: cache,
            cache_write: 0,
            output: out,
            reasoning: 0,
            credits: None,
        }
    }

    fn store() -> (UsageStore, std::path::PathBuf) {
        // 与 state.rs / codex_watch.rs 测试同款：temp_dir + 唯一名，不引 tempfile
        let dir = std::env::temp_dir().join(format!(
            "bark-usage-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let s = UsageStore::new(dir.join("usage.sqlite3"));
        s.set_enabled(true);
        (s, dir)
    }

    #[test]
    fn insert_is_idempotent_across_reopen() {
        let (s, dir) = store();
        let ts = 1_794_000_000_000;
        assert_eq!(s.insert(&[rec("a", ts, 10, 100, 5)]), 1);
        assert_eq!(s.insert(&[rec("a", ts, 10, 100, 5)]), 0);
        drop(s);
        let s2 = UsageStore::new(dir.join("usage.sqlite3"));
        s2.set_enabled(true);
        assert_eq!(s2.insert(&[rec("a", ts, 10, 100, 5)]), 0);
        let day = chrono::DateTime::from_timestamp_millis(ts)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d")
            .to_string();
        let daily = s2.daily(&day, &day);
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].input_new, 10);
        assert_eq!(daily[0].cache_read, 100);
        assert_eq!(daily[0].output, 5);
        drop(s2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_store_is_noop_and_queries_empty() {
        let (s, dir) = store();
        s.set_enabled(false);
        assert!(!s.is_enabled());
        assert_eq!(s.insert(&[rec("a", 1_794_000_000_000, 1, 1, 1)]), 0);
        assert!(s.daily("2020-01-01", "2030-01-01").is_empty());
        s.set_enabled(true);
        assert!(s.is_enabled());
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn qoder_credit_only_rows_are_purged_on_open() {
        let (s, dir) = store();
        let ts = 1_794_000_000_000;
        let mut credit = rec("q-credit", ts, 0, 0, 0);
        credit.agent = "qoder".into();
        credit.credits = Some(2.9);
        let mut token = rec("q-token", ts, 10, 20, 5);
        token.agent = "qoder".into();
        assert_eq!(s.insert(&[credit, token]), 2);
        drop(s);

        let s2 = UsageStore::new(dir.join("usage.sqlite3"));
        s2.set_enabled(true);
        let agents = s2.by_agent("2020-01-01", "2030-01-01");
        let q = agents.iter().find(|a| a.agent == "qoder").unwrap();
        assert_eq!(q.requests, 1, "credit-only 行开库时被清掉");
        assert_eq!(q.credits, 0.0);
        assert_eq!(q.input_new, 10);
        drop(s2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hourly_buckets_by_local_hour() {
        let (s, dir) = store();
        let tz = chrono::Local::now().timezone();
        let today = chrono::Local::now().date_naive();
        let ts = |h: u32| {
            today
                .and_hms_opt(h, 30, 0)
                .unwrap()
                .and_local_timezone(tz)
                .single()
                .unwrap()
                .timestamp_millis()
        };
        s.insert(&[rec("a", ts(10), 1, 2, 3), rec("b", ts(10), 4, 5, 6), rec("c", ts(11), 7, 8, 9)]);
        let start = today.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(tz).single().unwrap().timestamp_millis();
        let end = (today + chrono::Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(tz)
            .single()
            .unwrap()
            .timestamp_millis();
        let rows = s.hourly(start, end);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].hour, "10");
        assert_eq!(rows[0].requests, 2);
        assert_eq!(rows[0].input_new, 5);
        assert_eq!(rows[1].hour, "11");
        assert_eq!(rows[1].output, 9);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}