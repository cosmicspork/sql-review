//! `sql-review mcp` over stdio, as an agent client would drive it.

mod common;

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use serde_json::{Value, json};

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn start(env: &[(&str, String)], cwd: &std::path::Path) -> Self {
        Self::start_with(&[], env, cwd)
    }

    fn start_with(args: &[&str], env: &[(&str, String)], cwd: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sql-review"))
            .arg("mcp")
            .args(args)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut s = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let init = s.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"},
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "sql-review", "{init}");
        s.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        s
    }

    fn send(&mut self, msg: Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "server closed stdout"
            );
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg["id"] == json!(id) {
                return msg;
            }
        }
    }

    /// Call a tool: its text content parsed as JSON, or its error text.
    fn call(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        let r = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        let result = &r["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if result["isError"] == json!(true) {
            Err(text)
        } else {
            Ok(serde_json::from_str(&text).unwrap_or_else(|_| panic!("{r}")))
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db = common::apps_db(dir.path(), "default.sqlite");
    let qa = common::apps_db(dir.path(), "qa.sqlite");
    rusqlite::Connection::open(&qa)
        .unwrap()
        .execute("DELETE FROM apps WHERE app_id > 1", [])
        .unwrap();
    let env = [
        ("DB_BACKEND", "sqlite".to_string()),
        ("DB_SERVICE", db.display().to_string()),
        ("QA_DB_SERVICE", qa.display().to_string()),
        (
            "SQL_REVIEW_STATE",
            dir.path().join("state").display().to_string(),
        ),
    ];
    let server = Server::start(&env, dir.path());
    (dir, db, server)
}

#[test]
fn tools_list_offers_the_profiles() {
    let (_dir, _db, mut s) = setup();
    let r = s.request("tools/list", json!({}));
    let tools = r["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["query", "describe", "write", "proposal_status"]);
    for t in &tools[..3] {
        assert_eq!(
            t["inputSchema"]["properties"]["profile"]["enum"],
            json!(["default", "qa"])
        );
    }
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    assert_eq!(tools[2]["annotations"]["readOnlyHint"], false);
}

#[test]
fn query_describe_and_profiles() {
    let (_dir, _db, mut s) = setup();
    let r = s
        .call(
            "query",
            json!({"sql": "SELECT app_id, email FROM apps WHERE strm = :strm ORDER BY app_id", "params": {"strm": "1261"}}),
        )
        .unwrap();
    assert_eq!(
        r,
        json!({
            "columns": ["app_id", "email"],
            "rows": [{"app_id": 1, "email": "a@x.com"}, {"app_id": 2, "email": "b@y.net"}],
            "row_count": 2,
            "truncated": false,
        })
    );
    let r = s
        .call("query", json!({"sql": "SELECT app_id FROM apps WHERE app_id > :n", "params": {"n": 0}, "limit": 1}))
        .unwrap();
    assert_eq!(r["row_count"], 1);
    assert_eq!(r["truncated"], true);
    let r = s
        .call(
            "query",
            json!({"sql": "SELECT count(*) AS n FROM apps", "profile": "qa"}),
        )
        .unwrap();
    assert_eq!(r["rows"][0]["n"], 1);
    let r = s.call("describe", json!({"table": "apps"})).unwrap();
    assert_eq!(r["row_count"], 4);
    assert_eq!(r["rows"][1]["name"], "email");
}

#[test]
fn a_write_is_an_error_result() {
    let (_dir, db, mut s) = setup();
    let e = s
        .call("query", json!({"sql": "DELETE FROM apps"}))
        .unwrap_err();
    assert!(e.contains("this statement writes (DELETE)"), "{e}");
    assert_eq!(common::count(&db), 3);
}

#[test]
fn an_unknown_profile_is_an_error_result() {
    let (_dir, _db, mut s) = setup();
    let e = s
        .call("query", json!({"sql": "SELECT 1", "profile": "prd"}))
        .unwrap_err();
    assert!(e.contains("no profile \"prd\""), "{e}");
}

fn write_env(dir: &std::path::Path, db: &std::path::Path) -> Vec<(&'static str, String)> {
    vec![
        ("DB_BACKEND", "sqlite".to_string()),
        ("DB_SERVICE", db.display().to_string()),
        ("DB_WRITE_USER", "writer".to_string()),
        ("DB_WRITE_PASSWORD", "w".to_string()),
        ("SQL_REVIEW_STATE", dir.join("state").display().to_string()),
    ]
}

/// `sql-review review` run beside the server, against the same state.
fn review(env: &[(&str, String)], cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sql-review"))
        .arg("review")
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn a_write_waits_for_review_in_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let db = common::apps_db(dir.path(), "apps.sqlite");
    let env = write_env(dir.path(), &db);
    let mut s = Server::start(&env, dir.path());

    let r = s
        .call(
            "write",
            json!({"sql": "DELETE FROM apps WHERE strm = :s", "params": {"s": "1261"}}),
        )
        .unwrap();
    assert_eq!(r["status"], "pending");
    assert_eq!(r["row_count"], 2);
    assert_eq!(r["previewable"], true);
    assert!(r["preview"].as_str().unwrap().contains("before"), "{r}");
    let id = r["proposal_id"].as_str().unwrap().to_string();
    assert_eq!(common::count(&db), 3, "only proposed");

    let pending = s
        .call("proposal_status", json!({"id": id, "wait_secs": 0}))
        .unwrap();
    assert_eq!(pending["status"], "pending");

    let out = review(&env, dir.path(), &["--approve", &id]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let done = s
        .call("proposal_status", json!({"id": id, "wait_secs": 5}))
        .unwrap();
    assert_eq!(done["status"], "approved");
    assert_eq!(done["row_count"], 2);
    assert_eq!(common::count(&db), 1);

    let r = s
        .call(
            "write",
            json!({"sql": "ALTER TABLE apps ADD COLUMN x INTEGER"}),
        )
        .unwrap();
    assert_eq!(r["previewable"], false);
    let id = r["proposal_id"].as_str().unwrap().to_string();
    let out = review(
        &env,
        dir.path(),
        &["--reject", &id, "--reason", "no DDL today"],
    );
    assert!(out.status.success());
    let done = s.call("proposal_status", json!({"id": id})).unwrap();
    assert_eq!(done["status"], "rejected");
    assert_eq!(done["reason"], "no DDL today");

    let e = s.call("write", json!({"sql": "SELECT 1"})).unwrap_err();
    assert!(e.contains("only reads"), "{e}");
}

#[test]
fn read_only_offers_no_write_tools() {
    let dir = tempfile::tempdir().unwrap();
    let db = common::apps_db(dir.path(), "apps.sqlite");
    let mut env = write_env(dir.path(), &db);
    // No state directory is needed, or made, under --read-only.
    env.retain(|(k, _)| *k != "SQL_REVIEW_STATE");
    let mut s = Server::start_with(&["--read-only"], &env, dir.path());
    let r = s.request("tools/list", json!({}));
    let names: Vec<&str> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["query", "describe"]);
    let r = s.request(
        "tools/call",
        json!({"name": "write", "arguments": {"sql": "DELETE FROM apps"}}),
    );
    assert!(r.get("error").is_some(), "{r}");
    let e = s
        .call("query", json!({"sql": "DELETE FROM apps"}))
        .unwrap_err();
    assert!(e.contains("writes are disabled (--read-only)"), "{e}");
    assert_eq!(common::count(&db), 3);
    assert!(!dir.path().join("state").exists());
}
