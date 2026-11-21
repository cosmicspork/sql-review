use std::{
    collections::BTreeMap,
    io::{IsTerminal, Read},
    process::ExitCode,
};

fn piped_stdin() -> Option<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut s = String::new();
    stdin.lock().read_to_string(&mut s).ok().map(|_| s)
}

/// A password typed at the terminal, unechoed.
#[cfg(feature = "keyring")]
fn prompt_secret() -> Option<String> {
    rpassword::prompt_password("password (not echoed): ").ok()
}

#[cfg(not(feature = "keyring"))]
fn prompt_secret() -> Option<String> {
    None
}

use clap::Parser;
use rmcp::ServiceExt;
use sql_review::{
    check_oracle,
    cli::{self, Cli, Command, Io},
    config::EnvSource,
    keyring::Keyring,
    mcp::{Handler, Mode, Standalone},
    serve, store,
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
            cli::run(
                cli.query,
                Io {
                    env,
                    cwd,
                    stdin: Box::new(piped_stdin),
                    out: &mut std::io::stdout(),
                    err: &mut std::io::stderr(),
                    keyring: Some(Keyring::os()),
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
                    keyring: Some(Keyring::os()),
                },
            )
            .await
        }
        Some(
            command @ (Command::Propose(_)
            | Command::Show(_)
            | Command::Apply(_)
            | Command::Review(_)),
        ) => {
            cli::run_write(
                command,
                Io {
                    env,
                    cwd,
                    stdin: Box::new(piped_stdin),
                    out: &mut std::io::stdout(),
                    err: &mut std::io::stderr(),
                    keyring: Some(Keyring::os()),
                },
                &mut std::io::stdin().lock(),
            )
            .await
        }
        Some(Command::Keyring(args)) => cli::run_keyring(
            args,
            Io {
                env,
                cwd,
                stdin: Box::new(|| piped_stdin().or_else(prompt_secret)),
                out: &mut std::io::stdout(),
                err: &mut std::io::stderr(),
                keyring: Some(Keyring::os()),
            },
        ),
        Some(Command::Mcp(args)) => match mcp(args, env, cwd).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("ERROR: {e:#}");
                2
            }
        },
        Some(Command::Serve(args)) => {
            // The plugin contract: nothing but the three start variables is
            // read from the environment, and no keyring is opened: every
            // credential arrives with its call.
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
    let store = if args.read_only {
        None
    } else {
        Some(
            store::default_root(&env)
                .and_then(store::Store::open)
                .map_err(anyhow::Error::msg)?,
        )
    };
    let handler = Handler::new(Mode::Standalone(Standalone {
        env,
        cwd,
        env_file: args.env_file,
        read_only: args.read_only,
        profiles,
        store,
        keyring: Some(Keyring::os()),
    }));
    handler
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
