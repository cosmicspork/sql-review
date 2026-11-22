//! `sql-review serve` over a Unix socket, with signed caller contexts and
//! lent credentials, as Tracon's node drives it.

mod common;

use std::{
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ed25519_dalek::{Signer, SigningKey};
use http_body_util::{BodyExt, Full};
use hyper::{Request, body::Bytes};
use hyper_util::rt::TokioIo;
use rand::Rng;
use serde_json::{Value, json};
use sql_review::contract::{arguments_sha256, sha256_hex, signing_bytes};

const TOKEN: &str = "this-spawn's-token";

struct Plugin {
    child: Child,
    socket: PathBuf,
    session: Option<String>,
    key: SigningKey,
    next_id: u64,
    _dir: tempfile::TempDir,
    db: PathBuf,
    qa: PathBuf,
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn nonce() -> String {
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}

impl Plugin {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = common::apps_db(dir.path(), "default.sqlite");
        let qa = common::apps_db(dir.path(), "qa.sqlite");
        rusqlite::Connection::open(&qa)
            .unwrap()
            .execute("DELETE FROM apps WHERE app_id > 1", [])
            .unwrap();
        // Settings in the environment and a .env file, which serve must ignore.
        std::fs::write(
            dir.path().join(".env"),
            format!("DB_BACKEND=sqlite\nDB_SERVICE={}\n", db.display()),
        )
        .unwrap();
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        let key = SigningKey::from_bytes(&seed);
        let socket = dir.path().join("s.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_sql-review"))
            .args(["serve", "--socket"])
            .arg(&socket)
            .arg("--data")
            .arg(dir.path().join("d"))
            .env_clear()
            .env("TRACON_PLUGIN_TOKEN", TOKEN)
            .env(
                "TRACON_NODE_KEY",
                hex::encode(key.verifying_key().to_bytes()),
            )
            .env("TRACON_CONTRACT", "1")
            .env("DB_BACKEND", "sqlite")
            .env("DB_SERVICE", &db)
            .current_dir(dir.path())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "no socket within 30 s");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut p = Self {
            child,
            socket,
            session: None,
            key,
            next_id: 1,
            _dir: dir,
            db,
            qa,
        };
        let init = p
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "tracon", "version": "0"},
                }),
            )
            .await;
        assert_eq!(init["result"]["serverInfo"]["name"], "sql-review", "{init}");
        p.post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        p
    }

    /// POST one message; returns the JSON-RPC messages answered, whether
    /// the body was JSON or an event stream.
    async fn post(&mut self, msg: Value) -> Vec<Value> {
        let stream = tokio::net::UnixStream::connect(&self.socket).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(conn);
        let mut req = Request::post("/")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(s) = &self.session {
            req = req.header("mcp-session-id", s);
        }
        let resp = sender
            .send_request(req.body(Full::new(Bytes::from(msg.to_string()))).unwrap())
            .await
            .unwrap();
        if let Some(s) = resp.headers().get("mcp-session-id") {
            self.session = Some(s.to_str().unwrap().to_string());
        }
        let status = resp.status();
        let sse = resp
            .headers()
            .get("content-type")
            .is_some_and(|c| c.to_str().unwrap().starts_with("text/event-stream"));
        let want_id = msg.get("id").cloned();
        let mut body = resp.into_body();
        let mut text = String::new();
        let mut out = Vec::new();
        while let Some(frame) = tokio::time::timeout(Duration::from_secs(10), body.frame())
            .await
            .expect("a response within 10 s")
        {
            if let Ok(data) = frame.unwrap().into_data() {
                text.push_str(std::str::from_utf8(&data).unwrap());
            }
            if sse {
                while let Some(end) = text.find("\n\n") {
                    let event: String = text.drain(..end + 2).collect();
                    let data: String = event
                        .lines()
                        .filter_map(|l| l.strip_prefix("data:"))
                        .map(str::trim_start)
                        .collect();
                    if !data.is_empty() {
                        out.push(serde_json::from_str::<Value>(&data).unwrap());
                    }
                }
                if want_id.is_some() && out.iter().any(|m| m.get("id") == want_id.as_ref()) {
                    break;
                }
            }
        }
        if !sse && !text.trim().is_empty() {
            out.push(serde_json::from_str(&text).unwrap_or_else(|_| panic!("{status}: {text}")));
        }
        out
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let msgs = self
            .post(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        msgs.into_iter()
            .find(|m| m["id"] == json!(id))
            .expect("an answer to the request")
    }

    /// A context for a call, signed with this node's key unless `tamper`
    /// changes it after signing.
    fn context(&self, tool: &str, args: &Value, credentials: Value) -> Value {
        let now = now_ms();
        let mut ctx = json!({
            "v": 1,
            "plugin": "sql-review",
            "instance": sha256_hex(TOKEN.as_bytes()),
            "tool": tool,
            "arguments_sha256": arguments_sha256(args.as_object()).unwrap(),
            "caller": {"kind": "agent", "session": "sess-1", "harness": "claude"},
            "channel": "personal",
            "approval_id": null,
            "issued_ms": now,
            "expires_ms": now + 30_000,
            "nonce": nonce(),
            "credentials": credentials,
        });
        self.sign(&mut ctx);
        ctx
    }

    fn sign(&self, ctx: &mut Value) {
        let sig = self.key.sign(&signing_bytes(ctx).unwrap());
        ctx["signature"] = json!(hex::encode(sig.to_bytes()));
    }

    async fn call_with(&mut self, tool: &str, args: Value, ctx: Option<Value>) -> Value {
        let mut params = json!({"name": tool, "arguments": args});
        if let Some(ctx) = ctx {
            params["_meta"] = json!({"tracon": ctx});
        }
        self.request("tools/call", params).await
    }

    fn read_cred(&self, db: &Path) -> Value {
        json!({"DB_BACKEND": "sqlite", "DB_SERVICE": db.display().to_string()})
    }

    /// A context as the node signs it for a call dispatched from an
    /// approved card: an approval id, and the preview's token if it had one.
    fn approved(&self, tool: &str, args: &Value, credentials: Value, token: &Value) -> Value {
        let mut ctx = self.context(tool, args, credentials);
        ctx["approval_id"] = json!("approval-1");
        if !token.is_null() {
            ctx["preview_token"] = token.clone();
        }
        self.sign(&mut ctx);
        ctx
    }

    /// Both accounts, as the node lends them to an approved write. SQLite
    /// has no logins: the write account's user only opens it read-write.
    fn write_creds(&self) -> Value {
        json!({"sql-read": self.read_cred(&self.db), "sql-write": {"DB_USER": "writer"}})
    }

    /// `tracon/preview` of `tool`, with a context signed for it.
    async fn preview(&mut self, tool: &str, args: Value, credentials: Value) -> Value {
        let ctx = self.context(tool, &args, credentials);
        self.preview_with(tool, args, Some(ctx)).await
    }

    async fn preview_with(&mut self, tool: &str, args: Value, ctx: Option<Value>) -> Value {
        let mut params = json!({"name": tool, "arguments": args});
        if let Some(ctx) = ctx {
            params["_meta"] = json!({"tracon": ctx});
        }
        self.request("tracon/preview", params).await
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

    fn execute(&self, sql: &str) {
        rusqlite::Connection::open(&self.db)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    fn data(&self) -> PathBuf {
        self._dir.path().join("d")
    }
}

/// The JSON-RPC error message, asserting the call was refused.
fn refused(r: &Value) -> String {
    assert!(r.get("result").is_none(), "expected a refusal, got {r}");
    r["error"]["message"].as_str().unwrap().to_string()
}

/// A JSON-RPC result, asserting there was one.
fn result(r: &Value) -> &Value {
    assert!(r.get("error").is_none(), "expected a result, got {r}");
    &r["result"]
}

fn rows(r: &Value) -> Value {
    assert_ne!(r["result"]["isError"], json!(true), "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn tools_list_matches_the_manifest() {
    let mut p = Plugin::start().await;
    let r = p.request("tools/list", json!({})).await;
    let listed: Vec<(String, bool)> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["annotations"]["readOnlyHint"] == json!(true),
            )
        })
        .collect();
    let manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string("tracon-plugin.toml").unwrap()).unwrap();
    let declared: Vec<(String, bool)> = manifest["tool"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["hint"].as_str() == Some("read"),
            )
        })
        .collect();
    assert_eq!(listed, declared);
    assert_eq!(manifest["name"].as_str(), Some(sql_review::PLUGIN_NAME));
    assert_eq!(manifest["version"].as_str(), Some(sql_review::VERSION));
    assert_eq!(
        manifest["exec"],
        toml::Value::Array(vec!["bin/sql-review".into(), "serve".into()])
    );
    // The profile is a free string here: the node knows the channel's.
    let profile = &r["result"]["tools"][0]["inputSchema"]["properties"]["profile"];
    assert_eq!(profile["type"], "string");
    assert!(profile.get("enum").is_none());

    // Each credential is lent only as the binding the call's argument
    // names, so every tool must take that argument by that name: a tool
    // without it would only ever be lent the bare binding.
    let credentials = manifest["credential"].as_array().unwrap();
    assert_eq!(credentials.len(), 2);
    for c in credentials {
        assert_eq!(c["select"].as_str(), Some("profile"), "{c}");
    }
    for t in r["result"]["tools"].as_array().unwrap() {
        assert_eq!(
            t["inputSchema"]["properties"]["profile"]["type"], "string",
            "{t}"
        );
    }
}

