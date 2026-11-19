//! SQL Server through tiberius over rustls, with SQL authentication.
//!
//! SQL Server has no read-only transaction. A read runs inside `BEGIN TRAN`
//! that is always rolled back, which undoes transactional writes only, and
//! the connection asks for `ApplicationIntent=ReadOnly`, which matters only
//! on an availability-group secondary. The read-only login is the boundary.
//!
//! A proposed write runs on the write login with `XACT_ABORT ON`: a preview
//! rolls back, and an apply reads the rows `WITH (UPDLOCK, HOLDLOCK)` before
//! it runs and commits. Nothing here has run against a server yet.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use futures_util::TryStreamExt;
use tiberius::{AuthMethod, Client, ColumnData, EncryptionLevel, FromSql, ToSql};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{
    ApplyFail, DbError, Expected, Previewed, WritePlan, align, placeholders, split_locator,
};
use crate::{
    classify::Dialect,
    config::{Account, Config},
    value::{Cell, Param, Params, ResultSet},
};

type Conn = Client<Compat<TcpStream>>;

async fn connect(cfg: &Config) -> Result<Conn, DbError> {
    connect_as(cfg, &cfg.account(), true).await
}

async fn connect_as(cfg: &Config, account: &Account, read_only: bool) -> Result<Conn, DbError> {
    let mut c = tiberius::Config::new();
    c.host(cfg.host.as_deref().unwrap_or("localhost"));
    c.port(cfg.port.unwrap_or(1433));
    if let Some(db) = &cfg.database {
        c.database(db);
    }
    c.authentication(AuthMethod::sql_server(
        &account.user,
        account.password.as_deref().unwrap_or_default(),
    ));
    c.encryption(EncryptionLevel::Required);
    if cfg.trust_server_certificate {
        c.trust_cert();
    }
    c.readonly(read_only);
    c.application_name("sql-review");
    let tcp = TcpStream::connect(c.get_addr())
        .await
        .map_err(DbError::connect)?;
    tcp.set_nodelay(true).map_err(DbError::connect)?;
    Client::connect(c, tcp.compat_write())
        .await
        .map_err(DbError::connect)
}

pub async fn check(cfg: &Config) -> Result<(), DbError> {
    let mut client = connect(cfg).await?;
    client
        .simple_query("SELECT 1")
        .await
        .map_err(DbError::query)?
        .into_results()
        .await
        .map_err(DbError::query)?;
    Ok(())
}

async fn simple(client: &mut Conn, sql: &str) -> Result<(), DbError> {
    client
        .simple_query(sql)
        .await
        .map_err(DbError::query)?
        .into_results()
        .await
        .map_err(DbError::query)?;
    Ok(())
}

