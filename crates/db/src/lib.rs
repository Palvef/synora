//! Persistence layer. M1: SQLite (rusqlite, bundled). Pg backend lands in M3
//! behind the same `Store` API.

pub mod migrator;
pub mod sqlite;
pub mod store;

pub use sqlite::{DbValue, SqliteDb};
pub use store::{RunRow, ScheduleRow, Store};
