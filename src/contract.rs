//! The Tracon plugin contract's caller context, wire format v1.
//!
//! Every `tools/call` from the node carries `params._meta.tracon`:
//!
//! ```json
//! {
//!   "v": 1,
//!   "plugin": "<manifest name>",
//!   "instance": "<lowercase hex sha256 of TRACON_PLUGIN_TOKEN>",
//!   "tool": "<tool name, unprefixed>",
//!   "arguments_sha256": "<lowercase hex sha256 of the canonical JSON of params.arguments>",
//!   "caller": { "kind": "operator" | "agent" | "plugin", "session": "<id or null>", "harness": "<id or null>" },
//!   "channel": "<channel>",
//!   "approval_id": "<id>" | null,
//!   "issued_ms": 0,
//!   "expires_ms": 0,
//!   "nonce": "<32 lowercase hex chars (16 random bytes)>",
//!   "credentials": { "<credential name>": { "<FIELD>": "<value>" } },
//!   "signature": "<lowercase hex ed25519 signature>"
//! }
//! ```
//!
//! **Signing bytes:** the ASCII label `tracon/v1/plugin-call`, then the
//! canonical JSON of the object without `signature`, prefixed by its length
//! as a big-endian u32. They are computed on the object as received, so a
//! field this build doesn't know is still covered by the signature.
//!
//! **Canonical JSON:** object keys sorted by byte order, no whitespace,
//! strings escaped as `serde_json` escapes them, `null` written out, and
//! integers only: a float anywhere refuses the call. A missing
//! `params.arguments` hashes as `{}`.
//!
//! **Checks, in order**, each refusing with its own error and no side effect:
//! 1. `v == 1`, and `plugin` is this plugin's name.
//! 2. `instance` is the sha256 of this process's `TRACON_PLUGIN_TOKEN`.
//! 3. `tool` and `arguments_sha256` match the call being served.
//! 4. The signature verifies under `TRACON_NODE_KEY`.
//! 5. `issued_ms - 5000 <= now <= expires_ms + 5000`, and
//!    `expires_ms - issued_ms <= 60000`.
//! 6. `nonce` is 32 lowercase hex and unseen; it is remembered until
//!    `expires_ms + 5000`. Only a call that passed 1–5 records its nonce.
//! 7. A write tool needs a non-null `approval_id`.

use std::{collections::BTreeMap, sync::Mutex};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const LABEL: &str = "tracon/v1/plugin-call";
pub const VERSION: u64 = 1;
pub const SKEW_MS: i64 = 5_000;
pub const MAX_LIFETIME_MS: i64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallerKind {
    Operator,
    Agent,
    /// Reserved for plugin-to-plugin calls through the node; nothing sends it yet.
    Plugin,
}

impl CallerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CallerKind::Operator => "operator",
            CallerKind::Agent => "agent",
            CallerKind::Plugin => "plugin",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CallerId {
    pub kind: CallerKind,
    pub session: Option<String>,
    pub harness: Option<String>,
}

/// `_meta.tracon`, parsed. Unknown fields are allowed and signed over.
#[derive(Clone, Deserialize)]
pub struct TraconContext {
    pub v: u64,
    pub plugin: String,
    pub instance: String,
    pub tool: String,
    pub arguments_sha256: String,
    pub caller: CallerId,
    pub channel: String,
    pub approval_id: Option<String>,
    pub issued_ms: i64,
    pub expires_ms: i64,
    pub nonce: String,
    #[serde(default)]
    pub credentials: BTreeMap<String, BTreeMap<String, String>>,
    pub signature: String,
}

/// Who is calling, as verified: what tool code may see of the context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub kind: CallerKind,
    pub session: Option<String>,
    pub harness: Option<String>,
    pub channel: String,
    pub approval_id: Option<String>,
}

/// A call that passed every check.
pub struct Verified {
    pub caller: Caller,
    /// The credentials lent to this call, by bound name.
    pub credentials: BTreeMap<String, BTreeMap<String, String>>,
}

