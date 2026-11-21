//! The command line: consulta's flags, unchanged, plus `--read-only`, and
//! `propose`, `show`, `apply` and `review` for writes.
//!
//! Diagnostics go to stderr and results to stdout, so output pipes cleanly.
//! Any error or refusal exits 2 with `ERROR: …` as the last stderr line; an
//! apply refused because the rows moved exits 3.

use std::{
    collections::BTreeMap,
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

use clap::{Args, Parser, Subcommand};

use crate::{
    change::{self, ApplyError, Outcome, Proposal, Status},
    config::{self, Config, EnvSource, Resolve},
    keyring::Keyring,
    render::{Format, render},
    review,
    run::{self, Request},
    store::{self, Store},
    value::{Param, Params},
};

/// Exit code for an apply refused because the rows moved since.
pub const STALE: i32 = 3;

#[derive(Debug, Parser)]
#[command(
    name = "sql-review",
    version,
    about = "Read-only SQL for people and agents: a CLI, an MCP server and a Tracon plugin.",
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[command(flatten)]
    pub query: QueryArgs,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Serve `query` and `describe` as an MCP server on stdio.
    Mcp(McpArgs),
    /// Serve the Tracon plugin contract on a Unix socket.
    Serve(ServeArgs),
    /// Check an Oracle profile step by step: connect, query, describe,
    /// read-only transaction, binds.
    CheckOracle(CheckOracleArgs),
    /// Propose a write without running it: read the rows it would change on
    /// the read-only account (a single-table UPDATE or DELETE), and write a
    /// proposal file.
    Propose(ProposeArgs),
    /// Show a proposal file, or a stored proposal by id.
    Show(ShowArgs),
    /// Apply a proposal on the write account, refused if its rows moved since
    /// it was proposed.
    Apply(ApplyArgs),
    /// Approve or reject the proposals `mcp` is waiting on.
    Review(ReviewArgs),
    /// Store or remove a password in the OS keyring, for the CLI and `mcp`
    /// to read when neither the environment nor .env gives one.
    Keyring(KeyringArgs),
}

#[derive(Debug, Clone, Args)]
pub struct KeyringArgs {
    #[command(subcommand)]
    pub action: KeyringAction,
}

#[derive(Debug, Clone, Subcommand)]
pub enum KeyringAction {
    /// Store a password, read from stdin (or prompted for, unechoed, on a
    /// terminal).
    Set(KeyringTarget),
    /// Remove a stored password.
    Delete(KeyringTarget),
}

#[derive(Debug, Clone, Default, Args)]
pub struct KeyringTarget {
    /// Connection profile (e.g. qa). Default: DB_PROFILE, else `default`.
    #[arg(long)]
    pub profile: Option<String>,
    /// The write account's password (DB_WRITE_PASSWORD), not the read-only
    /// account's.
    #[arg(long)]
    pub write: bool,
    /// Path to a specific .env file, read for DB_PROFILE.
    #[arg(long)]
    pub env_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Args)]
pub struct ProposeArgs {
    /// SQL text of the write.
    #[arg(long)]
    pub sql: Option<String>,
    /// Path to a .sql file holding the write.
    #[arg(long)]
    pub file: Option<PathBuf>,
    /// Bind a :NAME placeholder. Repeatable.
    #[arg(long, value_name = "NAME=VALUE")]
    pub param: Vec<String>,
    /// Connection profile prefix in .env (e.g. qa).
    #[arg(long)]
    pub profile: Option<String>,
    /// Path to a specific .env file.
    #[arg(long)]
    pub env_file: Option<PathBuf>,
    /// Where to write the proposal: a path, or - for stdout. ./<id>.json
    /// unless given.
    #[arg(long)]
    pub out: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct ShowArgs {
    /// A proposal file, or the id of a stored proposal.
    pub change: String,
}

#[derive(Debug, Clone, Args)]
pub struct ApplyArgs {
    /// A proposal file, or the id of a stored proposal.
    pub change: String,
    /// Path to a specific .env file.
    #[arg(long)]
    pub env_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Args)]
