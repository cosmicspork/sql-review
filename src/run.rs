//! One entry for every mode: classify, refuse writes, run the read.

use crate::{
    backend::{self, DbError, Kind},
    classify::{Classification, Unclassifiable, classify},
    config::{Config, ConfigError},
    value::{Param, Params, ResultSet},
};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Config(#[from] ConfigError),
    /// A write sent where only reads run.
    #[error(
        "this statement writes ({0}); queries run reads only: propose a write for review instead"
    )]
    WritesUnavailable(String),
    #[error("this statement writes ({0}); writes are disabled (--read-only)")]
    WritesDisabled(String),
    #[error("refused: {0}")]
    Unclassifiable(#[from] Unclassifiable),
    #[error("{0:?} is not a table name")]
    BadTableName(String),
    #[error("no table {0} visible to this account")]
    NoSuchTable(String),
    #[error("{0}")]
    Db(#[from] DbError),
}

/// A read to run. `limit`: `None` is the configured `DB_FETCH_LIMIT`, `0`
/// no cap.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub sql: String,
    pub params: Params,
    pub limit: Option<u64>,
}

/// Refuse anything that is not a read, before connecting.
pub fn refuse_writes(cfg: &Config, sql: &str) -> Result<(), Error> {
    match classify(sql, cfg.backend.dialect())? {
        Classification::Read => Ok(()),
        Classification::Write(why) if cfg.read_only => Err(Error::WritesDisabled(why)),
        Classification::Write(why) => Err(Error::WritesUnavailable(why)),
    }
}

pub async fn query(cfg: &Config, req: &Request) -> Result<ResultSet, Error> {
    refuse_writes(cfg, &req.sql)?;
    let limit = req.limit.unwrap_or(cfg.fetch_limit);
    Ok(backend::read(cfg, &req.sql, &req.params, limit).await?)
}

/// A table name as `describe` takes it: `TABLE` or `OWNER.TABLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableName {
    /// The owner (Oracle), schema (SQL Server) or attached database
    /// (SQLite), when the name gave one.
    pub owner: Option<String>,
    pub table: String,
}

impl TableName {
    /// Split a bare or once-qualified name. Anything else, `a.b.c`
    /// included, is not a table name.
    pub fn parse(name: &str) -> Result<Self, Error> {
        if !is_table_name(name) {
            return Err(Error::BadTableName(name.into()));
        }
        match name.split_once('.') {
            None => Ok(TableName {
                owner: None,
                table: name.into(),
            }),
            Some((owner, table)) if !table.contains('.') => Ok(TableName {
                owner: Some(owner.into()),
                table: table.into(),
            }),
            Some(_) => Err(Error::BadTableName(name.into())),
        }
    }

    /// The catalog query for this name on `kind`, and what it binds: `:t`
    /// always, `:o` only for a qualified name.
    pub fn query(&self, kind: Kind) -> (&'static str, Params) {
        let mut params = Params::from([("t".to_string(), Param::Text(self.table.clone()))]);
        if let Some(owner) = &self.owner {
            params.insert("o".into(), Param::Text(owner.clone()));
        }
        (kind.describe_sql(self.owner.is_some()), params)
    }

    /// Turn the catalog's rows into the answer: an error when nothing
    /// matched, and a notice when a bare name matched more than one owner.
    pub fn finish(&self, kind: Kind, result: ResultSet) -> Result<Description, Error> {
        if result.rows.is_empty() {
            return Err(Error::NoSuchTable(self.to_string()));
        }
        let notice = match (&self.owner, kind.describe_owner_column()) {
            (None, Some(column)) => {
                let owners = result.distinct_text(column);
                (owners.len() > 1).then(|| {
                    format!(
                        "{table} is in {n} schemas ({list}), so the rows mix them: each row's \
                         {column} says which; describe {first}.{table} for one",
                        table = self.table,
                        n = owners.len(),
                        list = owners.join(", "),
                        first = owners[0],
                    )
                })
            }
            _ => None,
        };
        Ok(Description { result, notice })
    }
}

impl std::fmt::Display for TableName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.owner {
            Some(o) => write!(f, "{o}.{}", self.table),
            None => f.write_str(&self.table),
        }
    }
}

/// A table's columns, and anything the caller should know about them.
#[derive(Debug, Clone, PartialEq)]
pub struct Description {
    pub result: ResultSet,
    /// Set when a bare name matched tables under more than one owner.
    pub notice: Option<String>,
}

