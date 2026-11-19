//! The databases sql-review reads from, and how each runs a read.
//!
//! Every backend runs a read inside whatever read-only mode it has, and none
//! commits. That is defence in depth behind the read-only account, which is
//! the boundary; see the README for what each backend can and can't enforce.

pub mod mssql;
pub mod oracle;
pub mod sqlite;

use std::time::{Duration, Instant};

use crate::{
    classify::Dialect,
    config::{Account, Config},
    value::{Cell, Params, ResultSet},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Oracle,
    MsSql,
    Sqlite,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Oracle, Kind::MsSql, Kind::Sqlite];
    /// The `DB_BACKEND` names, sorted.
    pub const NAMES: [&str; 3] = ["mssql", "oracle", "sqlite"];

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "oracle" => Some(Kind::Oracle),
            "mssql" => Some(Kind::MsSql),
            "sqlite" => Some(Kind::Sqlite),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Oracle => "oracle",
            Kind::MsSql => "mssql",
            Kind::Sqlite => "sqlite",
        }
    }

    pub fn default_port(self) -> Option<u16> {
        match self {
            Kind::Oracle => Some(1521),
            Kind::MsSql => Some(1433),
            Kind::Sqlite => None,
        }
    }

    /// The scheme an assembled `DATABASE_URL` uses, as consulta wrote it.
    pub fn url_scheme(self) -> &'static str {
        match self {
            Kind::Oracle => "oracle+oracledb",
            Kind::MsSql => "mssql",
            Kind::Sqlite => "sqlite",
        }
    }

    pub fn dialect(self) -> Dialect {
        match self {
            Kind::Oracle => Dialect::Oracle,
            Kind::MsSql => Dialect::MsSql,
            Kind::Sqlite => Dialect::Sqlite,
        }
    }

    /// A data-dictionary query listing a table's columns, binding `:t`.
    /// `DESCRIBE` itself is a client command on most backends, not SQL.
    pub fn describe_sql(self) -> &'static str {
        match self {
            Kind::Oracle => {
                "SELECT column_name, data_type, data_length, data_precision, \
                 data_scale, nullable, column_id \
                 FROM all_tab_columns WHERE table_name = UPPER(:t) ORDER BY column_id"
            }
            Kind::MsSql => {
                "SELECT column_name, data_type, character_maximum_length, \
                 numeric_precision, numeric_scale, is_nullable, ordinal_position \
                 FROM information_schema.columns WHERE table_name = :t \
                 ORDER BY ordinal_position"
            }
            Kind::Sqlite => {
                r#"SELECT name, type, "notnull", dflt_value, pk FROM pragma_table_info(:t)"#
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DbError(pub String);

impl DbError {
    pub fn connect(e: impl std::fmt::Display) -> Self {
        DbError(format!("connection failed: {e}"))
    }

    pub fn query(e: impl std::fmt::Display) -> Self {
        DbError(format!("query failed: {e}"))
    }
}

/// Connect and run the backend's trivial query.
pub async fn check(cfg: &Config) -> Result<(), DbError> {
    match cfg.backend {
        Kind::Sqlite => sqlite::check(cfg).await,
        Kind::MsSql => mssql::check(cfg).await,
        Kind::Oracle => oracle::check(cfg).await,
    }
}

/// Run one read on the read-only account. `limit` 0 means no cap.
///
/// This does not classify: callers go through [`crate::run`], which refuses
/// writes first. It is public so the tests can show a write sent straight
/// here still fails on the read-only connection.
pub async fn read(
    cfg: &Config,
    sql: &str,
    params: &Params,
    limit: u64,
) -> Result<ResultSet, DbError> {
    match cfg.backend {
        Kind::Sqlite => sqlite::read(cfg, sql, params, limit).await,
        Kind::MsSql => mssql::read(cfg, sql, params, limit).await,
        Kind::Oracle => oracle::read(cfg, sql, params, limit).await,
    }
}

/// A write, as propose previews it and apply runs it.
#[derive(Debug, Clone)]
pub struct WritePlan {
    pub statement: String,
    pub params: Params,
    /// For a single-table UPDATE or DELETE: the rows it changes.
    pub rows: Option<RowQueries>,
}

/// Queries over the rows a single-table UPDATE or DELETE changes, derived
/// from its WHERE clause.
#[derive(Debug, Clone)]
pub struct RowQueries {
    /// `SELECT *` over the affected rows.
    pub select: String,
    /// The same rows, locked until the transaction ends.
    pub lock: String,
    /// For an UPDATE on a backend with row locators: how to find the sampled
    /// rows again after the statement ran.
    pub located: Option<Located>,
}

#[derive(Debug, Clone)]
pub struct Located {
    /// [`RowQueries::select`] with the row's locator as an extra first column.
    pub select: String,
    /// Completed with the locators, comma-separated, and `)`: the rows by
    /// locator, locator first.
    pub after_prefix: String,
}

/// What a preview saw, before it rolled back.
#[derive(Debug, Clone, PartialEq)]
pub struct Previewed {
    /// Every affected row, before the statement ran.
    pub before: Option<ResultSet>,
    /// The row count the statement reported.
    pub count: u64,
    /// The sampled rows after the statement ran, in the order of `before`.
    pub after: Option<ResultSet>,
}

/// What an apply checks before it commits.
#[derive(Debug, Clone, Default)]
pub struct Expected {
    pub count: Option<u64>,
    pub hash: Option<String>,
}

/// Why an apply did not commit.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ApplyFail {
    /// What the proposal was taken against has moved: rolled back.
    #[error("{0}")]
    Stale(String),
    #[error("{0}")]
    Db(#[from] DbError),
}

impl Expected {
    pub fn check_rows(&self, rows: &ResultSet) -> Result<(), ApplyFail> {
        match &self.hash {
            Some(h) if *h != rows.content_hash() => Err(ApplyFail::Stale(format!(
                "the rows it changes moved since it was proposed ({} row(s) now; rows hash was {}, now {})",
                rows.rows.len(),
                short(h),
                short(&rows.content_hash())
            ))),
            _ => Ok(()),
        }
    }

    pub fn check_count(&self, count: u64) -> Result<(), ApplyFail> {
        match self.count {
            Some(want) if want != count => Err(ApplyFail::Stale(format!(
                "it changed {count} row(s), not the {want} proposed; rolled back"
            ))),
            _ => Ok(()),
        }
    }
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

/// Preview a write on the write account: run it in a transaction, then roll
/// back. `None` when it ran over `limit`.
pub async fn preview(
    cfg: &Config,
    account: &Account,
    plan: &WritePlan,
    sample: usize,
    limit: Duration,
) -> Result<Option<Previewed>, DbError> {
    match cfg.backend {
        Kind::Sqlite => sqlite::preview(cfg, plan, sample, limit).await,
        Kind::MsSql => mssql::preview(cfg, account, plan, sample, limit).await,
        Kind::Oracle => oracle::preview(cfg, account, plan, sample, limit).await,
    }
}

/// Apply a write on the write account in one transaction: lock and check
/// the rows, run it, check the count, and commit only if both held.
pub async fn apply(
    cfg: &Config,
    account: &Account,
    plan: &WritePlan,
    expect: &Expected,
) -> Result<u64, ApplyFail> {
    match cfg.backend {
        Kind::Sqlite => sqlite::apply(cfg, plan, expect).await,
        Kind::MsSql => mssql::apply(cfg, account, plan, expect).await,
        Kind::Oracle => oracle::apply(cfg, account, plan, expect).await,
    }
}

/// A connection the blocking backends (SQLite, Oracle) preview and apply on.
pub(crate) trait SyncConn {
    fn read(&self, sql: &str, params: &Params) -> Result<ResultSet, DbError>;
    fn execute(&self, sql: &str, params: &Params) -> Result<u64, DbError>;
}

/// The preview's steps, inside a transaction the caller rolls back.
pub(crate) fn preview_on(
    conn: &dyn SyncConn,
    plan: &WritePlan,
    sample: usize,
    deadline: Instant,
) -> Result<Previewed, DbError> {
    let over = || {
        if Instant::now() > deadline {
            Err(DbError("the preview ran over its time limit".into()))
        } else {
            Ok(())
        }
    };
    let mut before = None;
    let mut found_again = None;
    if let Some(rows) = &plan.rows {
        if let Some(located) = &rows.located
            && let Ok(rs) = conn.read(&located.select, &plan.params)
        {
            let (rs, locators) = split_locator(rs, sample);
            found_again = Some((located.after_prefix.clone(), locators));
            before = Some(rs);
        }
        if before.is_none() {
            before = Some(conn.read(&rows.select, &plan.params)?);
        }
        over()?;
    }
    let count = conn.execute(&plan.statement, &plan.params)?;
    over()?;
    let after = match found_again {
        Some((prefix, locators)) if !locators.is_empty() => {
            let rs = conn.read(&format!("{prefix}{})", locators.join(", ")), &Params::new())?;
            Some(align(rs, &locators))
        }
        _ => None,
    };
    Ok(Previewed {
        before,
        count,
        after,
    })
}

/// The apply's steps, inside a transaction the caller commits on `Ok`.
pub(crate) fn apply_on(
    conn: &dyn SyncConn,
    plan: &WritePlan,
    expect: &Expected,
) -> Result<u64, ApplyFail> {
    if let Some(rows) = &plan.rows {
        expect.check_rows(&conn.read(&rows.lock, &plan.params)?)?;
    }
    let count = conn.execute(&plan.statement, &plan.params)?;
    expect.check_count(count)?;
    Ok(count)
}

/// A row locator as a SQL literal: an integer rowid as it is, an Oracle
/// ROWID quoted (its characters are base64, so quoting is all it needs).
pub(crate) fn locator_literal(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Int(i) => Some(i.to_string()),
        Cell::Text(s)
            if !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/') =>
        {
            Some(format!("'{s}'"))
        }
        _ => None,
    }
}

/// Drop the locator column, keeping the locators of the first `sample` rows.
pub(crate) fn split_locator(mut rs: ResultSet, sample: usize) -> (ResultSet, Vec<String>) {
    if rs.columns.is_empty() {
        return (rs, Vec::new());
    }
    rs.columns.remove(0);
    let mut locators = Vec::new();
    for (i, row) in rs.rows.iter_mut().enumerate() {
        let loc = row.remove(0);
        if i < sample
            && let Some(l) = locator_literal(&loc)
        {
            locators.push(l);
        }
    }
    (rs, locators)
}

/// The rows found again by locator, locator column dropped, in the order of
/// `locators`.
pub(crate) fn align(rs: ResultSet, locators: &[String]) -> ResultSet {
    let ResultSet {
        mut columns, rows, ..
    } = rs;
    if !columns.is_empty() {
        columns.remove(0);
    }
    let mut keyed: Vec<(Option<String>, Vec<Cell>)> = rows
        .into_iter()
        .map(|mut r| {
            let loc = if r.is_empty() {
                None
            } else {
                locator_literal(&r.remove(0))
            };
            (loc, r)
        })
        .collect();
    let mut out = Vec::new();
    for l in locators {
        if let Some(i) = keyed.iter().position(|(k, _)| k.as_deref() == Some(l)) {
            out.push(keyed.swap_remove(i).1);
        }
    }
    ResultSet {
        columns,
        rows: out,
        truncated: false,
    }
}

/// Fetch at most `limit` rows (0: all), then probe for one more so the
/// result can say whether it was cut short.
pub(crate) fn take_limited<E>(
    rows: impl Iterator<Item = Result<Vec<crate::value::Cell>, E>>,
    limit: u64,
) -> Result<(Vec<Vec<crate::value::Cell>>, bool), E> {
    let mut out = Vec::new();
    let mut rows = rows;
    for row in rows.by_ref() {
        if limit > 0 && out.len() as u64 == limit {
            row?;
            return Ok((out, true));
        }
        out.push(row?);
    }
    Ok((out, false))
}

/// The `:name` placeholders in a statement, outside literals and comments:
/// each one's byte range (colon included) and name.
pub(crate) fn placeholders(
    sql: &str,
    dialect: Dialect,
) -> Result<Vec<(std::ops::Range<usize>, String)>, DbError> {
    use sqlparser::tokenizer::{Token, Tokenizer};

    let d = dialect.get();
    let tokens = Tokenizer::new(d.as_ref(), sql)
        .tokenize_with_location()
        .map_err(|e| DbError(format!("could not read the statement: {e}")))?;
    // Byte offset of each (line, column), both counted from 1 in chars.
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(sql.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let offset = |line: u64, col: u64| -> usize {
        let start = line_starts[(line - 1) as usize];
        sql[start..]
            .char_indices()
            .nth((col - 1) as usize)
            .map(|(i, _)| start + i)
            .unwrap_or(sql.len())
    };
    let mut out = Vec::new();
    for pair in tokens.windows(2) {
        let (colon, next) = (&pair[0], &pair[1]);
        if colon.token != Token::Colon {
            continue;
        }
        let name = match &next.token {
            Token::Word(w) if w.quote_style.is_none() => w.value.clone(),
            Token::Number(n, _) => n.clone(),
            _ => continue,
        };
        let start = offset(colon.span.start.line, colon.span.start.column);
        let name_start = offset(next.span.start.line, next.span.start.column);
        // `: name` with a space is not a placeholder.
        if name_start != start + 1 {
            continue;
        }
        out.push((start..name_start + name.len(), name));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_skip_literals_and_comments() {
        let sql = "SELECT ':no' AS a, x::int -- :nope\nFROM t WHERE a = :one AND b = :two /* :x */ AND c = :one";
        let found = placeholders(sql, Dialect::MsSql).unwrap();
        let names: Vec<&str> = found.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, ["one", "two", "one"]);
        for (range, name) in &found {
            assert_eq!(&sql[range.clone()], format!(":{name}"));
        }
    }

    #[test]
    fn placeholders_count_chars_not_bytes() {
        let sql = "SELECT 'é' AS e FROM t WHERE a = :v";
        let found = placeholders(sql, Dialect::Oracle).unwrap();
        assert_eq!(&sql[found[0].0.clone()], ":v");
    }

    #[test]
    fn locators_are_split_and_rows_found_again_in_order() {
        let rs = ResultSet {
            columns: vec!["loc".into(), "a".into()],
            rows: vec![
                vec![Cell::Int(7), Cell::Text("x".into())],
                vec![Cell::Text("AAAx+/9".into()), Cell::Text("y".into())],
                vec![Cell::Int(9), Cell::Text("z".into())],
            ],
            truncated: false,
        };
        let (rs, locs) = split_locator(rs, 2);
        assert_eq!(rs.columns, ["a"]);
        assert_eq!(rs.rows[2], [Cell::Text("z".into())]);
        assert_eq!(locs, ["7", "'AAAx+/9'"]);
        let again = ResultSet {
            columns: vec!["loc".into(), "a".into()],
            rows: vec![
                vec![Cell::Text("AAAx+/9".into()), Cell::Text("Y".into())],
                vec![Cell::Int(7), Cell::Text("X".into())],
            ],
            truncated: false,
        };
        let a = align(again, &locs);
        assert_eq!(a.rows, [[Cell::Text("X".into())], [Cell::Text("Y".into())]]);
        assert_eq!(locator_literal(&Cell::Text("a'b".into())), None);
    }

    #[test]
    fn limit_probes_one_more_row() {
        let rows = |n: usize| (0..n).map(|i| Ok::<_, ()>(vec![crate::value::Cell::Int(i as i64)]));
        assert_eq!(take_limited(rows(3), 2).unwrap().0.len(), 2);
        assert!(take_limited(rows(3), 2).unwrap().1);
        assert!(!take_limited(rows(2), 2).unwrap().1);
        assert_eq!(
            take_limited(rows(3), 0).unwrap(),
            (take_limited(rows(3), 5).unwrap().0, false)
        );
    }
}
