//! SQL Server, only when one is reachable: set `SQL_REVIEW_TEST_MSSQL_URL`
//! to `mssql://user:password@host:port/database`, and
//! `SQL_REVIEW_TEST_MSSQL_TRUST_CERT=1` for a self-signed server.
//! Without it these tests pass without running anything.

use sql_review::{
    backend,
    config::{CredentialSource, Resolve, resolve},
    run::{self, Request},
    value::{Cell, Param, Params},
};

fn config() -> Option<sql_review::config::Config> {
    let url = std::env::var("SQL_REVIEW_TEST_MSSQL_URL").ok()?;
    let mut fields = std::collections::BTreeMap::from([("DATABASE_URL".to_string(), url)]);
    if let Ok(t) = std::env::var("SQL_REVIEW_TEST_MSSQL_TRUST_CERT") {
        fields.insert("DB_TRUST_SERVER_CERTIFICATE".into(), t);
    }
    Some(
        resolve(
            &CredentialSource(fields),
            Resolve {
                read_only: true,
                ..Default::default()
            },
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn query_with_rewritten_params() {
    let Some(cfg) = config() else { return };
    backend::check(&cfg).await.unwrap();
    let params = Params::from([
        ("a".to_string(), Param::Int(7)),
        ("b".to_string(), Param::Text("x".into())),
    ]);
    let rs = run::query(
        &cfg,
        &Request {
            sql: "SELECT :a AS a, :b AS b, ':a' AS lit, :a + 1 AS a1".into(),
            params,
            limit: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(rs.columns, ["a", "b", "lit", "a1"]);
    assert_eq!(
        rs.rows,
        [vec![
            Cell::Int(7),
            Cell::Text("x".into()),
            Cell::Text(":a".into()),
            Cell::Int(8)
        ]]
    );
}

#[tokio::test]
async fn describe_a_catalog_view() {
    let Some(cfg) = config() else { return };
    // information_schema lists views too, so this needs no user table.
    let rs = run::describe(&cfg, "COLUMNS").await.unwrap();
    assert!(!rs.rows.is_empty());
}

#[tokio::test]
async fn a_write_sent_past_the_classifier_is_rolled_back() {
    let Some(cfg) = config() else { return };
    // Straight to the backend, as a misclassified write would arrive. It
    // either fails (a read-only login) or runs and is rolled back.
    let _ = backend::read(
        &cfg,
        "SELECT 1 AS x INTO sql_review_probe",
        &Params::new(),
        0,
    )
    .await;
    let rs = backend::read(
        &cfg,
        "SELECT OBJECT_ID('sql_review_probe') AS id",
        &Params::new(),
        0,
    )
    .await
    .unwrap();
    assert_eq!(rs.rows, [vec![Cell::Null]]);
}
