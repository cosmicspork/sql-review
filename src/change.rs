//! A write as a proposal: what it would change, and a version of the rows
//! it would change. `propose` runs the statement on the write account in a
//! transaction and rolls it back; `apply` refuses when those rows moved.
//!
//! What a preview can show depends on the statement:
//!
//! - a single-table `UPDATE` or `DELETE`: the rows its `WHERE` selects,
//!   before and (for an update) after, their count, and a hash of them;
//! - an `INSERT`: the row count (it has no prior rows);
//! - any other DML (joins, subqueries, `MERGE`): the row count only;
//! - DDL, procedure calls, blocks, and anything a rollback can't undo: never
//!   run at propose, shown as the statement alone, "not previewable".
//!
//! The proposal file is pretty-printed JSON with a fixed key order, so it
//! can be reviewed in git like any other diff.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlparser::{
    ast::{FromTable, Statement, TableFactor, TableWithJoins},
    keywords::Keyword,
    parser::Parser,
    tokenizer::{Token, Tokenizer},
};

use crate::{
    backend::{self, ApplyFail, Expected, Kind, Located, RowQueries, WritePlan},
    classify::{Classification, Dialect, classify},
    config::Config,
    render::{Format, render as render_rows},
    value::{Cell, Param, Params, ResultSet},
};

/// The proposal file format this build reads and writes.
pub const FORMAT: u32 = 1;

/// Rows shown before and after; the hash covers every affected row.
pub const SAMPLE_ROWS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewKind {
    /// A single-table UPDATE or DELETE: rows, count and hash.
    Rows,
    /// An INSERT: count only, no prior rows.
    Insert,
    /// Other DML: count only.
    CountOnly,
    /// Never run at propose.
    NotPreviewable,
}

/// Rows as the proposal keeps them: column names and JSON values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl Sample {
    fn of(rs: &ResultSet, limit: usize) -> Self {
        Self {
            columns: rs.columns.clone(),
            rows: rs
                .rows
                .iter()
                .take(limit)
                .map(|r| r.iter().map(Cell::to_json).collect())
                .collect(),
        }
    }

    fn result_set(&self) -> ResultSet {
        let cell = |v: &Value| match v {
            Value::Null => Cell::Null,
            Value::Bool(b) => Cell::Bool(*b),
            Value::Number(n) => n
                .as_i64()
                .map(Cell::Int)
                .unwrap_or_else(|| Cell::Float(n.as_f64().unwrap_or(f64::NAN))),
            Value::String(s) => Cell::Text(s.clone()),
            other => Cell::Text(other.to_string()),
        };
        ResultSet {
            columns: self.columns.clone(),
            rows: self
                .rows
                .iter()
                .map(|r| r.iter().map(cell).collect())
                .collect(),
            truncated: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    pub kind: PreviewKind,
    /// Why it is count-only or not previewable, or what to know about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The query that found the affected rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_query: Option<String>,
    /// The row count the statement reported, which apply must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
    /// The hash of every affected row, which apply must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_hash: Option<String>,
    /// How many rows the affected-row query found (all of them, not the sample).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_found: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Sample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Sample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub sql_review: u32,
    pub id: String,
    pub created: String,
    pub backend: String,
    /// The profile it was proposed under; apply resolves the same one.
    #[serde(default)]
    pub profile: Option<String>,
    /// The database, without any login (see [`Config::target`]).
    pub target: String,
    pub statement: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
    pub preview: Preview,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub status: Status,
    pub decided: String,
    /// The rows the applied statement changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Approved and committed.
    Approved,
    /// Rejected by a reviewer.
    Rejected,
    /// Approved, but the rows had moved: rolled back.
    Stale,
    /// Approved, and the database refused it.
    Failed,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Approved => "approved",
            Status::Rejected => "rejected",
            Status::Stale => "stale",
            Status::Failed => "failed",
        }
    }
}

impl Outcome {
    pub fn new(status: Status, row_count: Option<u64>, reason: Option<String>) -> Self {
        Self {
            status,
            decided: now(),
            row_count,
            reason,
        }
    }

    /// The outcome of an apply.
    pub fn of(applied: &Result<u64, ApplyError>) -> Self {
        match applied {
            Ok(n) => Self::new(Status::Approved, Some(*n), None),
            Err(e @ ApplyError::Stale(_)) => Self::new(Status::Stale, None, Some(e.to_string())),
            Err(e) => Self::new(Status::Failed, None, Some(e.to_string())),
        }
    }
}

