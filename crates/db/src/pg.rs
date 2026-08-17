//! PostgreSQL backend (spec §26): tokio-postgres, optional — the Manager
//! defaults to SQLite; PG is for high-concurrency fleets. SQL written for
//! SQLite (`?` placeholders) is rewritten to `$n` on the fly.

use crate::sqlite::{DbError, DbResult, DbValue, Param};

/// Host of a DSN: between the last `@` (userinfo) and the first `/`
/// (dbname), minus the optional :port. Empty = unix socket form.
fn dsn_host(rest: &str) -> &str {
    let after = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    after
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
}

pub struct PgDb {
    client: tokio_postgres::Client,
}

impl PgDb {
    pub async fn connect(url: &str) -> DbResult<PgDb> {
        // Local deployments talk to a loopback PG; a remote DSN over NoTls
        // would send credentials in the clear — refuse and tell the operator.
        if let Some(rest) = url
            .strip_prefix("postgres://")
            .or_else(|| url.strip_prefix("postgresql://"))
        {
            // host = between the last `@` (userinfo) and the first `/` (dbname),
            // minus the optional :port.
            let host = dsn_host(rest);
            if !host.is_empty()
                && host != "localhost"
                && host != "127.0.0.1"
                && host != "::1"
                && !url.contains("sslmode=require")
                && !url.contains("sslmode=verify")
            {
                return Err(DbError::Sql(format!(
                "refusing plaintext PostgreSQL to remote host `{host}` (add sslmode=require to the DSN)"
            )));
            }
        }
        let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
            .await
            .map_err(|e| DbError::Sql(e.to_string()))?;
        // The connection must be driven; spawn it detached.
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(PgDb { client })
    }

    pub async fn execute(&self, sql: &str, params: &[Param]) -> DbResult<usize> {
        let (sql, _) = rewrite_placeholders(sql);
        let pg: Vec<PgParam> = params.iter().map(PgParam::from).collect();
        let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = pg
            .iter()
            .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();
        self.client
            .execute(&sql, &refs)
            .await
            .map(|n| n as usize)
            .map_err(|e| DbError::Sql(e.to_string()))
    }

    pub async fn query(
        &self,
        sql: &str,
        params: &[Param],
    ) -> DbResult<Vec<Vec<(String, DbValue)>>> {
        let (sql, _) = rewrite_placeholders(sql);
        let pg: Vec<PgParam> = params.iter().map(PgParam::from).collect();
        let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = pg
            .iter()
            .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();
        let rows = self
            .client
            .query(&sql, &refs)
            .await
            .map_err(|e| DbError::Sql(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            let mut cells = Vec::with_capacity(row.len());
            for (i, col) in row.columns().iter().enumerate() {
                let value = match col.type_() {
                    &tokio_postgres::types::Type::INT2
                    | &tokio_postgres::types::Type::INT4
                    | &tokio_postgres::types::Type::INT8 => row
                        .try_get::<_, i64>(i)
                        .ok()
                        .map(DbValue::Int)
                        .unwrap_or(DbValue::Null),
                    &tokio_postgres::types::Type::FLOAT4 | &tokio_postgres::types::Type::FLOAT8 => {
                        row.try_get::<_, f64>(i)
                            .ok()
                            .map(|f| DbValue::Text(f.to_string()))
                            .unwrap_or(DbValue::Null)
                    }
                    _ => row
                        .try_get::<_, String>(i)
                        .ok()
                        .map(DbValue::Text)
                        .unwrap_or(DbValue::Null),
                };
                cells.push((col.name().to_string(), value));
            }
            out.push(cells);
        }
        Ok(out)
    }

    /// Run raw statements (migrations). No placeholder rewriting — DDL has none.
    pub async fn simple(&self, sql: &str) -> DbResult<()> {
        self.client
            .simple_query(sql)
            .await
            .map(|_| ())
            .map_err(|e| DbError::Sql(e.to_string()))
    }
}

/// `?` → `$1`, `$2`, ... (our SQL has no literal `?` in strings).
fn rewrite_placeholders(sql: &str) -> (String, usize) {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0;
    for c in sql.chars() {
        if c == '?' {
            n += 1;
            out.push('$');
            out.push_str(&n.to_string());
        } else {
            out.push(c);
        }
    }
    (out, n)
}

/// A single bound parameter for tokio-postgres.
#[derive(Debug)]
enum PgParam {
    Int(i64),
    Real(f64),
    Text(String),
    Null,
}

impl From<&Param> for PgParam {
    fn from(p: &Param) -> Self {
        match p {
            Param::Int(i) => PgParam::Int(*i),
            Param::Real(f) => PgParam::Real(*f),
            Param::Text(s) => PgParam::Text(s.clone()),
            Param::Null => PgParam::Null,
        }
    }
}

impl tokio_postgres::types::ToSql for PgParam {
    fn to_sql(
        &self,
        ty: &tokio_postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self {
            PgParam::Int(i) => i.to_sql(ty, out),
            PgParam::Real(f) => f.to_sql(ty, out),
            PgParam::Text(s) => s.to_sql(ty, out),
            PgParam::Null => Ok(tokio_postgres::types::IsNull::Yes),
        }
    }

    fn accepts(ty: &tokio_postgres::types::Type) -> bool {
        <i64 as tokio_postgres::types::ToSql>::accepts(ty)
            || <f64 as tokio_postgres::types::ToSql>::accepts(ty)
            || <String as tokio_postgres::types::ToSql>::accepts(ty)
    }

    fn to_sql_checked(
        &self,
        ty: &tokio_postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self {
            PgParam::Int(i) => i.to_sql_checked(ty, out),
            PgParam::Real(f) => f.to_sql_checked(ty, out),
            PgParam::Text(s) => s.to_sql_checked(ty, out),
            PgParam::Null => Ok(tokio_postgres::types::IsNull::Yes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::dsn_host;

    #[test]
    fn dsn_host_forms() {
        // credentialed loopback
        assert_eq!(dsn_host("user:pass@127.0.0.1:5432/db"), "127.0.0.1");
        // credentialed remote
        assert_eq!(dsn_host("user:pass@remote.com/db"), "remote.com");
        // no userinfo remote
        assert_eq!(dsn_host("db.example.com/synora"), "db.example.com");
        // no userinfo remote with port
        assert_eq!(dsn_host("db.example.com:5432/synora"), "db.example.com");
        // unix socket form → empty, allowed
        assert_eq!(dsn_host("/db"), "");
    }
}