/// What the node lends by `select = "profile"`: one call names no profile
/// and is lent `sql-read`, or names one and is lent `sql-read-<profile>`.
/// sql-review reads with exactly that binding, never another lent one.
#[tokio::test]
async fn a_call_reads_with_the_binding_its_profile_names() {
    let mut p = Plugin::start().await;
    let n = |r: &Value| rows(r)["rows"][0]["n"].clone();
    let sql = "SELECT count(*) AS n FROM apps";

    // No profile, or an empty one, is the default: sql-read.
    for args in [
        json!({"sql": sql}),
        json!({"sql": sql, "profile": null}),
        json!({"sql": sql, "profile": ""}),
    ] {
        let ctx = p.context("query", &args, json!({"sql-read": p.read_cred(&p.db)}));
        let r = p.call_with("query", args, Some(ctx)).await;
        assert_eq!(n(&r), 3, "{r}");
    }

    // No profile, with only another profile's binding lent (which the node
    // never does for such a call): no falling back to it.
    let args = json!({"sql": sql});
    let ctx = p.context("query", &args, json!({"sql-read-qa": p.read_cred(&p.qa)}));
    let r = p.call_with("query", args, Some(ctx)).await;
    assert_eq!(
        r["result"]["content"][0]["text"], "no sql-read credential was lent to this call",
        "{r}"
    );

    // `default` names `sql-read-default`, not `sql-read`: refused rather
    // than read with a binding the node did not select.
    let args = json!({"sql": sql, "profile": "default"});
    let creds = json!({"sql-read-default": p.read_cred(&p.qa), "sql-read": p.read_cred(&p.db)});
    let ctx = p.context("query", &args, creds);
    let r = p.call_with("query", args, Some(ctx)).await;
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("leave profile out"),
        "{r}"
    );

    // An approved write with no profile applies on sql-write.
    let args = json!({"sql": "DELETE FROM apps WHERE app_id = 1", "profile": ""});
    let ctx = p.approved("write", &args, p.write_creds(), &Value::Null);
    let r = p.call_with("write", args, Some(ctx)).await;
    assert_eq!(rows(&r)["row_count"], 1, "{r}");
    assert_eq!(common::count(&p.db), 2);
}