pub struct ReviewArgs {
    /// List what is waiting, and exit.
    #[arg(long, conflicts_with_all = ["approve", "reject"])]
    pub list: bool,
    /// Approve and apply this proposal.
    #[arg(long, conflicts_with = "reject")]
    pub approve: Option<String>,
    /// Reject this proposal.
    #[arg(long)]
    pub reject: Option<String>,
    /// Why it was rejected; the agent waiting on it sees this.
    #[arg(long, requires = "reject")]
    pub reason: Option<String>,
    /// Path to a specific .env file.
    #[arg(long)]
    pub env_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Args)]
pub struct QueryArgs {
    /// SQL text to run (a single read).
    #[arg(long)]
    pub sql: Option<String>,
    /// Path to a .sql file to run.
    #[arg(long)]
    pub file: Option<PathBuf>,
    /// List a table's columns via a read-only data-dictionary query.
    #[arg(long, value_name = "TABLE")]
    pub describe: Option<String>,
    /// Bind a :NAME placeholder. Repeatable.
    #[arg(long, value_name = "NAME=VALUE")]
    pub param: Vec<String>,
    /// Max rows (0 = no cap). Default: DB_FETCH_LIMIT.
    #[arg(long)]
    pub limit: Option<u64>,
    /// Output format.
    #[arg(long, value_enum, default_value = "table")]
    pub format: Format,
    /// Write output to this file instead of stdout.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Connection profile prefix in .env (e.g. qa).
    #[arg(long)]
    pub profile: Option<String>,
    /// Path to a specific .env file.
    #[arg(long)]
    pub env_file: Option<PathBuf>,
    /// Test the connection and exit.
    #[arg(long)]
    pub check: bool,
    /// Suppress the diagnostic header.
    #[arg(long)]
    pub quiet: bool,
    /// Offer no write path, and never read the write account (DB_WRITE_*).
    #[arg(long)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, Args)]
pub struct McpArgs {
    /// Offer only these profiles (repeatable). Default: every configured one.
    #[arg(long)]
    pub profile: Vec<String>,
    /// Path to a specific .env file.
    #[arg(long)]
    pub env_file: Option<PathBuf>,
    /// Never read the write account (DB_WRITE_*).
    #[arg(long)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeArgs {
    /// The Unix socket to serve on. Tracon names it.
    #[arg(long)]
    pub socket: PathBuf,
    /// A directory this plugin may keep state in.
    #[arg(long)]
    pub data: PathBuf,
}

#[derive(Debug, Clone, Default, Args)]
pub struct CheckOracleArgs {
    /// Connection profile prefix in .env (e.g. qa).
    #[arg(long)]
    pub profile: Option<String>,
    /// Path to a specific .env file.
    #[arg(long)]
    pub env_file: Option<PathBuf>,
    /// The table to describe.
    #[arg(long, default_value = "DUAL")]
    pub table: String,
}

/// What the CLI reads and writes, so tests can run it in process.
pub struct Io<'a> {
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// Piped stdin, read only if no other SQL is given. `None` for a terminal.
    pub stdin: Box<dyn FnOnce() -> Option<String> + 'a>,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    /// Where a password neither the environment nor `.env` gives is looked
    /// up. `None` reads none.
    pub keyring: Option<Keyring>,
}

/// Load settings the way consulta does: env over `.env`, profile prefixes.
pub fn load_config(
    env: BTreeMap<String, String>,
    cwd: &Path,
    env_file: Option<&Path>,
    profile: Option<&str>,
    read_only: bool,
    keyring: Option<&Keyring>,
) -> Result<Config, config::ConfigError> {
    let src = EnvSource::load(env, env_file, cwd)?;
    config::resolve(
        &src,
        Resolve {
            profile,
            prefixed: true,
            read_only,
            env_path: src.path.clone(),
            keyring,
        },
    )
}

/// Run a query from the command line. Returns the exit code.
pub async fn run(args: QueryArgs, io: Io<'_>) -> i32 {
    let Io {
        env,
        cwd,
        stdin,
        out,
        err,
        keyring,
    } = io;
    match run_inner(args, env, &cwd, stdin, out, err, keyring.as_ref()).await {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(err, "ERROR: {e}");
            2
        }
    }
}

