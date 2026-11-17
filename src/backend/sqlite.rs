//! SQLite through rusqlite (bundled). The file is opened read-only and the
//! connection set to `query_only`, so SQLite itself refuses any write.

use rusqlite::{Connection, OpenFlags, types::ValueRef};

use super::{DbError, take_limited};
use crate::{
    config::Config,
    value::{Cell, Param, Params, ResultSet},
};

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
    // Bind only the names the statement uses, as consulta's SQLAlchemy did.
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
