//! The caller context wire format v1 against `tests/contract_vectors.json`.
//!
//! The file is in the shape shared with jira-review and Tracon's node: a
//! fixed seed and its public key, one valid context with its signing bytes
//! and signature, and one spoiled context per check with the check that
//! must refuse it. Until jira-review publishes its file, this repository's
//! own is checked in; regenerate it with
//! `cargo test --test contract_vectors -- --ignored`.

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use sql_review::contract::{Verifier, arguments_sha256, sha256_hex, signing_bytes};

const PATH: &str = "tests/contract_vectors.json";

fn key_from(vectors: &Value) -> SigningKey {
    let seed: [u8; 32] = hex::decode(vectors["seed"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    SigningKey::from_bytes(&seed)
}

fn verifier(v: &Value) -> Verifier {
    Verifier::new(
        v["plugin"].as_str().unwrap(),
        v["instance"].as_str().unwrap().into(),
        key_from(v).verifying_key(),
        v["write_tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect(),
    )
}

#[test]
fn vectors_hold() {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(PATH).unwrap()).unwrap();
    let key = key_from(&v);
    assert_eq!(
        v["public_key"].as_str().unwrap(),
        hex::encode(key.verifying_key().to_bytes())
    );
    assert_eq!(
        v["instance"].as_str().unwrap(),
        sha256_hex(v["plugin_token"].as_str().unwrap().as_bytes())
    );
    let now = v["now_ms"].as_i64().unwrap();
    let call = &v["call"];

    // The valid context: its bytes and signature reproduce, and it passes.
    let ctx = &v["valid"]["context"];
    let bytes = signing_bytes(ctx).unwrap();
    assert_eq!(
        hex::encode(&bytes),
        v["valid"]["signing_bytes"].as_str().unwrap()
    );
    let sig = hex::encode(key.sign(&bytes).to_bytes());
    assert_eq!(sig, v["valid"]["signature"].as_str().unwrap());
    assert_eq!(ctx["signature"].as_str().unwrap(), sig);
    assert_eq!(
        ctx["arguments_sha256"].as_str().unwrap(),
        arguments_sha256(call["arguments"].as_object()).unwrap()
    );
    verifier(&v)
        .verify(
            ctx,
            call["tool"].as_str().unwrap(),
            call["arguments"].as_object(),
            now,
        )
        .expect("the valid context passes");

    // Each spoiled context fails at its check, on a fresh verifier.
    let tampered = v["tampered"].as_array().unwrap();
    let mut checks: Vec<u64> = tampered
        .iter()
        .map(|t| t["check"].as_u64().unwrap())
        .collect();
    checks.sort();
    assert_eq!(checks, (1..=7).collect::<Vec<_>>(), "one vector per check");
    for t in tampered {
        let verifier = verifier(&v);
        let call = t.get("call").unwrap_or(call);
        let (tool, args) = (
            call["tool"].as_str().unwrap(),
            call["arguments"].as_object(),
        );
        if t["replay"] == json!(true) {
            verifier.verify(&t["context"], tool, args, now).unwrap();
        }
        let err = verifier
            .verify(&t["context"], tool, args, now)
            .expect_err(t["name"].as_str().unwrap());
        assert_eq!(
            u64::from(err.check()),
            t["check"].as_u64().unwrap(),
            "{}: {err}",
            t["name"]
        );
    }
}

/// Writes the vectors file. Signing is deterministic, so this reproduces it
/// byte for byte.
#[test]
#[ignore = "regenerates tests/contract_vectors.json"]
fn generate() {
    let seed = [0x5au8; 32];
    let key = SigningKey::from_bytes(&seed);
    let token = "plugin-token-for-vectors";
    let now: i64 = 1_794_900_000_000;
    let arguments = json!({"sql": "SELECT app_id FROM apps WHERE strm = :strm", "params": {"strm": "1261"}, "limit": 10});
    let sign = |ctx: &mut Value| {
        let sig = key.sign(&signing_bytes(ctx).unwrap());
        ctx["signature"] = json!(hex::encode(sig.to_bytes()));
    };
    let mut valid = json!({
        "v": 1,
        "plugin": "sql-review",
        "instance": sha256_hex(token.as_bytes()),
        "tool": "query",
        "arguments_sha256": arguments_sha256(arguments.as_object()).unwrap(),
        "caller": {"kind": "agent", "session": "01a1e94d-1013-7fc2-9c04-292e776c2161", "harness": "claude"},
        "channel": "personal",
        "approval_id": null,
        "issued_ms": now,
        "expires_ms": now + 30_000,
        "nonce": "0123456789abcdef0123456789abcdef",
        "credentials": {"sql-read": {"DB_BACKEND": "sqlite", "DB_SERVICE": "apps.sqlite"}},
    });
    sign(&mut valid);
    let resigned = |nonce: &str, f: &dyn Fn(&mut Value)| {
        let mut c = valid.clone();
        c["nonce"] = json!(nonce);
        f(&mut c);
        sign(&mut c);
        c
    };
    let c1 = resigned("11111111111111111111111111111111", &|c| {
        c["plugin"] = json!("jira-review")
    });
    let c2 = resigned("22222222222222222222222222222222", &|c| {
        c["instance"] = json!(sha256_hex(b"previous-token"))
    });
    let c3 = resigned("33333333333333333333333333333333", &|_| {});
    let mut c4 = resigned("44444444444444444444444444444444", &|_| {});
    c4["channel"] = json!("work");
    let c5 = resigned("55555555555555555555555555555555", &|c| {
        c["issued_ms"] = json!(now - 120_000);
        c["expires_ms"] = json!(now - 90_000);
    });
    let c6 = resigned("66666666666666666666666666666666", &|_| {});
    let c7 = resigned("77777777777777777777777777777777", &|c| {
        c["tool"] = json!("apply");
        c["arguments_sha256"] = json!(arguments_sha256(None).unwrap());
    });
    let tampered = json!([
        {"name": "another plugin's context", "check": 1, "context": c1},
        {"name": "a previous spawn's instance", "check": 2, "context": c2},
        {"name": "arguments changed after signing", "check": 3,
         "call": {"tool": "query", "arguments": {"sql": "SELECT * FROM apps"}}, "context": c3},
        {"name": "a field changed without re-signing", "check": 4, "context": c4},
        {"name": "expired", "check": 5, "context": c5},
        {"name": "a replayed nonce", "check": 6, "replay": true, "context": c6},
        {"name": "a write tool without an approval", "check": 7,
         "call": {"tool": "apply", "arguments": {}}, "context": c7},
    ]);
    let file = json!({
        "description": "Tracon plugin caller context, wire format v1. `replay`: verify the context once before the check. `call` overrides the call a context is checked against. `write_tools`: tools the verifier treats as writes.",
        "seed": hex::encode(seed),
        "public_key": hex::encode(key.verifying_key().to_bytes()),
        "plugin": "sql-review",
        "plugin_token": token,
        "instance": sha256_hex(token.as_bytes()),
        "now_ms": now,
        "write_tools": ["apply"],
        "call": {"tool": "query", "arguments": arguments},
        "valid": {
            "context": valid,
            "signing_bytes": hex::encode(signing_bytes(&valid).unwrap()),
            "signature": valid["signature"],
        },
        "tampered": tampered,
    });
    std::fs::write(PATH, serde_json::to_string_pretty(&file).unwrap() + "\n").unwrap();
}
