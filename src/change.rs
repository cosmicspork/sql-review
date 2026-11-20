//! A write as a proposal: the statement, and a version of the rows it
//! would change. **No write is ever run to preview it**: a rollback is not
//! reliable everywhere (DDL commits implicitly on Oracle, and sequences,
//! autonomous-transaction triggers and external effects survive one).
//!
//! What a proposal can show depends on the statement:
//!
//! - a single-table `UPDATE` or `DELETE`: a `SELECT` derived from its
//!   `WHERE`, run on the read-only account, gives the rows as they are now
//!   (a sample), their count, and a hash of them. `apply` refuses if the
//!   count or the hash moved, and commits only if the statement changed
//!   that many rows.
//! - anything else (`INSERT`, other DML, DDL, procedure calls, blocks): the
//!   statement alone, "not previewable", with the reason. `apply` runs it as
//!   written.
//!
//! The proposal file is pretty-printed JSON with a fixed key order, so it
//! can be reviewed in git like any other diff.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlparser::{
    ast::{FromTable, Statement, TableFactor, TableWithJoins},
    keywords::Keyword,
    parser::Parser,
    tokenizer::{Token as SqlToken, Tokenizer},
};

use crate::{
    backend::{self, ApplyFail, Expected, Kind, RowQueries, WritePlan},
    classify::{Classification, Dialect, classify},
    config::Config,
    render::{Format, render as render_rows},
    value::{Cell, Param, Params, ResultSet},
};

/// The proposal file format this build reads and writes. Format 1 was
/// written by sql-review 0.2, which ran the statement to preview it.
pub const FORMAT: u32 = 2;

/// Rows shown in a proposal; the hash and count cover every affected row.
pub const SAMPLE_ROWS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewKind {
    /// A single-table UPDATE or DELETE: rows, count and hash, read on the
    /// read-only account.
    Rows,
    /// The statement alone.
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
    /// Why it is not previewable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The read-only query that found the affected rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_query: Option<String>,
    /// How many rows that query found, which the locked rows and the
    /// statement's own count must both match at apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
    /// The hash of every affected row, which apply must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_hash: Option<String>,
    /// The first [`SAMPLE_ROWS`] of them, as they were when proposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<Sample>,
}

impl Preview {
    /// The statement alone, and why.
    pub fn not_previewable(why: impl Into<String>) -> Self {
        Self {
            kind: PreviewKind::NotPreviewable,
            note: Some(why.into()),
            affected_query: None,
            row_count: None,
            rows_hash: None,
            sample: None,
        }
    }
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
                "proposal format {} is not {FORMAT}, the one this build reads; propose it again",
                p.sql_review
            ));
        }
        Ok(p)
    }

    /// A new, undecided proposal of `statement` against `cfg`'s database.
    pub fn new(cfg: &Config, statement: String, params: &Params, preview: Preview) -> Self {
        let word = first_word(&statement, cfg.backend.dialect()).to_ascii_lowercase();
        let mut id = format!("{}-{word}", uuid::Uuid::now_v7());
        id.retain(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        Self {
            sql_review: FORMAT,
            id,
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
        }
    }

    /// One line for a card's heading: the statement's first words, and
    /// what was read of its rows.
    pub fn title(&self) -> String {
        match (self.preview.kind, self.preview.row_count) {
            (PreviewKind::Rows, Some(n)) => format!("{} ({n} row(s))", self.summary()),
            _ => format!("{} (not previewable)", self.summary()),
        }
    }

    /// The version a preview was taken against, as `tracon/preview`'s
    /// token: the rows' count and hash. `None` when not previewable.
    pub fn token(&self) -> Option<String> {
        match (
            &self.preview.kind,
            self.preview.row_count,
            &self.preview.rows_hash,
        ) {
            (PreviewKind::Rows, Some(count), Some(hash)) => Some(
                Token {
                    count,
                    rows_hash: hash.clone(),
                }
                .to_string(),
            ),
            _ => None,
        }
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

/// A preview's token: what an approved write is applied against. On the
/// wire it is the canonical JSON `{"count":N,"rows_hash":"<sha256 hex>"}`;
/// the node never reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub count: u64,
    pub rows_hash: String,
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keys in byte order, so the token is canonical JSON.
        write!(
            f,
            "{{\"count\":{},\"rows_hash\":{}}}",
            self.count,
            Value::String(self.rows_hash.clone())
        )
    }
}

impl std::str::FromStr for Token {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || "preview_token is not a row token from this plugin".to_string();
        let t: Token = serde_json::from_str(s).map_err(|_| bad())?;
        if t.rows_hash.len() != 64
            || !t
                .rows_hash
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(bad());
        }
        Ok(t)
    }
}