/// Why an apply committed nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    /// The proposal can't be applied as it stands; nothing ran.
    #[error("{0}")]
    Invalid(String),
    /// The rows moved since it was proposed; rolled back.
    #[error("{0}; propose it again")]
    Stale(String),
    /// The database refused it; rolled back.
    #[error("{0}")]
    Db(String),
}

impl Proposal {
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("a proposal serializes");
        s.push('\n');
        s
    }

    pub fn from_json(s: &str) -> Result<Self, String> {
        let p: Proposal =
            serde_json::from_str(s).map_err(|e| format!("not a proposal file: {e}"))?;
        if p.sql_review != FORMAT {
            return Err(format!(
                "proposal format {} is not {FORMAT}, the one this build reads",
                p.sql_review
            ));
        }
        Ok(p)
    }

    pub fn params(&self) -> Result<Params, String> {
        self.params
            .iter()
            .map(|(k, v)| Ok((k.clone(), Param::from_json(v)?)))
            .collect()
    }

    /// One line for a list: the statement's first words.
    pub fn summary(&self) -> String {
        let flat: String = self
            .statement
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if flat.chars().count() > 60 {
            format!("{}…", flat.chars().take(59).collect::<String>())
        } else {
            flat
        }
    }

    fn kind(&self) -> Result<Kind, String> {
        Kind::from_name(&self.backend).ok_or_else(|| format!("unknown backend {:?}", self.backend))
    }
}

/// An RFC 3339 UTC timestamp, to the second.
pub fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// How a statement can be previewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    Rows {
        delete: bool,
        /// The table, as written.
        table: String,
        /// Its alias, if it has one.
        alias: Option<String>,
        filter: Option<String>,
    },
    Insert,
    CountOnly(String),
    NotPreviewable(String),
}

/// Read a write's shape. Refuses a read and more than one statement.
pub fn shape(sql: &str, dialect: Dialect) -> Result<Shape, String> {
    match classify(sql, dialect) {
        Ok(Classification::Read) => {
            return Err("this statement only reads; run it as a query, not a proposal".into());
        }
        Ok(Classification::Write(_)) => {}
        Err(e) => return Err(format!("refused: {e}")),
    }
    let statements = match Parser::parse_sql(dialect.get().as_ref(), sql) {
        Ok(s) => s,
        Err(_) => {
            return Ok(Shape::NotPreviewable(format!(
                "{} could not be read as plain DML, so it is never run at propose",
                first_word(sql, dialect)
            )));
        }
    };
    let statement = match statements.as_slice() {
        [one] => one,
        many => {
            return Err(format!("{} statements; propose one at a time", many.len()));
        }
    };
    Ok(match statement {
        Statement::Insert(_) => Shape::Insert,
        Statement::Update(u) => {
            if u.from.is_some() || !u.table.joins.is_empty() {
                return Ok(Shape::CountOnly("it joins other tables".into()));
            }
            if u.limit.is_some() || !u.order_by.is_empty() {
                return Ok(Shape::CountOnly("ORDER BY or LIMIT picks its rows".into()));
            }
            let set: Vec<String> = u.assignments.iter().map(|a| a.to_string()).collect();
            let filter = u.selection.as_ref().map(|e| e.to_string());
            if reads_other_tables(&set.join(", "), dialect)
                || filter
                    .as_deref()
                    .is_some_and(|f| reads_other_tables(f, dialect))
            {
                return Ok(Shape::CountOnly(
                    "it reads other tables in a subquery".into(),
                ));
            }
            match single_table(&u.table) {
                Some((table, alias)) => Shape::Rows {
                    delete: false,
                    table,
                    alias,
                    filter,
                },
                None => Shape::CountOnly("its target is not a plain table".into()),
            }
        }
        Statement::Delete(d) => {
            let from = match &d.from {
                FromTable::WithFromKeyword(t) | FromTable::WithoutKeyword(t) => t,
            };
            if !d.tables.is_empty() || d.using.is_some() || from.len() != 1 {
                return Ok(Shape::CountOnly("it names more than one table".into()));
            }
            if !from[0].joins.is_empty() {
                return Ok(Shape::CountOnly("it joins other tables".into()));
            }
            if d.limit.is_some() || !d.order_by.is_empty() {
                return Ok(Shape::CountOnly("ORDER BY or LIMIT picks its rows".into()));
            }
            let filter = d.selection.as_ref().map(|e| e.to_string());
            if filter
                .as_deref()
                .is_some_and(|f| reads_other_tables(f, dialect))
            {
                return Ok(Shape::CountOnly(
                    "it reads other tables in a subquery".into(),
                ));
            }
            match single_table(&from[0]) {
                Some((table, alias)) => Shape::Rows {
                    delete: true,
                    table,
                    alias,
                    filter,
                },
                None => Shape::CountOnly("its target is not a plain table".into()),
            }
        }
        Statement::Merge(_) => Shape::CountOnly("MERGE".into()),
        other => Shape::NotPreviewable(not_previewable_reason(&statement_word(other))),
    })
}

