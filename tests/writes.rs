//! Proposed writes end to end, in process, against SQLite: propose, show,
//! apply and review from the CLI, and the rows-moved, not-previewable and
//! read-only-connection cases. A proposal never runs the write: it reads
//! the rows on the read-only connection.

mod common;

use std::{
    collections::BTreeMap,
    io::Cursor,
    path::{Path, PathBuf},
};

use clap::Parser;
use serde_json::Value;
use sql_review::cli::{Cli, Io, load_config, run, run_write};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

struct Fixture {
    dir: tempfile::TempDir,
    db: PathBuf,
    env: Vec<(String, String)>,
}

const WRITE_PASSWORD: &str = "write-password-never-logged";

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = common::apps_db(dir.path(), "apps.sqlite");
        let env = vec![
            ("DB_BACKEND".into(), "sqlite".into()),
            ("DB_SERVICE".into(), db.display().to_string()),
            ("DB_WRITE_USER".into(), "writer".into()),
            ("DB_WRITE_PASSWORD".into(), WRITE_PASSWORD.into()),
            (
                "SQL_REVIEW_STATE".into(),
                dir.path().join("state").display().to_string(),
            ),
        ];
        Self { dir, db, env }
    }

    fn set(&mut self, key: &str, value: &str) {
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.into(), value.into()));
    }

    fn unset(&mut self, key: &str) {
        self.env.retain(|(k, _)| k != key);
    }

    fn env_map(&self) -> BTreeMap<String, String> {
        self.env.iter().cloned().collect()
    }

    fn cwd(&self) -> &Path {
        self.dir.path()
    }

    async fn cli(&self, args: &[&str]) -> Out {
        self.cli_input(args, "").await
    }

    /// Run the CLI with `input` answering `review`'s questions.
    async fn cli_input(&self, args: &[&str], input: &str) -> Out {
        let parsed = Cli::try_parse_from(std::iter::once("sql-review").chain(args.iter().copied()))
            .expect("arguments parse");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let io = Io {
            env: self.env_map(),
            cwd: self.cwd().to_path_buf(),
            stdin: Box::new(|| None),
            out: &mut out,
            err: &mut err,
        };
        let code = match parsed.command {
            Some(command) => run_write(command, io, &mut Cursor::new(input.as_bytes())).await,
            None => run(parsed.query, io).await,
        };
        Out {
            code,
            stdout: String::from_utf8(out).unwrap(),
            stderr: String::from_utf8(err).unwrap(),
        }
    }

    /// Propose to `name.json` in the fixture's directory; the parsed file.
    async fn propose(&self, sql: &str, name: &str) -> Value {
        let out = format!("{name}.json");
        let r = self.cli(&["propose", "--sql", sql, "--out", &out]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        self.read(name)
    }

    fn read(&self, name: &str) -> Value {
        let text = std::fs::read_to_string(self.cwd().join(format!("{name}.json"))).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn scores(&self) -> Vec<(i64, i64)> {
        let conn = rusqlite::Connection::open(&self.db).unwrap();
        let mut stmt = conn
            .prepare("SELECT app_id, score FROM apps ORDER BY app_id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn columns(&self) -> usize {
        rusqlite::Connection::open(&self.db)
            .unwrap()
            .query_row("SELECT count(*) FROM pragma_table_info('apps')", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap() as usize
    }

    fn execute(&self, sql: &str) {
        rusqlite::Connection::open(&self.db)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.cwd().join("state/log.jsonl")).unwrap_or_default()
    }
}

#[tokio::test]
async fn an_update_previews_its_rows_and_applies() {
    let f = Fixture::new();
    let r = f
        .cli(&[
            "propose",
            "--sql",
            "UPDATE apps SET score = score + 1 WHERE strm = :strm;",
            "--param",
            "strm=1261",
            "--out",
            "bump.json",
        ])
        .await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("2 row(s) would change"), "{}", r.stdout);
    assert!(r.stdout.contains("rows now:"), "{}", r.stdout);
    assert!(!r.stdout.contains("after"), "{}", r.stdout);
    // Read, never run: nothing changed.
    assert_eq!(f.scores(), [(1, 10), (2, 30), (3, 20)]);

    let p = f.read("bump");
    assert_eq!(p["sql_review"], 2);
    assert_eq!(
        p["statement"],
        "UPDATE apps SET score = score + 1 WHERE strm = :strm"
    );
    assert_eq!(p["params"]["strm"], "1261");
    assert_eq!(
        p["target"],
        format!("sqlite:///{}", f.db.canonicalize().unwrap().display())
    );
    let pv = &p["preview"];
    assert_eq!(pv["kind"], "rows");
    assert_eq!(pv["row_count"], 2);
    assert_eq!(
        pv["affected_query"],
        "SELECT * FROM apps WHERE strm = :strm"
    );
    assert_eq!(pv["rows_hash"].as_str().unwrap().len(), 64);
    assert_eq!(pv["sample"]["columns"][2], "score");
    let mut now: Vec<i64> = pv["sample"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[2].as_i64().unwrap())
        .collect();
    now.sort();
    assert_eq!(now, [10, 30]);
    assert!(pv.get("after").is_none() && pv.get("before").is_none());

    let shown = f.cli(&["show", "bump.json"]).await;
    assert_eq!(shown.code, 0, "{}", shown.stderr);
    assert!(
        shown.stdout.contains(":strm = \"1261\""),
        "{}",
        shown.stdout
    );

    let r = f.cli(&["apply", "bump.json"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.contains("approved, 2 row(s) changed"),
        "{}",
        r.stdout
    );
    assert_eq!(f.scores(), [(1, 11), (2, 31), (3, 20)]);
    assert_eq!(f.read("bump")["outcome"]["status"], "approved");

    let again = f.cli(&["apply", "bump.json"]).await;
    assert_eq!(again.code, 2);
    assert!(again.stderr.contains("already decided"), "{}", again.stderr);
    assert_eq!(f.scores(), [(1, 11), (2, 31), (3, 20)]);

    let log = f.log();
    let events: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(events[0]["event"], "proposed");
    assert_eq!(events[0]["row_count"], 2);
    assert_eq!(events[0]["rows_hash"], pv["rows_hash"]);
    assert_eq!(events[1]["event"], "approved");
    assert_eq!(events[1]["statement"], p["statement"]);
    assert!(
        !log.contains(WRITE_PASSWORD) && !log.contains("writer"),
        "{log}"
    );
}

#[tokio::test]
async fn a_change_to_the_rows_after_propose_makes_apply_refuse_as_stale() {
    let f = Fixture::new();
    f.propose("UPDATE apps SET score = 0 WHERE strm = '1261'", "zero")
        .await;
    // Someone else edits one of the rows the proposal covers.
    f.execute("UPDATE apps SET email = 'new@y.net' WHERE app_id = 2");
    let r = f.cli(&["apply", "zero.json"]).await;
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert!(
        r.stderr.contains("moved since it was proposed"),
        "{}",
        r.stderr
    );
    assert_eq!(f.scores(), [(1, 10), (2, 30), (3, 20)]);
    // Nothing was committed, so the file is left for another look.
    assert!(f.read("zero").get("outcome").is_none());
    assert!(f.log().contains("\"event\":\"stale\""));

    // A new row the WHERE now selects moves them too.
    f.propose("DELETE FROM apps WHERE strm = '1265'", "drop")
        .await;
    f.execute("INSERT INTO apps VALUES (4, 'd@w.com', 5, '1265')");
    let r = f.cli(&["apply", "drop.json"]).await;
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert_eq!(common::count(&f.db), 4);

    assert!(
        r.stderr.contains("selects 2 row(s) now, 1 then"),
        "{}",
        r.stderr
    );
}

/// The count apply checks is the one read on the read-only account at
/// propose: a proposal file whose count no longer matches the rows is
/// refused, even with the rows' hash intact.
#[tokio::test]
async fn apply_checks_the_count_read_at_propose() {
    let f = Fixture::new();
    let mut p = f
        .propose("UPDATE apps SET score = 0 WHERE strm = '1261'", "zero")
        .await;
    assert_eq!(p["preview"]["row_count"], 2);
    p["preview"]["row_count"] = 3.into();
    std::fs::write(f.cwd().join("zero.json"), p.to_string()).unwrap();
    let r = f.cli(&["apply", "zero.json"]).await;
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert!(
        r.stderr.contains("selects 2 row(s) now, 3 then"),
        "{}",
        r.stderr
    );
    assert_eq!(f.scores(), [(1, 10), (2, 30), (3, 20)]);
}

/// A proposal made by 0.2, which ran the statement to preview it, is not
/// applied by this build.
#[tokio::test]
async fn a_proposal_from_the_old_format_is_refused() {
    let f = Fixture::new();
    let mut p = f.propose("DELETE FROM apps WHERE app_id = 1", "old").await;
    p["sql_review"] = 1.into();
    std::fs::write(f.cwd().join("old.json"), p.to_string()).unwrap();
    let r = f.cli(&["apply", "old.json"]).await;
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("propose it again"), "{}", r.stderr);
    assert_eq!(common::count(&f.db), 3);
}

#[tokio::test]
async fn alter_is_not_previewable_and_applies_only_when_applied() {
    let f = Fixture::new();
    let p = f
        .propose("ALTER TABLE apps ADD COLUMN note TEXT", "alter")
        .await;
    let pv = &p["preview"];
    assert_eq!(pv["kind"], "not_previewable");
    assert!(pv["note"].as_str().unwrap().contains("DDL"), "{pv}");
    assert!(pv.get("row_count").is_none() && pv.get("rows_hash").is_none());
    assert_eq!(f.columns(), 4, "never run at propose");
    let shown = f.cli(&["show", "alter.json"]).await;
    assert!(shown.stdout.contains("not previewable"), "{}", shown.stdout);

    let r = f.cli(&["apply", "alter.json"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(f.columns(), 5);
}

#[tokio::test]
async fn a_write_sent_through_query_fails_on_the_read_only_connection() {
    let f = Fixture::new();
    // Through the classifier, it is refused before it connects.
    let r = f.cli(&["--sql", "DELETE FROM apps", "--quiet"]).await;
    assert_eq!(r.code, 2);
    assert!(
        r.stderr.contains("propose a write for review instead"),
        "{}",
        r.stderr
    );
    // Past the classifier, straight to the read path, the read-only
    // connection refuses it, write account configured or not.
    let cfg = load_config(f.env_map(), f.cwd(), None, None, false).unwrap();
    assert!(cfg.write.is_some());
    let e = sql_review::backend::read(&cfg, "DELETE FROM apps", &Default::default(), 0)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("readonly"), "{e}");
    assert_eq!(common::count(&f.db), 3);
}

#[tokio::test]
async fn other_dml_is_not_previewable_and_runs_as_written() {
    let f = Fixture::new();
    for (sql, says) in [
        (
            "UPDATE apps SET score = 0 WHERE app_id IN (SELECT app_id FROM apps WHERE score > 15)",
            "subquery",
        ),
        (
            "INSERT INTO apps SELECT * FROM apps WHERE strm = '1261'",
            "INSERT",
        ),
    ] {
        let p = f.propose(sql, "other").await;
        let pv = &p["preview"];
        assert_eq!(pv["kind"], "not_previewable", "{sql}");
        assert!(pv["note"].as_str().unwrap().contains(says), "{pv}");
        assert!(pv.get("row_count").is_none() && pv.get("sample").is_none());
        let shown = f.cli(&["show", "other.json"]).await;
        assert!(
            shown.stdout.contains("nothing checks what it changes"),
            "{}",
            shown.stdout
        );
    }
    assert_eq!(common::count(&f.db), 3, "never run at propose");
    let r = f.cli(&["apply", "other.json"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(common::count(&f.db), 5);

    let p = f
        .propose("DELETE FROM apps WHERE strm = '1265'", "del")
        .await;
    assert_eq!(p["preview"]["kind"], "rows");
    assert_eq!(p["preview"]["sample"]["rows"][0][0], 3);
}

#[tokio::test]
async fn reading_the_rows_has_a_time_limit() {
    let mut f = Fixture::new();
    f.set("DB_PREVIEW_TIMEOUT", "0.3");
    let cfg = load_config(f.env_map(), f.cwd(), None, None, false).unwrap();
    let slow = "SELECT * FROM apps WHERE (WITH RECURSIVE c(n) AS \
                (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 200000000) \
                SELECT count(*) FROM c) > 0";
    let started = std::time::Instant::now();
    let read = sql_review::backend::read_rows(&cfg, slow, &Default::default(), cfg.preview_timeout)
        .await
        .unwrap();
    assert!(read.is_none(), "interrupted");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[tokio::test]
async fn propose_needs_only_the_read_account() {
    let mut f = Fixture::new();
    let r = f.cli(&["propose", "--sql", "SELECT 1"]).await;
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("only reads"), "{}", r.stderr);
    let r = f
        .cli(&["propose", "--sql", "DELETE FROM apps; DELETE FROM apps"])
        .await;
    assert!(r.stderr.contains("one at a time"), "{}", r.stderr);

    // No write account, and a database file nobody may write: propose
    // still reads the rows, because it never opens the file for writing.
    f.unset("DB_WRITE_USER");
    let mut perms = std::fs::metadata(&f.db).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o444);
    std::fs::set_permissions(&f.db, perms.clone()).unwrap();
    let p = f
        .propose("DELETE FROM apps WHERE strm = '1261'", "del")
        .await;
    assert_eq!(p["preview"]["row_count"], 2);
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o644);
    std::fs::set_permissions(&f.db, perms).unwrap();

    let r = f.cli(&["apply", "del.json"]).await;
    assert_eq!(r.code, 2);
    assert!(
        r.stderr
            .contains("no write account: set DB_WRITE_USER and DB_WRITE_PASSWORD"),
        "{}",
        r.stderr
    );
    assert_eq!(common::count(&f.db), 3);
}

#[tokio::test]
async fn a_proposal_for_another_database_is_refused() {
    let mut f = Fixture::new();
    f.propose("DELETE FROM apps WHERE app_id = 1", "one").await;
    let other = common::apps_db(f.cwd(), "other.sqlite");
    f.set("DB_SERVICE", &other.display().to_string());
    let r = f.cli(&["apply", "one.json"]).await;
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("was proposed against"), "{}", r.stderr);
    assert_eq!(common::count(&other), 3);
}

/// What `sql-review mcp` does with a `write`: a pending proposal in the
/// store, decided here as `review` would decide it.
async fn put_pending(f: &Fixture, sql: &str) -> String {
    let cfg = load_config(f.env_map(), f.cwd(), None, None, false).unwrap();
    let p = sql_review::change::propose(&cfg, sql, &Default::default())
        .await
        .unwrap();
    let store = sql_review::store::Store::open(f.cwd().join("state")).unwrap();
    store.put(&p).unwrap();
    p.id
}

#[tokio::test]
async fn review_approves_rejects_and_refuses_stale() {
    let f = Fixture::new();
    let a = put_pending(&f, "UPDATE apps SET score = 1 WHERE app_id = 1").await;
    let b = put_pending(&f, "DELETE FROM apps WHERE app_id = 3").await;
    let c = put_pending(&f, "UPDATE apps SET score = 2 WHERE app_id = 2").await;

    let r = f.cli(&["review", "--list"]).await;
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{}", r.stdout);
    assert!(lines[0].starts_with(&a) && lines[0].contains("1 row(s)"));

    let r = f.cli(&["review", "--approve", &a]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(f.scores()[0], (1, 1));
    let r = f.cli(&["review", "--approve", &a]).await;
    assert!(r.stderr.contains("already decided"), "{}", r.stderr);

    let r = f
        .cli(&["review", "--reject", &b, "--reason", "keep it"])
        .await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("rejected: keep it"), "{}", r.stdout);
    assert_eq!(common::count(&f.db), 3);

    f.execute("UPDATE apps SET score = 99 WHERE app_id = 2");
    let r = f.cli(&["review", "--approve", &c]).await;
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert!(r.stdout.contains("stale"), "{}", r.stdout);
    assert_eq!(f.scores()[1], (2, 99));

    let shown = f.cli(&["show", &c]).await;
    assert!(
        shown.stdout.contains(&format!("{c}: stale")),
        "{}",
        shown.stdout
    );
    assert_eq!(
        f.cli(&["review", "--list"]).await.stdout,
        "nothing is waiting for review\n"
    );
}

#[tokio::test]
async fn review_asks_about_each_in_a_terminal() {
    let f = Fixture::new();
    let a = put_pending(&f, "UPDATE apps SET score = 1 WHERE app_id = 1").await;
    let b = put_pending(&f, "UPDATE apps SET score = 2 WHERE app_id = 2").await;
    let c = put_pending(&f, "UPDATE apps SET score = 3 WHERE app_id = 3").await;
    let r = f.cli_input(&["review"], "s\nx\nr\nnot now\na\n").await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("[1/3]") && r.stdout.contains("a, r, s or q"));
    assert!(
        r.stdout.contains(&format!("{b}: rejected: not now")),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains(&format!("{c}: approved, 1 row(s) changed"))
    );
    assert_eq!(f.scores(), [(1, 10), (2, 30), (3, 3)]);
    let r = f.cli(&["review", "--list"]).await;
    assert!(r.stdout.starts_with(&a), "{}", r.stdout);
}