#[tokio::test]
async fn reads_with_lent_credentials() {
    let mut p = Plugin::start().await;
    let creds = json!({"sql-read": p.read_cred(&p.db), "sql-read-qa": p.read_cred(&p.qa)});
    let args = json!({"sql": "SELECT count(*) AS n FROM apps"});
    let ctx = p.context("query", &args, creds.clone());
    let r = p.call_with("query", args, Some(ctx)).await;
    assert_eq!(rows(&r)["rows"], json!([{"n": 3}]));

    let args = json!({"sql": "SELECT count(*) AS n FROM apps", "profile": "qa"});
    let ctx = p.context("query", &args, creds.clone());
    let r = p.call_with("query", args, Some(ctx)).await;
    assert_eq!(rows(&r)["rows"], json!([{"n": 1}]));

    let args = json!({"table": "apps"});
    let ctx = p.context("describe", &args, creds.clone());
    let r = p.call_with("describe", args, Some(ctx)).await;
    assert_eq!(rows(&r)["row_count"], 4);

    // A write is a tool error, not a refusal: the call itself was sound.
    let args = json!({"sql": "DELETE FROM apps"});
    let ctx = p.context("query", &args, creds);
    let r = p.call_with("query", args, Some(ctx)).await;
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert_eq!(common::count(&p.db), 3);
}

