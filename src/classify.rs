//! Is a statement a read or a write?
//!
//! **This is not a security control.** sql-review keeps a parser only to sort
//! statements: a read runs, a write is refused for now (and becomes a
//! proposal for review in a later version). Reads always run on the
//! read-only account, in the backend's read-only mode where it has one, so a
//! write this misclassifies as a read still runs read-only and fails. The
//! database account is the boundary, not this module.
//!
//! Ported from Tracon's node guard (`node/src/broker/guard.rs`), which refused
//! rather than classified. It still errs the same way: a statement it cannot
//! show to be a lone query is a write, and one it cannot parse at all is
//! neither, so the caller refuses it.

use sqlparser::{
    ast::{Query, SetExpr, Statement},
    dialect::{Dialect as SqlDialect, GenericDialect, MsSqlDialect, OracleDialect, SQLiteDialect},
    keywords::Keyword,
    parser::Parser,
    tokenizer::{Token, Tokenizer},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    Read,
    /// A write, with the reason it counts as one.
    Write(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unclassifiable {
    #[error("no SQL statement found")]
    Empty,
    #[error("could not parse the statement: {0}")]
    Unparseable(String),
}

/// Which SQL grammar to read a statement with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Generic,
    Oracle,
    MsSql,
    Sqlite,
}

impl Dialect {
    pub(crate) fn get(self) -> Box<dyn SqlDialect> {
        match self {
            Dialect::Generic => Box::new(GenericDialect {}),
            Dialect::Oracle => Box::new(OracleDialect {}),
            Dialect::MsSql => Box::new(MsSqlDialect {}),
            Dialect::Sqlite => Box::new(SQLiteDialect {}),
        }
    }
}

/// Keywords that make a statement a write wherever they appear outside a
/// quoted identifier or a literal, even inside a statement that parses as a
/// query. `FOR UPDATE` takes locks; the DDL and DML words would only appear in
/// a query through a parse the dialect read differently from the backend.
/// `BEGIN` and `DECLARE` open PL/SQL and T-SQL blocks.
const FORBIDDEN: &[Keyword] = &[
    Keyword::INSERT,
    Keyword::UPDATE,
    Keyword::DELETE,
    Keyword::MERGE,
    Keyword::TRUNCATE,
    Keyword::DROP,
    Keyword::ALTER,
    Keyword::CREATE,
    Keyword::REPLACE,
    Keyword::GRANT,
    Keyword::REVOKE,
    Keyword::COMMIT,
    Keyword::ROLLBACK,
    Keyword::SAVEPOINT,
    Keyword::LOCK,
    Keyword::CALL,
    Keyword::EXEC,
    Keyword::EXECUTE,
    Keyword::PRAGMA,
    Keyword::ATTACH,
    Keyword::DETACH,
    Keyword::VACUUM,
    Keyword::ANALYZE,
    Keyword::COPY,
    Keyword::SET,
    Keyword::BEGIN,
    Keyword::DECLARE,
];

pub fn classify(sql: &str, dialect: Dialect) -> Result<Classification, Unclassifiable> {
    let dialect = dialect.get();

    // Token pass first: it sees keywords the parser may fold into an
    // expression, and it sees them outside string literals.
    let tokens = Tokenizer::new(dialect.as_ref(), sql)
        .tokenize()
        .map_err(|e| Unclassifiable::Unparseable(e.to_string()))?;
    let meaningful: Vec<&Token> = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect();
    if meaningful.iter().all(|t| matches!(t, Token::SemiColon)) {
        return Err(Unclassifiable::Empty);
    }
    for (i, token) in meaningful.iter().enumerate() {
        if let Token::Word(w) = token {
            // A quoted word is an identifier, not a keyword: a column named
            // "delete" is data.
            if w.quote_style.is_some() || !FORBIDDEN.contains(&w.keyword) {
                continue;
            }
            // REPLACE( is the string function, common on Oracle, not
            // MySQL's REPLACE INTO. A change from the node guard, which
            // refused it.
            if w.keyword == Keyword::REPLACE && meaningful.get(i + 1) == Some(&&Token::LParen) {
                continue;
            }
            return Ok(Classification::Write(w.value.to_ascii_uppercase()));
        }
    }

    let statements = Parser::parse_sql(dialect.as_ref(), sql)
        .map_err(|e| Unclassifiable::Unparseable(e.to_string()))?;
    match statements.as_slice() {
        [] => Err(Unclassifiable::Empty),
        [Statement::Query(q)] => Ok(match query_writes(q) {
            // A query that parses as a SELECT can still write or lock:
            // `SELECT … INTO t` creates and fills a table on SQL Server, and
            // a locking clause takes row locks.
            Some(what) => Classification::Write(what.into()),
            None => Classification::Read,
        }),
        [other] => Ok(Classification::Write(statement_name(other))),
        many => Ok(Classification::Write(format!(
            "{} statements; only one may run",
            many.len()
        ))),
    }
}

/// The reason a query is not a read, or `None` if it is. Walks CTEs, set
/// operations and subqueries so a write nested inside is caught too.
fn query_writes(q: &Query) -> Option<&'static str> {
    if !q.locks.is_empty() {
        return Some("a locking clause (FOR UPDATE / FOR SHARE)");
    }
    if let Some(with) = &q.with {
        for cte in &with.cte_tables {
            if let Some(w) = query_writes(&cte.query) {
                return Some(w);
            }
        }
    }
    body_writes(&q.body)
}