impl std::fmt::Debug for Verified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Credential names only: the values are secrets.
        f.debug_struct("Verified")
            .field("caller", &self.caller)
            .field("credentials", &self.credentials.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("the Tracon caller context is malformed: {0}")]
    Malformed(String),
    #[error("caller context check 1 failed: {0}")]
    Version(String),
    #[error("caller context check 2 failed: the instance does not match this process's token")]
    Instance,
    #[error("caller context check 3 failed: {0}")]
    Call(String),
    #[error("caller context check 4 failed: {0}")]
    Signature(String),
    #[error("caller context check 5 failed: {0}")]
    Time(String),
    #[error("caller context check 6 failed: {0}")]
    Nonce(String),
    #[error("caller context check 7 failed: {0} is a write tool and the call carries no approval")]
    Approval(String),
}

impl Refusal {
    /// The check that refused, 1–7; 0 for a context that could not be read.
    pub fn check(&self) -> u8 {
        match self {
            Refusal::Malformed(_) => 0,
            Refusal::Version(_) => 1,
            Refusal::Instance => 2,
            Refusal::Call(_) => 3,
            Refusal::Signature(_) => 4,
            Refusal::Time(_) => 5,
            Refusal::Nonce(_) => 6,
            Refusal::Approval(_) => 7,
        }
    }
}

/// A value with no canonical form: it holds a float.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a float has no canonical form (pass decimals as strings)")]
pub struct NotCanonical;

/// The canonical JSON of a value, as Tracon's `proto/src/admin.rs` writes it.
pub fn canonical(value: &Value) -> Result<String, NotCanonical> {
    let mut out = String::new();
    write_canonical(value, &mut out)?;
    Ok(out)
}

fn write_canonical(value: &Value, out: &mut String) -> Result<(), NotCanonical> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                write_canonical(&map[*key], out)?;
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
        }
        Value::Number(n) if n.is_f64() => return Err(NotCanonical),
        other => out.push_str(&other.to_string()),
    }
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// `arguments_sha256` for a call's arguments; missing arguments hash as `{}`.
pub fn arguments_sha256(arguments: Option<&Map<String, Value>>) -> Result<String, NotCanonical> {
    let value = Value::Object(arguments.cloned().unwrap_or_default());
    Ok(sha256_hex(canonical(&value)?.as_bytes()))
}

/// The bytes the node signs: label, then length-prefixed canonical JSON of
/// the context without `signature`.
pub fn signing_bytes(context: &Value) -> Result<Vec<u8>, NotCanonical> {
    let mut unsigned = context.clone();
    if let Value::Object(map) = &mut unsigned {
        map.remove("signature");
    }
    let body = canonical(&unsigned)?;
    let mut out = Vec::with_capacity(LABEL.len() + 4 + body.len());
    out.extend_from_slice(LABEL.as_bytes());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body.as_bytes());
    Ok(out)
}

/// Parse `TRACON_NODE_KEY`: the node's ed25519 public key in lowercase hex.
pub fn parse_node_key(hex_key: &str) -> Result<VerifyingKey, String> {
    if hex_key.len() != 64 || hex_key.chars().any(|c| !matches!(c, '0'..='9' | 'a'..='f')) {
        return Err("TRACON_NODE_KEY must be 64 lowercase hex characters".into());
    }
    let bytes: [u8; 32] = hex::decode(hex_key)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "TRACON_NODE_KEY must be 32 bytes".to_string())?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| format!("TRACON_NODE_KEY is not a valid key: {e}"))
}

/// Checks calls for one plugin process.
pub struct Verifier {
    plugin: String,
    instance: String,
    key: VerifyingKey,
    write_tools: Vec<String>,
    /// Seen nonces, each with the time it may be forgotten.
    nonces: Mutex<BTreeMap<String, i64>>,
}

impl Verifier {
    /// `instance` is the lowercase hex sha256 of the plugin token; the token
    /// itself is not kept.
    pub fn new(
        plugin: &str,
        instance: String,
        key: VerifyingKey,
        write_tools: Vec<String>,
    ) -> Self {
        Self {
            plugin: plugin.into(),
            instance,
            key,
            write_tools,
            nonces: Mutex::new(BTreeMap::new()),
        }
    }

