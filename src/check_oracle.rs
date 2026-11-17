//! `sql-review check-oracle`: walk an Oracle profile through every step a
//! read takes, printing PASS or FAIL for each. There is no write probe.
//!
//! This is how the Oracle backend is verified: nothing in the test suite
//! reaches an Oracle database.

use crate::{
    backend::{DbError, Kind, oracle},
    cli::{CheckOracleArgs, Io},
    render::{Format, render},
    value::{Cell, Param, Params},
};

const TYPED: &str = "SELECT 1 n, 'x' s, SYSDATE d, CAST(1.5 AS NUMBER(5,2)) f, NULL z FROM DUAL";

/// Returns the exit code: 0 only if every step passed.
pub async fn run(args: CheckOracleArgs, io: Io<'_>) -> i32 {
    let cfg = match crate::cli::load_config(
        io.env,
        &io.cwd,
        args.env_file.as_deref(),
        args.profile.as_deref(),
        true,
    ) {
        Ok(cfg) => cfg,
        Err(e) => {
            let _ = writeln!(io.err, "ERROR: {e}");
            return 2;
        }
    };
    if cfg.backend != Kind::Oracle {
        let _ = writeln!(
            io.err,
            "ERROR: check-oracle needs an oracle profile; this one is {}",
            cfg.backend.name()
        );
        return 2;
    }
    let _ = writeln!(io.out, "checking {}", cfg.redacted_url());
    let table = args.table.clone();
    let results = tokio::task::spawn_blocking(move || steps(&cfg, &table))
        .await
        .unwrap_or_else(|e| vec![("run", Err(DbError(format!("the check task failed: {e}"))))]);
    let mut ok = true;
    for (step, result) in results {
        match result {
            Ok(detail) => {
                let _ = writeln!(io.out, "PASS {step}: {detail}");
            }
            Err(e) => {
                ok = false;
                let _ = writeln!(io.out, "FAIL {step}: {e}");
            }
        }
    }
    if ok { 0 } else { 1 }
}

type Step = (&'static str, Result<String, DbError>);

fn steps(cfg: &crate::config::Config, table: &str) -> Vec<Step> {
    let conn = match oracle::connect(cfg) {
        Ok(c) => c,
        Err(e) => {
            let skipped = || Err(DbError("skipped: no connection".into()));
            return vec![
                ("connect", Err(e)),
                ("select", skipped()),
                ("types", skipped()),
                ("describe", skipped()),
                ("read-only transaction", skipped()),
                ("binds", skipped()),
            ];
        }
    };
    let none = Params::new();
    let mut out: Vec<Step> = vec![("connect", Ok("connected".into()))];

    out.push((
        "select",
        conn.query_row("SELECT 1 FROM DUAL", &[])
            .map(|_| "SELECT 1 FROM DUAL".into())
            .map_err(DbError::query),
    ));

    out.push((
        "types",
        oracle::read_only(&conn, |c| oracle::read_on(c, TYPED, &none, 0)).and_then(|rs| {
            if rs.columns.len() != 5 || rs.rows.len() != 1 {
                return Err(DbError(format!(
                    "expected 5 columns and 1 row, got {} and {}",
                    rs.columns.len(),
                    rs.rows.len()
                )));
            }
            for f in [Format::Table, Format::Csv, Format::Tsv, Format::Markdown] {
                render(&rs, f);
            }
            Ok(render(&rs, Format::Json))
        }),
    ));

    let describe_params = Params::from([("t".to_string(), Param::Text(table.into()))]);
    out.push((
        "describe",
        oracle::read_only(&conn, |c| {
            oracle::read_on(c, Kind::Oracle.describe_sql(), &describe_params, 0)
        })
        .and_then(|rs| match rs.rows.len() {
            0 => Err(DbError(format!(
                "{table} has no columns visible to this account"
            ))),
            n => Ok(format!("{table}: {n} column(s)")),
        }),
    ));

    out.push((
        "read-only transaction",
        oracle::read_only(&conn, |c| {
            oracle::read_on(c, "SELECT 1 n FROM DUAL", &none, 0)
        })
        .map(|_| "SET TRANSACTION READ ONLY, a query, ROLLBACK".into()),
    ));

    let bind = Params::from([("v".to_string(), Param::Text("sql-review".into()))]);
    out.push((
        "binds",
        oracle::read_only(&conn, |c| {
            oracle::read_on(c, "SELECT :v AS v FROM DUAL", &bind, 0)
        })
        .and_then(|rs| match rs.rows.first().and_then(|r| r.first()) {
            Some(Cell::Text(s)) if s == "sql-review" => Ok(":v bound and returned".into()),
            other => Err(DbError(format!("expected 'sql-review', got {other:?}"))),
        }),
    ));
    out
}
