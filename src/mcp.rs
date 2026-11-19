//! The `query` and `describe` tools, served over MCP, and standalone, the
//! `write` and `proposal_status` tools.
//!
//! One handler serves both modes. Standalone (`sql-review mcp`), settings
//! come from the environment and `.env`, read again on each call, and a
//! write is proposed into the store for a person to decide with `sql-review
//! review`. As a Tracon plugin (`sql-review serve`), every call must pass
//! the caller context checks first, its settings come only from the
//! credentials lent to that call, and there is no write tool yet.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
};
use serde_json::{Map, Value, json};

use crate::{
    change,
    config::{self, CredentialSource, DEFAULT_PROFILE, EnvSource, Resolve},
    contract::{Caller, Verifier},
    run::{self, Request},
    store::{Place, Store},
    value::{Param, Params},
};

pub const QUERY: &str = "query";
pub const DESCRIBE: &str = "describe";
pub const WRITE: &str = "write";
pub const PROPOSAL_STATUS: &str = "proposal_status";

/// The longest `proposal_status` waits in one call.
pub const MAX_WAIT_SECS: u64 = 45;

/// The credential a plugin call reads with: `sql-read` for the default
/// profile, `sql-read-<profile>` for another.
pub const READ_CREDENTIAL: &str = "sql-read";

/// Where a call's settings come from.
#[derive(Clone)]
pub enum Mode {
    Standalone(Standalone),
    Plugin(Arc<Verifier>),
}

#[derive(Clone)]
pub struct Standalone {
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub env_file: Option<PathBuf>,
    pub read_only: bool,
    /// The profiles offered, fixed at start.
    pub profiles: Vec<String>,
    /// Where proposed writes wait for review. `None` under `--read-only`,
    /// which offers no write tools.
    pub store: Option<Store>,
}

#[derive(Clone)]
pub struct Handler {
    mode: Mode,
}

impl Handler {
    pub fn new(mode: Mode) -> Self {
        Self { mode }
    }

    /// The store writes are proposed into, when this server offers writes.
    fn store(&self) -> Option<&Store> {
        match &self.mode {
            Mode::Standalone(s) if !s.read_only => s.store.as_ref(),
            _ => None,
        }
    }

    fn profiles(&self) -> Option<&[String]> {
        match &self.mode {
            Mode::Standalone(s) => Some(&s.profiles),
            Mode::Plugin(_) => None,
        }
    }

    /// Settings for one call, from the environment or from its credentials.
    fn config_for(
        &self,
        profile: Option<&str>,
        credentials: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<config::Config, String> {
        match &self.mode {
            Mode::Standalone(s) => {
                let chosen = if s.profiles.is_empty() {
                    profile.map(str::to_string)
                } else {
                    Some(select_profile(profile, &s.profiles)?)
                };
                let src = EnvSource::load(s.env.clone(), s.env_file.as_deref(), &s.cwd)
                    .map_err(|e| e.to_string())?;
                config::resolve(
                    &src,
                    Resolve {
                        profile: chosen.as_deref(),
                        prefixed: true,
                        read_only: s.read_only,
                        env_path: src.path.clone(),
                    },
                )
                .map_err(|e| e.to_string())
            }
            Mode::Plugin(_) => {
                let lent = lent_profiles(credentials);
                let chosen = match profile {
                    Some(p) if !lent.iter().any(|x| x == p) => {
                        return Err(format!(
                            "credential {} was not lent to this call",
                            credential_name(p)
                        ));
                    }
                    _ if lent.is_empty() => {
                        return Err(format!(
                            "no {READ_CREDENTIAL} credential was lent to this call"
                        ));
                    }
                    _ => select_profile(profile, &lent)?,
                };
                let fields = credentials[&credential_name(&chosen)].clone();
                // Plugin calls never read a write account.
                config::resolve(
                    &CredentialSource(fields),
                    Resolve {
                        profile: Some(&chosen),
                        prefixed: false,
                        read_only: true,
                        env_path: None,
                    },
                )
                .map_err(|e| e.to_string())
            }
        }
    }

    async fn call(
        &self,
        tool: &str,
        args: &Map<String, Value>,
        credentials: &BTreeMap<String, BTreeMap<String, String>>,
        _caller: Option<&Caller>,
    ) -> Result<String, String> {
        let profile = match args.get("profile") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => return Err("profile must be a string".into()),
        };
        if tool == PROPOSAL_STATUS {
            let store = self.store().ok_or("this server offers no writes")?;
            return proposal_status(store, args).await.map(|v| v.to_string());
        }
        let cfg = self.config_for(profile, credentials)?;
        if tool == WRITE {
            let store = self.store().ok_or("this server offers no writes")?;
            let sql = args
                .get("sql")
                .and_then(Value::as_str)
                .ok_or("write needs sql")?;
            let params = params_of(args)?;
            let p = change::propose(&cfg, sql, &params).await?;
            store.put(&p)?;
            return Ok(json!({
                "proposal_id": p.id,
                "status": "pending",
                "previewable": p.preview.kind != change::PreviewKind::NotPreviewable,
                "row_count": p.preview.row_count,
                "preview": change::render(&p),
                "review": "nothing is written yet: a person approves or rejects this with \
                           `sql-review review` in a terminal; wait for that with proposal_status",
            })
            .to_string());
        }
        let result = match tool {
            QUERY => {
                let sql = args
                    .get("sql")
                    .and_then(Value::as_str)
                    .ok_or("query needs sql")?;
                let params = params_of(args)?;
                let limit = match args.get("limit") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(
                        v.as_u64()
                            .ok_or("limit must be a whole number, 0 or more")?,
                    ),
                };
                run::query(
                    &cfg,
                    &Request {
                        sql: sql.into(),
                        params,
                        limit,
                    },
                )
                .await
            }
            DESCRIBE => {
                let table = args
                    .get("table")
                    .and_then(Value::as_str)
                    .ok_or("describe needs a table")?;
                run::describe(&cfg, table).await
            }
            other => return Err(format!("no tool {other:?}")),
        };
        result
            .map(|rs| rs.to_tool_json())
            .map_err(|e| e.to_string())
    }
}

