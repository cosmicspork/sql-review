//! The databases sql-review reads from, and how each runs a read.
//!
//! Every backend runs a read inside whatever read-only mode it has, and none
//! commits. That is defence in depth behind the read-only account, which is
//! the boundary; see the README for what each backend can and can't enforce.

pub mod mssql;
pub mod oracle;
pub mod sqlite;

use crate::{
    classify::Dialect,
    config::Config,
    value::{Params, ResultSet},
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
