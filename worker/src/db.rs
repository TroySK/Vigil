//! D1 storage: typed key/value helpers plus schema setup.

use std::collections::BTreeMap;

use serde::Deserialize;
use worker::d1::{D1Database, D1PreparedStatement};
use worker::{Env, Result};

/// D1 binding name in `wrangler.toml`.
pub const DB_BINDING: &str = "DB";

/// Max bound params per D1 query chunk. The platform limit is 100 params
/// per query; chunks stay below it so `IN (...)` reads keep working as
/// the fleet grows (see `kv_map` / `open_gaps` / `reads`).
pub const MAX_PARAMS: usize = 90;

/// How long samples and closed gaps are kept (see the tick's prune pass):
/// readers clamp "first seen" stamps to this edge so pre-retention days
/// render as data instead of phantom history.
pub const RETENTION_SECS: i64 = 90 * 86_400;

/// Schema generation marker stored in `kv` once the tick's self-heal has
/// run: later ticks skip the DDL round trip entirely (CI migrations own
/// DDL; the tick only covers fresh/manual databases).
pub const SCHEMA_VERSION: &str = "2";

/// Bind an integer to D1 as a JS `Number`.
///
/// D1 rejects `BigInt` values, and `wasm-bindgen` converts Rust
/// `i64`/`u64` to `BigInt` via `From`/`Into`. Every integer bind must go
/// through this helper (or an `f64`/`i32` conversion) so D1 sees a `Number`.
/// All Vigil integers (epoch seconds, millis, flags, limits) fit exactly in
/// an `f64` (`< 2^53`), so the cast is lossless in practice.
#[must_use]
#[allow(clippy::cast_precision_loss)] // all bound values are < 2^53, exactly representable
pub fn num(value: i64) -> wasm_bindgen::JsValue {
    wasm_bindgen::JsValue::from_f64(value as f64)
}

/// Thin wrapper around the D1 database with query helpers.
///
/// `D1Database` is not `Clone`, so concurrent readers share `&Db` — fine
/// on the worker's single-threaded runtime.
#[derive(Debug)]
pub struct Db {
    inner: D1Database,
}

impl Db {
    /// Open the D1 database from the worker environment.
    pub fn of(env: &Env) -> Result<Self> {
        Ok(Self {
            inner: env.d1(DB_BINDING)?,
        })
    }

    /// Prepare a statement (always parameterized — never interpolate values).
    pub fn q(&self, sql: &str) -> D1PreparedStatement {
        self.inner.prepare(sql)
    }

    /// Upsert a `kv` value, prepared for inclusion in a write batch.
    /// Prefer batched [`Db::batch`] writes on hot paths; see [`Db::batch`].
    pub fn put_stmt(&self, key: &str, value: &str) -> Result<D1PreparedStatement> {
        self.q("INSERT INTO kv (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v")
            .bind(&[key.into(), value.into()])
    }

    /// Execute prepared statements in as few round trips as possible.
    ///
    /// The cron tick batches every write through here: dozens of sequential
    /// D1 round trips burned the free-plan CPU budget and got the isolate
    /// killed (`exceededCpu`) before the tick finished.
    pub async fn batch(&self, statements: Vec<D1PreparedStatement>) -> Result<()> {
        // D1 serves up to 100 statements per batch call; chunk so a
        // full-transition tick with every monitor due still fits in as few
        // round trips as possible.
        let mut rest = statements;
        while !rest.is_empty() {
            let take = rest.len().min(100);
            let chunk: Vec<D1PreparedStatement> = rest.drain(..take).collect();
            self.inner.batch(chunk).await?;
        }
        Ok(())
    }

    /// Create tables/indexes on first boot. Idempotent.
    pub async fn init(&self) -> Result<()> {
        self.inner
            .exec(
                "CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);\
                 CREATE TABLE IF NOT EXISTS samples (id TEXT NOT NULL, at INTEGER NOT NULL, ms INTEGER NOT NULL, up INTEGER NOT NULL, place TEXT NOT NULL);\
                 CREATE INDEX IF NOT EXISTS idx_samples ON samples (id, at);\
                 CREATE INDEX IF NOT EXISTS idx_samples_at ON samples (at);\
                 CREATE INDEX IF NOT EXISTS idx_samples_region ON samples (id, place, at DESC);\
                 CREATE TABLE IF NOT EXISTS gaps (id TEXT NOT NULL, at INTEGER NOT NULL, upto INTEGER, why TEXT NOT NULL);\
                 CREATE INDEX IF NOT EXISTS idx_gaps ON gaps (id, at);\
                 CREATE INDEX IF NOT EXISTS idx_gaps_upto ON gaps (upto);\
                 CREATE INDEX IF NOT EXISTS idx_gaps_open ON gaps (id, upto, at DESC);",
            )
            .await
            .map(|_| ())
    }

