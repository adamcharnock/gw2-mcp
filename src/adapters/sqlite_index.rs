//! On-disk search index backed by `SQLite` + FTS5 (Tier 6C).
//!
//! Schema is owned by the SQL files under `migrations/` and applied via
//! [`sqlx::migrate!`] at connect time. The macro embeds every migration
//! at compile time so the production binary doesn't need the migrations
//! directory at runtime. To evolve the schema, drop a new
//! `<timestamp>_<slug>.sql` file in `migrations/` — `SQLX` discovers it
//! lexicographically and tracks applied state in its own `_sqlx_migrations`
//! table.
//!
//! Schema sketch (one block per entity kind):
//!
//! ```sql
//! CREATE TABLE skills (
//!   id INTEGER PRIMARY KEY,
//!   name TEXT NOT NULL,
//!   description TEXT,
//!   ...
//!   raw_json TEXT NOT NULL,
//!   fetched_at INTEGER NOT NULL,
//!   build INTEGER NOT NULL
//! );
//! CREATE VIRTUAL TABLE skills_fts USING fts5(
//!   name, description,
//!   content='skills', content_rowid='id',
//!   tokenize='unicode61 remove_diacritics 1'
//! );
//! -- contentless mirror via triggers
//! ```
//!
//! Query strategy: FTS5 `MATCH 'foo*'` for prefix matching, BM25 ranking
//! via `ORDER BY rank`. Filters are added as plain `WHERE` clauses on the
//! parent (content) table, joined via the FTS rowid.
//!
//! Concurrency: `SQLX` is async-native; we keep a small [`SqlitePool`]
//! (4 connections in production, 1 in tests / in-memory). WAL allows N
//! concurrent readers plus one writer, so this tiny pool covers the
//! single-MCP-client traffic comfortably and avoids the
//! `spawn_blocking` + `Mutex<Connection>` dance the rusqlite-era adapter
//! used.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use async_trait::async_trait;
use chrono::Utc;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::domain::{Achievement, Item, Skill, Specialization, Trait};
use crate::ports::{
    AchievementRef, AchievementSearchFilter, IndexStatus, ItemRef, ItemSearchFilter, KindStatus,
    SearchError, SearchIndex, SkillRef, SkillSearchFilter, SpecRef, SpecSearchFilter, TraitRef,
    TraitSearchFilter,
};

/// Default cache file name beneath the OS-standard cache directory.
pub const INDEX_FILE_NAME: &str = "index.sqlite";

/// On-disk SQLite-backed [`SearchIndex`].
pub struct SqliteSearchIndex {
    pool: SqlitePool,
    /// Path the index lives at. Useful for diagnostics and CLI output.
    path: PathBuf,
}

impl SqliteSearchIndex {
    /// Open (or create) the index at `path`. Creates the parent directory
    /// if missing and runs every pending migration before returning.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SearchError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                SearchError::Storage(format!(
                    "failed to create cache directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
        // WAL is the right mode for read-heavy workloads with infrequent
        // writes — readers don't block writers and vice versa. Synchronous
        // NORMAL is durable enough for a cache (a crash mid-flight at worst
        // costs us the last batch; the next startup re-indexes).
        let opts = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .pragma("temp_store", "MEMORY")
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .map_err(|e| SearchError::Storage(format!("open {}: {e}", path.display())))?;
        Self::migrate(&pool).await?;
        Ok(Self { pool, path })
    }

    /// Open an in-memory database. Used by unit tests so each `cargo test`
    /// run starts from a clean slate without disk I/O.
    ///
    /// `:memory:` databases in `SQLite` are per-connection, so the pool
    /// must hold exactly one connection — otherwise readers spawned by
    /// the pool would see an empty schema.
    pub async fn open_in_memory() -> Result<Self, SearchError> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(|e| SearchError::Storage(format!("parse :memory: opts: {e}")))?;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .map_err(|e| SearchError::Storage(format!("open :memory:: {e}")))?;
        Self::migrate(&pool).await?;
        Ok(Self {
            pool,
            path: PathBuf::from(":memory:"),
        })
    }

    /// Run every pending migration against `pool`. Idempotent — `SQLX`
    /// records the applied set in its `_sqlx_migrations` table.
    async fn migrate(pool: &SqlitePool) -> Result<(), SearchError> {
        sqlx::migrate!("./migrations")
            .run(pool)
            .await
            .map_err(|e| SearchError::Storage(format!("migrate: {e}")))?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a safe FTS5 MATCH expression from a free-form user query.
///
/// Strategy:
/// - Strip everything other than alphanumerics, hyphen, apostrophe.
/// - Split on whitespace.
/// - Wrap each token in double quotes (so we can pass strings with
///   apostrophes / hyphens to FTS5 without it interpreting them as
///   operators).
/// - Append `*` to the last token for prefix matching.
///
/// Returns `None` for queries that have no usable tokens, in which case the
/// caller short-circuits to `Vec::new()` rather than execute a query that
/// would either error or return everything.
fn build_match_expr(q: &str) -> Option<String> {
    let tokens: Vec<String> = q
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|s| !s.is_empty())
        .map(|t| t.replace('"', ""))
        .filter(|s| !s.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(tokens.len());
    let last_idx = tokens.len() - 1;
    for (i, tok) in tokens.into_iter().enumerate() {
        if i == last_idx {
            parts.push(format!("\"{tok}\" *"));
        } else {
            parts.push(format!("\"{tok}\""));
        }
    }
    Some(parts.join(" "))
}

fn now_ts() -> i64 {
    Utc::now().timestamp()
}

fn extract_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_owned)
}

