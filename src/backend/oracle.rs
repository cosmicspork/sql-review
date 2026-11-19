//! Oracle through Oracle's pure-Rust driver, `oracledb` (no Instant Client).
//!
//! The driver is blocking, so each read runs on a blocking thread. A read
//! opens `SET TRANSACTION READ ONLY`, runs, and always rolls back.
//!
//! A proposed write runs on the write account with autocommit off: a
//! preview rolls back under a call timeout, and an apply locks the rows
//! `FOR UPDATE` before it runs and commits.
//!
//! Built and unit-tested here, but not run against an Oracle database:
//! `sql-review check-oracle` is how an operator verifies reads, and nothing
//! has verified writes yet.

use std::time::{Duration, Instant};

use oracledb::{Connection, Row, ToDbValue};

use super::{
    ApplyFail, DbError, Expected, Previewed, SyncConn, WritePlan, apply_on, placeholders,
    preview_on, take_limited,
};
use crate::{
    classify::Dialect,
    config::{Account, Config},
    value::{Cell, Param, Params, ResultSet},
};

/// The EZConnect string for these settings: `host:port/service`.
pub fn connect_string(cfg: &Config) -> String {
    let host = cfg.host.as_deref().unwrap_or_default();
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let mut out = host;
    if let Some(p) = cfg.port {
        out.push_str(&format!(":{p}"));
    }
    if let Some(s) = &cfg.database {
        out.push('/');
        out.push_str(s);
    }
    out
}

pub fn connect(cfg: &Config) -> Result<Connection, DbError> {
    connect_as(cfg, &cfg.account())
}

fn connect_as(cfg: &Config, account: &Account) -> Result<Connection, DbError> {
    let config = oracledb::Config::default()
        .set_credentials(
            &account.user,
            account.password.as_deref().unwrap_or_default(),
        )
        .set_connect_string(&connect_string(cfg))
        .map_err(DbError::connect)?;
    oracledb::connect(config).map_err(DbError::connect)
}

impl SyncConn for Connection {
    fn read(&self, sql: &str, params: &Params) -> Result<ResultSet, DbError> {
        read_on(self, sql, params, 0)
    }

    fn execute(&self, sql: &str, params: &Params) -> Result<u64, DbError> {
        let values = named_binds(sql, params)?;
        let binds: Vec<(&str, &dyn ToDbValue)> =
            values.iter().map(|(n, v)| (*n, v.as_ref())).collect();
        self.execute_named(sql, &binds)
            .map(|r| r.rows_affected())
            .map_err(DbError::query)
    }
}

pub async fn preview(
    cfg: &Config,
    account: &Account,
    plan: &WritePlan,
    sample: usize,
    limit: Duration,
) -> Result<Option<Previewed>, DbError> {
    let (cfg, account, plan) = (cfg.clone(), account.clone(), plan.clone());
    blocking(move || {
        let conn = connect_as(&cfg, &account)?;
        conn.set_call_timeout(Some(limit))
            .map_err(DbError::connect)?;
        let started = Instant::now();
        let out = preview_on(&conn, &plan, sample, started + limit);
        let rolled_back = conn
            .rollback()
            .map_err(|e| DbError(format!("rollback failed: {e}")));
        match out {
            Err(_) if started.elapsed() >= limit => Ok(None),
            out => {
                let out = out?;
                rolled_back?;
                Ok(Some(out))
            }
        }
    })
    .await
}

pub async fn apply(
    cfg: &Config,
    account: &Account,
    plan: &WritePlan,
    expect: &Expected,
) -> Result<u64, ApplyFail> {
    let (cfg, account, plan, expect) = (cfg.clone(), account.clone(), plan.clone(), expect.clone());
    tokio::task::spawn_blocking(move || {
        let conn = connect_as(&cfg, &account)?;
        match apply_on(&conn, &plan, &expect) {
            Ok(n) => {
                conn.commit()
                    .map_err(|e| DbError(format!("commit failed: {e}")))?;
                Ok(n)
            }
            Err(e) => {
                let _ = conn.rollback();
                Err(e)
            }
        }
    })
    .await
    .map_err(|e| DbError(format!("the apply task failed: {e}")))?
}

pub async fn check(cfg: &Config) -> Result<(), DbError> {
    let cfg = cfg.clone();
    blocking(move || {
        let conn = connect(&cfg)?;
        conn.query_row("SELECT 1 FROM DUAL", &[])
            .map_err(DbError::query)?;
        Ok(())
    })
    .await
}

pub async fn read(
    cfg: &Config,
    sql: &str,
    params: &Params,
    limit: u64,
) -> Result<ResultSet, DbError> {
    let (cfg, sql, params) = (cfg.clone(), sql.to_string(), params.clone());
    blocking(move || {
        let conn = connect(&cfg)?;
        read_only(&conn, |conn| read_on(conn, &sql, &params, limit))
    })
    .await
}

/// Run `f` inside a read-only transaction, rolling back whatever happens.
pub fn read_only<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> Result<T, DbError>,
) -> Result<T, DbError> {
    conn.execute("SET TRANSACTION READ ONLY", &[])
        .map_err(|e| DbError(format!("could not start a read-only transaction: {e}")))?;
    let out = f(conn);
    let rolled_back = conn
        .rollback()
        .map_err(|e| DbError(format!("rollback failed: {e}")));
    let out = out?;
    rolled_back?;
    Ok(out)
}