#[tokio::test]
async fn a_credential_not_lent_cannot_be_used() {
    let mut p = Plugin::start().await;
    let args = json!({"sql": "SELECT 1 AS n", "profile": "qa"});
    let ctx = p.context("query", &args, json!({"sql-read": p.read_cred(&p.db)}));
    let r = p.call_with("query", args, Some(ctx)).await;
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert_eq!(
        r["result"]["content"][0]["text"],
        "credential sql-read-qa was not lent to this call"
    );
}

#[tokio::test]
async fn the_process_environment_is_never_read() {
    // DB_* in the environment and a .env in the working directory point at
    // a real database; with nothing lent, the call has no settings at all.
    let mut p = Plugin::start().await;
    let args = json!({"sql": "SELECT 1 AS n"});
    let ctx = p.context("query", &args, json!({}));
    let r = p.call_with("query", args, Some(ctx)).await;
    assert_eq!(
        r["result"]["content"][0]["text"], "no sql-read credential was lent to this call",
        "{r}"
    );
    // sql-write is never read either.
    let args = json!({"sql": "SELECT 1 AS n"});
    let ctx = p.context("query", &args, json!({"sql-write": p.read_cred(&p.db)}));
    let r = p.call_with("query", args, Some(ctx)).await;
    assert_eq!(r["result"]["isError"], true, "{r}");
}