fn not_previewable_reason(word: &str) -> String {
    match word {
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "COMMENT" | "GRANT" | "REVOKE" => {
            format!(
                "{word} is DDL, which a rollback can't be trusted to undo (Oracle commits it \
                 implicitly), so it is never run at propose"
            )
        }
        "CALL" | "EXEC" | "EXECUTE" => format!(
            "{word} runs a procedure, which can commit on its own, so it is never run at propose"
        ),
        _ => {
            format!("{word} is not DML a rollback is known to undo, so it is never run at propose")
        }
    }
}

fn statement_word(s: &Statement) -> String {
    s.to_string()
        .split_whitespace()
        .next()
        .unwrap_or("statement")
        .to_ascii_uppercase()
}

fn first_word(sql: &str, dialect: Dialect) -> String {
    Tokenizer::new(dialect.get().as_ref(), sql)
        .tokenize()
        .ok()
        .and_then(|ts| {
            ts.into_iter().find_map(|t| match t {
                Token::Word(w) => Some(w.value.to_ascii_uppercase()),
                _ => None,
            })
        })
        .unwrap_or_else(|| "the statement".into())
}

/// A subquery anywhere in this text.
fn reads_other_tables(text: &str, dialect: Dialect) -> bool {
    Tokenizer::new(dialect.get().as_ref(), text)
        .tokenize()
        .map(|ts| {
            ts.iter()
                .any(|t| matches!(t, Token::Word(w) if w.keyword == Keyword::SELECT && w.quote_style.is_none()))
        })
        .unwrap_or(true)
}

fn single_table(t: &TableWithJoins) -> Option<(String, Option<String>)> {
    match &t.relation {
        TableFactor::Table {
            name, alias, args, ..
        } if args.is_none() => Some((name.to_string(), alias.as_ref().map(|a| a.name.to_string()))),
        _ => None,
    }
}

/// The queries over a single-table write's rows, for this backend.
pub fn row_queries(
    kind: Kind,
    delete: bool,
    table: &str,
    alias: Option<&str>,
    filter: Option<&str>,
) -> RowQueries {
    let relation = match alias {
        Some(a) => format!("{table} {a}"),
        None => table.to_string(),
    };
    let filter = filter.map(|f| format!(" WHERE {f}")).unwrap_or_default();
    let select = format!("SELECT * FROM {relation}{filter}");
    let qualifier = alias.unwrap_or(table);
    match kind {
        Kind::Sqlite => RowQueries {
            lock: select.clone(),
            located: (!delete).then(|| Located {
                select: format!("SELECT rowid AS sql_review_rowid, * FROM {relation}{filter}"),
                after_prefix: format!(
                    "SELECT rowid AS sql_review_rowid, * FROM {table} WHERE rowid IN ("
                ),
            }),
            select,
        },
        Kind::Oracle => RowQueries {
            lock: format!("SELECT {qualifier}.* FROM {relation}{filter} FOR UPDATE"),
            located: (!delete).then(|| Located {
                select: format!(
                    "SELECT {qualifier}.ROWID AS sql_review_rowid, {qualifier}.* FROM {relation}{filter}"
                ),
                after_prefix: format!(
                    "SELECT sql_review_t.ROWID AS sql_review_rowid, sql_review_t.* \
                     FROM {table} sql_review_t WHERE sql_review_t.ROWID IN ("
                ),
            }),
            select,
        },
        Kind::MsSql => RowQueries {
            lock: format!("SELECT * FROM {relation} WITH (UPDLOCK, HOLDLOCK){filter}"),
            located: None,
            select,
        },
    }
}