fn params_of(args: &Map<String, Value>) -> Result<Params, String> {
    let mut params = Params::new();
    match args.get("params") {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            for (k, v) in m {
                params.insert(k.clone(), Param::from_json(v)?);
            }
        }
        Some(_) => return Err("params must be an object".into()),
    }
    Ok(params)
}

/// Wait up to `wait_secs` for a proposal to be decided.
async fn proposal_status(store: &Store, args: &Map<String, Value>) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or("proposal_status needs an id")?;
    let wait = match args.get("wait_secs") {
        None | Some(Value::Null) => 30,
        Some(v) => v.as_u64().ok_or("wait_secs must be a whole number")?,
    }
    .min(MAX_WAIT_SECS);
    let deadline = Instant::now() + Duration::from_secs(wait);
    loop {
        let found = store.get(id)?;
        let decided = match &found {
            None => return Err(format!("no proposal {id}")),
            Some((Place::Decided, p)) => p.outcome.as_ref(),
            Some((Place::Pending, _)) => None,
        };
        if let Some(o) = decided {
            let mut v = serde_json::to_value(o).map_err(|e| e.to_string())?;
            v["id"] = json!(id);
            return Ok(v);
        }
        if Instant::now() >= deadline {
            // Claimed but not yet written back: someone is applying it.
            let status = match found {
                Some((Place::Decided, _)) => "applying",
                _ => "pending",
            };
            return Ok(json!({ "id": id, "status": status }));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The standalone write tools, `profile` as [`definitions`] gives it.
pub fn write_definitions(profiles: &[String]) -> Vec<Value> {
    let mut write = json!({
        "name": WRITE,
        "description": "Propose one write (INSERT, UPDATE, DELETE, MERGE, or DDL) for a person to \
                        review. Nothing is written yet: the statement is previewed on the write \
                        account in a transaction that is rolled back, showing the rows it would \
                        change, and returns a proposal_id. A person approves or rejects it with \
                        `sql-review review`; wait for that with proposal_status. DDL and procedure \
                        calls are never run at preview and show as not previewable.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "sql": { "type": "string", "description": "One write statement." },
                "params": {
                    "type": "object",
                    "description": "Bind values for :name placeholders: strings, integers, booleans or null. Pass decimals as strings.",
                },
            },
            "required": ["sql"],
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": true },
    });
    // The same profile argument the reads have.
    if let Some(profile) =
        definitions(Some(profiles))[0]["inputSchema"]["properties"].get("profile")
    {
        write["inputSchema"]["properties"]["profile"] = profile.clone();
        if profiles.len() > 1 && !profiles.iter().any(|p| p == DEFAULT_PROFILE) {
            write["inputSchema"]["required"] = json!(["sql", "profile"]);
        }
    }
    vec![
        write,
        json!({
            "name": PROPOSAL_STATUS,
            "description": format!(
                "Wait for a proposed write to be decided, up to wait_secs (at most \
                 {MAX_WAIT_SECS}, 30 unless you say). status is pending while nobody has \
                 decided; approved with the rows it changed; rejected with the reviewer's \
                 reason; stale when its rows moved after it was proposed (propose again); \
                 failed when the database refused it."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The proposal_id write returned." },
                    "wait_secs": { "type": "integer", "minimum": 0 },
                },
                "required": ["id"],
            },
            "annotations": { "readOnlyHint": true },
        }),
    ]
}