#[tokio::test]
async fn bad_contexts_are_refused() {
    let mut p = Plugin::start().await;
    let creds = json!({"sql-read": p.read_cred(&p.db)});
    let args = json!({"sql": "SELECT 1 AS n"});

    let r = p.call_with("query", args.clone(), None).await;
    assert_eq!(refused(&r), "this call carries no Tracon caller context");

    // Each case: (what, check that refuses, how to spoil a signed context).
    type Spoil = Box<dyn Fn(&Plugin, &mut Value)>;
    let cases: Vec<(&str, u8, Spoil)> = vec![
        (
            "wrong version",
            1,
            Box::new(|p, c| {
                c["v"] = json!(2);
                p.sign(c)
            }),
        ),
        (
            "wrong plugin",
            1,
            Box::new(|p, c| {
                c["plugin"] = json!("jira-review");
                p.sign(c)
            }),
        ),
        (
            "stale instance token",
            2,
            Box::new(|p, c| {
                c["instance"] = json!(sha256_hex(b"previous spawn's token"));
                p.sign(c)
            }),
        ),
        (
            "wrong tool",
            3,
            Box::new(|p, c| {
                c["tool"] = json!("describe");
                p.sign(c)
            }),
        ),
        (
            "tampered arguments",
            3,
            Box::new(|p, c| {
                c["arguments_sha256"] =
                    json!(arguments_sha256(json!({"sql": "SELECT 2"}).as_object()).unwrap());
                p.sign(c)
            }),
        ),
        (
            "bad signature",
            4,
            Box::new(|_, c| c["channel"] = json!("other")),
        ),
        (
            "signed by another key",
            4,
            Box::new(|_, c| {
                let other = SigningKey::from_bytes(&[9; 32]);
                c["signature"] = json!(hex::encode(
                    other.sign(&signing_bytes(c).unwrap()).to_bytes()
                ));
            }),
        ),
        (
            "expired",
            5,
            Box::new(|p, c| {
                let t = now_ms() - 60_000;
                c["issued_ms"] = json!(t);
                c["expires_ms"] = json!(t + 30_000);
                p.sign(c)
            }),
        ),
        (
            "too long a lifetime",
            5,
            Box::new(|p, c| {
                c["expires_ms"] = json!(c["issued_ms"].as_i64().unwrap() + 120_000);
                p.sign(c)
            }),
        ),
        (
            "issued in the future",
            5,
            Box::new(|p, c| {
                let t = now_ms() + 30_000;
                c["issued_ms"] = json!(t);
                c["expires_ms"] = json!(t + 30_000);
                p.sign(c)
            }),
        ),
        (
            "malformed nonce",
            6,
            Box::new(|p, c| {
                c["nonce"] = json!("NOT-HEX");
                p.sign(c)
            }),
        ),
    ];
    for (what, check, spoil) in cases {
        let mut ctx = p.context("query", &args, creds.clone());
        spoil(&p, &mut ctx);
        let r = p.call_with("query", args.clone(), Some(ctx)).await;
        let msg = refused(&r);
        assert_eq!(r["error"]["data"]["check"], json!(check), "{what}: {msg}");
        if check > 0 {
            assert!(
                msg.contains(&format!("check {check} failed")),
                "{what}: {msg}"
            );
        }
    }

    // A replay: the same signed context twice.
    let ctx = p.context("query", &args, creds.clone());
    let r = p.call_with("query", args.clone(), Some(ctx.clone())).await;
    assert_eq!(rows(&r)["rows"], json!([{"n": 1}]));
    let r = p.call_with("query", args.clone(), Some(ctx)).await;
    assert!(refused(&r).contains("already used"), "{r}");

    // A float in the arguments has no canonical form.
    let float_args = json!({"sql": "SELECT :x AS x", "params": {"x": 1.5}});
    let ctx = p.context("query", &json!({}), creds);
    let r = p.call_with("query", float_args, Some(ctx)).await;
    assert!(refused(&r).contains("float"), "{r}");
}

#[tokio::test]
async fn a_signed_field_this_build_does_not_know_is_accepted() {
    let mut p = Plugin::start().await;
    let creds = json!({"sql-read": p.read_cred(&p.db)});
    let args = json!({"sql": "SELECT 1 AS n"});

    let mut ctx = p.context("query", &args, creds.clone());
    ctx["preview_token"] = json!("{\"count\":1,\"hash\":\"ab12\"}");
    p.sign(&mut ctx);
    let r = p.call_with("query", args.clone(), Some(ctx)).await;
    assert_eq!(rows(&r)["rows"], json!([{"n": 1}]));

    let mut ctx = p.context("query", &args, creds);
    ctx["preview_token"] = json!("{\"count\":1,\"hash\":\"ab12\"}");
    p.sign(&mut ctx);
    ctx["preview_token"] = json!("{\"count\":1,\"hash\":\"cd34\"}");
    let r = p.call_with("query", args, Some(ctx)).await;
    assert_eq!(r["error"]["data"]["check"], json!(4), "{r}");
}

