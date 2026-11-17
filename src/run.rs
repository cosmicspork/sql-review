//! One entry for every mode: classify, refuse writes, run the read.

use crate::{
    backend::{self, DbError},
    classify::{Classification, Unclassifiable, classify},
    config::{Config, ConfigError},
    value::{Param, Params, ResultSet},
};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Config(#[from] ConfigError),
    /// A write. Writes will be proposed for review in a later version.
    #[error(
        "this statement writes ({0}); sql-review runs reads only for now: \
         writes will be proposed for review in a later version"
    )]
    WritesUnavailable(String),
    #[error("this statement writes ({0}); writes are disabled (--read-only)")]
    WritesDisabled(String),
    #[error("refused: {0}")]
    Unclassifiable(#[from] Unclassifiable),
    #[error("{0:?} is not a table name")]
    BadTableName(String),
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

/// List a table's columns through the backend's data dictionary.
pub async fn describe(cfg: &Config, table: &str) -> Result<ResultSet, Error> {
    if !is_table_name(table) {
        return Err(Error::BadTableName(table.into()));
    }
    let params = Params::from([("t".to_string(), Param::Text(table.into()))]);
    query(
        cfg,
        &Request {
            sql: cfg.backend.describe_sql().into(),
            params,
            limit: Some(0),
        },
    )
    .await
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

    #[test]
    fn table_names() {
        for ok in ["apps", "ps.adm_appl_data", "V$SESSION", "t_1"] {
            assert!(is_table_name(ok), "{ok}");
        }
        for bad in ["", ".a", "a.", "a..b", "a b", "a;drop", "a'--"] {
            assert!(!is_table_name(bad), "{bad}");
        }
    }
}