/// The profiles a call's credentials lend: `sql-read` is `default`,
/// `sql-read-<p>` is `<p>`.
pub fn lent_profiles(credentials: &BTreeMap<String, BTreeMap<String, String>>) -> Vec<String> {
    let mut out: Vec<String> = credentials
        .keys()
        .filter_map(|n| {
            if n == READ_CREDENTIAL {
                Some(DEFAULT_PROFILE.to_string())
            } else {
                n.strip_prefix("sql-read-")
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
            }
        })
        .collect();
    out.sort();
    out
}

fn credential_name(profile: &str) -> String {
    if profile == DEFAULT_PROFILE {
        READ_CREDENTIAL.to_string()
    } else {
        format!("{READ_CREDENTIAL}-{profile}")
    }
}

/// The profile a call uses. Naming none means the default, or the only one
/// there is; with several and no default, the call has to say which.
pub fn select_profile(profile: Option<&str>, profiles: &[String]) -> Result<String, String> {
    match profile {
        Some(p) if profiles.iter().any(|x| x == p) => Ok(p.to_string()),
        Some(p) => Err(format!(
            "no profile {p:?}; the profiles are {}",
            profiles.join(", ")
        )),
        None if profiles.iter().any(|x| x == DEFAULT_PROFILE) => Ok(DEFAULT_PROFILE.into()),
        None if profiles.len() == 1 => Ok(profiles[0].clone()),
        None => Err(format!("say which profile: {}", profiles.join(", "))),
    }
}