fn plan(kind: Kind, statement: &str, params: Params, shape: &Shape) -> WritePlan {
    let rows = match shape {
        Shape::Rows {
            delete,
            table,
            alias,
            filter,
        } => Some(row_queries(
            kind,
            *delete,
            table,
            alias.as_deref(),
            filter.as_deref(),
        )),
        _ => None,
    };
    WritePlan {
        statement: statement.to_string(),
        params,
        rows,
    }
}

/// The statement as it will run: trailing semicolons and space dropped,
/// which Oracle would otherwise refuse.
pub fn normalize(sql: &str) -> String {
    sql.trim()
        .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
        .to_string()
}

const PREVIEW_CAVEAT: &str = "the preview ran the statement on the write account and rolled it \
                              back: triggers fired, and sequences it advanced stay advanced";

/// The write account, or why there isn't one.
fn write_account(cfg: &Config) -> Result<crate::config::Account, String> {
    cfg.write.clone().ok_or_else(|| {
        if cfg.read_only {
            "writes are disabled (--read-only)".to_string()
        } else {
            let place = cfg
                .profile
                .as_deref()
                .filter(|p| *p != crate::config::DEFAULT_PROFILE)
                .map(|p| format!(" (or {}_DB_WRITE_USER)", p.to_ascii_uppercase()))
                .unwrap_or_default();
            format!("no write account: set DB_WRITE_USER and DB_WRITE_PASSWORD{place}")
        }
    })
}

