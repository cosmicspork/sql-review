use std::{
    collections::BTreeMap,
    io::{IsTerminal, Read},
    process::ExitCode,
};

use clap::Parser;
use rmcp::ServiceExt;
use sql_review::{
    check_oracle,
    cli::{self, Cli, Command, Io},
    config::EnvSource,
    mcp::{Handler, Mode, Standalone},
    serve,
};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ERROR: no working directory: {e}");
            return ExitCode::from(2);
        }
    };
    let code = match cli.command {
        None => {
            let stdin = || {
                let stdin = std::io::stdin();
                if stdin.is_terminal() {
                    return None;
                }
                let mut s = String::new();
                stdin.lock().read_to_string(&mut s).ok().map(|_| s)
            };
            cli::run(
                cli.query,
                Io {
                    env,
                    cwd,
                    stdin: Box::new(stdin),
                    out: &mut std::io::stdout(),
                    err: &mut std::io::stderr(),
                },
            )
            .await
        }
        Some(Command::CheckOracle(args)) => {
            check_oracle::run(
                args,
                Io {
                    env,
                    cwd,
                    stdin: Box::new(|| None),
                    out: &mut std::io::stdout(),
                    err: &mut std::io::stderr(),
                },
            )
            .await
        }
        Some(Command::Mcp(args)) => match mcp(args, env, cwd).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("ERROR: {e:#}");
                2
            }
        },
        Some(Command::Serve(args)) => {
            // The plugin contract: nothing but the three start variables is
            // read from the environment.
            let result = match serve::Start::from_env(|k| std::env::var(k).ok()) {
                Ok(start) => serve::serve(args, start).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("ERROR: {e:#}");
                    2
                }
            }
        }
    };
    ExitCode::from(code as u8)
}

async fn mcp(
    args: cli::McpArgs,
    env: BTreeMap<String, String>,
    cwd: std::path::PathBuf,
) -> anyhow::Result<()> {
    let src = EnvSource::load(env.clone(), args.env_file.as_deref(), &cwd)?;
    let mut profiles = src.profiles();
    if !args.profile.is_empty() {
        let unknown: Vec<&String> = args
            .profile
            .iter()
            .filter(|p| !profiles.contains(p))
            .collect();
        if !unknown.is_empty() {
            anyhow::bail!(
                "no profile {}; configured: {}",
                unknown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                profiles.join(", ")
            );
        }
        profiles.retain(|p| args.profile.contains(p));
    }
    let handler = Handler::new(Mode::Standalone(Standalone {
        env,
        cwd,
        env_file: args.env_file,
        read_only: args.read_only,
        profiles,
    }));
    handler
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