#[tokio::test]
async fn an_update_is_previewed_without_running_and_applied_once_approved() {
    let mut p = Plugin::start().await;
    let args = json!({
        "sql": "UPDATE apps SET score = score + 1 WHERE strm = :strm",
        "params": {"strm": "1261"},
    });
    let r = p
        .preview(
            "write",
            args.clone(),
            json!({"sql-read": p.read_cred(&p.db)}),
        )
        .await;
    let pv = result(&r);
    assert_eq!(pv["previewable"], true, "{r}");
    assert_eq!(
        pv["summary"],
        "UPDATE apps SET score = score + 1 WHERE strm = :strm (2 row(s))"
    );
    let body = pv["body"].as_str().unwrap();
    assert!(
        body.starts_with("```sql\nUPDATE apps SET score = score + 1 WHERE strm = :strm\n```\n"),
        "{body}"
    );
    assert!(body.contains("**2 row(s) would change.**"), "{body}");
    assert!(body.contains("| app_id | email | score | strm |"), "{body}");
    assert!(body.contains("| 2 | b@y.net | 30 | 1261 |"), "{body}");
    let token = pv["token"].clone();
    let parsed: Value = serde_json::from_str(token.as_str().unwrap()).unwrap();
    assert_eq!(parsed["count"], 2);
    assert_eq!(parsed["rows_hash"].as_str().unwrap().len(), 64);
    // A preview runs nothing and writes nothing, not even the plugin's log.
    assert_eq!(p.scores(), [(1, 10), (2, 30), (3, 20)]);
    assert!(!p.data().join("log.jsonl").exists());

    let ctx = p.approved("write", &args, p.write_creds(), &token);
    let r = p.call_with("write", args, Some(ctx)).await;
    let done = rows(&r);
    assert_eq!(done["status"], "approved", "{done}");
    assert_eq!(done["row_count"], 2);
    assert_eq!(p.scores(), [(1, 11), (2, 31), (3, 20)]);

    // Recorded in the plugin's data directory, without the credentials.
    let id = done["proposal_id"].as_str().unwrap();
    let kept = std::fs::read_to_string(p.data().join(format!("decided/{id}.json"))).unwrap();
    assert!(kept.contains("\"status\": \"approved\""), "{kept}");
    let log = std::fs::read_to_string(p.data().join("log.jsonl")).unwrap();
    assert!(log.contains("\"event\":\"approved\""), "{log}");
    assert!(!log.contains("writer"), "{log}");
}

#[tokio::test]
async fn a_change_between_preview_and_apply_is_refused_as_stale() {
    let mut p = Plugin::start().await;
    let args = json!({"sql": "UPDATE apps SET score = 0 WHERE strm = '1261'"});
    let r = p
        .preview(
            "write",
            args.clone(),
            json!({"sql-read": p.read_cred(&p.db)}),
        )
        .await;
    let token = result(&r)["token"].clone();
    // Someone else edits a row the card showed.
    p.execute("UPDATE apps SET email = 'new@y.net' WHERE app_id = 2");

    let ctx = p.approved("write", &args, p.write_creds(), &token);
    let r = p.call_with("write", args.clone(), Some(ctx)).await;
    assert_eq!(r["error"]["code"], -32002, "{r}");
    assert_eq!(r["error"]["data"], json!({"stale": true}));
    assert!(refused(&r).contains("moved since it was proposed"), "{r}");
    assert_eq!(p.scores(), [(1, 10), (2, 30), (3, 20)]);

    // A new row the WHERE now selects is stale too, by count.
    let r = p
        .preview(
            "write",
            args.clone(),
            json!({"sql-read": p.read_cred(&p.db)}),
        )
        .await;
    let token = result(&r)["token"].clone();
    p.execute("UPDATE apps SET strm = '1261' WHERE app_id = 3");
    let ctx = p.approved("write", &args, p.write_creds(), &token);
    let r = p.call_with("write", args, Some(ctx)).await;
    assert_eq!(r["error"]["data"]["stale"], true, "{r}");
    assert!(refused(&r).contains("selects 3 row(s) now, 2 then"), "{r}");
    assert_eq!(p.scores(), [(1, 10), (2, 30), (3, 20)]);
}

