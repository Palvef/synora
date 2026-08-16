//! SQLite backend: rusqlite (bundled) behind a tokio Mutex.
//! Single-writer by design — enough for a Manager up to Yuki-scale.

use std::path::Path;

/// Cell values in result rows.
#[derive(Debug, Clone, PartialEq)]
pub enum DbValue {
    Int(i64),
    Text(String),
    Null,
}

impl DbValue {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            DbValue::Int(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            DbValue::Text(v) => Some(v),
            _ => None,
        }
    }
}

/// A bound parameter.
#[derive(Debug, Clone)]
pub enum Param {
    Int(i64),
    Real(f64),
    Text(String),
    Null,
}

impl From<i64> for Param {
    fn from(v: i64) -> Self {
        Param::Int(v)
    }
}
impl From<f64> for Param {
    fn from(v: f64) -> Self {
        Param::Real(v)
    }
}
impl From<i32> for Param {
    fn from(v: i32) -> Self {
        Param::Int(v as i64)
    }
}
impl From<u64> for Param {
    fn from(v: u64) -> Self {
        Param::Int(v as i64)
    }
}
impl From<&str> for Param {
    fn from(v: &str) -> Self {
        Param::Text(v.to_string())
    }
}
impl From<String> for Param {
    fn from(v: String) -> Self {
        Param::Text(v)
    }
}
impl From<Option<String>> for Param {
    fn from(v: Option<String>) -> Self {
        match v {
            Some(s) => Param::Text(s),
            None => Param::Null,
        }
    }
}
impl From<Option<i64>> for Param {
    fn from(v: Option<i64>) -> Self {
        match v {
            Some(i) => Param::Int(i),
            None => Param::Null,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sql(String),
}

pub type DbResult<T> = Result<T, DbError>;

pub struct SqliteDb {
    conn: tokio::sync::Mutex<rusqlite::Connection>,
}

impl SqliteDb {
    pub fn open(path: &Path) -> DbResult<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| DbError::Sql(e.to_string()))?;
            }
        }
        let conn = rusqlite::Connection::open(path).map_err(|e| DbError::Sql(e.to_string()))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| DbError::Sql(e.to_string()))?;
        Ok(SqliteDb {
            conn: tokio::sync::Mutex::new(conn),
        })
    }

    /// Execute a statement (INSERT/UPDATE/DELETE/DDL), returning rows affected.
    pub async fn execute(&self, sql: &str, params: &[Param]) -> DbResult<usize> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(sql).map_err(|e| DbError::Sql(e.to_string()))?;
        let values: Vec<rusqlite::types::Value> = params.iter().map(to_sql_value).collect();
        let n = stmt
            .execute(rusqlite::params_from_iter(values.iter()))
            .map_err(|e| DbError::Sql(e.to_string()))?;
        Ok(n)
    }

    /// Query rows as name→value maps.
    pub async fn query(
        &self,
        sql: &str,
        params: &[Param],
    ) -> DbResult<Vec<Vec<(String, DbValue)>>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(sql).map_err(|e| DbError::Sql(e.to_string()))?;
        let values: Vec<rusqlite::types::Value> = params.iter().map(to_sql_value).collect();
        let column_names: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                let mut cells = Vec::with_capacity(column_names.len());
                for (i, name) in column_names.iter().enumerate() {
                    let v: rusqlite::types::Value = row.get(i)?;
                    cells.push((name.clone(), from_sql_value(v)));
                }
                Ok(cells)
            })
            .map_err(|e| DbError::Sql(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| DbError::Sql(e.to_string()))?);
        }
        Ok(out)
    }

    /// Run a closure with exclusive access (multi-statement transactions).
    pub async fn with_conn<F, T>(&self, f: F) -> DbResult<T>
    where
        F: FnOnce(&mut rusqlite::Connection) -> DbResult<T> + Send,
    {
        let mut conn = self.conn.lock().await;
        f(&mut conn)
    }
}

fn to_sql_value(p: &Param) -> rusqlite::types::Value {
    match p {
        Param::Int(i) => rusqlite::types::Value::Integer(*i),
        Param::Real(f) => rusqlite::types::Value::Real(*f),
        Param::Text(s) => rusqlite::types::Value::Text(s.clone()),
        Param::Null => rusqlite::types::Value::Null,
    }
}

fn from_sql_value(v: rusqlite::types::Value) -> DbValue {
    match v {
        rusqlite::types::Value::Integer(i) => DbValue::Int(i),
        rusqlite::types::Value::Text(s) => DbValue::Text(s),
        rusqlite::types::Value::Null => DbValue::Null,
        rusqlite::types::Value::Real(f) => DbValue::Text(f.to_string()),
        rusqlite::types::Value::Blob(b) => DbValue::Text(String::from_utf8_lossy(&b).into_owned()),
    }
}