/// An RFC 3339 UTC timestamp, to the second.
pub fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// How a statement can be previewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// A single-table UPDATE or DELETE: its rows can be read beforehand.
    Rows {
        delete: bool,
        /// The table, as written.
        table: String,
        /// Its alias, if it has one.
        alias: Option<String>,
        filter: Option<String>,
    },
    /// Anything else, and why.
    NotPreviewable(String),
}

/// Why DML other than a single-table UPDATE or DELETE has no rows to show.
fn not_single_table(why: &str) -> Shape {
    Shape::NotPreviewable(format!(
        "{why}, so no single-table SELECT can find the rows it changes"
    ))
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
                "{} could not be read as plain DML, so there are no rows to read beforehand",
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
        Statement::Insert(_) => Shape::NotPreviewable(
            "an INSERT adds rows rather than changing rows already there, so there are none to \
             read beforehand"
                .into(),
        ),
        Statement::Update(u) => {
            if u.from.is_some() || !u.table.joins.is_empty() {
                return Ok(not_single_table("it joins other tables"));
            }
            if u.limit.is_some() || !u.order_by.is_empty() {
                return Ok(not_single_table("ORDER BY or LIMIT picks its rows"));
            }
            let set: Vec<String> = u.assignments.iter().map(|a| a.to_string()).collect();
            let filter = u.selection.as_ref().map(|e| e.to_string());
            if reads_other_tables(&set.join(", "), dialect)
                || filter
                    .as_deref()
                    .is_some_and(|f| reads_other_tables(f, dialect))
            {
                return Ok(not_single_table("it reads other tables in a subquery"));
            }
            match single_table(&u.table) {
                Some((table, alias)) => Shape::Rows {
                    delete: false,
                    table,
                    alias,
                    filter,
                },
                None => not_single_table("its target is not a plain table"),
            }
        }
        Statement::Delete(d) => {
            let from = match &d.from {
                FromTable::WithFromKeyword(t) | FromTable::WithoutKeyword(t) => t,
            };
            if !d.tables.is_empty() || d.using.is_some() || from.len() != 1 {
                return Ok(not_single_table("it names more than one table"));
            }
            if !from[0].joins.is_empty() {
                return Ok(not_single_table("it joins other tables"));
            }
            if d.limit.is_some() || !d.order_by.is_empty() {
                return Ok(not_single_table("ORDER BY or LIMIT picks its rows"));
            }
            let filter = d.selection.as_ref().map(|e| e.to_string());
            if filter
                .as_deref()
                .is_some_and(|f| reads_other_tables(f, dialect))
            {
                return Ok(not_single_table("it reads other tables in a subquery"));
            }
            match single_table(&from[0]) {
                Some((table, alias)) => Shape::Rows {
                    delete: true,
                    table,
                    alias,
                    filter,
                },
                None => not_single_table("its target is not a plain table"),
            }
        }
        Statement::Merge(_) => not_single_table("a MERGE reads a second table"),
        other => Shape::NotPreviewable(not_previewable_reason(&statement_word(other))),
    })
}

