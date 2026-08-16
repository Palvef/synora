//! Hand-rolled migration runner (spec §96): numbered `NNNN_*.sql` files run
//! in order against either backend, tracked in `schema_migrations`.

use crate::sqlite::{DbError, DbResult, Param};
use crate::Db;
use std::path::Path;

pub struct Migrator {
    dir: std::path::PathBuf,
}

impl Migrator {
    pub fn new(dir: &Path) -> Self {
        Migrator { dir: dir.to_path_buf() }
    }

    pub async fn run(&self, db: &Db) -> DbResult<Vec<String>> {
        db.execute(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL)",
            &[],
        )
        .await?;

        let mut files: Vec<(u64, std::path::PathBuf)> = Vec::new();
        let entries = std::fs::read_dir(&self.dir).map_err(|e| DbError::Sql(e.to_string()))?;
        for entry in entries {
            let path = entry.map_err(|e| DbError::Sql(e.to_string()))?.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if let Some(num) = name.split('_').next().and_then(|s| s.parse::<u64>().ok()) {
                if name.ends_with(".sql") {
                    files.push((num, path));
                }
            }
        }
        files.sort_by_key(|(num, _)| *num);

        let now = unix_now();
        let mut applied = Vec::new();
        for (version, path) in files {
            let rows = db
                .query(
                    "SELECT version FROM schema_migrations WHERE version = ?",
                    &[Param::Int(version as i64)],
                )
                .await?;
            if !rows.is_empty() {
                continue;
            }
            let sql = std::fs::read_to_string(&path).map_err(|e| DbError::Sql(e.to_string()))?;
            let path_display = path.display().to_string();
            for stmt in split_statements(&sql) {
                match db {
                    Db::Sqlite(d) => {
                        let pd = path_display.clone();
                        d.with_conn(move |conn| {
                            conn.execute(&stmt, rusqlite::params![])
                                .map_err(|e| DbError::Sql(format!("in {pd}: {e}")))?;
                            Ok(())
                        })
                        .await?;
                    }
                    Db::Pg(d) => {
                        d.simple(&stmt)
                            .await
                            .map_err(|e| DbError::Sql(format!("in {path_display}: {e}")))?;
                    }
                }
            }
            db.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?,?)",
                &[Param::Int(version as i64), Param::Int(now)],
            )
            .await?;
            applied.push(format!("{} (v{version})", path.display()));
        }
        Ok(applied)
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Split on `;` — `--` comments are stripped first so a `;` inside a comment
/// cannot break a statement (our own DDL files only use `--` comments).
fn split_statements(sql: &str) -> Vec<String> {
    let mut cleaned = String::with_capacity(sql.len());
    for line in sql.lines() {
        match line.find("--") {
            Some(i) => cleaned.push_str(&line[..i]),
            None => cleaned.push_str(line),
        }
        cleaned.push('\n');
    }
    cleaned
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}