fn body_writes(body: &SetExpr) -> Option<&'static str> {
    match body {
        SetExpr::Select(s) if s.into.is_some() => Some("SELECT … INTO"),
        SetExpr::Query(q) => query_writes(q),
        SetExpr::SetOperation { left, right, .. } => {
            body_writes(left).or_else(|| body_writes(right))
        }
        SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_) => {
            Some("a data-modifying statement")
        }
        _ => None,
    }
}

fn statement_name(s: &Statement) -> String {
    let text = format!("{s}");
    text.split_whitespace()
        .next()
        .unwrap_or("statement")
        .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIALECTS: [Dialect; 4] = [
        Dialect::Generic,
        Dialect::Oracle,
        Dialect::MsSql,
        Dialect::Sqlite,
    ];

    fn is_read(sql: &str) -> bool {
        classify(sql, Dialect::Generic) == Ok(Classification::Read)
    }

    fn is_write(sql: &str) -> bool {
        matches!(
            classify(sql, Dialect::Generic),
            Ok(Classification::Write(_))
        )
    }

    // The node guard's tests, re-expressed as read or write.

    #[test]
    fn plain_queries_are_reads() {
        for sql in [
            "SELECT 1",
            "select id, name from people where id = :id",
            "WITH recent AS (SELECT * FROM people) SELECT count(*) FROM recent",
            "SELECT a.id FROM people a JOIN roles b ON a.id = b.person_id ORDER BY a.id",
        ] {
            assert!(is_read(sql), "should be a read: {sql}");
        }
    }

    #[test]
    fn writes_are_writes() {
        for sql in [
            "DELETE FROM people",
            "INSERT INTO people VALUES (1, 'x')",
            "UPDATE people SET name = 'x'",
            "DROP TABLE people",
            "TRUNCATE TABLE people",
            "CREATE TABLE t (id int)",
            "GRANT SELECT ON people TO other",
        ] {
            assert!(is_write(sql), "should be a write: {sql}");
        }
    }

    #[test]
    fn stacked_statements_are_writes() {
        assert!(is_write("SELECT 1; DELETE FROM people"));
        // Two reads still are not one statement.
        assert_eq!(
            classify("SELECT 1; SELECT 2", Dialect::Generic),
            Ok(Classification::Write(
                "2 statements; only one may run".into()
            ))
        );
    }

    #[test]
    fn locking_reads_are_writes() {
        for sql in [
            "SELECT * FROM people FOR UPDATE",
            "SELECT * FROM people FOR SHARE",
            "SELECT * FROM people FOR NO KEY UPDATE",
        ] {
            assert!(is_write(sql), "should be a write: {sql}");
        }
    }

    #[test]
    fn select_into_is_a_write() {
        for sql in [
            "SELECT * INTO backup FROM people",
            "SELECT id INTO #tmp FROM people",
            "WITH c AS (SELECT 1 AS x) SELECT x INTO keep FROM c",
        ] {
            assert_eq!(
                classify(sql, Dialect::Generic),
                Ok(Classification::Write("SELECT … INTO".into())),
                "{sql}"
            );
        }
    }

    #[test]
    fn for_update_inside_a_string_literal_is_a_read() {
        assert!(is_read(
            "SELECT * FROM notes WHERE body = 'please FOR UPDATE the row'"
        ));
    }

    #[test]
    fn a_write_hidden_in_a_cte_is_a_write() {
        assert!(is_write(
            "WITH x AS (DELETE FROM people RETURNING id) SELECT * FROM x"
        ));
    }

    #[test]
    fn nonsense_is_unclassifiable() {
        assert!(matches!(
            classify("not sql at all ((", Dialect::Generic),
            Err(Unclassifiable::Unparseable(_))
        ));
        assert_eq!(classify("", Dialect::Generic), Err(Unclassifiable::Empty));
        assert_eq!(
            classify("  ;  ", Dialect::Generic),
            Err(Unclassifiable::Empty)
        );
    }

    #[test]
    fn a_keyword_inside_a_string_literal_is_a_read() {
        assert!(is_read("SELECT * FROM people WHERE name = 'delete me'"));
    }

    #[test]
    fn a_quoted_keyword_is_an_identifier() {
        assert!(is_read(r#"SELECT "delete" FROM people"#));
    }

    // The deliberate change from the node guard.

    #[test]
    fn replace_the_function_is_a_read() {
        for d in DIALECTS {
            assert_eq!(
                classify("SELECT REPLACE(name, 'a', 'b') FROM people", d),
                Ok(Classification::Read),
                "{d:?}"
            );
            assert_eq!(
                classify("SELECT replace (name, 'a', 'b') FROM people", d),
                Ok(Classification::Read),
                "{d:?}"
            );
        }
        assert!(is_write("REPLACE INTO people VALUES (1)"));
    }

    // consulta's guard vectors, under every dialect.

    const ALLOWED: [&str; 7] = [
        "SELECT 1 FROM dual",
        "select * from apps",
        "  -- a comment\nSELECT app_id FROM apps",
        "WITH c AS (SELECT 1 AS a) SELECT * FROM c",
        "SELECT updated_by, created_dt FROM apps",
        "SELECT 'DROP TABLE x' AS note FROM apps",
        "SELECT count(*) FROM apps OFFSET 5 ROWS",
    ];

    #[test]
    fn consulta_allowed_vectors_are_reads() {
        for d in DIALECTS {
            for sql in ALLOWED {
                assert_eq!(classify(sql, d), Ok(Classification::Read), "{d:?}: {sql}");
            }
        }
    }

    /// consulta's rejected vectors and the reason each is a write. One is
    /// not a write but cannot be classified at all; it is refused all the
    /// same.
    const REJECTED: [(&str, &str); 16] = [
        ("DELETE FROM apps", "DELETE"),
        ("UPDATE apps SET score = 0", "UPDATE"),
        ("INSERT INTO apps VALUES (1)", "INSERT"),
        ("DROP TABLE apps", "DROP"),
        ("TRUNCATE TABLE apps", "TRUNCATE"),
        ("CREATE TABLE x (a INTEGER)", "CREATE"),
        ("ALTER TABLE apps ADD c INTEGER", "ALTER"),
        ("GRANT SELECT ON apps TO bob", "GRANT"),
        (
            "MERGE INTO apps USING dual ON (1=1) WHEN MATCHED THEN UPDATE SET score = 1",
            "MERGE",
        ),
        ("SELECT * FROM apps FOR UPDATE", "UPDATE"),
        ("SELECT 1; DROP TABLE apps", "DROP"),
        ("WITH c AS (SELECT 1) DELETE FROM apps", "DELETE"),
        ("BEGIN dbms_output.put_line('x'); END;", "BEGIN"),
        ("EXEC some_proc", "EXEC"),
        ("COMMIT", "COMMIT"),
        ("SELECT * FROM apps INTO new_apps", ""),
    ];

    #[test]
    fn consulta_rejected_vectors_are_writes_or_refused() {
        for d in DIALECTS {
            for (sql, reason) in REJECTED {
                let got = classify(sql, d);
                if reason.is_empty() {
                    // `INTO` after `FROM` is not a grammar any of these
                    // dialects knows. Unparseable is refused, so the vector
                    // still holds: it never runs.
                    assert!(
                        matches!(got, Err(Unclassifiable::Unparseable(_))),
                        "{d:?}: {sql}: {got:?}"
                    );
                } else {
                    assert_eq!(
                        got,
                        Ok(Classification::Write(reason.into())),
                        "{d:?}: {sql}"
                    );
                }
            }
            assert_eq!(classify("", d), Err(Unclassifiable::Empty), "{d:?}");
        }
    }

    #[test]
    fn describe_sql_is_a_read() {
        for backend in crate::backend::Kind::ALL {
            assert_eq!(
                classify(backend.describe_sql(), backend.dialect()),
                Ok(Classification::Read),
                "{backend:?}"
            );
        }
    }
}