    /// Run the seven checks on `context` (the raw `_meta.tracon` value) for a
    /// call of `tool` with `arguments`, at `now_ms`.
    pub fn verify(
        &self,
        context: &Value,
        tool: &str,
        arguments: Option<&Map<String, Value>>,
        now_ms: i64,
    ) -> Result<Verified, Refusal> {
        // 1. Version and plugin, read before anything else so a context from
        // a later version is refused as one, not as malformed.
        let obj = context
            .as_object()
            .ok_or_else(|| Refusal::Malformed("not an object".into()))?;
        if obj.get("v").and_then(Value::as_u64) != Some(VERSION) {
            return Err(Refusal::Version(format!(
                "unsupported version {}",
                obj.get("v").unwrap_or(&Value::Null)
            )));
        }
        if obj.get("plugin").and_then(Value::as_str) != Some(self.plugin.as_str()) {
            return Err(Refusal::Version(format!(
                "the call is for plugin {}, not {}",
                obj.get("plugin").unwrap_or(&Value::Null),
                self.plugin
            )));
        }
        let ctx: TraconContext = serde_json::from_value(context.clone())
            .map_err(|e| Refusal::Malformed(e.to_string()))?;

        // 2. This process, not an earlier spawn.
        if ctx.instance != self.instance {
            return Err(Refusal::Instance);
        }

        // 3. The call it was signed for.
        if ctx.tool != tool {
            return Err(Refusal::Call(format!(
                "signed for tool {:?}, called as {tool:?}",
                ctx.tool
            )));
        }
        let hash = arguments_sha256(arguments).map_err(|e| Refusal::Call(e.to_string()))?;
        if ctx.arguments_sha256 != hash {
            return Err(Refusal::Call(
                "the arguments are not the ones signed".into(),
            ));
        }

        // 4. The node signed it.
        let signature: [u8; 64] = hex::decode(&ctx.signature)
            .ok()
            .filter(|_| ctx.signature.chars().all(|c| !c.is_ascii_uppercase()))
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| Refusal::Signature("not 64 bytes of lowercase hex".into()))?;
        let bytes = signing_bytes(context).map_err(|e| Refusal::Signature(e.to_string()))?;
        self.key
            .verify_strict(&bytes, &Signature::from_bytes(&signature))
            .map_err(|_| Refusal::Signature("the signature does not verify".into()))?;

        // 5. Fresh, and short-lived.
        if now_ms < ctx.issued_ms - SKEW_MS {
            return Err(Refusal::Time("issued in the future".into()));
        }
        if now_ms > ctx.expires_ms + SKEW_MS {
            return Err(Refusal::Time("expired".into()));
        }
        if ctx.expires_ms - ctx.issued_ms > MAX_LIFETIME_MS {
            return Err(Refusal::Time("valid for longer than 60 s".into()));
        }

        // 6. Once only.
        if ctx.nonce.len() != 32
            || !ctx
                .nonce
                .chars()
                .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
        {
            return Err(Refusal::Nonce(
                "the nonce is not 32 lowercase hex characters".into(),
            ));
        }
        {
            let mut nonces = self.nonces.lock().unwrap_or_else(|e| e.into_inner());
            nonces.retain(|_, forget_at| *forget_at >= now_ms);
            if nonces.contains_key(&ctx.nonce) {
                return Err(Refusal::Nonce("the nonce was already used".into()));
            }
            nonces.insert(ctx.nonce.clone(), ctx.expires_ms + SKEW_MS);
        }

        // 7. A write only with an approval.
        if self.write_tools.contains(&ctx.tool) && ctx.approval_id.is_none() {
            return Err(Refusal::Approval(ctx.tool));
        }