async fn run_inner(
    args: QueryArgs,
    env: BTreeMap<String, String>,
    cwd: &Path,
    stdin: Box<dyn FnOnce() -> Option<String> + '_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    keyring: Option<&Keyring>,
) -> Result<(), String> {
    let cfg = load_config(
        env,
        cwd,
        args.env_file.as_deref(),
        args.profile.as_deref(),
        args.read_only,
        keyring,
    )
    .map_err(|e| e.to_string())?;

    if !args.quiet {
        let _ = writeln!(err, "sql-review {}", crate::VERSION);
        let _ = writeln!(err, "  backend: {}", cfg.backend.name());
        let _ = writeln!(err, "  url:     {}", cfg.redacted_url());
        if let Some(p) = &cfg.profile {
            let _ = writeln!(err, "  profile: {p}");
        }
        if let Some(p) = &cfg.env_path {
            let _ = writeln!(err, "  env:     {}", p.display());
        }
        if cfg.read_only {
            let _ = writeln!(err, "  mode:    read-only");
        }
        let _ = writeln!(err);
    }

    if args.check {
        crate::backend::check(&cfg)
            .await
            .map_err(|e| e.to_string())?;
        let _ = writeln!(err, "OK: connection succeeded");
        return Ok(());
    }

    let result = if let Some(table) = &args.describe {
        run::describe(&cfg, table).await
    } else {
        let sql = resolve_sql(&args, cwd, stdin)?
            .ok_or("provide --sql, --file, --describe, or pipe SQL on stdin")?;
        let params = parse_params(&args.param)?;
        run::query(
            &cfg,
            &Request {
                sql,
                params,
                limit: args.limit,
            },
        )
        .await
    }
    .map_err(|e| e.to_string())?;

    let text = render(&result, args.format);
    match &args.output {
        Some(given) => {
            let path = cwd.join(given);
            std::fs::write(&path, format!("{text}\n"))
                .map_err(|e| format!("could not write {}: {e}", path.display()))?;
            let _ = writeln!(err, "{} row(s) -> {}", result.rows.len(), given.display());
        }
        None => {
            writeln!(out, "{text}").map_err(|e| format!("could not write output: {e}"))?;
        }
    }
    if result.truncated {
        let limit = args.limit.unwrap_or(cfg.fetch_limit);
        let _ = writeln!(err, "(stopped at --limit {limit}; more rows may exist)");
    }
    Ok(())
}

fn resolve_sql(
    args: &QueryArgs,
    cwd: &Path,
    stdin: Box<dyn FnOnce() -> Option<String> + '_>,
) -> Result<Option<String>, String> {
    if let Some(sql) = &args.sql {
        return Ok(Some(sql.clone()));
    }
    if let Some(file) = &args.file {
        let path = cwd.join(file);
        return std::fs::read_to_string(&path)
            .map(Some)
            .map_err(|e| format!("could not read {}: {e}", path.display()));
    }
    Ok(stdin()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}

/// Run `propose`, `show`, `apply` or `review`. `input` answers `review`'s
/// questions. Returns the exit code.
pub async fn run_write(command: Command, io: Io<'_>, input: &mut dyn BufRead) -> i32 {
    let Io {
        env,
        cwd,
        stdin,
        out,
        err,
        keyring,
    } = io;
    let settings = Settings {
        env: &env,
        cwd: &cwd,
        keyring: keyring.as_ref(),
    };
    match write_inner(command, settings, stdin, input, out, err).await {
        Ok(code) => code,
        Err((code, e)) => {
            let _ = writeln!(err, "ERROR: {e}");
            code
        }
    }
}

type Failed = (i32, String);

fn failed(e: impl std::fmt::Display) -> Failed {
    (2, e.to_string())
}

/// The exit code a decided proposal ends with.
fn outcome_code(p: &Proposal) -> i32 {
    match p.outcome.as_ref().map(|o| o.status) {
        Some(Status::Stale) => STALE,
        Some(Status::Failed) => 2,
        _ => 0,
    }
}

/// Where a write command's settings come from.
struct Settings<'a> {
    env: &'a BTreeMap<String, String>,
    cwd: &'a Path,
    keyring: Option<&'a Keyring>,
}