    /// Read a `kv` value.
    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        let query = self
            .q("SELECT v FROM kv WHERE k = ?1")
            .bind(&[key.into()])?;
        query.first::<String>(Some("v")).await
    }

    /// Read many `kv` entries in as few D1 round trips as possible: one
    /// query per [`MAX_PARAMS`] key chunk (empty input: no query). A
    /// single `IN (3N)` query would exceed D1's 100-param budget past ~33
    /// monitors and abort the whole tick.
    pub async fn kv_map(&self, keys: &[String]) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for chunk in keys.chunks(MAX_PARAMS) {
            let stmt = self.q(&kv_select_sql(chunk.len())).bind(
                &chunk
                    .iter()
                    .map(|key| key.clone().into())
                    .collect::<Vec<wasm_bindgen::JsValue>>(),
            )?;
            let rows: Vec<KvRow> = stmt.all().await?.results()?;
            for row in rows {
                out.insert(row.k, row.v);
            }
        }
        Ok(out)
    }
}

/// The three `kv` clock keys every page and API needs: tick clock,
/// up/down counts, and last-probed clock.
#[must_use]
pub fn clock_keys() -> [&'static str; 3] {
    ["at", "counts", "data_at"]
}

/// Exact query behind [`Db::kv_map`], exposed so tests execute the shipped
/// text against bundled SQLite.
fn kv_select_sql(n: usize) -> String {
    format!(
        "SELECT k, v FROM kv WHERE k IN ({})",
        (0..n)
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// One `kv` row for batched reads.
#[derive(Debug, Deserialize)]
struct KvRow {
    k: String,
    v: String,
}

#[cfg(test)]
mod tests {
    use super::{clock_keys, kv_select_sql, MAX_PARAMS};
    use rusqlite::params_from_iter;
    use std::collections::BTreeMap;

    #[test]
    fn clock_keys_cover_tick_clock_counts_and_probe_clock() {
        assert_eq!(clock_keys(), ["at", "counts", "data_at"]);
    }

    #[test]
    fn kv_select_covers_exactly_its_keys() {
        let sql = kv_select_sql(3);
        assert!(sql.contains("FROM kv WHERE k IN (?1,?2,?3)"));
    }

    #[test]
    fn kv_reads_chunk_within_param_budget() {
        // 102 keys exceed one query's param budget: split into chunks and
        // merge, exactly like `Db::kv_map` must. Every chunk stays within
        // `MAX_PARAMS` and the merged map is complete.
        let keys: Vec<String> = (0..102).map(|i| format!("k:{i}")).collect();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);")
            .unwrap();
        for key in &keys {
            conn.execute(
                "INSERT INTO kv VALUES (?1, ?2)",
                rusqlite::params![key, "v"],
            )
            .unwrap();
        }
        let mut merged = BTreeMap::new();
        for chunk in keys.chunks(MAX_PARAMS) {
            assert!(!chunk.is_empty() && chunk.len() <= MAX_PARAMS);
            let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
            let mut stmt = conn.prepare(&kv_select_sql(chunk.len())).unwrap();
            let mut rows = stmt.query(params_from_iter(refs)).unwrap();
            while let Some(row) = rows.next().unwrap() {
                merged.insert(
                    row.get::<_, String>(0).unwrap(),
                    row.get::<_, String>(1).unwrap(),
                );
            }
        }
        assert_eq!(merged.len(), 102);
    }

    /// Every index `Db::init` creates must also exist in the shipped
    /// migrations: prod databases migrate via CI, and the tick self-heal
    /// only closes the gap until its marker lands. A missing index means
    /// weeks of full-table scans on the request path (P99 tail).
    #[test]
    fn migrations_cover_every_init_index() {
        const INIT: &str = include_str!("../migrations/0001_init.sql");
        const IDX: &str = include_str!("../migrations/0002_indexes.sql");
        const REGION: &str = include_str!("../migrations/0004_region_index.sql");
        let shipped = format!("{INIT}\n{IDX}\n{REGION}");
        for index in [
            "idx_samples",
            "idx_samples_at",
            "idx_samples_region",
            "idx_gaps",
            "idx_gaps_upto",
            "idx_gaps_open",
        ] {
            assert!(
                shipped.contains(index),
                "migrations must create {index} (see Db::init)"
            );
        }
    }
}
