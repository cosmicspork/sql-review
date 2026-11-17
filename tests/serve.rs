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
use rand::RngCore;
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
    rand::thread_rng().fill_bytes(&mut b);
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
        rand::thread_rng().fill_bytes(&mut seed);
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
}

/// The JSON-RPC error message, asserting the call was refused.
fn refused(r: &Value) -> String {
    assert!(r.get("result").is_none(), "expected a refusal, got {r}");
    r["error"]["message"].as_str().unwrap().to_string()
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