async fn write_inner(
    command: Command,
    settings: Settings<'_>,
    stdin: Box<dyn FnOnce() -> Option<String> + '_>,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, Failed> {
    let Settings { env, cwd, keyring } = settings;
    let store = || {
        store::default_root(env)
            .and_then(Store::open)
            .map_err(failed)
    };
    let config_for = |env_file| config_for(env, cwd, env_file, keyring);
    match command {
        Command::Propose(args) => {
            let cfg = load_config(
                env.clone(),
                cwd,
                args.env_file.as_deref(),
                args.profile.as_deref(),
                false,
                keyring,
            )
            .map_err(failed)?;
            let query = QueryArgs {
                sql: args.sql,
                file: args.file,
                ..QueryArgs::default()
            };
            let sql = resolve_sql(&query, cwd, stdin)
                .map_err(failed)?
                .ok_or_else(|| failed("provide --sql, --file, or pipe SQL on stdin"))?;
            let params = parse_params(&args.param).map_err(failed)?;
            let store = store()?;
            let p = change::propose(&cfg, &sql, &params).await.map_err(failed)?;
            let target = args.out.unwrap_or_else(|| format!("{}.json", p.id));
            if target == "-" {
                let _ = write!(out, "{}", p.to_json());
                let _ = write!(err, "{}", change::render(&p));
            } else {
                let path = cwd.join(&target);
                store::write_atomic(&path, &p.to_json()).map_err(failed)?;
                let _ = write!(out, "{}", change::render(&p));
                let _ = writeln!(err, "proposal written to {}", path.display());
            }
            store.log_proposed(&p);
            Ok(0)
        }
        Command::Show(args) => {
            let (_, p) = load(&args.change, cwd, store)?;
            let _ = write!(out, "{}", change::render(&p));
            Ok(0)
        }
        Command::Apply(args) => {
            let (path, p) = load(&args.change, cwd, store)?;
            let store = store()?;
            let config = config_for(args.env_file.as_deref());
            let Some(path) = path else {
                // A stored proposal is decided through the store, so an
                // agent waiting on it hears.
                let done = review::approve(&store, &args.change, config)
                    .await
                    .map_err(failed)?;
                let _ = write!(out, "{}", review::describe(&done));
                return Ok(outcome_code(&done));
            };
            let cfg = config(p.profile.as_deref()).map_err(failed)?;
            let applied = change::apply(&p, &cfg).await;
            let mut decided = p.clone();
            decided.outcome = Some(Outcome::of(&applied));
            store.log_decided(&decided);
            match applied {
                Err(ApplyError::Invalid(e)) => Err(failed(e)),
                // Rolled back: leave the file as it was.
                Err(e @ ApplyError::Stale(_)) => Err((STALE, e.to_string())),
                _ => {
                    store::write_atomic(&path, &decided.to_json()).map_err(failed)?;
                    let _ = write!(out, "{}", review::describe(&decided));
                    Ok(outcome_code(&decided))
                }
            }
        }
        Command::Review(args) => {
            let store = store()?;
            let config = config_for(args.env_file.as_deref());
            if args.list {
                let _ = write!(out, "{}", review::list(&store).map_err(failed)?);
            } else if let Some(id) = &args.approve {
                let done = review::approve(&store, id, config).await.map_err(failed)?;
                let _ = write!(out, "{}", review::describe(&done));
                return Ok(outcome_code(&done));
            } else if let Some(id) = &args.reject {
                let done = review::reject(&store, id, args.reason.clone()).map_err(failed)?;
                let _ = write!(out, "{}", review::describe(&done));
            } else {
                review::interactive(&store, config, input, out)
                    .await
                    .map_err(failed)?;
            }
            Ok(0)
        }
        _ => Err(failed("not a write command")),
    }
}

