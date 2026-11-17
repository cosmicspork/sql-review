//! The CLI end to end, in process, against SQLite.

mod common;

use std::{collections::BTreeMap, path::Path};

use clap::Parser;
use sql_review::cli::{Cli, Io, run};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

async fn cli_with(args: &[&str], env: &[(&str, &str)], cwd: &Path, stdin: Option<&str>) -> Out {
    let parsed = Cli::try_parse_from(std::iter::once("sql-review").chain(args.iter().copied()))
        .expect("arguments parse");
    let env: BTreeMap<String, String> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let stdin = stdin.map(str::to_string);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(
        parsed.query,
        Io {
            env,
            cwd: cwd.to_path_buf(),
            stdin: Box::new(move || stdin),
            out: &mut out,
            err: &mut err,
        },
    )
    .await;
    Out {
        code,
        stdout: String::from_utf8(out).unwrap(),
        stderr: String::from_utf8(err).unwrap(),
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    db: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = common::apps_db(dir.path(), "apps.sqlite");
        Self { dir, db }
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("DB_BACKEND", "sqlite".into()),
            ("DB_SERVICE", self.db.display().to_string()),
        ]
    }

    async fn cli(&self, args: &[&str]) -> Out {
        self.cli_stdin(args, None).await
    }

    async fn cli_stdin(&self, args: &[&str], stdin: Option<&str>) -> Out {
        let env = self.env();
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        cli_with(args, &env, self.dir.path(), stdin).await
    }
}

#[tokio::test]
async fn every_format() {
    let f = Fixture::new();
    let sql = "SELECT app_id, email FROM apps ORDER BY app_id";
    let out = f.cli(&["--sql", sql, "--quiet", "--format", "csv"]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "app_id,email\n1,a@x.com\n2,b@y.net\n3,c@z.org\n"
    );
    let out = f.cli(&["--sql", sql, "--quiet", "--format", "tsv"]).await;
    assert_eq!(out.stdout.lines().next(), Some("app_id\temail"));
    let out = f.cli(&["--sql", sql, "--quiet", "--format", "json"]).await;
    assert_eq!(
        out.stdout.trim(),
        r#"[{"app_id":1,"email":"a@x.com"},{"app_id":2,"email":"b@y.net"},{"app_id":3,"email":"c@z.org"}]"#
    );
    for fmt in ["markdown", "md"] {
        let out = f.cli(&["--sql", sql, "--quiet", "--format", fmt]).await;
        assert_eq!(out.stdout.lines().nth(2), Some("| 1 | a@x.com |"));
    }
    let out = f.cli(&["--sql", sql, "--quiet"]).await;
    assert!(out.stdout.starts_with('┌'), "{}", out.stdout);
    assert!(
        out.stdout.contains("│ 1      ┆ a@x.com │"),
        "{}",
        out.stdout
    );
}

#[tokio::test]
async fn header_goes_to_stderr() {
    let f = Fixture::new();
    let out = f.cli(&["--sql", "SELECT 1 AS n", "--format", "csv"]).await;
    assert_eq!(out.stdout, "n\n1\n");
    assert!(out.stderr.starts_with("sql-review "), "{}", out.stderr);
    assert!(out.stderr.contains("  backend: sqlite"));
    assert!(!out.stderr.contains("mode:"));
    let out = f.cli(&["--sql", "SELECT 1 AS n", "--read-only"]).await;
    assert!(
        out.stderr.contains("  mode:    read-only"),
        "{}",
        out.stderr
    );
}

#[tokio::test]
async fn describe() {
    let f = Fixture::new();
    let out = f
        .cli(&["--describe", "apps", "--quiet", "--format", "csv"])
        .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let names: Vec<&str> = out
        .stdout
        .lines()
        .skip(1)
        .map(|l| l.split(',').next().unwrap())
        .collect();
    assert_eq!(names, ["app_id", "email", "score", "strm"]);
    let out = f
        .cli(&["--describe", "apps; DROP TABLE apps", "--quiet"])
        .await;
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("is not a table name"), "{}", out.stderr);
}

