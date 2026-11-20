//! The caller context wire format v1 against `tests/contract_vectors.json`.
//!
//! The file is jira-review's `tests/contract_vectors.json`, copied
//! verbatim: one set of vectors for both plugins and Tracon's node. Its
//! contexts are addressed to jira-review and its `update` tool is a write,
//! so the verifier here is built for that plugin and tool; sql-review's
//! `Verifier` takes both as parameters. When jira-review regenerates the
//! file, copy it again.

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use sql_review::contract::{Verifier, arguments_sha256, canonical, sha256_hex, signing_bytes};

const PATH: &str = "tests/contract_vectors.json";

fn vectors() -> Value {
    serde_json::from_str(&std::fs::read_to_string(PATH).unwrap()).unwrap()
}

fn key_from(v: &Value) -> SigningKey {
    let seed: [u8; 32] = hex::decode(v["seed"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    SigningKey::from_bytes(&seed)
}

fn verifier(v: &Value) -> Verifier {
    let valid = &v["valid"];
    Verifier::new(
        valid["context"]["plugin"].as_str().unwrap(),
        v["instance"].as_str().unwrap().into(),
        key_from(v).verifying_key(),
        vec![valid["call"]["tool"].as_str().unwrap().into()],
    )
}

fn verify(verifier: &Verifier, v: &Value, call: &Value, ctx: &Value) -> Result<(), u8> {
    verifier
        .verify(
            ctx,
            call["tool"].as_str().unwrap(),
            call["arguments"].as_object(),
            v["now_ms"].as_i64().unwrap(),
        )
        .map(drop)
        .map_err(|e| e.check())
}

#[test]
fn our_signer_reproduces_the_signing_bytes_and_signature() {
    let v = vectors();
    let key = key_from(&v);
    assert_eq!(
        v["public_key"].as_str().unwrap(),
        hex::encode(key.verifying_key().to_bytes())
    );
    assert_eq!(
        v["instance"].as_str().unwrap(),
        sha256_hex(v["plugin_token"].as_str().unwrap().as_bytes())
    );
    let valid = &v["valid"];
    let args = &valid["call"]["arguments"];
    assert_eq!(
        canonical(args).unwrap(),
        valid["arguments_canonical"].as_str().unwrap()
    );
    assert_eq!(
        valid["context"]["arguments_sha256"].as_str().unwrap(),
        arguments_sha256(args.as_object()).unwrap()
    );
    let bytes = signing_bytes(&valid["context"]).unwrap();
    assert!(bytes.starts_with(v["label"].as_str().unwrap().as_bytes()));
    assert_eq!(
        hex::encode(&bytes),
        valid["signing_bytes_hex"].as_str().unwrap()
    );
    let sig = hex::encode(key.sign(&bytes).to_bytes());
    assert_eq!(sig, valid["signature"].as_str().unwrap());
    assert_eq!(sig, valid["context"]["signature"].as_str().unwrap());
}

#[test]
fn the_valid_context_passes_and_each_refused_one_fails_at_its_check() {
    let v = vectors();
    let after_valid = verifier(&v);
    verify(
        &after_valid,
        &v,
        &v["valid"]["call"],
        &v["valid"]["context"],
    )
    .expect("the valid context passes");

    let cases = v["refused"].as_array().unwrap();
    let mut checks: Vec<u64> = cases.iter().map(|c| c["check"].as_u64().unwrap()).collect();
    checks.sort();
    checks.dedup();
    assert_eq!(
        checks,
        (1..=7).collect::<Vec<_>>(),
        "every check is covered"
    );
    for case in cases {
        assert_eq!(case["expect"], "refused", "{}", case["name"]);
        let fresh = verifier(&v);
        let verifier = if case["after"] == "valid" {
            &after_valid
        } else {
            &fresh
        };
        assert_eq!(
            verify(verifier, &v, &case["call"], &case["context"]),
            Err(case["check"].as_u64().unwrap() as u8),
            "{}",
            case["name"]
        );
    }
}

/// A call dispatched from an approved card carries the preview's token in
/// its signed context: the shared vector for it signs and passes as it is.
#[test]
fn the_preview_token_vector_reproduces_and_passes() {
    let v = vectors();
    let case = &v["valid_preview_token"];
    let ctx = &case["context"];
    assert!(ctx["preview_token"].is_string());
    assert_eq!(
        canonical(&case["call"]["arguments"]).unwrap(),
        case["arguments_canonical"].as_str().unwrap()
    );
    let bytes = signing_bytes(ctx).unwrap();
    assert_eq!(
        hex::encode(&bytes),
        case["signing_bytes_hex"].as_str().unwrap()
    );
    let sig = hex::encode(key_from(&v).sign(&bytes).to_bytes());
    assert_eq!(sig, case["signature"].as_str().unwrap());
    assert_eq!(sig, ctx["signature"].as_str().unwrap());
    assert_eq!(verify(&verifier(&v), &v, &case["call"], ctx), Ok(()));
}

/// The node adds fields to the context over time (`preview_token` on an
/// approved dispatch is the first). The signature covers the object as
/// received, so a field this build doesn't know passes when signed and is
/// refused when changed after signing.
#[test]
fn an_unknown_signed_field_such_as_preview_token_is_covered() {
    let v = vectors();
    let key = key_from(&v);
    let call = &v["valid"]["call"];
    let mut ctx = v["valid"]["context"].clone();
    ctx["nonce"] = json!("0000000000000000000000000000aaaa");
    ctx["preview_token"] = json!("{\"count\":3,\"hash\":\"ab12\"}");
    ctx["signature"] = json!(hex::encode(
        key.sign(&signing_bytes(&ctx).unwrap()).to_bytes()
    ));
    assert_eq!(verify(&verifier(&v), &v, call, &ctx), Ok(()));

    let mut moved = ctx.clone();
    moved["preview_token"] = json!("{\"count\":3,\"hash\":\"cd34\"}");
    assert_eq!(verify(&verifier(&v), &v, call, &moved), Err(4));

    let mut dropped = ctx;
    dropped.as_object_mut().unwrap().remove("preview_token");
    assert_eq!(verify(&verifier(&v), &v, call, &dropped), Err(4));
}
