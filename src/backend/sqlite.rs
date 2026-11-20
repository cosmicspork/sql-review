//! SQLite through rusqlite (bundled). For reads the file is opened read-only
//! and the connection set to `query_only`, so SQLite itself refuses any
//! write. An approved write opens it read-write; SQLite has no logins, so
//! the write account's settings only switch that on.
//!
//! A proposal reads the rows a write would change on the read-only
//! connection and never runs the write. SQLite has no row locks, so an
//! apply starts with `BEGIN IMMEDIATE`, which takes the database's write
//! lock: nothing else writes between reading the rows and committing.

use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Statement, types::ValueRef};

use super::{ApplyFail, DbError, Expected, SyncConn, WritePlan, apply_on, take_limited};
use crate::{
    config::Config,
    value::{Cell, Param, Params, ResultSet},
};

/// How long an apply waits for another writer to finish.
const APPLY_BUSY_TIMEOUT: Duration = Duration::from_secs(30);

fn open(cfg: &Config) -> Result<Connection, DbError> {
    let path = cfg.database.as_deref().unwrap_or(":memory:");
    let conn = if path == ":memory:" {
        Connection::open_in_memory()
    } else {
        Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    }
    .map_err(DbError::connect)?;
    conn.pragma_update(None, "query_only", "ON")
        .map_err(DbError::connect)?;
    Ok(conn)
}

fn open_write(cfg: &Config, busy: Duration) -> Result<Connection, DbError> {
    let path = cfg.database.as_deref().unwrap_or(":memory:");
    let conn = if path == ":memory:" {
        Connection::open_in_memory()
    } else {
        Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    }
    .map_err(DbError::connect)?;
    conn.busy_timeout(busy).map_err(DbError::connect)?;
    Ok(conn)
}

impl SyncConn for Connection {
    fn read(&self, sql: &str, params: &Params) -> Result<ResultSet, DbError> {
        read_on(self, sql, params, 0)
    }

    fn execute(&self, sql: &str, params: &Params) -> Result<u64, DbError> {
        let mut stmt = self.prepare(sql).map_err(DbError::query)?;
        bind(&mut stmt, params)?;
        stmt.raw_execute().map(|n| n as u64).map_err(DbError::query)
    }
}

/// The rows a write would change, read on a read-only, `query_only`
/// connection. Interrupted, and `None`, after `limit`.
pub async fn read_rows(
    cfg: &Config,
    sql: &str,
    params: &Params,
    limit: Duration,
) -> Result<Option<ResultSet>, DbError> {
    let (cfg, sql, params) = (cfg.clone(), sql.to_string(), params.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    let mut task = tokio::task::spawn_blocking(move || {
        let conn = open(&cfg)?;
        let _ = tx.send(conn.get_interrupt_handle());
        read_on(&conn, &sql, &params, 0)
    });
    match tokio::time::timeout(limit, &mut task).await {
        Ok(joined) => joined
            .map_err(|e| DbError(format!("the read task failed: {e}")))?
            .map(Some),
        Err(_) => {
            if let Ok(handle) = rx.try_recv() {
                handle.interrupt();
            }
            let _ = task.await;
            Ok(None)
        }
    }
}

pub async fn apply(cfg: &Config, plan: &WritePlan, expect: &Expected) -> Result<u64, ApplyFail> {
    let (cfg, plan, expect) = (cfg.clone(), plan.clone(), expect.clone());
    tokio::task::spawn_blocking(move || {
        let conn = open_write(&cfg, APPLY_BUSY_TIMEOUT)?;
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(DbError::query)?;
        match apply_on(&conn, &plan, &expect) {
            Ok(n) => {
                conn.execute_batch("COMMIT")
                    .map_err(|e| DbError(format!("commit failed: {e}")))?;
                Ok(n)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
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
        let conn = open(&cfg)?;
        conn.query_row("SELECT 1", [], |_| Ok(()))
            .map_err(DbError::query)
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
        let conn = open(&cfg)?;
        read_on(&conn, &sql, &params, limit)
    })
    .await
}

fn read_on(
    conn: &Connection,
    sql: &str,
    params: &Params,
    limit: u64,
) -> Result<ResultSet, DbError> {
    let mut stmt = conn.prepare(sql).map_err(DbError::query)?;
    bind(&mut stmt, params)?;
    let columns: Vec<String> = stmt.column_names().map(String::from).collect();
    let width = columns.len();
    let mut rows = stmt.raw_query();
    let iter = std::iter::from_fn(|| match rows.next() {
        Ok(Some(row)) => Some(
            (0..width)
                .map(|i| row.get_ref(i).map(cell))
                .collect::<Result<Vec<_>, _>>(),
        ),
        Ok(None) => None,
        Err(e) => Some(Err(e)),
    });
    let (rows, truncated) = take_limited(iter, limit).map_err(DbError::query)?;
    Ok(ResultSet {
        columns,
        rows,
        truncated,
    })
}

/// Bind only the names the statement uses, as consulta's SQLAlchemy did.
fn bind(stmt: &mut Statement<'_>, params: &Params) -> Result<(), DbError> {
    for (name, value) in params {
        let Some(index) = stmt
            .parameter_index(&format!(":{name}"))
            .map_err(DbError::query)?
        else {
            continue;
        };
        let bound = match value {
            Param::Null => stmt.raw_bind_parameter(index, rusqlite::types::Null),
            Param::Bool(b) => stmt.raw_bind_parameter(index, b),
            Param::Int(i) => stmt.raw_bind_parameter(index, i),
            Param::Float(f) => stmt.raw_bind_parameter(index, f),
            Param::Text(s) => stmt.raw_bind_parameter(index, s),
        };
        bound.map_err(DbError::query)?;
    }
    Ok(())
}

fn cell(v: ValueRef<'_>) -> Cell {
    match v {
        ValueRef::Null => Cell::Null,
        ValueRef::Integer(i) => Cell::Int(i),
        ValueRef::Real(f) => Cell::Float(f),
        ValueRef::Text(t) => Cell::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Cell::Bytes(b.to_vec()),
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, DbError> + Send + 'static,
) -> Result<T, DbError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| DbError(format!("the query task failed: {e}")))?
}