/// The tools, with `profile` an enum of `profiles` when given (standalone)
/// or a free string (plugin: the node knows the channel's profiles).
pub fn definitions(profiles: Option<&[String]>) -> Vec<Value> {
    let mut defs = vec![
        json!({
            "name": QUERY,
            "description": "Run a read-only SQL query. Reads only: a statement that writes is \
                            refused, as is more than one statement. Bind values with :name \
                            placeholders and pass them in params.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "A single read, such as a SELECT or WITH statement." },
                    "params": {
                        "type": "object",
                        "description": "Bind values for :name placeholders: strings, integers, booleans or null. Pass decimals as strings.",
                    },
                    "limit": { "type": "integer", "minimum": 0, "description": "Maximum rows; 0 means uncapped. Default: the configured fetch limit." },
                },
                "required": ["sql"],
            },
            "annotations": { "readOnlyHint": true },
        }),
        json!({
            "name": DESCRIBE,
            "description": "List the columns of a table, with types and nullability, from the \
                            database's data dictionary.",
            "inputSchema": {
                "type": "object",
                "properties": { "table": { "type": "string", "description": "A table name, optionally schema-qualified." } },
                "required": ["table"],
            },
            "annotations": { "readOnlyHint": true },
        }),
    ];
    for d in &mut defs {
        let schema = match profiles {
            None => json!({
                "type": "string",
                "description": "Which database connection to use. Default: the default profile, or the only one.",
            }),
            Some([]) => continue,
            Some(ps) => json!({
                "type": "string",
                "enum": ps,
                "description": "Which database connection to use.",
            }),
        };
        d["inputSchema"]["properties"]["profile"] = schema;
        if let Some(ps) = profiles {
            if ps.len() > 1 && !ps.iter().any(|p| p == DEFAULT_PROFILE) {
                d["inputSchema"]["required"]
                    .as_array_mut()
                    .expect("required is an array")
                    .push(json!("profile"));
            }
            let description = format!(
                "{} Profiles: {}.",
                d["description"].as_str().unwrap_or_default(),
                ps.join(", ")
            );
            d["description"] = json!(description);
        }
    }
    defs
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        let instructions = if self.store().is_some() {
            "SQL. `query` runs one read; `describe` lists a table's columns; a write sent to \
             `query` is refused. `write` proposes a write for a person to review with \
             `sql-review review`, and `proposal_status` waits for their decision."
        } else {
            "Read-only SQL. `query` runs one read; `describe` lists a table's columns. \
             A statement that writes is refused."
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("sql-review", crate::VERSION))
            .with_instructions(instructions)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut defs = definitions(self.profiles());
        if self.store().is_some() {
            defs.extend(write_definitions(self.profiles().unwrap_or_default()));
        }
        let tools = defs
            .into_iter()
            .map(serde_json::from_value::<Tool>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let tool = request.name.to_string();
        let args = request.arguments.clone().unwrap_or_default();
        let (credentials, caller) = match &self.mode {
            Mode::Standalone(_) => (BTreeMap::new(), None),
            Mode::Plugin(verifier) => {
                // The checks run before any tool code, and a refusal is a
                // protocol error, not a tool result.
                let meta = context
                    .meta
                    .0
                    .0
                    .get("tracon")
                    .or_else(|| request.meta.as_ref().and_then(|m| m.0.0.get("tracon")))
                    .ok_or_else(|| {
                        ErrorData::invalid_request(
                            "this call carries no Tracon caller context",
                            None,
                        )
                    })?;
                let verified = verifier
                    .verify(meta, &tool, request.arguments.as_ref(), now_ms())
                    .map_err(|e| {
                        eprintln!("sql-review: refused a call to {tool}: {e}");
                        ErrorData::invalid_request(e.to_string(), Some(json!({"check": e.check()})))
                    })?;
                let c = &verified.caller;
                eprintln!(
                    "sql-review: {tool} for {} session={} channel={}",
                    c.kind.as_str(),
                    c.session.as_deref().unwrap_or("-"),
                    c.channel
                );
                (verified.credentials, Some(verified.caller))
            }
        };
        let writes = self.store().is_some() && (tool == WRITE || tool == PROPOSAL_STATUS);
        if tool != QUERY && tool != DESCRIBE && !writes {
            return Err(ErrorData::invalid_params(format!("no tool {tool:?}"), None));
        }
        let result = match self.call(&tool, &args, &credentials, caller.as_ref()).await {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            // An error result, not a protocol error, so the caller can correct itself.
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
        };
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn profile_selection() {
        let both = ps(&["default", "qa"]);
        assert_eq!(select_profile(None, &both).unwrap(), "default");
        assert_eq!(select_profile(Some("qa"), &both).unwrap(), "qa");
        assert!(
            select_profile(Some("prd"), &both)
                .unwrap_err()
                .contains("no profile \"prd\"")
        );
        let one = ps(&["qa"]);
        assert_eq!(select_profile(None, &one).unwrap(), "qa");
        let two = ps(&["qa", "tst"]);
        assert_eq!(
            select_profile(None, &two).unwrap_err(),
            "say which profile: qa, tst"
        );
    }

    #[test]
    fn credentials_name_profiles() {
        let creds: BTreeMap<String, BTreeMap<String, String>> = [
            "sql-read",
            "sql-read-qa",
            "sql-read-",
            "sql-write",
            "sql-write-qa",
            "other",
        ]
        .into_iter()
        .map(|n| (n.to_string(), BTreeMap::new()))
        .collect();
        assert_eq!(lent_profiles(&creds), ps(&["default", "qa"]));
        assert_eq!(credential_name("default"), "sql-read");
        assert_eq!(credential_name("qa"), "sql-read-qa");
    }

    #[test]
    fn profile_is_required_only_when_ambiguous() {
        let req = |defs: &[Value]| defs[0]["inputSchema"]["required"].clone();
        assert_eq!(
            req(&definitions(Some(&ps(&["qa", "tst"])))),
            json!(["sql", "profile"])
        );
        assert_eq!(
            req(&definitions(Some(&ps(&["default", "qa"])))),
            json!(["sql"])
        );
        let defs = definitions(Some(&ps(&["default", "qa"])));
        assert_eq!(
            defs[1]["inputSchema"]["properties"]["profile"]["enum"],
            json!(["default", "qa"])
        );
        assert!(
            definitions(Some(&[]))[0]["inputSchema"]["properties"]
                .get("profile")
                .is_none()
        );
        let plugin = definitions(None);
        assert_eq!(
            plugin[0]["inputSchema"]["properties"]["profile"]["type"],
            "string"
        );
        assert!(
            plugin[0]["inputSchema"]["properties"]["profile"]
                .get("enum")
                .is_none()
        );
    }
}