/// The statement with `@P` positions, and its values in order.
fn bound(sql: &str, params: &Params) -> Result<(String, Vec<Box<dyn ToSql>>), DbError> {
    let (sql, names) = rewrite_placeholders(sql)?;
    let values = names
        .iter()
        .map(|n| {
            params
                .get(n)
                .map(bind)
                .ok_or_else(|| DbError(format!("no value given for :{n}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((sql, values))
}

async fn select(client: &mut Conn, sql: &str, params: &Params) -> Result<ResultSet, DbError> {
    let (sql, values) = bound(sql, params)?;
    read_on(client, &sql, &values, 0).await
}

async fn execute(client: &mut Conn, sql: &str, params: &Params) -> Result<u64, DbError> {
    let (sql, values) = bound(sql, params)?;
    let refs: Vec<&dyn ToSql> = values.iter().map(|v| v.as_ref()).collect();
    Ok(client
        .execute(sql, &refs)
        .await
        .map_err(DbError::query)?
        .total())
}

pub async fn preview(
    cfg: &Config,
    account: &Account,
    plan: &WritePlan,
    sample: usize,
    limit: std::time::Duration,
) -> Result<Option<Previewed>, DbError> {
    let run = async {
        let mut client = connect_as(cfg, account, false).await?;
        simple(&mut client, "SET XACT_ABORT ON; BEGIN TRANSACTION").await?;
        let out = preview_on(&mut client, plan, sample).await;
        let rolled_back = rollback(&mut client).await;
        let out = out?;
        rolled_back?;
        Ok(out)
    };
    // On timeout the connection is dropped, which ends the session, and SQL
    // Server rolls back its open transaction.
    match tokio::time::timeout(limit, run).await {
        Ok(out) => out.map(Some),
        Err(_) => Ok(None),
    }
}

async fn preview_on(
    client: &mut Conn,
    plan: &WritePlan,
    sample: usize,
) -> Result<Previewed, DbError> {
    let mut before = None;
    let mut found_again = None;
    if let Some(rows) = &plan.rows {
        if let Some(located) = &rows.located
            && let Ok(rs) = select(client, &located.select, &plan.params).await
        {
            let (rs, locators) = split_locator(rs, sample);
            found_again = Some((located.after_prefix.clone(), locators));
            before = Some(rs);
        }
        if before.is_none() {
            before = Some(select(client, &rows.select, &plan.params).await?);
        }
    }
    let count = execute(client, &plan.statement, &plan.params).await?;
    let after = match found_again {
        Some((prefix, locators)) if !locators.is_empty() => {
            let sql = format!("{prefix}{})", locators.join(", "));
            Some(align(
                select(client, &sql, &Params::new()).await?,
                &locators,
            ))
        }
        _ => None,
    };
    Ok(Previewed {
        before,
        count,
        after,
    })
}

pub async fn apply(
    cfg: &Config,
    account: &Account,
    plan: &WritePlan,
    expect: &Expected,
) -> Result<u64, ApplyFail> {
    let mut client = connect_as(cfg, account, false).await?;
    simple(&mut client, "SET XACT_ABORT ON; BEGIN TRANSACTION").await?;
    let out = async {
        if let Some(rows) = &plan.rows {
            expect.check_rows(&select(&mut client, &rows.lock, &plan.params).await?)?;
        }
        let count = execute(&mut client, &plan.statement, &plan.params).await?;
        expect.check_count(count)?;
        Ok::<_, ApplyFail>(count)
    }
    .await;
    match out {
        Ok(n) => {
            simple(&mut client, "COMMIT TRANSACTION")
                .await
                .map_err(|e| DbError(format!("commit failed: {e}")))?;
            Ok(n)
        }
        Err(e) => {
            let _ = rollback(&mut client).await;
            Err(e)
        }
    }
}

pub async fn read(
    cfg: &Config,
    sql: &str,
    params: &Params,
    limit: u64,
) -> Result<ResultSet, DbError> {
    let (sql, values) = bound(sql, params)?;

    let mut client = connect(cfg).await?;
    // consulta's isolation level, so a read never waits on a writer's locks.
    client
        .simple_query("SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; BEGIN TRANSACTION")
        .await
        .map_err(DbError::query)?
        .into_results()
        .await
        .map_err(DbError::query)?;
    let out = read_on(&mut client, &sql, &values, limit).await;
    let rolled_back = rollback(&mut client).await;
    let out = out?;
    rolled_back?;
    Ok(out)
}

async fn rollback(client: &mut Conn) -> Result<(), DbError> {
    client
        .simple_query("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION")
        .await
        .map_err(|e| DbError(format!("rollback failed: {e}")))?
        .into_results()
        .await
        .map_err(|e| DbError(format!("rollback failed: {e}")))?;
    Ok(())
}

async fn read_on(
    client: &mut Conn,
    sql: &str,
    values: &[Box<dyn ToSql>],
    limit: u64,
) -> Result<ResultSet, DbError> {
    let refs: Vec<&dyn ToSql> = values.iter().map(|v| v.as_ref()).collect();
    let mut stream = client.query(sql, &refs).await.map_err(DbError::query)?;
    let columns: Vec<String> = stream
        .columns()
        .await
        .map_err(DbError::query)?
        .map(|cs| cs.iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default();
    let mut rows = stream.into_row_stream();
    let mut out = Vec::new();
    let mut truncated = false;
    while let Some(row) = rows.try_next().await.map_err(DbError::query)? {
        if limit > 0 && out.len() as u64 == limit {
            truncated = true;
            break;
        }
        out.push(
            row.cells()
                .map(|(_, data)| cell(data))
                .collect::<Result<Vec<_>, _>>()
                .map_err(DbError::query)?,
        );
    }
    Ok(ResultSet {
        columns,
        rows: out,
        truncated,
    })
}

fn bind(p: &Param) -> Box<dyn ToSql> {
    match p {
        Param::Null => Box::new(None::<String>),
        Param::Bool(b) => Box::new(*b),
        Param::Int(i) => Box::new(*i),
        Param::Float(f) => Box::new(*f),
        Param::Text(s) => Box::new(s.clone()),
    }
}

fn cell(data: &ColumnData<'static>) -> tiberius::Result<Cell> {
    let text = |v: Option<String>| v.map_or(Cell::Null, Cell::Text);
    Ok(match data {
        ColumnData::U8(v) => v.map_or(Cell::Null, |v| Cell::Int(v.into())),
        ColumnData::I16(v) => v.map_or(Cell::Null, |v| Cell::Int(v.into())),
        ColumnData::I32(v) => v.map_or(Cell::Null, |v| Cell::Int(v.into())),
        ColumnData::I64(v) => v.map_or(Cell::Null, Cell::Int),
        ColumnData::F32(v) => v.map_or(Cell::Null, |v| Cell::Float(v.into())),
        ColumnData::F64(v) => v.map_or(Cell::Null, Cell::Float),
        ColumnData::Bit(v) => v.map_or(Cell::Null, Cell::Bool),
        ColumnData::String(v) => text(v.as_ref().map(|s| s.to_string())),
        ColumnData::Guid(v) => text(v.map(|g| g.to_string())),
        ColumnData::Binary(v) => v.as_ref().map_or(Cell::Null, |b| Cell::Bytes(b.to_vec())),
        ColumnData::Numeric(v) => v.map_or(Cell::Null, |n| {
            let s = n.to_string();
            s.parse::<i64>().map_or(Cell::Text(s), Cell::Int)
        }),
        ColumnData::Xml(v) => text(v.as_ref().map(|x| x.to_string())),
        ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => text(
            NaiveDateTime::from_sql(data)?.map(|d| d.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
        ),
        ColumnData::Date(_) => text(NaiveDate::from_sql(data)?.map(|d| d.to_string())),
        ColumnData::Time(_) => text(NaiveTime::from_sql(data)?.map(|t| t.to_string())),
        ColumnData::DateTimeOffset(_) => {
            text(DateTime::<FixedOffset>::from_sql(data)?.map(|d| d.to_rfc3339()))
        }
    })
}

/// Rewrite `:name` placeholders to the `@P1`, `@P2`… tiberius binds, leaving
/// literals and comments alone. Returns the new SQL and the name bound to
/// each position; a name used twice keeps one position.
pub fn rewrite_placeholders(sql: &str) -> Result<(String, Vec<String>), DbError> {
    let mut out = String::with_capacity(sql.len());
    let mut names: Vec<String> = Vec::new();
    let mut last = 0;
    for (range, name) in placeholders(sql, Dialect::MsSql)? {
        let n = match names.iter().position(|x| *x == name) {
            Some(i) => i + 1,
            None => {
                names.push(name);
                names.len()
            }
        };
        out.push_str(&sql[last..range.start]);
        out.push_str(&format!("@P{n}"));
        last = range.end;
    }
    out.push_str(&sql[last..]);
    Ok((out, names))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_become_positions() {
        let (sql, names) = rewrite_placeholders(
            "SELECT ':strm' AS lit, a FROM t -- :c\nWHERE strm = :strm AND id = :id OR strm2 = :strm",
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT ':strm' AS lit, a FROM t -- :c\nWHERE strm = @P1 AND id = @P2 OR strm2 = @P1"
        );
        assert_eq!(names, ["strm", "id"]);
    }

    #[test]
    fn no_placeholders_no_change() {
        let sql = "SELECT [a:b], x FROM t WHERE y = N'a :b'";
        assert_eq!(
            rewrite_placeholders(sql).unwrap(),
            (sql.to_string(), vec![])
        );
    }
}