#[tokio::test]
async fn params_bind() {
    let f = Fixture::new();
    let out = f
        .cli(&[
            "--sql",
            "SELECT app_id FROM apps WHERE strm = :strm ORDER BY app_id",
            "--param",
            "strm=1261",
            "--quiet",
            "--format",
            "csv",
        ])
        .await;
    assert_eq!(out.stdout, "app_id\n1\n2\n");
    let out = f.cli(&["--sql", "SELECT 1", "--param", "oops"]).await;
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("--param expects NAME=VALUE"));
}

#[tokio::test]
async fn limits() {
    let f = Fixture::new();
    let sql = "SELECT app_id FROM apps ORDER BY app_id";
    let out = f
        .cli(&["--sql", sql, "--limit", "2", "--quiet", "--format", "csv"])
        .await;
    assert_eq!(out.stdout, "app_id\n1\n2\n");
    assert!(
        out.stderr
            .contains("(stopped at --limit 2; more rows may exist)")
    );
    let out = f
        .cli(&["--sql", sql, "--limit", "3", "--quiet", "--format", "csv"])
        .await;
    assert!(
        !out.stderr.contains("stopped"),
        "exactly the limit is not cut short"
    );
    let out = f
        .cli(&["--sql", sql, "--limit", "0", "--quiet", "--format", "csv"])
        .await;
    assert_eq!(out.stdout.lines().count(), 4);
    // DB_FETCH_LIMIT is the default.
    let env = [
        ("DB_BACKEND", "sqlite"),
        ("DB_SERVICE", f.db.to_str().unwrap()),
        ("DB_FETCH_LIMIT", "1"),
    ];
    let out = cli_with(
        &["--sql", sql, "--quiet", "--format", "csv"],
        &env,
        f.dir.path(),
        None,
    )
    .await;
    assert_eq!(out.stdout, "app_id\n1\n");
    assert!(out.stderr.contains("(stopped at --limit 1;"));
}

#[tokio::test]
async fn output_file() {
    let f = Fixture::new();
    let out = f
        .cli(&[
            "--sql",
            "SELECT app_id FROM apps",
            "--format",
            "csv",
            "--output",
            "out.csv",
            "--quiet",
        ])
        .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert!(out.stderr.contains("3 row(s) -> out.csv"));
    assert_eq!(
        std::fs::read_to_string(f.dir.path().join("out.csv")).unwrap(),
        "app_id\n1\n2\n3\n"
    );
}

#[tokio::test]
async fn sql_from_stdin_and_file() {
    let f = Fixture::new();
    let out = f
        .cli_stdin(
            &["--quiet", "--format", "csv"],
            Some("  SELECT count(*) AS n FROM apps \n"),
        )
        .await;
    assert_eq!(out.stdout, "n\n3\n");
    std::fs::write(
        f.dir.path().join("q.sql"),
        "SELECT max(score) AS m FROM apps",
    )
    .unwrap();
    let out = f
        .cli(&["--file", "q.sql", "--quiet", "--format", "csv"])
        .await;
    assert_eq!(out.stdout, "m\n30\n");
    let out = f.cli_stdin(&["--quiet"], Some("   ")).await;
    assert_eq!(out.code, 2);
    assert!(
        out.stderr
            .ends_with("ERROR: provide --sql, --file, --describe, or pipe SQL on stdin\n")
    );
}

#[tokio::test]
async fn check() {
    let f = Fixture::new();
    let out = f.cli(&["--check", "--quiet"]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("OK: connection succeeded"));
    let env = [
        ("DB_BACKEND", "sqlite"),
        ("DB_SERVICE", "/nonexistent/x.db"),
    ];
    let out = cli_with(&["--check", "--quiet"], &env, f.dir.path(), None).await;
    assert_eq!(out.code, 2);
    assert!(
        out.stderr.contains("ERROR: connection failed"),
        "{}",
        out.stderr
    );
}