        Ok(Verified {
            caller: Caller {
                kind: ctx.caller.kind,
                session: ctx.caller.session,
                harness: ctx.caller.harness,
                channel: ctx.channel,
                approval_id: ctx.approval_id,
            },
            credentials: ctx.credentials,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;

    const SEED: [u8; 32] = [7; 32];

    fn key() -> SigningKey {
        SigningKey::from_bytes(&SEED)
    }

    fn sign(ctx: &mut Value) {
        let sig = key().sign(&signing_bytes(ctx).unwrap());
        ctx["signature"] = json!(hex::encode(sig.to_bytes()));
    }

    fn context(args: &Value, now: i64) -> Value {
        let mut ctx = json!({
            "v": 1,
            "plugin": "sql-review",
            "instance": sha256_hex(b"token"),
            "tool": "query",
            "arguments_sha256": arguments_sha256(args.as_object()).unwrap(),
            "caller": {"kind": "agent", "session": "s1", "harness": null},
            "channel": "personal",
            "approval_id": null,
            "issued_ms": now,
            "expires_ms": now + 30_000,
            "nonce": "00112233445566778899aabbccddeeff",
            "credentials": {"sql-read": {"DB_BACKEND": "sqlite"}},
        });
        sign(&mut ctx);
        ctx
    }

    fn verifier() -> Verifier {
        Verifier::new(
            "sql-review",
            sha256_hex(b"token"),
            key().verifying_key(),
            vec!["apply".into()],
        )
    }

    #[test]
    fn canonical_sorts_and_refuses_floats() {
        let v = json!({"b": [1, null, "x\"y"], "a": {"d": true, "c": -2}});
        assert_eq!(
            canonical(&v).unwrap(),
            r#"{"a":{"c":-2,"d":true},"b":[1,null,"x\"y"]}"#
        );
        assert_eq!(canonical(&json!({"f": 1.5})), Err(NotCanonical));
    }

    #[test]
    fn missing_arguments_hash_as_an_empty_object() {
        assert_eq!(arguments_sha256(None).unwrap(), sha256_hex(b"{}"));
    }

    #[test]
    fn signing_bytes_are_label_length_body() {
        let ctx = json!({"z": 1, "a": "x", "signature": "ff"});
        let bytes = signing_bytes(&ctx).unwrap();
        let body = br#"{"a":"x","z":1}"#;
        let mut want = b"tracon/v1/plugin-call".to_vec();
        want.extend_from_slice(&(body.len() as u32).to_be_bytes());
        want.extend_from_slice(body);
        assert_eq!(bytes, want);
    }

    #[test]
    fn a_good_call_passes_once() {
        let args = json!({"sql": "SELECT 1"});
        let ctx = context(&args, 1_000_000);
        let v = verifier();
        let ok = v
            .verify(&ctx, "query", args.as_object(), 1_000_000)
            .unwrap();
        assert_eq!(ok.caller.kind, CallerKind::Agent);
        assert_eq!(ok.caller.session.as_deref(), Some("s1"));
        assert_eq!(ok.credentials["sql-read"]["DB_BACKEND"], "sqlite");
        let again = v.verify(&ctx, "query", args.as_object(), 1_000_001);
        assert_eq!(again.err().map(|e| e.check()), Some(6));
    }

    #[test]
    fn a_refused_call_does_not_burn_its_nonce() {
        let args = json!({"sql": "SELECT 1"});
        let ctx = context(&args, 1_000_000);
        let v = verifier();
        // Expired: refused at 5, before the nonce is recorded.
        let late = v.verify(&ctx, "query", args.as_object(), 1_100_000);
        assert_eq!(late.err().map(|e| e.check()), Some(5));
        assert!(v.verify(&ctx, "query", args.as_object(), 1_000_000).is_ok());
    }

    #[test]
    fn unknown_fields_are_signed_over() {
        let args = json!({});
        let mut ctx = context(&args, 1_000_000);
        ctx["future"] = json!("x");
        assert_eq!(
            verifier()
                .verify(&ctx, "query", args.as_object(), 1_000_000)
                .err()
                .map(|e| e.check()),
            Some(4)
        );
        sign(&mut ctx);
        assert!(
            verifier()
                .verify(&ctx, "query", args.as_object(), 1_000_000)
                .is_ok()
        );
    }

    #[test]
    fn a_plugin_caller_parses() {
        let args = json!({});
        let mut ctx = context(&args, 1_000_000);
        ctx["caller"]["kind"] = json!("plugin");
        sign(&mut ctx);
        let ok = verifier()
            .verify(&ctx, "query", args.as_object(), 1_000_000)
            .unwrap();
        assert_eq!(ok.caller.kind, CallerKind::Plugin);
    }

    #[test]
    fn floats_in_arguments_are_refused() {
        let args = json!({"params": {"x": 1.5}});
        let v = verifier();
        let ctx = context(&json!({}), 1_000_000);
        let e = v
            .verify(&ctx, "query", args.as_object(), 1_000_000)
            .unwrap_err();
        assert_eq!(e.check(), 3);
        assert!(e.to_string().contains("float"), "{e}");
    }

    #[test]
    fn node_keys_parse() {
        assert!(parse_node_key(&hex::encode(key().verifying_key().to_bytes())).is_ok());
        assert!(parse_node_key("abc").is_err());
        assert!(parse_node_key(&"A".repeat(64)).is_err());
    }
}
