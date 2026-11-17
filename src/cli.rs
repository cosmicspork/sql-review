//! The command line: consulta's flags, unchanged, plus `--read-only`.
//!
//! Diagnostics go to stderr and results to stdout, so output pipes cleanly.
//! Any error or refusal exits 2 with `ERROR: …` as the last stderr line.

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

use clap::{Args, Parser, Subcommand};

use crate::{
    config::{self, Config, EnvSource, Resolve},
    render::{Format, render},
    run::{self, Request},
    value::{Param, Params},
};

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
}

/// Load settings the way consulta does: env over `.env`, profile prefixes.
pub fn load_config(
    env: BTreeMap<String, String>,
    cwd: &Path,
    env_file: Option<&Path>,
    profile: Option<&str>,
    read_only: bool,
) -> Result<Config, config::ConfigError> {
    let src = EnvSource::load(env, env_file, cwd)?;
    config::resolve(
        &src,
        Resolve {
            profile,
            prefixed: true,
            read_only,
            env_path: src.path.clone(),
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
    } = io;
    match run_inner(args, env, &cwd, stdin, out, err).await {
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
) -> Result<(), String> {
    let cfg = load_config(
        env,
        cwd,
        args.env_file.as_deref(),
        args.profile.as_deref(),
        args.read_only,
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