pub fn read_on(
    conn: &Connection,
    sql: &str,
    params: &Params,
    limit: u64,
) -> Result<ResultSet, DbError> {
    let values = named_binds(sql, params)?;
    let binds: Vec<(&str, &dyn ToDbValue)> = values.iter().map(|(n, v)| (*n, v.as_ref())).collect();
    let cursor = conn.query_named(sql, &binds).map_err(DbError::query)?;
    let columns: Vec<String> = cursor
        .columns()
        .iter()
        .map(|m| m.name().to_string())
        .collect();
    let rows = cursor.map(|row| row.and_then(|r| cells(&r)));
    let (rows, truncated) = take_limited(rows, limit).map_err(DbError::query)?;
    Ok(ResultSet {
        columns,
        rows,
        truncated,
    })
}

type NamedBind<'a> = (&'a str, Box<dyn ToDbValue>);

/// Oracle refuses a bind the statement doesn't name, so bind only the names
/// it uses. Oracle names are case-insensitive.
fn named_binds<'a>(sql: &str, params: &'a Params) -> Result<Vec<NamedBind<'a>>, DbError> {
    let used: Vec<String> = placeholders(sql, Dialect::Oracle)?
        .into_iter()
        .map(|(_, n)| n.to_ascii_uppercase())
        .collect();
    Ok(params
        .iter()
        .filter(|(name, _)| used.contains(&name.to_ascii_uppercase()))
        .map(|(name, value)| (name.as_str(), bind(value)))
        .collect())
}

fn bind(p: &Param) -> Box<dyn ToDbValue> {
    match p {
        Param::Null => Box::new(None::<String>),
        Param::Bool(b) => Box::new(*b),
        Param::Int(i) => Box::new(*i),
        Param::Float(f) => Box::new(*f),
        Param::Text(s) => Box::new(s.clone()),
    }
}

fn cells(row: &Row) -> Result<Vec<Cell>, oracledb::Error> {
    row.columns()
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let t = m.db_type();
            let opt = |c: Option<Cell>| c.unwrap_or(Cell::Null);
            Ok(
                if t == oracledb::DB_TYPE_NUMBER || t == oracledb::DB_TYPE_BINARY_INTEGER {
                    opt(row.get::<Option<String>>(i)?.map(|s| number_cell(&s)))
                } else if t == oracledb::DB_TYPE_BINARY_DOUBLE
                    || t == oracledb::DB_TYPE_BINARY_FLOAT
                {
                    opt(row.get::<Option<f64>>(i)?.map(Cell::Float))
                } else if t == oracledb::DB_TYPE_BOOLEAN {
                    opt(row.get::<Option<bool>>(i)?.map(Cell::Bool))
                } else if t.is_string_type() {
                    opt(row.get::<Option<String>>(i)?.map(Cell::Text))
                } else if t.is_date_type() {
                    opt(row
                        .get::<Option<oracledb::OracleTimestamp>>(i)?
                        .map(|ts| Cell::Text(ts.to_string())))
                } else if t.is_binary_type() {
                    opt(row.get::<Option<Vec<u8>>>(i)?.map(Cell::Bytes))
                } else if t == oracledb::DB_TYPE_INTERVAL_DS {
                    opt(row
                        .get::<Option<oracledb::OracleIntervalDS>>(i)?
                        .map(|v| Cell::Text(v.to_string())))
                } else if t == oracledb::DB_TYPE_INTERVAL_YM {
                    opt(row
                        .get::<Option<oracledb::OracleIntervalYM>>(i)?
                        .map(|v| Cell::Text(v.to_string())))
                } else {
                    match row.get::<Option<String>>(i) {
                        Ok(v) => opt(v.map(Cell::Text)),
                        Err(_) => Cell::Text(format!("<{}>", t.name())),
                    }
                },
            )
        })
        .collect()
}

/// An Oracle NUMBER, from the driver's exact decimal text. Integers that fit
/// an i64 are integers; anything else stays a decimal string, so no digit
/// is lost to a float.
pub fn number_cell(text: &str) -> Cell {
    match text.parse::<i64>() {
        Ok(i) => Cell::Int(i),
        Err(_) => Cell::Text(text.to_string()),
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, DbError> + Send + 'static,
) -> Result<T, DbError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| DbError(format!("the query task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Resolve, resolve};

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let src = crate::config::CredentialSource(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        resolve(&src, Resolve::default()).unwrap()
    }

    #[test]
    fn connect_string_from_parts() {
        let c = cfg(&[
            ("DB_HOST", "db.example.edu"),
            ("DB_PORT", "2982"),
            ("DB_SERVICE", "prduno.db"),
            ("DB_USER", "u"),
        ]);
        assert_eq!(connect_string(&c), "db.example.edu:2982/prduno.db");
        let c = cfg(&[("DB_HOST", "h"), ("DB_USER", "u")]);
        assert_eq!(connect_string(&c), "h:1521");
        let c = cfg(&[("DB_HOST", "::1"), ("DB_USER", "u"), ("DB_SERVICE", "s")]);
        assert_eq!(connect_string(&c), "[::1]:1521/s");
    }

    #[test]
    fn connect_string_from_database_url() {
        let c = cfg(&[(
            "DATABASE_URL",
            "oracle+oracledb://u:p@h:1522/?service_name=PRD",
        )]);
        assert_eq!(connect_string(&c), "h:1522/PRD");
        assert_eq!(c.account().user, "u");
    }

    #[test]
    fn numbers_keep_their_digits() {
        assert_eq!(number_cell("42"), Cell::Int(42));
        assert_eq!(number_cell("-7"), Cell::Int(-7));
        assert_eq!(number_cell("1.5"), Cell::Text("1.5".into()));
        assert_eq!(
            number_cell("123456789012345678901234567890"),
            Cell::Text("123456789012345678901234567890".into())
        );
    }
}
