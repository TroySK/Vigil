//! D1 storage: typed key/value helpers plus schema setup.

use worker::d1::{D1Database, D1PreparedStatement};
use worker::{Env, Result};

/// D1 binding name in `wrangler.toml`.
pub const DB_BINDING: &str = "DB";

/// Max bound params per D1 query chunk. The platform limit is 100 params
/// per query; chunks stay below it so `IN (...)` reads keep working as
/// the fleet grows (see `kv_map` / `open_gaps` / `reads`).
pub const MAX_PARAMS: usize = 90;

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

/// Whether the schema setup should run on a tick/request landing at
/// `now_secs`: the first two minutes of each clock hour. CI applies
/// migrations on deploy, so this is only a self-heal net (fresh databases,
/// manual setups) rather than the primary mechanism — no need to pay an
/// idempotent DDL round trip on every request.
#[must_use]
pub fn schema_due(now_secs: i64) -> bool {
    now_secs % 3_600 < 120
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
}

#[cfg(test)]
mod tests {
    use super::schema_due;

    /// Every index `Db::init` creates must also exist in the shipped
    /// migrations: prod databases migrate via CI, and the hourly self-heal
    /// only closes the gap one hour at a time. A missing index means weeks
    /// of full-table scans on the request path (P99 tail).
    #[test]
    fn migrations_cover_every_init_index() {
        const INIT: &str = include_str!("../migrations/0001_init.sql");
        const IDX: &str = include_str!("../migrations/0002_indexes.sql");
        let shipped = format!("{INIT}\n{IDX}");
        for index in [
            "idx_samples",
            "idx_samples_at",
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

    #[test]
    fn schema_init_runs_once_per_clock_hour() {
        assert!(schema_due(0));
        assert!(schema_due(119));
        assert!(!schema_due(120));
        assert!(!schema_due(3_599));
        assert!(schema_due(3_600));
        assert!(!schema_due(7_199));
        assert!(schema_due(7_200));
    }
}