#[tokio::test]
async fn profile_and_env_discovery() {
    let f = Fixture::new();
    let qa = common::apps_db(f.dir.path(), "qa.sqlite");
    rusqlite::Connection::open(&qa)
        .unwrap()
        .execute("DELETE FROM apps WHERE app_id > 1", [])
        .unwrap();
    std::fs::write(
        f.dir.path().join(".env"),
        format!(
            "DB_BACKEND=sqlite\nDB_SERVICE={}\nQA_DB_SERVICE={}\n",
            f.db.display(),
            qa.display()
        ),
    )
    .unwrap();
    let deep = f.dir.path().join("a/b");
    std::fs::create_dir_all(&deep).unwrap();
    let sql = ["--sql", "SELECT count(*) AS n FROM apps", "--format", "csv"];
    let out = cli_with(&sql, &[], &deep, None).await;
    assert_eq!(out.stdout, "n\n3\n", "{}", out.stderr);
    assert!(out.stderr.contains("  env:     "), "{}", out.stderr);
    let mut with_profile = sql.to_vec();
    with_profile.extend(["--profile", "qa"]);
    let out = cli_with(&with_profile, &[], &deep, None).await;
    assert_eq!(out.stdout, "n\n1\n", "{}", out.stderr);
    assert!(out.stderr.contains("  profile: qa"));
    // --env-file names a file outright.
    let mut with_file = sql.to_vec();
    with_file.extend(["--env-file", "../../.env", "--quiet"]);
    let out = cli_with(&with_file, &[], &deep, None).await;
    assert_eq!(out.stdout, "n\n3\n");
    let out = cli_with(
        &["--sql", "SELECT 1", "--env-file", "missing.env"],
        &[],
        &deep,
        None,
    )
    .await;
    assert_eq!(out.code, 2);
    assert!(
        out.stderr.contains("ERROR: .env file not found: "),
        "{}",
        out.stderr
    );
}

#[tokio::test]
async fn writes_are_refused_and_change_nothing() {
    let f = Fixture::new();
    let out = f.cli(&["--sql", "DELETE FROM apps", "--quiet"]).await;
    assert_eq!(out.code, 2);
    assert_eq!(
        out.stderr.lines().last().unwrap(),
        "ERROR: this statement writes (DELETE); sql-review runs reads only for now: \
         writes will be proposed for review in a later version"
    );
    let out = f
        .cli(&["--sql", "DELETE FROM apps", "--quiet", "--read-only"])
        .await;
    assert_eq!(
        out.stderr.lines().last().unwrap(),
        "ERROR: this statement writes (DELETE); writes are disabled (--read-only)"
    );
    let out = f.cli(&["--sql", "not sql ((", "--quiet"]).await;
    assert_eq!(out.code, 2);
    assert!(
        out.stderr.contains("ERROR: refused: could not parse"),
        "{}",
        out.stderr
    );
    assert_eq!(common::count(&f.db), 3);
}

#[tokio::test]
async fn read_only_never_reads_the_write_account() {
    // A write account that is not even well-formed would still not be read.
    let f = Fixture::new();
    let env = [
        ("DB_BACKEND", "sqlite"),
        ("DB_SERVICE", f.db.to_str().unwrap()),
        ("DB_WRITE_USER", "writer"),
    ];
    let out = cli_with(
        &[
            "--sql",
            "SELECT 1 AS n",
            "--read-only",
            "--quiet",
            "--format",
            "csv",
        ],
        &env,
        f.dir.path(),
        None,
    )
    .await;
    assert_eq!(out.stdout, "n\n1\n");
    let cfg = sql_review::cli::load_config(
        env.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        f.dir.path(),
        None,
        None,
        true,
    )
    .unwrap();
    assert_eq!(cfg.write, None);
}