impl Description {
    /// The tools' result shape, with a `notice` field when there is one.
    pub fn to_tool_json(&self) -> String {
        let rows = self.result.to_tool_json();
        match &self.notice {
            Some(n) => format!(
                "{},\"notice\":{}}}",
                &rows[..rows.len() - 1],
                serde_json::Value::String(n.clone())
            ),
            None => rows,
        }
    }
}

/// List a table's columns through the backend's data dictionary.
pub async fn describe(cfg: &Config, name: &str) -> Result<Description, Error> {
    let name = TableName::parse(name)?;
    let (sql, params) = name.query(cfg.backend);
    let result = query(
        cfg,
        &Request {
            sql: sql.into(),
            params,
            limit: Some(0),
        },
    )
    .await?;
    name.finish(cfg.backend, result)
}

/// A bare, optionally schema-qualified table name: letters, digits, `_`,
/// `$` and single interior dots.
pub fn is_table_name(t: &str) -> bool {
    !t.is_empty()
        && !t.starts_with('.')
        && !t.ends_with('.')
        && !t.contains("..")
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '$')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Cell;

    #[test]
    fn table_names() {
        for ok in ["apps", "ps.adm_appl_data", "V$SESSION", "t_1"] {
            assert!(is_table_name(ok), "{ok}");
        }
        for bad in ["", ".a", "a.", "a..b", "a b", "a;drop", "a'--"] {
            assert!(!is_table_name(bad), "{bad}");
        }
    }

    #[test]
    fn names_split_into_owner_and_table() {
        assert_eq!(
            TableName::parse("APP4ADM.ADM_APP_DATA").unwrap(),
            TableName {
                owner: Some("APP4ADM".into()),
                table: "ADM_APP_DATA".into()
            }
        );
        assert_eq!(
            TableName::parse("apps").unwrap(),
            TableName {
                owner: None,
                table: "apps".into()
            }
        );
        for bad in ["", ".a", "a.", "a.b.c", "a..b", "\"MixedCase\"", "a.\"b\""] {
            assert_eq!(
                TableName::parse(bad),
                Err(Error::BadTableName(bad.into())),
                "{bad}"
            );
        }
        assert_eq!(
            TableName::parse("APP4ADM.ADM_APP_DATA")
                .unwrap()
                .to_string(),
            "APP4ADM.ADM_APP_DATA"
        );
    }

    #[test]
    fn a_qualified_name_binds_its_owner() {
        let name = TableName::parse("APP4ADM.ADM_APP_DATA").unwrap();
        for kind in Kind::ALL {
            let (sql, params) = name.query(kind);
            assert_eq!(sql, kind.describe_sql(true));
            assert_eq!(
                params,
                Params::from([
                    ("o".to_string(), Param::Text("APP4ADM".into())),
                    ("t".to_string(), Param::Text("ADM_APP_DATA".into())),
                ])
            );
        }
    }

    #[test]
    fn a_bare_name_binds_only_the_table() {
        // Oracle refuses a bind with no placeholder, so `:o` must be absent.
        let name = TableName::parse("ADM_APP_DATA").unwrap();
        for kind in Kind::ALL {
            let (sql, params) = name.query(kind);
            assert_eq!(sql, kind.describe_sql(false));
            assert_eq!(
                params,
                Params::from([("t".to_string(), Param::Text("ADM_APP_DATA".into()))])
            );
        }
    }

    /// The placeholders each catalog query uses, outside literals.
    fn binds(kind: Kind, qualified: bool) -> Vec<String> {
        let mut names: Vec<String> =
            backend::placeholders(kind.describe_sql(qualified), kind.dialect())
                .unwrap()
                .into_iter()
                .map(|(_, n)| n)
                .collect();
        names.sort();
        names.dedup();
        names
    }

    #[test]
    fn catalog_queries_bind_what_the_name_gives() {
        for kind in Kind::ALL {
            assert_eq!(binds(kind, true), ["o", "t"], "{kind:?}");
            assert_eq!(binds(kind, false), ["t"], "{kind:?}");
        }
    }

    #[test]
    fn oracle_catalog_queries() {
        assert_eq!(
            Kind::Oracle.describe_sql(true),
            "SELECT column_name, data_type, data_length, data_precision, data_scale, nullable, \
             column_id FROM all_tab_columns WHERE owner = UPPER(:o) AND table_name = UPPER(:t) \
             ORDER BY column_id"
        );
        assert_eq!(
            Kind::Oracle.describe_sql(false),
            "SELECT owner, column_name, data_type, data_length, data_precision, data_scale, \
             nullable, column_id FROM all_tab_columns WHERE table_name = UPPER(:t) \
             ORDER BY owner, column_id"
        );
    }

    #[test]
    fn mssql_catalog_queries() {
        assert_eq!(
            Kind::MsSql.describe_sql(true),
            "SELECT column_name, data_type, character_maximum_length, numeric_precision, \
             numeric_scale, is_nullable, ordinal_position FROM information_schema.columns \
             WHERE table_schema = :o AND table_name = :t ORDER BY ordinal_position"
        );
        assert_eq!(
            Kind::MsSql.describe_sql(false),
            "SELECT table_schema, column_name, data_type, character_maximum_length, \
             numeric_precision, numeric_scale, is_nullable, ordinal_position \
             FROM information_schema.columns WHERE table_name = :t \
             ORDER BY table_schema, ordinal_position"
        );
    }

    #[test]
    fn sqlite_catalog_queries() {
        assert_eq!(
            Kind::Sqlite.describe_sql(true),
            r#"SELECT name, type, "notnull", dflt_value, pk FROM pragma_table_info(:t, :o)"#
        );
        assert_eq!(
            Kind::Sqlite.describe_sql(false),
            r#"SELECT name, type, "notnull", dflt_value, pk FROM pragma_table_info(:t)"#
        );
    }

    fn rows(columns: &[&str], rows: &[&[&str]]) -> ResultSet {
        ResultSet {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|v| Cell::Text(v.to_string())).collect())
                .collect(),
            truncated: false,
        }
    }

    #[test]
    fn nothing_matching_is_an_error_naming_the_table() {
        let name = TableName::parse("APP4ADM.ADM_APP_DATA").unwrap();
        let empty = rows(&["COLUMN_NAME"], &[]);
        let e = name.finish(Kind::Oracle, empty).unwrap_err();
        assert_eq!(
            e.to_string(),
            "no table APP4ADM.ADM_APP_DATA visible to this account"
        );
    }

    #[test]
    fn a_bare_name_under_two_owners_says_so() {
        let name = TableName::parse("ADM_APP_DATA").unwrap();
        // Oracle names its columns in capitals.
        let rs = rows(
            &["OWNER", "COLUMN_NAME"],
            &[&["APP4ADM", "ID"], &["APP4ADM", "NAME"], &["PS", "ID"]],
        );
        let d = name.finish(Kind::Oracle, rs.clone()).unwrap();
        assert_eq!(d.result, rs, "the rows are kept");
        assert_eq!(
            d.notice.as_deref(),
            Some(
                "ADM_APP_DATA is in 2 schemas (APP4ADM, PS), so the rows mix them: each row's \
                 owner says which; describe APP4ADM.ADM_APP_DATA for one"
            )
        );
        let json: serde_json::Value = serde_json::from_str(&d.to_tool_json()).unwrap();
        assert_eq!(json["row_count"], 3);
        assert_eq!(json["notice"], d.notice.unwrap().as_str());

        let rs = rows(
            &["table_schema", "column_name"],
            &[&["dbo", "id"], &["sales", "id"]],
        );
        let d = name.finish(Kind::MsSql, rs).unwrap();
        assert!(
            d.notice
                .unwrap()
                .contains("each row's table_schema says which")
        );
    }

    #[test]
    fn one_owner_needs_no_notice() {
        let bare = TableName::parse("ADM_APP_DATA").unwrap();
        let rs = rows(&["OWNER", "COLUMN_NAME"], &[&["PS", "ID"], &["PS", "NAME"]]);
        let d = bare.finish(Kind::Oracle, rs).unwrap();
        assert_eq!(d.notice, None);
        let json: serde_json::Value = serde_json::from_str(&d.to_tool_json()).unwrap();
        assert!(json.get("notice").is_none());

        let qualified = TableName::parse("PS.ADM_APP_DATA").unwrap();
        let rs = rows(&["COLUMN_NAME"], &[&["ID"]]);
        assert_eq!(qualified.finish(Kind::Oracle, rs).unwrap().notice, None);
    }
}
