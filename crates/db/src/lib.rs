//! Persistence layer: SQLite (default) and PostgreSQL (optional) behind one
//! `Db` facade (spec §26). The Store writes portable SQL (`?` placeholders,
//! TEXT keys, INTEGER timestamps); the PG backend rewrites on the fly.

pub mod migrator;
pub mod pg;
pub mod sqlite;
pub mod store;

pub use pg::PgDb;
pub use sqlite::{DbError, DbResult, DbValue, Param, SqliteDb};
pub use store::{RunRow, ScheduleRow, Store};

/// One of the two backends. Same SQL, same result shape.
#[derive(Clone)]
pub enum Db {
    Sqlite(std::sync::Arc<SqliteDb>),
    Pg(std::sync::Arc<PgDb>),
}

impl Db {
    pub async fn execute(&self, sql: &str, params: &[Param]) -> DbResult<usize> {
        match self {
            Db::Sqlite(d) => d.execute(sql, params).await,
            Db::Pg(d) => d.execute(sql, params).await,
        }
    }

    pub async fn query(
        &self,
        sql: &str,
        params: &[Param],
    ) -> DbResult<Vec<Vec<(String, DbValue)>>> {
        match self {
            Db::Sqlite(d) => d.query(sql, params).await,
            Db::Pg(d) => d.query(sql, params).await,
        }
    }
}