#[tokio::test]
async fn an_empty_result_keeps_its_columns() {
    let f = Fixture::new();
    let out = f
        .cli(&[
            "--sql",
            "SELECT app_id, email FROM apps WHERE app_id = :id",
            "--param",
            "id=-1",
            "--quiet",
            "--format",
            "csv",
        ])
        .await;
    assert_eq!(out.stdout, "app_id,email\n");
    let out = f
        .cli(&["--sql", "SELECT app_id, email FROM apps WHERE 0", "--quiet"])
        .await;
    assert!(out.stdout.contains("app_id"), "{}", out.stdout);
}

/// Decision 7: the read-only connection is the boundary. A write that got
/// past the classifier still fails, because SQLite was opened read-only.
#[tokio::test]
async fn a_write_past_the_classifier_fails_on_the_read_only_connection() {
    let f = Fixture::new();
    let cfg = sql_review::cli::load_config(
        f.env()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        f.dir.path(),
        None,
        None,
        true,
    )
    .unwrap();
    for sql in [
        "INSERT INTO apps VALUES (4, 'd', 0, 'x')",
        "DELETE FROM apps",
    ] {
        let err = sql_review::backend::read(&cfg, sql, &Default::default(), 0)
            .await
            .unwrap_err();
        assert!(
            err.0.contains("readonly") || err.0.contains("read-only"),
            "{err}"
        );
    }
    assert_eq!(common::count(&f.db), 3);
}

#[tokio::test]
async fn sqlite_types() {
    let f = Fixture::new();
    let out = f
        .cli(&[
            "--sql",
            "SELECT 1 AS i, 1.5 AS r, 'x' AS t, x'cafe' AS b, NULL AS n",
            "--quiet",
            "--format",
            "json",
        ])
        .await;
    assert_eq!(
        out.stdout.trim(),
        r#"[{"i":1,"r":1.5,"t":"x","b":"cafe","n":null}]"#
    );
}

async fn check_oracle(env: &[(&str, &str)], cwd: &Path) -> Out {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = sql_review::check_oracle::run(
        sql_review::cli::CheckOracleArgs {
            table: "DUAL".into(),
            ..Default::default()
        },
        Io {
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            cwd: cwd.to_path_buf(),
            stdin: Box::new(|| None),
            out: &mut out,
            err: &mut err,
        },
    )
    .await;
    Out {
        code,
        stdout: String::from_utf8(out).unwrap(),
        stderr: String::from_utf8(err).unwrap(),
    }
}

#[tokio::test]
async fn check_oracle_needs_an_oracle_profile() {
    let f = Fixture::new();
    let out = check_oracle(&[("DB_BACKEND", "sqlite")], f.dir.path()).await;
    assert_eq!(out.code, 2);
    assert!(
        out.stderr
            .contains("check-oracle needs an oracle profile; this one is sqlite")
    );
}

#[tokio::test]
async fn check_oracle_reports_each_step() {
    // Nothing listens on port 1, so the connection fails and every later
    // step reports itself skipped. This is as far as Oracle goes here.
    let f = Fixture::new();
    let env = [
        ("DB_BACKEND", "oracle"),
        ("DB_HOST", "127.0.0.1"),
        ("DB_PORT", "1"),
        ("DB_SERVICE", "FREEPDB1"),
        ("DB_USER", "u"),
        ("DB_PASSWORD", "secret-pw"),
    ];
    let out = check_oracle(&env, f.dir.path()).await;
    assert_eq!(out.code, 1, "{}", out.stdout);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(
        lines[0],
        "checking oracle+oracledb://u:***@127.0.0.1:1/?service_name=FREEPDB1"
    );
    assert!(
        lines[1].starts_with("FAIL connect: connection failed"),
        "{}",
        lines[1]
    );
    assert_eq!(lines.len(), 7);
    assert!(
        lines[2..]
            .iter()
            .all(|l| l.ends_with("skipped: no connection"))
    );
    assert!(!out.stdout.contains("secret-pw"));
}
