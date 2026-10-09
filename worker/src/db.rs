//! D1 storage: typed key/value helpers plus schema setup.

use worker::d1::{D1Database, D1PreparedStatement};
use worker::{Env, Result};

/// D1 binding name in `wrangler.toml`.
pub const DB_BINDING: &str = "DB";

/// Thin wrapper around the D1 database with query helpers.
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

    /// Create tables/indexes on first boot. Idempotent.
    pub async fn init(&self) -> Result<()> {
        self.inner
            .exec(
                "CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);\
                 CREATE TABLE IF NOT EXISTS samples (id TEXT NOT NULL, at INTEGER NOT NULL, ms INTEGER NOT NULL, up INTEGER NOT NULL, place TEXT NOT NULL);\
                 CREATE INDEX IF NOT EXISTS idx_samples ON samples (id, at);\
                 CREATE TABLE IF NOT EXISTS gaps (id TEXT NOT NULL, at INTEGER NOT NULL, upto INTEGER, why TEXT NOT NULL);\
                 CREATE INDEX IF NOT EXISTS idx_gaps ON gaps (id, at);",
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

    /// Upsert a `kv` value.
    pub async fn put(&self, key: &str, value: &str) -> Result<()> {
        self.q("INSERT INTO kv (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v")
            .bind(&[key.into(), value.into()])?
            .run()
            .await
            .map(|_| ())
    }
}