/// Settings for a proposal's profile, write account included.
fn config_for<'a>(
    env: &'a BTreeMap<String, String>,
    cwd: &'a Path,
    env_file: Option<&'a Path>,
    keyring: Option<&'a Keyring>,
) -> impl Fn(Option<&str>) -> Result<Config, String> + 'a {
    move |profile| {
        load_config(env.clone(), cwd, env_file, profile, false, keyring).map_err(|e| e.to_string())
    }
}

/// A proposal from a file, else from the store by id.
fn load(
    change: &str,
    cwd: &Path,
    store: impl FnOnce() -> Result<Store, Failed>,
) -> Result<(Option<PathBuf>, Proposal), Failed> {
    let path = cwd.join(change);
    if path.is_file() {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| failed(format!("could not read {}: {e}", path.display())))?;
        return Ok((Some(path), Proposal::from_json(&text).map_err(failed)?));
    }
    match store()?.get(change).map_err(failed)? {
        Some((_, p)) => Ok((None, p)),
        None => Err(failed(format!(
            "no proposal file or stored proposal {change}"
        ))),
    }
}

/// `NAME=VALUE` pairs, bound as text as consulta bound them.
pub fn parse_params(raw: &[String]) -> Result<Params, String> {
    raw.iter()
        .map(|item| {
            let (name, value) = item
                .split_once('=')
                .ok_or_else(|| format!("--param expects NAME=VALUE, got {item:?}"))?;
            Ok((name.trim().to_string(), Param::Text(value.to_string())))
        })
        .collect()
}

/// Run `keyring set` or `keyring delete`. Returns the exit code.
pub fn run_keyring(args: KeyringArgs, io: Io<'_>) -> i32 {
    let Io {
        env,
        cwd,
        stdin,
        err,
        keyring,
        ..
    } = io;
    let result = (|| {
        let keyring = keyring.ok_or("no keyring to use")?;
        let target = match &args.action {
            KeyringAction::Set(t) | KeyringAction::Delete(t) => t,
        };
        // The profile a query run here would use, so `set` stores what it
        // reads.
        let profile = match &target.profile {
            Some(p) => Some(p.clone()),
            None => {
                use config::Source as _;
                EnvSource::load(env, target.env_file.as_deref(), &cwd)
                    .map_err(|e| e.to_string())?
                    .get("DB_PROFILE")
            }
        };
        let account = crate::keyring::account(profile.as_deref(), target.write);
        let entry = format!("{}/{account}", crate::keyring::SERVICE);
        match &args.action {
            KeyringAction::Set(_) => {
                let raw = stdin().ok_or("no password given: pipe it on stdin")?;
                let secret = raw
                    .strip_suffix('\n')
                    .map(|s| s.strip_suffix('\r').unwrap_or(s))
                    .unwrap_or(&raw);
                if secret.is_empty() {
                    return Err("no password given: stdin was empty".to_string());
                }
                keyring.set(&account, secret)?;
                let _ = writeln!(err, "stored {entry} in the OS keyring");
            }
            KeyringAction::Delete(_) => {
                if keyring.delete(&account)? {
                    let _ = writeln!(err, "removed {entry} from the OS keyring");
                } else {
                    let _ = writeln!(err, "nothing stored for {entry}");
                }
            }
        }
        Ok::<_, String>(())
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(err, "ERROR: {e}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_parse() {
        let p = parse_params(&["strm=1261".into(), " a =b=c".into()]).unwrap();
        assert_eq!(p["strm"], Param::Text("1261".into()));
        assert_eq!(p["a"], Param::Text("b=c".into()));
        assert!(
            parse_params(&["nope".into()])
                .unwrap_err()
                .contains("NAME=VALUE")
        );
    }

    #[test]
    fn flags_without_a_subcommand_are_a_query() {
        let cli =
            Cli::try_parse_from(["sql-review", "--sql", "SELECT 1", "--format", "md"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.query.sql.as_deref(), Some("SELECT 1"));
        assert_eq!(cli.query.format, Format::Md);
        let cli = Cli::try_parse_from(["sql-review", "mcp", "--profile", "qa"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Mcp(_))));
        assert!(Cli::try_parse_from(["sql-review", "--format", "yaml"]).is_err());
    }
}