#[tokio::test]
async fn an_alter_is_not_previewable_and_applies_once_approved() {
    let mut p = Plugin::start().await;
    let columns = |db: &Path| -> i64 {
        rusqlite::Connection::open(db)
            .unwrap()
            .query_row("SELECT count(*) FROM pragma_table_info('apps')", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    let args = json!({"sql": "ALTER TABLE apps ADD COLUMN note TEXT"});
    let r = p
        .preview(
            "write",
            args.clone(),
            json!({"sql-read": p.read_cred(&p.db)}),
        )
        .await;
    let pv = result(&r);
    assert_eq!(pv["previewable"], false, "{r}");
    assert_eq!(pv["token"], Value::Null);
    assert_eq!(
        pv["summary"],
        "ALTER TABLE apps ADD COLUMN note TEXT (not previewable)"
    );
    let body = pv["body"].as_str().unwrap();
    assert!(
        body.contains("ALTER TABLE apps ADD COLUMN note TEXT"),
        "{body}"
    );
    assert!(body.contains("**Not previewable:** ALTER is DDL"), "{body}");
    assert_eq!(columns(&p.db), 4, "never run to preview it");

    let ctx = p.approved("write", &args, p.write_creds(), &Value::Null);
    let r = p.call_with("write", args, Some(ctx)).await;
    assert_eq!(rows(&r)["status"], "approved", "{r}");
    assert_eq!(columns(&p.db), 5);
}

#[tokio::test]
async fn a_write_without_an_approval_is_refused() {
    let mut p = Plugin::start().await;
    let args = json!({"sql": "DELETE FROM apps"});
    // Even with sql-write lent, as the node never would without an approval.
    let ctx = p.context("write", &args, p.write_creds());
    let r = p.call_with("write", args, Some(ctx)).await;
    assert_eq!(r["error"]["data"]["check"], 7, "{r}");
    assert!(refused(&r).contains("check 7 failed"), "{r}");
    assert_eq!(common::count(&p.db), 3);
}

#[tokio::test]
async fn a_preview_never_touches_sql_write() {
    let mut p = Plugin::start().await;
    // A database file nobody may write: a preview that ran the statement,
    // or opened it for writing, would fail.
    let mut perms = std::fs::metadata(&p.db).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o444);
    std::fs::set_permissions(&p.db, perms).unwrap();
    // And an sql-write that can't even be resolved, lent as the node never
    // would lend it to a preview.
    let creds = json!({
        "sql-read": p.read_cred(&p.db),
        "sql-write": {"DB_BACKEND": "nope", "DB_USER": "writer"},
    });
    let args = json!({"sql": "DELETE FROM apps WHERE strm = '1261'"});
    let r = p.preview("write", args.clone(), creds).await;
    let pv = result(&r);
    assert_eq!(pv["previewable"], true, "{r}");
    assert_eq!(common::count(&p.db), 3);

    // With only sql-write lent, a preview has nothing to read with.
    let r = p
        .preview("write", args, json!({"sql-write": p.read_cred(&p.db)}))
        .await;
    assert_eq!(r["error"]["code"], -32003, "{r}");
    assert!(
        refused(&r).contains("no sql-read credential was lent"),
        "{r}"
    );
}

#[tokio::test]
async fn a_write_sent_through_query_still_runs_read_only_and_fails() {
    let mut p = Plugin::start().await;
    // Approved, with sql-write lent and a token: query still reads only.
    let args = json!({"sql": "UPDATE apps SET score = 0"});
    let token = json!(format!(
        "{{\"count\":3,\"rows_hash\":\"{}\"}}",
        "0".repeat(64)
    ));
    let ctx = p.approved("query", &args, p.write_creds(), &token);
    let r = p.call_with("query", args, Some(ctx)).await;
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("this statement writes"),
        "{r}"
    );
    // Past the classifier, the settings a plugin call reads with are the
    // read-only connection's, and it refuses the write.
    let creds: std::collections::BTreeMap<String, String> =
        serde_json::from_value(p.read_cred(&p.db)).unwrap();
    let cfg = sql_review::config::resolve(
        &sql_review::config::CredentialSource(creds),
        sql_review::config::Resolve {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    let e = sql_review::backend::read(&cfg, "UPDATE apps SET score = 0", &Default::default(), 0)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("readonly"), "{e}");
    assert_eq!(p.scores(), [(1, 10), (2, 30), (3, 20)]);
}

#[tokio::test]
async fn reads_preview_as_not_previewable() {
    let mut p = Plugin::start().await;
    let creds = json!({"sql-read": p.read_cred(&p.db)});
    for (tool, args) in [
        ("query", json!({"sql": "SELECT 1 AS n"})),
        ("describe", json!({"table": "apps"})),
    ] {
        let r = p.preview(tool, args, creds.clone()).await;
        let pv = result(&r);
        assert_eq!(pv["previewable"], false, "{r}");
        assert_eq!(pv["token"], Value::Null);
        assert_eq!(pv["summary"], format!("{tool} only reads"));
        assert_eq!(pv["body"].as_str().unwrap().lines().count(), 1);
    }
}

#[tokio::test]
async fn bad_previews_and_tokens_are_refused() {
    let mut p = Plugin::start().await;
    let creds = json!({"sql-read": p.read_cred(&p.db)});
    let args = json!({"sql": "DELETE FROM apps WHERE app_id = 1"});

    // The preview's context goes through the same checks, except 7.
    let r = p.preview_with("write", args.clone(), None).await;
    assert!(refused(&r).contains("no Tracon caller context"), "{r}");
    let mut ctx = p.context("write", &args, creds.clone());
    ctx["channel"] = json!("other");
    let r = p.preview_with("write", args.clone(), Some(ctx)).await;
    assert_eq!(r["error"]["data"]["check"], 4, "{r}");
    let r = p.preview("drop_everything", json!({}), creds.clone()).await;
    assert_eq!(r["error"]["code"], -32602, "{r}");
    let r = p
        .preview("write", json!({"sql": "SELECT 1"}), creds.clone())
        .await;
    assert_eq!(r["error"]["code"], -32003, "{r}");
    assert!(refused(&r).contains("only reads"), "{r}");
    let r = p.request("tracon/nothing", json!({})).await;
    assert_eq!(r["error"]["code"], -32601, "{r}");

    // A token that isn't ours is a malformed call, and nothing runs.
    for token in [
        json!("{\"WRK-1\":\"2026-11-16T09:12:33.000+0000\"}"),
        json!("not json"),
    ] {
        let ctx = p.approved("write", &args, p.write_creds(), &token);
        let r = p.call_with("write", args.clone(), Some(ctx)).await;
        assert_eq!(r["error"]["code"], -32602, "{r}");
    }
    // A token for a statement that has no rows to check is malformed too.
    let alter = json!({"sql": "ALTER TABLE apps ADD COLUMN x TEXT"});
    let token = json!(format!(
        "{{\"count\":0,\"rows_hash\":\"{}\"}}",
        "0".repeat(64)
    ));
    let ctx = p.approved("write", &alter, p.write_creds(), &token);
    let r = p.call_with("write", alter, Some(ctx)).await;
    assert_eq!(r["error"]["code"], -32602, "{r}");
    assert_eq!(common::count(&p.db), 3);
}

#[tokio::test]
async fn an_approved_write_needs_the_write_account_for_the_same_database() {
    let mut p = Plugin::start().await;
    let args = json!({"sql": "DELETE FROM apps WHERE app_id = 1"});
    let text = |r: &Value| {
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let ctx = p.approved(
        "write",
        &args,
        json!({"sql-read": p.read_cred(&p.db)}),
        &Value::Null,
    );
    let r = p.call_with("write", args.clone(), Some(ctx)).await;
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(
        text(&r).contains("credential sql-write was not lent"),
        "{r}"
    );

    let creds = json!({"sql-read": p.read_cred(&p.db), "sql-write": p.read_cred(&p.qa)});
    let ctx = p.approved("write", &args, creds, &Value::Null);
    let r = p.call_with("write", args.clone(), Some(ctx)).await;
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(text(&r).contains("names sqlite://"), "{r}");
    assert_eq!(common::count(&p.db), 3);
    assert_eq!(common::count(&p.qa), 1);

    // A profile's write account is sql-write-<profile>.
    let qa = json!({"sql": "DELETE FROM apps WHERE app_id = 1", "profile": "qa"});
    let creds = json!({"sql-read-qa": p.read_cred(&p.qa), "sql-write-qa": {"DB_USER": "w"}});
    let ctx = p.approved("write", &qa, creds, &Value::Null);
    let r = p.call_with("write", qa, Some(ctx)).await;
    assert_eq!(rows(&r)["row_count"], 1, "{r}");
    assert_eq!(common::count(&p.qa), 0);
    assert_eq!(common::count(&p.db), 3);
}