fn not_previewable_reason(word: &str) -> String {
    match word {
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "COMMENT" | "GRANT" | "REVOKE" => {
            format!("{word} is DDL: it changes the schema, not rows a read could show beforehand")
        }
        "CALL" | "EXEC" | "EXECUTE" => {
            format!("{word} runs a procedure: what it changes can't be read beforehand")
        }
        _ => format!(
            "{word} is not a single-table UPDATE or DELETE, so there are no rows to read beforehand"
        ),
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
                SqlToken::Word(w) => Some(w.value.to_ascii_uppercase()),
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
                .any(|t| matches!(t, SqlToken::Word(w) if w.keyword == Keyword::SELECT && w.quote_style.is_none()))
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
    let lock = match kind {
        Kind::Sqlite => select.clone(),
        Kind::Oracle => format!("SELECT {qualifier}.* FROM {relation}{filter} FOR UPDATE"),
        Kind::MsSql => format!("SELECT * FROM {relation} WITH (UPDLOCK, HOLDLOCK){filter}"),
    };
    RowQueries { select, lock }
}

fn plan(kind: Kind, statement: &str, params: Params, shape: &Shape) -> WritePlan {
    let rows = match shape {
        Shape::Rows {
            table,
            alias,
            filter,
            ..
        } => Some(row_queries(
            kind,
            table,
            alias.as_deref(),
            filter.as_deref(),
        )),
        Shape::NotPreviewable(_) => None,
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

/// Propose a write without running it. For a single-table UPDATE or
/// DELETE, read the rows its WHERE selects on `cfg`'s read-only account;
/// anything else is the statement alone. The write account is not used.
pub async fn propose(cfg: &Config, sql: &str, params: &Params) -> Result<Proposal, String> {
    let statement = normalize(sql);
    let preview = match shape(&statement, cfg.backend.dialect())? {
        Shape::NotPreviewable(why) => Preview::not_previewable(why),
        Shape::Rows {
            table,
            alias,
            filter,
            ..
        } => {
            let q = row_queries(cfg.backend, &table, alias.as_deref(), filter.as_deref());
            match backend::read_rows(cfg, &q.select, params, cfg.preview_timeout)
                .await
                .map_err(|e| {
                    format!(
                        "could not read the rows it would change on the read-only account, so \
                         nothing was proposed: {e}"
                    )
                })? {
                None => Preview::not_previewable(format!(
                    "reading the rows it would change ran over its {} s limit \
                     (DB_PREVIEW_TIMEOUT)",
                    cfg.preview_timeout.as_secs_f64()
                )),
                Some(rows) => Preview {
                    kind: PreviewKind::Rows,
                    note: None,
                    affected_query: Some(q.select),
                    row_count: Some(rows.rows.len() as u64),
                    rows_hash: Some(rows.content_hash()),
                    sample: Some(Sample::of(&rows, SAMPLE_ROWS)),
                },
            }
        }
    };
    Ok(Proposal::new(cfg, statement, params, preview))
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
            s.push_str(&format!(
                "not previewable: {}\napplying runs the statement as written, and nothing checks \
                 what it changes\n",
                pv.note.as_deref().unwrap_or("no rows to read beforehand")
            ));
        }
        PreviewKind::Rows => {
            let n = pv.row_count.unwrap_or_default();
            s.push_str(&format!("{n} row(s) would change"));
            if let Some(h) = &pv.rows_hash {
                s.push_str(&format!(" (rows hash {})", &h[..h.len().min(12)]));
            }
            s.push_str("; apply refuses if they move first\n");
            if let Some(sample) = &pv.sample {
                s.push_str(&format!("\nrows now{}:\n", shown_of(sample, n)));
                s.push_str(&render_rows(&sample.result_set(), Format::Table));
                s.push('\n');
            }
        }
    }
    if let Some(o) = &p.outcome {
        s.push_str(&format!("\n{}", describe(p, o)));
    }
    s
}

/// " (first 20 of 30)" when the sample is not every row.
fn shown_of(sample: &Sample, total: u64) -> String {
    let shown = sample.rows.len() as u64;
    if total > shown {
        format!(" (first {shown} of {total})")
    } else {
        String::new()
    }
}

/// A proposal as Markdown, for an approval card: the statement, then the
/// rows and their count, or why it is not previewable.
pub fn render_markdown(p: &Proposal) -> String {
    let longest = p
        .statement
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let mut s = format!("{fence}sql\n{}\n{fence}\n", p.statement);
    if !p.params.is_empty() {
        let params: Vec<String> = p
            .params
            .iter()
            .map(|(k, v)| format!("`:{k}` = `{v}`"))
            .collect();
        s.push_str(&format!("\nWith {}.\n", params.join(", ")));
    }
    let pv = &p.preview;
    match pv.kind {
        PreviewKind::NotPreviewable => {
            s.push_str(&format!(
                "\n**Not previewable:** {}. Approving runs the statement as written, and nothing \
                 checks what it changes.\n",
                pv.note
                    .as_deref()
                    .unwrap_or("there are no rows to read beforehand")
            ));
        }
        PreviewKind::Rows => {
            let n = pv.row_count.unwrap_or_default();
            s.push_str(&format!(
                "\n**{n} row(s) would change.** They are shown as they are now, read on the \
                 read-only account; the statement has not run. It is refused as stale if they \
                 change before it commits.\n"
            ));
            if let Some(sample) = pv.sample.as_ref().filter(|x| !x.rows.is_empty()) {
                s.push_str(&format!("\nRows now{}:\n\n", shown_of(sample, n)));
                s.push_str(&render_rows(&sample.result_set(), Format::Markdown));
                s.push('\n');
            }
        }
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
    fn everything_else_is_not_previewable_with_a_reason() {
        let why = |sql: &str, d: Dialect| match shape(sql, d).unwrap() {
            Shape::NotPreviewable(why) => why,
            other => panic!("{sql}: {other:?}"),
        };
        let s = Dialect::Sqlite;
        assert!(why("INSERT INTO apps VALUES (4, 'd', 1, 'x')", s).contains("INSERT"));
        for (sql, d, says) in [
            (
                "UPDATE apps SET score = 1 WHERE app_id IN (SELECT app_id FROM other)",
                s,
                "subquery",
            ),
            (
                "UPDATE apps SET score = (SELECT max(score) FROM other)",
                s,
                "subquery",
            ),
            (
                "UPDATE a SET a.x = b.x FROM apps a JOIN other b ON a.id = b.id",
                Dialect::MsSql,
                "joins",
            ),
            (
                "MERGE INTO apps a USING other b ON (a.id = b.id) \
                 WHEN MATCHED THEN UPDATE SET a.x = b.x",
                Dialect::Oracle,
                "MERGE",
            ),
            ("EXEC dbo.do_things", Dialect::MsSql, "procedure"),
            ("BEGIN do_things; END;", Dialect::Oracle, "beforehand"),
        ] {
            let why = why(sql, d);
            assert!(why.contains(says), "{sql}: {why}");
        }
        for ddl in [
            "ALTER TABLE apps ADD COLUMN x INTEGER",
            "DROP TABLE apps",
            "CREATE TABLE t (a INTEGER)",
        ] {
            assert!(why(ddl, s).contains("DDL"), "{ddl}");
        }
    }

    #[test]
    fn tokens_round_trip_and_refuse_anything_else() {
        let t = Token {
            count: 2,
            rows_hash: "ab".repeat(32),
        };
        let wire = t.to_string();
        assert_eq!(
            wire,
            format!("{{\"count\":2,\"rows_hash\":\"{}\"}}", "ab".repeat(32))
        );
        assert_eq!(
            crate::contract::canonical(&serde_json::from_str(&wire).unwrap()).unwrap(),
            wire
        );
        assert_eq!(wire.parse::<Token>().unwrap(), t);
        for bad in [
            "",
            "nope",
            "{\"count\":2}",
            "{\"count\":-1,\"rows_hash\":\"ab\"}",
            &format!("{{\"count\":2,\"rows_hash\":\"{}\"}}", "AB".repeat(32)),
            &format!(
                "{{\"count\":2,\"rows_hash\":\"{}\",\"x\":1}}",
                "ab".repeat(32)
            ),
            "{\"WRK-1\":\"2026-11-16T09:12:33.000+0000\"}",
        ] {
            assert!(bad.parse::<Token>().is_err(), "{bad}");
        }
    }

    #[test]
    fn markdown_fences_the_statement_and_shows_the_rows() {
        let cfg = crate::config::resolve(
            &crate::config::CredentialSource(
                [("DB_BACKEND".to_string(), "sqlite".to_string())].into(),
            ),
            crate::config::Resolve::default(),
        )
        .unwrap();
        let rows = ResultSet {
            columns: vec!["id".into(), "note".into()],
            rows: (0..25)
                .map(|i| vec![Cell::Int(i), Cell::Text("a|b".into())])
                .collect(),
            truncated: false,
        };
        let p = Proposal::new(
            &cfg,
            "UPDATE t SET note = '```' WHERE id < :n".into(),
            &[("n".to_string(), Param::Int(25))].into(),
            Preview {
                kind: PreviewKind::Rows,
                note: None,
                affected_query: None,
                row_count: Some(25),
                rows_hash: Some(rows.content_hash()),
                sample: Some(Sample::of(&rows, SAMPLE_ROWS)),
            },
        );
        let md = render_markdown(&p);
        assert!(md.starts_with("````sql\nUPDATE t"), "{md}");
        assert!(md.contains("`:n` = `25`"), "{md}");
        assert!(md.contains("**25 row(s) would change.**"), "{md}");
        assert!(md.contains("(first 20 of 25)"), "{md}");
        assert!(md.contains("a\\|b"), "{md}");
        assert_eq!(
            p.title(),
            "UPDATE t SET note = '```' WHERE id < :n (25 row(s))"
        );
        assert!(p.token().unwrap().parse::<Token>().is_ok());

        let p = Proposal::new(
            &cfg,
            "DROP TABLE t".into(),
            &Params::new(),
            Preview::not_previewable("DROP is DDL"),
        );
        let md = render_markdown(&p);
        assert!(md.contains("**Not previewable:** DROP is DDL."), "{md}");
        assert_eq!(p.token(), None);
        assert_eq!(p.title(), "DROP TABLE t (not previewable)");
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
        let q = row_queries(Kind::Oracle, "ps.apps", Some("a"), Some("a.x = 1"));
        assert_eq!(q.select, "SELECT * FROM ps.apps a WHERE a.x = 1");
        assert_eq!(q.lock, "SELECT a.* FROM ps.apps a WHERE a.x = 1 FOR UPDATE");
        let q = row_queries(Kind::MsSql, "dbo.apps", None, Some("x = 1"));
        assert_eq!(q.select, "SELECT * FROM dbo.apps WHERE x = 1");
        assert_eq!(
            q.lock,
            "SELECT * FROM dbo.apps WITH (UPDLOCK, HOLDLOCK) WHERE x = 1"
        );
        let q = row_queries(Kind::Sqlite, "apps", None, None);
        assert_eq!(q.lock, "SELECT * FROM apps");
        assert_eq!(q.select, q.lock);
    }

    #[test]
    fn normalize_drops_trailing_semicolons() {
        assert_eq!(normalize("  DELETE FROM t ;\n;  "), "DELETE FROM t");
    }
}