/// Propose a write: preview it on the write account and roll back.
pub async fn propose(cfg: &Config, sql: &str, params: &Params) -> Result<Proposal, String> {
    let account = write_account(cfg)?;
    let statement = normalize(sql);
    let shape = shape(&statement, cfg.backend.dialect())?;
    let mut preview = Preview {
        kind: PreviewKind::NotPreviewable,
        note: None,
        affected_query: None,
        row_count: None,
        rows_hash: None,
        rows_found: None,
        before: None,
        after: None,
    };
    match &shape {
        Shape::NotPreviewable(why) => preview.note = Some(why.clone()),
        _ => {
            let plan = plan(cfg.backend, &statement, params.clone(), &shape);
            let seen = backend::preview(cfg, &account, &plan, SAMPLE_ROWS, cfg.preview_timeout)
                .await
                .map_err(|e| format!("the preview failed, so nothing was proposed: {e}"))?;
            match seen {
                None => {
                    preview.note = Some(format!(
                        "the preview ran over its {} s limit (DB_PREVIEW_TIMEOUT) and was rolled \
                         back, so it is shown as the statement alone",
                        cfg.preview_timeout.as_secs_f64()
                    ));
                }
                Some(seen) => {
                    preview.row_count = Some(seen.count);
                    let mut notes = Vec::new();
                    match &shape {
                        Shape::Rows { delete, .. } => {
                            let before = seen.before.unwrap_or_default();
                            preview.kind = PreviewKind::Rows;
                            preview.affected_query = plan.rows.as_ref().map(|r| r.select.clone());
                            preview.rows_hash = Some(before.content_hash());
                            preview.rows_found = Some(before.rows.len() as u64);
                            preview.before = Some(Sample::of(&before, SAMPLE_ROWS));
                            preview.after = seen.after.map(|a| Sample::of(&a, SAMPLE_ROWS));
                            if before.rows.len() as u64 != seen.count {
                                notes.push(format!(
                                    "the statement reported {} row(s); its WHERE selects {}",
                                    seen.count,
                                    before.rows.len()
                                ));
                            }
                            if !delete && preview.after.is_none() && !before.rows.is_empty() {
                                notes.push(
                                    "no after sample: this backend has no row locator to find \
                                     the rows again"
                                        .into(),
                                );
                            }
                        }
                        Shape::Insert => {
                            preview.kind = PreviewKind::Insert;
                            notes.push(
                                "an INSERT has no prior rows: apply checks the row count".into(),
                            );
                        }
                        Shape::CountOnly(why) => {
                            preview.kind = PreviewKind::CountOnly;
                            notes.push(format!(
                                "row count only ({why}): apply checks the count, not the rows"
                            ));
                        }
                        Shape::NotPreviewable(_) => unreachable!("not previewed"),
                    }
                    notes.push(PREVIEW_CAVEAT.into());
                    preview.note = Some(notes.join("; "));
                }
            }
        }
    }
    let word = first_word(&statement, cfg.backend.dialect()).to_ascii_lowercase();
    let mut p = Proposal {
        sql_review: FORMAT,
        id: format!("{}-{word}", uuid::Uuid::now_v7()),
        created: now(),
        backend: cfg.backend.name().into(),
        profile: cfg.profile.clone(),
        target: cfg.target(),
        statement,
        params: params
            .iter()
            .map(|(k, v)| (k.clone(), v.to_json()))
            .collect(),
        preview,
        outcome: None,
    };
    p.id.retain(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    Ok(p)
}

/// Apply a proposal on `cfg`'s write account. Commits only if its rows and
/// row count are what the proposal recorded.
pub async fn apply(p: &Proposal, cfg: &Config) -> Result<u64, ApplyError> {
    if let Some(o) = &p.outcome {
        return Err(ApplyError::Invalid(format!(
            "{} was already decided ({}, {})",
            p.id,
            o.status.name(),
            o.decided
        )));
    }
    let account = write_account(cfg).map_err(ApplyError::Invalid)?;
    let kind = p.kind().map_err(ApplyError::Invalid)?;
    if kind != cfg.backend || p.target != cfg.target() {
        return Err(ApplyError::Invalid(format!(
            "{} was proposed against {}, and this profile is {}",
            p.id,
            p.target,
            cfg.target()
        )));
    }
    let params = p.params().map_err(ApplyError::Invalid)?;
    let shape = match p.preview.kind {
        PreviewKind::Rows => shape(&p.statement, kind.dialect()).map_err(ApplyError::Invalid)?,
        _ => Shape::NotPreviewable(String::new()),
    };
    if p.preview.kind == PreviewKind::Rows && !matches!(shape, Shape::Rows { .. }) {
        return Err(ApplyError::Invalid(
            "the statement no longer reads as a single-table write".into(),
        ));
    }
    let plan = plan(kind, &p.statement, params, &shape);
    let expect = match p.preview.kind {
        PreviewKind::Rows => Expected {
            count: p.preview.row_count,
            hash: p.preview.rows_hash.clone(),
        },
        PreviewKind::Insert | PreviewKind::CountOnly => Expected {
            count: p.preview.row_count,
            hash: None,
        },
        PreviewKind::NotPreviewable => Expected::default(),
    };
    backend::apply(cfg, &account, &plan, &expect)
        .await
        .map_err(|e| match e {
            ApplyFail::Stale(s) => ApplyError::Stale(s),
            ApplyFail::Db(e) => ApplyError::Db(e.to_string()),
        })
}

/// A proposal for a person to read: the statement, what it would change,
/// and how it was decided.
pub fn render(p: &Proposal) -> String {
    let mut s = format!("proposal {}\n", p.id);
    s.push_str(&format!("  database: {}", p.target));
    if let Some(profile) = &p.profile {
        s.push_str(&format!(" (profile {profile})"));
    }
    s.push_str(&format!("\n  proposed: {}\n\n", p.created));
    s.push_str(&p.statement);
    s.push('\n');
    if !p.params.is_empty() {
        let params: Vec<String> = p
            .params
            .iter()
            .map(|(k, v)| format!(":{k} = {v}"))
            .collect();
        s.push_str(&format!("  with {}\n", params.join(", ")));
    }
    s.push('\n');
    let pv = &p.preview;
    match pv.kind {
        PreviewKind::NotPreviewable => {
            s.push_str("not previewable: applying runs the statement as written, and nothing checks what it changes\n");
        }
        _ => {
            let n = pv.row_count.unwrap_or_default();
            s.push_str(&format!("{n} row(s) would change"));
            if let Some(h) = &pv.rows_hash {
                s.push_str(&format!(" (rows hash {})", &h[..h.len().min(12)]));
            }
            s.push('\n');
        }
    }
    let table = |label: &str, sample: &Sample, total: Option<u64>, s: &mut String| {
        let shown = sample.rows.len() as u64;
        let more = match total {
            Some(t) if t > shown => format!(" (first {shown} of {t})"),
            _ => String::new(),
        };
        s.push_str(&format!("\n{label}{more}:\n"));
        s.push_str(&render_rows(&sample.result_set(), Format::Table));
        s.push('\n');
    };
    if let Some(b) = &pv.before {
        table("before", b, pv.rows_found, &mut s);
    }
    if let Some(a) = &pv.after {
        table("after", a, None, &mut s);
    }
    if let Some(note) = &pv.note {
        s.push_str(&format!("\nnote: {note}\n"));
    }
    if let Some(o) = &p.outcome {
        s.push_str(&format!("\n{}", describe(p, o)));
    }
    s
}

/// How a decided proposal turned out.
pub fn describe(p: &Proposal, o: &Outcome) -> String {
    let mut s = format!("{}: {}", p.id, o.status.name());
    if let Some(n) = o.row_count {
        s.push_str(&format!(", {n} row(s) changed"));
    }
    if let Some(r) = &o.reason {
        s.push_str(&format!(": {r}"));
    }
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(sql: &str) -> Shape {
        shape(sql, Dialect::Sqlite).unwrap()
    }

    #[test]
    fn single_table_updates_and_deletes_have_rows() {
        assert_eq!(
            sh("UPDATE apps SET score = 1 WHERE strm = :strm"),
            Shape::Rows {
                delete: false,
                table: "apps".into(),
                alias: None,
                filter: Some("strm = :strm".into()),
            }
        );
        assert_eq!(
            sh("DELETE FROM apps"),
            Shape::Rows {
                delete: true,
                table: "apps".into(),
                alias: None,
                filter: None,
            }
        );
        let Shape::Rows { alias, .. } = shape(
            "UPDATE ps.apps a SET a.score = 1 WHERE a.app_id = 2",
            Dialect::Oracle,
        )
        .unwrap() else {
            panic!("rows")
        };
        assert_eq!(alias.as_deref(), Some("a"));
    }

    #[test]
    fn other_writes_are_counted_or_not_previewable() {
        assert_eq!(
            sh("INSERT INTO apps VALUES (4, 'd', 1, 'x')"),
            Shape::Insert
        );
        assert!(matches!(
            sh("UPDATE apps SET score = 1 WHERE app_id IN (SELECT app_id FROM other)"),
            Shape::CountOnly(_)
        ));
        assert!(matches!(
            sh("UPDATE apps SET score = (SELECT max(score) FROM other)"),
            Shape::CountOnly(_)
        ));
        assert!(matches!(
            shape(
                "UPDATE a SET a.x = b.x FROM apps a JOIN other b ON a.id = b.id",
                Dialect::MsSql
            )
            .unwrap(),
            Shape::CountOnly(_)
        ));
        for ddl in [
            "ALTER TABLE apps ADD COLUMN x INTEGER",
            "DROP TABLE apps",
            "CREATE TABLE t (a INTEGER)",
        ] {
            let Shape::NotPreviewable(why) = sh(ddl) else {
                panic!("{ddl}")
            };
            assert!(why.contains("DDL"), "{why}");
        }
        assert!(matches!(
            shape("EXEC dbo.do_things", Dialect::MsSql).unwrap(),
            Shape::NotPreviewable(_)
        ));
        assert!(matches!(
            shape("BEGIN do_things; END;", Dialect::Oracle).unwrap(),
            Shape::NotPreviewable(_)
        ));
    }

    #[test]
    fn reads_and_several_statements_are_not_proposals() {
        assert!(
            shape("SELECT 1", Dialect::Sqlite)
                .unwrap_err()
                .contains("only reads")
        );
        assert!(
            shape("DELETE FROM a; DELETE FROM b", Dialect::Sqlite)
                .unwrap_err()
                .contains("one at a time")
        );
    }

    #[test]
    fn row_queries_per_backend() {
        let q = row_queries(Kind::Oracle, false, "ps.apps", Some("a"), Some("a.x = 1"));
        assert_eq!(q.select, "SELECT * FROM ps.apps a WHERE a.x = 1");
        assert_eq!(q.lock, "SELECT a.* FROM ps.apps a WHERE a.x = 1 FOR UPDATE");
        assert!(q.located.unwrap().select.starts_with("SELECT a.ROWID AS"));
        let q = row_queries(Kind::MsSql, false, "dbo.apps", None, Some("x = 1"));
        assert_eq!(
            q.lock,
            "SELECT * FROM dbo.apps WITH (UPDLOCK, HOLDLOCK) WHERE x = 1"
        );
        assert!(q.located.is_none());
        let q = row_queries(Kind::Sqlite, true, "apps", None, None);
        assert_eq!(q.lock, "SELECT * FROM apps");
        assert!(q.located.is_none());
    }

    #[test]
    fn normalize_drops_trailing_semicolons() {
        assert_eq!(normalize("  DELETE FROM t ;\n;  "), "DELETE FROM t");
    }
}