fn extract_u32(v: &serde_json::Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

fn extract_bool(v: &serde_json::Value, key: &str) -> Option<bool> {
    v.get(key).and_then(serde_json::Value::as_bool)
}

fn extract_string_array(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn extract_u32_array(v: &serde_json::Value, key: &str) -> Vec<u32> {
    v.get(key)
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_u64().and_then(|n| u32::try_from(n).ok()))
                .collect()
        })
        .unwrap_or_default()
}

/// Map a `sqlx` row-decoding error to our typed error.
fn row_err(e: sqlx::Error) -> SearchError {
    SearchError::Storage(format!("row: {e}"))
}

/// Map a `sqlx` query error to our typed error.
fn query_err(e: sqlx::Error) -> SearchError {
    SearchError::Storage(format!("query: {e}"))
}

/// Returns a typed `NotIndexed` error if the named table is empty. Used as
/// the single empty-state guard at the top of every `search_*` method so the
/// LLM gets a clear "still populating" message instead of an empty list.
async fn ensure_populated(pool: &SqlitePool, table: &'static str) -> Result<(), SearchError> {
    // Table name is a `&'static str` from a closed set the adapter owns
    // (skills / traits / specializations / items / achievements) — no
    // injection risk.
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} LIMIT 1"))
        .fetch_one(pool)
        .await
        .map_err(|e| SearchError::Storage(format!("count {table}: {e}")))?;
    if count == 0 {
        return Err(SearchError::NotIndexed { kind: table });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SearchIndex impl
// ---------------------------------------------------------------------------

#[async_trait]
impl SearchIndex for SqliteSearchIndex {
    async fn search_skills(
        &self,
        q: &str,
        limit: u32,
        filter: SkillSearchFilter,
    ) -> Result<Vec<SkillRef>, SearchError> {
        let Some(match_expr) = build_match_expr(q) else {
            return Ok(Vec::new());
        };
        let limit = limit.clamp(1, 500);
        // Empty-table guard: if no rows exist, the kind hasn't been
        // populated yet — surface that as a typed error so the caller
        // can produce a clear "still indexing" message rather than an
        // empty list (which the LLM would interpret as "no hits").
        ensure_populated(&self.pool, "skills").await?;

        let mut sql = String::from(
            "SELECT s.id, s.name, s.description, s.type, s.slot, s.professions, \
             s.weapon_type, fts.rank \
             FROM skills_fts AS fts \
             JOIN skills AS s ON s.id = fts.rowid \
             WHERE skills_fts MATCH ?",
        );
        if filter.profession.is_some() {
            // professions stored as JSON array; substring match is fine
            // since profession names don't overlap.
            sql.push_str(" AND s.professions LIKE ?");
        }
        if filter.slot.is_some() {
            sql.push_str(" AND s.slot = ?");
        }
        if filter.weapon_type.is_some() {
            sql.push_str(" AND s.weapon_type = ?");
        }
        sql.push_str(" ORDER BY fts.rank LIMIT ?");

        let mut query = sqlx::query(&sql).bind(match_expr);
        if let Some(p) = &filter.profession {
            query = query.bind(format!("%\"{p}\"%"));
        }
        if let Some(s) = &filter.slot {
            query = query.bind(s.clone());
        }
        if let Some(w) = &filter.weapon_type {
            query = query.bind(w.clone());
        }
        query = query.bind(i64::from(limit));

        let rows = query.fetch_all(&self.pool).await.map_err(query_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let professions_raw: Option<String> = row.try_get(5).map_err(row_err)?;
            let professions: Vec<String> = professions_raw
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            out.push(SkillRef {
                id: u32::try_from(row.try_get::<i64, _>(0).map_err(row_err)?).unwrap_or(0),
                name: row.try_get(1).map_err(row_err)?,
                description: row.try_get(2).map_err(row_err)?,
                skill_type: row.try_get(3).map_err(row_err)?,
                slot: row.try_get(4).map_err(row_err)?,
                professions,
                weapon_type: row.try_get(6).map_err(row_err)?,
                score: row.try_get(7).map_err(row_err)?,
            });
        }
        Ok(out)
    }

    async fn search_traits(
        &self,
        q: &str,
        limit: u32,
        filter: TraitSearchFilter,
    ) -> Result<Vec<TraitRef>, SearchError> {
        let Some(match_expr) = build_match_expr(q) else {
            return Ok(Vec::new());
        };
        let limit = limit.clamp(1, 500);
        ensure_populated(&self.pool, "traits").await?;

        let mut sql = String::from(
            "SELECT t.id, t.name, t.description, t.specialization, t.tier, t.slot, fts.rank \
             FROM traits_fts AS fts JOIN traits AS t ON t.id = fts.rowid \
             WHERE traits_fts MATCH ?",
        );
        if filter.specialization.is_some() {
            sql.push_str(" AND t.specialization = ?");
        }
        if filter.tier.is_some() {
            sql.push_str(" AND t.tier = ?");
        }
        sql.push_str(" ORDER BY fts.rank LIMIT ?");

        let mut query = sqlx::query(&sql).bind(match_expr);
        if let Some(s) = filter.specialization {
            query = query.bind(i64::from(s));
        }
        if let Some(t) = filter.tier {
            query = query.bind(i64::from(t));
        }
        query = query.bind(i64::from(limit));

        let rows = query.fetch_all(&self.pool).await.map_err(query_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(TraitRef {
                id: u32::try_from(row.try_get::<i64, _>(0).map_err(row_err)?).unwrap_or(0),
                name: row.try_get(1).map_err(row_err)?,
                description: row.try_get(2).map_err(row_err)?,
                specialization: row
                    .try_get::<Option<i64>, _>(3)
                    .map_err(row_err)?
                    .and_then(|n| u32::try_from(n).ok()),
                tier: row
                    .try_get::<Option<i64>, _>(4)
                    .map_err(row_err)?
                    .and_then(|n| u32::try_from(n).ok()),
                slot: row.try_get(5).map_err(row_err)?,
                score: row.try_get(6).map_err(row_err)?,
            });
        }
        Ok(out)
    }

    async fn search_specializations(
        &self,
        q: &str,
        limit: u32,
        filter: SpecSearchFilter,
    ) -> Result<Vec<SpecRef>, SearchError> {
        let Some(match_expr) = build_match_expr(q) else {
            return Ok(Vec::new());
        };
        let limit = limit.clamp(1, 500);
        ensure_populated(&self.pool, "specializations").await?;

        let mut sql = String::from(
            "SELECT s.id, s.name, s.profession, s.elite, fts.rank \
             FROM specializations_fts AS fts JOIN specializations AS s ON s.id = fts.rowid \
             WHERE specializations_fts MATCH ?",
        );
        if filter.profession.is_some() {
            sql.push_str(" AND s.profession = ?");
        }
        if filter.elite.is_some() {
            sql.push_str(" AND s.elite = ?");
        }
        sql.push_str(" ORDER BY fts.rank LIMIT ?");

        let mut query = sqlx::query(&sql).bind(match_expr);
        if let Some(p) = &filter.profession {
            query = query.bind(p.clone());
        }
        if let Some(e) = filter.elite {
            query = query.bind(i64::from(e));
        }
        query = query.bind(i64::from(limit));

        let rows = query.fetch_all(&self.pool).await.map_err(query_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(SpecRef {
                id: u32::try_from(row.try_get::<i64, _>(0).map_err(row_err)?).unwrap_or(0),
                name: row.try_get(1).map_err(row_err)?,
                profession: row.try_get(2).map_err(row_err)?,
                elite: row.try_get::<i64, _>(3).map_err(row_err)? != 0,
                score: row.try_get(4).map_err(row_err)?,
            });
        }
        Ok(out)
    }

    async fn search_items(
        &self,
        q: &str,
        limit: u32,
        filter: ItemSearchFilter,
    ) -> Result<Vec<ItemRef>, SearchError> {
        let Some(match_expr) = build_match_expr(q) else {
            return Ok(Vec::new());
        };
        let limit = limit.clamp(1, 500);
        ensure_populated(&self.pool, "items").await?;

        let mut sql = String::from(
            "SELECT i.id, i.name, i.type, i.rarity, i.level, i.weight_class, fts.rank \
             FROM items_fts AS fts JOIN items AS i ON i.id = fts.rowid \
             WHERE items_fts MATCH ?",
        );
        if filter.item_type.is_some() {
            sql.push_str(" AND i.type = ?");
        }
        if filter.rarity.is_some() {
            sql.push_str(" AND i.rarity = ?");
        }
        if filter.min_level.is_some() {
            sql.push_str(" AND i.level >= ?");
        }
        if filter.max_level.is_some() {
            sql.push_str(" AND i.level <= ?");
        }
        if filter.weight_class.is_some() {
            sql.push_str(" AND i.weight_class = ?");
        }
        sql.push_str(" ORDER BY fts.rank LIMIT ?");

        let mut query = sqlx::query(&sql).bind(match_expr);
        if let Some(t) = &filter.item_type {
            query = query.bind(t.clone());
        }
        if let Some(r) = &filter.rarity {
            query = query.bind(r.clone());
        }
        if let Some(min) = filter.min_level {
            query = query.bind(i64::from(min));
        }
        if let Some(max) = filter.max_level {
            query = query.bind(i64::from(max));
        }
        if let Some(w) = &filter.weight_class {
            query = query.bind(w.clone());
        }
        query = query.bind(i64::from(limit));

        let rows = query.fetch_all(&self.pool).await.map_err(query_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(ItemRef {
                id: u32::try_from(row.try_get::<i64, _>(0).map_err(row_err)?).unwrap_or(0),
                name: row.try_get(1).map_err(row_err)?,
                item_type: row.try_get(2).map_err(row_err)?,
                rarity: row.try_get(3).map_err(row_err)?,
                level: row
                    .try_get::<Option<i64>, _>(4)
                    .map_err(row_err)?
                    .and_then(|n| u32::try_from(n).ok()),
                weight_class: row.try_get(5).map_err(row_err)?,
                score: row.try_get(6).map_err(row_err)?,
            });
        }
        Ok(out)
    }

    async fn search_achievements(
        &self,
        q: &str,
        limit: u32,
        filter: AchievementSearchFilter,
    ) -> Result<Vec<AchievementRef>, SearchError> {
        let Some(match_expr) = build_match_expr(q) else {
            return Ok(Vec::new());
        };
        let limit = limit.clamp(1, 500);
        ensure_populated(&self.pool, "achievements").await?;

        let mut sql = String::from(
            "SELECT a.id, a.name, a.description, a.requirement, a.type, a.categories, fts.rank \
             FROM achievements_fts AS fts JOIN achievements AS a ON a.id = fts.rowid \
             WHERE achievements_fts MATCH ?",
        );
        if filter.achievement_type.is_some() {
            sql.push_str(" AND a.type = ?");
        }
        sql.push_str(" ORDER BY fts.rank LIMIT ?");

        let mut query = sqlx::query(&sql).bind(match_expr);
        if let Some(t) = &filter.achievement_type {
            query = query.bind(t.clone());
        }
        query = query.bind(i64::from(limit));

        let rows = query.fetch_all(&self.pool).await.map_err(query_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let cats_raw: Option<String> = row.try_get(5).map_err(row_err)?;
            let categories: Vec<u32> = cats_raw
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            out.push(AchievementRef {
                id: u32::try_from(row.try_get::<i64, _>(0).map_err(row_err)?).unwrap_or(0),
                name: row.try_get(1).map_err(row_err)?,
                description: row.try_get(2).map_err(row_err)?,
                requirement: row.try_get(3).map_err(row_err)?,
                achievement_type: row.try_get(4).map_err(row_err)?,
                categories,
                score: row.try_get(6).map_err(row_err)?,
            });
        }
        Ok(out)
    }

    async fn upsert_skills(&self, skills: &[Skill], build: u32) -> Result<(), SearchError> {
        let rows: Vec<SkillRow> = skills.iter().map(SkillRow::from_domain).collect();
        let build_i = i64::from(build);
        let now = now_ts();
        // Wrap the whole batch in one transaction — without this each
        // INSERT pays for an fsync. `sqlx` caches the prepared statement
        // for reuse across iterations on the same connection.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| SearchError::Storage(format!("begin tx: {e}")))?;
        for r in &rows {
            sqlx::query(
                "INSERT OR REPLACE INTO skills(id, name, description, type, slot, \
                 professions, weapon_type, chat_link, raw_json, fetched_at, build) \
                 VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            )
            .bind(i64::from(r.id))
            .bind(&r.name)
            .bind(&r.description)
            .bind(&r.skill_type)
            .bind(&r.slot)
            .bind(&r.professions_json)
            .bind(&r.weapon_type)
            .bind(&r.chat_link)
            .bind(&r.raw_json)
            .bind(now)
            .bind(build_i)
            .execute(&mut *tx)
            .await
            .map_err(|e| SearchError::Storage(format!("upsert skill: {e}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| SearchError::Storage(format!("commit: {e}")))?;
        Ok(())
    }

    async fn upsert_traits(&self, traits: &[Trait], build: u32) -> Result<(), SearchError> {
        let rows: Vec<TraitRow> = traits.iter().map(TraitRow::from_domain).collect();
        let build_i = i64::from(build);
        let now = now_ts();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| SearchError::Storage(format!("begin tx: {e}")))?;
        for r in &rows {
            sqlx::query(
                "INSERT OR REPLACE INTO traits(id, name, description, specialization, \
                 tier, slot, raw_json, fetched_at, build) VALUES(?,?,?,?,?,?,?,?,?)",
            )
            .bind(i64::from(r.id))
            .bind(&r.name)
            .bind(&r.description)
            .bind(r.specialization.map(i64::from))
            .bind(r.tier.map(i64::from))
            .bind(&r.slot)
            .bind(&r.raw_json)
            .bind(now)
            .bind(build_i)
            .execute(&mut *tx)
            .await
            .map_err(|e| SearchError::Storage(format!("upsert trait: {e}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| SearchError::Storage(format!("commit: {e}")))?;
        Ok(())
    }

    async fn upsert_specializations(
        &self,
        specs: &[Specialization],
        build: u32,
    ) -> Result<(), SearchError> {
        let rows: Vec<SpecRow> = specs.iter().map(SpecRow::from_domain).collect();
        let build_i = i64::from(build);
        let now = now_ts();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| SearchError::Storage(format!("begin tx: {e}")))?;
        for r in &rows {
            sqlx::query(
                "INSERT OR REPLACE INTO specializations(id, name, profession, elite, \
                 raw_json, fetched_at, build) VALUES(?,?,?,?,?,?,?)",
            )
            .bind(i64::from(r.id))
            .bind(&r.name)
            .bind(&r.profession)
            .bind(i64::from(r.elite))
            .bind(&r.raw_json)
            .bind(now)
            .bind(build_i)
            .execute(&mut *tx)
            .await
            .map_err(|e| SearchError::Storage(format!("upsert spec: {e}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| SearchError::Storage(format!("commit: {e}")))?;
        Ok(())
    }

    async fn upsert_items(&self, items: &[Item], build: u32) -> Result<(), SearchError> {
        let rows: Vec<ItemRow> = items.iter().map(ItemRow::from_domain).collect();
        let build_i = i64::from(build);
        let now = now_ts();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| SearchError::Storage(format!("begin tx: {e}")))?;
        for r in &rows {
            sqlx::query(
                "INSERT OR REPLACE INTO items(id, name, description, type, rarity, \
                 level, weight_class, chat_link, raw_json, fetched_at, build) \
                 VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            )
            .bind(i64::from(r.id))
            .bind(&r.name)
            .bind(&r.description)
            .bind(&r.item_type)
            .bind(&r.rarity)
            .bind(r.level.map(i64::from))
            .bind(&r.weight_class)
            .bind(&r.chat_link)
            .bind(&r.raw_json)
            .bind(now)
            .bind(build_i)
            .execute(&mut *tx)
            .await
            .map_err(|e| SearchError::Storage(format!("upsert item: {e}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| SearchError::Storage(format!("commit: {e}")))?;
        Ok(())
    }

    async fn upsert_achievements(
        &self,
        achievements: &[Achievement],
        build: u32,
    ) -> Result<(), SearchError> {
        let rows: Vec<AchievementRow> = achievements
            .iter()
            .map(AchievementRow::from_domain)
            .collect();
        let build_i = i64::from(build);
        let now = now_ts();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| SearchError::Storage(format!("begin tx: {e}")))?;
        for r in &rows {
            sqlx::query(
                "INSERT OR REPLACE INTO achievements(id, name, description, requirement, \
                 type, categories, repeatable, points, raw_json, fetched_at, build) \
                 VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            )
            .bind(i64::from(r.id))
            .bind(&r.name)
            .bind(&r.description)
            .bind(&r.requirement)
            .bind(&r.achievement_type)
            .bind(&r.categories_json)
            .bind(r.repeatable.map(i64::from))
            .bind(r.points.map(i64::from))
            .bind(&r.raw_json)
            .bind(now)
            .bind(build_i)
            .execute(&mut *tx)
            .await
            .map_err(|e| SearchError::Storage(format!("upsert achievement: {e}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| SearchError::Storage(format!("commit: {e}")))?;
        Ok(())
    }

    async fn build_number(&self) -> Result<Option<u32>, SearchError> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM meta WHERE key = ?")
            .bind("build_number")
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| SearchError::Storage(format!("read build_number: {e}")))?;
        Ok(row.and_then(|(s,)| s.parse::<u32>().ok()))
    }

    async fn set_build_number(&self, build: u32) -> Result<(), SearchError> {
        sqlx::query("INSERT OR REPLACE INTO meta(key, value) VALUES('build_number', ?)")
            .bind(build.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| SearchError::Storage(format!("set build_number: {e}")))?;
        Ok(())
    }

    async fn index_status(&self) -> Result<IndexStatus, SearchError> {
        let kinds = [
            "skills",
            "traits",
            "specializations",
            "items",
            "achievements",
        ];
        // build_number stored in meta — same value applies to every kind
        // because the indexer stamps it once at end of full pass.
        let build_row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM meta WHERE key = 'build_number'")
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| SearchError::Storage(format!("meta read: {e}")))?;
        let build_number = build_row.and_then(|(s,)| s.parse::<u32>().ok());

        let mut out = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {kind}"))
                .fetch_one(&self.pool)
                .await
                .map_err(|e| SearchError::Storage(format!("count {kind}: {e}")))?;
            // MAX(...) returns NULL on an empty table → Option<i64> = None.
            let last_refreshed_at: Option<i64> =
                sqlx::query_scalar(&format!("SELECT MAX(fetched_at) FROM {kind}"))
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|e| SearchError::Storage(format!("max(fetched_at): {e}")))?;
            let indexed_u = u32::try_from(total).unwrap_or(u32::MAX);
            // For now `indexed` and `total` are the same (we don't track
            // a separate "expected" count yet — that would require a
            // second meta row stamped by the indexer when it discovers
            // the upstream id list).
            out.push(KindStatus {
                name: kind.to_owned(),
                total: indexed_u,
                indexed: indexed_u,
                last_refreshed_at,
                build_number,
            });
        }
        Ok(IndexStatus { kinds: out })
    }
}

// ---------------------------------------------------------------------------
// Row builders — the bridge between domain types (which carry a flexible
// `extra: BTreeMap<String, Value>`) and the typed columns we index on.
// ---------------------------------------------------------------------------

struct SkillRow {
    id: u32,
    name: String,
    description: Option<String>,
    skill_type: Option<String>,
    slot: Option<String>,
    professions_json: Option<String>,
    weapon_type: Option<String>,
    chat_link: Option<String>,
    raw_json: String,
}

impl SkillRow {
    fn from_domain(s: &Skill) -> Self {
        let raw = serde_json::to_value(s).unwrap_or(serde_json::Value::Null);
        let professions = extract_string_array(&raw, "professions");
        Self {
            id: s.id.get(),
            name: s.name.clone(),
            description: extract_str(&raw, "description"),
            skill_type: extract_str(&raw, "type"),
            slot: extract_str(&raw, "slot"),
            professions_json: if professions.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&professions).unwrap_or_default())
            },
            weapon_type: extract_str(&raw, "weapon_type"),
            chat_link: extract_str(&raw, "chat_link"),
            raw_json: serde_json::to_string(&raw).unwrap_or_default(),
        }
    }
}

struct TraitRow {
    id: u32,
    name: String,
    description: Option<String>,
    specialization: Option<u32>,
    tier: Option<u32>,
    slot: Option<String>,
    raw_json: String,
}

impl TraitRow {
    fn from_domain(t: &Trait) -> Self {
        let raw = serde_json::to_value(t).unwrap_or(serde_json::Value::Null);
        Self {
            id: t.id.get(),
            name: t.name.clone(),
            description: extract_str(&raw, "description"),
            specialization: extract_u32(&raw, "specialization"),
            tier: extract_u32(&raw, "tier"),
            slot: extract_str(&raw, "slot"),
            raw_json: serde_json::to_string(&raw).unwrap_or_default(),
        }
    }
}

struct SpecRow {
    id: u32,
    name: String,
    profession: Option<String>,
    elite: bool,
    raw_json: String,
}

impl SpecRow {
    fn from_domain(s: &Specialization) -> Self {
        let raw = serde_json::to_value(s).unwrap_or(serde_json::Value::Null);
        Self {
            id: s.id.get(),
            name: s.name.clone(),
            profession: extract_str(&raw, "profession"),
            elite: extract_bool(&raw, "elite").unwrap_or(false),
            raw_json: serde_json::to_string(&raw).unwrap_or_default(),
        }
    }
}

struct ItemRow {
    id: u32,
    name: String,
    description: Option<String>,
    item_type: Option<String>,
    rarity: Option<String>,
    level: Option<u32>,
    weight_class: Option<String>,
    chat_link: Option<String>,
    raw_json: String,
}

impl ItemRow {
    fn from_domain(i: &Item) -> Self {
        let raw = serde_json::to_value(i).unwrap_or(serde_json::Value::Null);
        // weight_class can live at the top of the item OR nested under
        // `details.weight_class` (armor entries) — try both.
        let weight_class = extract_str(&raw, "weight_class").or_else(|| {
            raw.get("details")
                .and_then(|d| d.get("weight_class"))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        });
        Self {
            id: i.id.get(),
            name: i.name.clone(),
            description: extract_str(&raw, "description"),
            item_type: extract_str(&raw, "type"),
            rarity: extract_str(&raw, "rarity"),
            level: extract_u32(&raw, "level"),
            weight_class,
            chat_link: extract_str(&raw, "chat_link"),
            raw_json: serde_json::to_string(&raw).unwrap_or_default(),
        }
    }
}

struct AchievementRow {
    id: u32,
    name: String,
    description: Option<String>,
    requirement: Option<String>,
    achievement_type: Option<String>,
    categories_json: Option<String>,
    repeatable: Option<bool>,
    points: Option<u32>,
    raw_json: String,
}

impl AchievementRow {
    fn from_domain(a: &Achievement) -> Self {
        let raw = serde_json::to_value(a).unwrap_or(serde_json::Value::Null);
        let categories = extract_u32_array(&raw, "categories");
        // `points` is the sum of tiers[].points if present; cheap to compute
        // here so search results carry the rolled-up value.
        let points: Option<u32> = raw
            .get("tiers")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("points").and_then(serde_json::Value::as_u64))
                    .sum::<u64>()
            })
            .and_then(|n| u32::try_from(n).ok());
        let repeatable = raw
            .get("flags")
            .and_then(serde_json::Value::as_array)
            .map(|arr| arr.iter().any(|v| v.as_str() == Some("Repeatable")));
        Self {
            id: a.id.get(),
            name: a.name.clone(),
            description: extract_str(&raw, "description"),
            requirement: extract_str(&raw, "requirement"),
            achievement_type: extract_str(&raw, "type"),
            categories_json: if categories.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&categories).unwrap_or_default())
            },
            repeatable,
            points,
            raw_json: serde_json::to_string(&raw).unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Achievement, AchievementId, Item, ItemId, Skill, SkillId, Specialization, SpecializationId,
        Trait, TraitId,
    };
    use serde_json::json;

    fn skill(id: u32, name: &str, prof: &str, slot: &str, weapon: Option<&str>) -> Skill {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("description".into(), json!(format!("desc for {name}")));
        extra.insert("slot".into(), json!(slot));
        extra.insert("type".into(), json!("Weapon"));
        extra.insert("professions".into(), json!([prof]));
        if let Some(w) = weapon {
            extra.insert("weapon_type".into(), json!(w));
        }
        Skill {
            id: SkillId::new(i64::from(id)).unwrap(),
            name: name.to_owned(),
            extra,
        }
    }

    fn trait_obj(id: u32, name: &str, spec: u32, tier: u32) -> Trait {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("description".into(), json!(format!("desc for {name}")));
        extra.insert("specialization".into(), json!(spec));
        extra.insert("tier".into(), json!(tier));
        extra.insert("slot".into(), json!("Major"));
        Trait {
            id: TraitId::new(i64::from(id)).unwrap(),
            name: name.to_owned(),
            extra,
        }
    }

    fn spec(id: u32, name: &str, prof: &str, elite: bool) -> Specialization {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("profession".into(), json!(prof));
        extra.insert("elite".into(), json!(elite));
        Specialization {
            id: SpecializationId::new(i64::from(id)).unwrap(),
            name: name.to_owned(),
            extra,
        }
    }

    fn item(id: u32, name: &str, ty: &str, rarity: &str, level: u32) -> Item {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("description".into(), json!(format!("desc for {name}")));
        extra.insert("type".into(), json!(ty));
        extra.insert("rarity".into(), json!(rarity));
        extra.insert("level".into(), json!(level));
        Item {
            id: ItemId::new(i64::from(id)).unwrap(),
            name: name.to_owned(),
            extra,
        }
    }

    fn achievement(id: u32, name: &str, ty: &str) -> Achievement {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("description".into(), json!(format!("desc for {name}")));
        extra.insert("requirement".into(), json!(format!("do something {name}")));
        extra.insert("type".into(), json!(ty));
        extra.insert("categories".into(), json!([1, 2]));
        extra.insert("tiers".into(), json!([{"count":1,"points":10}]));
        Achievement {
            id: AchievementId::new(i64::from(id)).unwrap(),
            name: name.to_owned(),
            extra,
        }
    }

    #[tokio::test]
    async fn open_in_memory_initialises_schema() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        let status = idx.index_status().await.unwrap();
        assert_eq!(status.kinds.len(), 5);
        for k in &status.kinds {
            assert_eq!(k.indexed, 0);
        }
    }

    #[tokio::test]
    async fn empty_index_returns_not_indexed() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        let err = idx
            .search_skills("blade", 5, SkillSearchFilter::default())
            .await
            .unwrap_err();
        assert!(matches!(err, SearchError::NotIndexed { kind: "skills" }));
    }

    #[tokio::test]
    async fn upsert_and_search_skills_returns_match() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        idx.upsert_skills(
            &[
                skill(1, "Mind Wrack", "Mesmer", "Profession_1", None),
                skill(2, "Mind Stab", "Mesmer", "Weapon_2", Some("Greatsword")),
                skill(3, "Backstab", "Thief", "Weapon_1", Some("Dagger")),
            ],
            42,
        )
        .await
        .unwrap();

        let res = idx
            .search_skills("mind", 10, SkillSearchFilter::default())
            .await
            .unwrap();
        assert_eq!(res.len(), 2, "should match Mind Wrack + Mind Stab");
        assert!(res.iter().all(|r| r.name.starts_with("Mind")));
    }

    #[tokio::test]
    async fn search_skills_prefix_matching_works() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        idx.upsert_skills(
            &[skill(1, "Mind Wrack", "Mesmer", "Profession_1", None)],
            42,
        )
        .await
        .unwrap();
        // "wra" should prefix-match "Wrack"
        let res = idx
            .search_skills("wra", 10, SkillSearchFilter::default())
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "Mind Wrack");
    }

    #[tokio::test]
    async fn skill_filter_by_profession_excludes_others() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        idx.upsert_skills(
            &[
                skill(1, "Bladesong", "Mesmer", "Weapon_1", None),
                skill(2, "Bladestorm", "Warrior", "Weapon_3", None),
            ],
            42,
        )
        .await
        .unwrap();
        let res = idx
            .search_skills(
                "blade",
                10,
                SkillSearchFilter {
                    profession: Some("Mesmer".to_owned()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "Bladesong");
    }

    #[tokio::test]
    async fn upsert_traits_and_search_with_tier_filter() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        idx.upsert_traits(
            &[
                trait_obj(101, "Empowered", 5, 1),
                trait_obj(102, "Empowering Auras", 5, 3),
            ],
            42,
        )
        .await
        .unwrap();
        let res = idx
            .search_traits(
                "empower",
                10,
                TraitSearchFilter {
                    tier: Some(3),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "Empowering Auras");
    }

    #[tokio::test]
    async fn upsert_specs_and_filter_by_elite() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        idx.upsert_specializations(
            &[
                spec(40, "Chronomancer", "Mesmer", true),
                spec(41, "Mirage", "Mesmer", true),
                spec(50, "Domination", "Mesmer", false),
            ],
            42,
        )
        .await
        .unwrap();
        let res = idx
            .search_specializations(
                "chrono",
                10,
                SpecSearchFilter {
                    elite: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "Chronomancer");
    }

    #[tokio::test]
    async fn upsert_items_and_filter_by_rarity_and_level_range() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        idx.upsert_items(
            &[
                item(1, "Berserker's Greatsword", "Weapon", "Exotic", 80),
                item(2, "Berserker's Sword", "Weapon", "Ascended", 80),
                item(3, "Berserker's Dagger", "Weapon", "Exotic", 60),
            ],
            42,
        )
        .await
        .unwrap();
        let res = idx
            .search_items(
                "berserker",
                10,
                ItemSearchFilter {
                    rarity: Some("Exotic".to_owned()),
                    min_level: Some(80),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "Berserker's Greatsword");
    }

    #[tokio::test]
    async fn upsert_achievements_and_search() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        idx.upsert_achievements(
            &[
                achievement(1840, "Daily Completionist", "Daily"),
                achievement(283, "Tequatl Slayer", "WorldBoss"),
            ],
            42,
        )
        .await
        .unwrap();
        let res = idx
            .search_achievements("teq", 10, AchievementSearchFilter::default())
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "Tequatl Slayer");
    }

    #[tokio::test]
    async fn build_number_round_trip() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        assert_eq!(idx.build_number().await.unwrap(), None);
        idx.set_build_number(123_456).await.unwrap();
        assert_eq!(idx.build_number().await.unwrap(), Some(123_456));
    }

    #[tokio::test]
    async fn upsert_replaces_existing_row() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        idx.upsert_skills(&[skill(1, "Old Name", "Mesmer", "Weapon_1", None)], 10)
            .await
            .unwrap();
        idx.upsert_skills(&[skill(1, "New Name", "Mesmer", "Weapon_1", None)], 11)
            .await
            .unwrap();
        let res = idx
            .search_skills("name", 10, SkillSearchFilter::default())
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "New Name");
    }

    #[tokio::test]
    async fn diacritics_are_folded_for_search() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        idx.upsert_skills(&[skill(1, "Café", "Mesmer", "Weapon_1", None)], 1)
            .await
            .unwrap();
        // tokenizer = unicode61 remove_diacritics 1 → "cafe" matches "Café"
        let res = idx
            .search_skills("cafe", 10, SkillSearchFilter::default())
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
    }

    #[tokio::test]
    async fn empty_query_returns_empty_vec() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        idx.upsert_skills(&[skill(1, "Foo", "Mesmer", "Weapon_1", None)], 1)
            .await
            .unwrap();
        // Pure punctuation → no usable tokens.
        let res = idx
            .search_skills("!!!", 10, SkillSearchFilter::default())
            .await
            .unwrap();
        assert!(res.is_empty());
    }

    #[tokio::test]
    async fn open_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("idx.sqlite");
        let _idx = SqliteSearchIndex::open(&nested).await.unwrap();
        assert!(nested.exists());
    }

    #[tokio::test]
    async fn build_match_expr_ignores_punctuation_and_prefix_matches_last_token() {
        assert!(build_match_expr("").is_none());
        assert!(build_match_expr("---").is_none());
        let e = build_match_expr("Mind  Wrack!").unwrap();
        // Last token gets prefix `*`
        assert!(e.contains("\"Wrack\" *"));
        assert!(e.contains("\"Mind\""));
    }

    #[tokio::test]
    async fn index_status_reports_counts_and_build_number() {
        let idx = SqliteSearchIndex::open_in_memory().await.unwrap();
        idx.upsert_skills(&[skill(1, "S", "Mesmer", "Weapon_1", None)], 777)
            .await
            .unwrap();
        idx.set_build_number(777).await.unwrap();
        let status = idx.index_status().await.unwrap();
        let skills = status.kinds.iter().find(|k| k.name == "skills").unwrap();
        assert_eq!(skills.indexed, 1);
        assert_eq!(skills.build_number, Some(777));
    }
}
