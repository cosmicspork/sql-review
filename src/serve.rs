//! `sql-review serve`: the Tracon plugin contract on a Unix socket.
//!
//! MCP over streamable HTTP. The node starts this with an empty environment
//! but for `TRACON_PLUGIN_TOKEN`, `TRACON_NODE_KEY` and `TRACON_CONTRACT=1`;
//! credentials never come from the environment or a `.env` file here, only
//! lent per call in the signed caller context.

use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc};

use anyhow::{Context, bail};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio::net::UnixListener;

use crate::{
    cli::ServeArgs,
    contract::{self, Verifier},
    mcp::{Handler, Mode, Plugin},
    store::Store,
};

/// The settings the node starts a plugin with, read once.
pub struct Start {
    pub instance: String,
    pub node_key: ed25519_dalek::VerifyingKey,
}

impl Start {
    /// Read the start environment. Only the hash of the token is kept.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        match get("TRACON_CONTRACT").as_deref() {
            Some("1") => {}
            Some(other) => bail!("TRACON_CONTRACT is {other:?}; this build speaks contract 1"),
            None => {
                bail!("TRACON_CONTRACT is not set; serve runs under Tracon (try `sql-review mcp`)")
            }
        }
        let token = get("TRACON_PLUGIN_TOKEN")
            .filter(|t| !t.is_empty())
            .context("TRACON_PLUGIN_TOKEN is not set")?;
        let instance = contract::sha256_hex(token.as_bytes());
        drop(token);
        let node_key = contract::parse_node_key(
            &get("TRACON_NODE_KEY").context("TRACON_NODE_KEY is not set")?,
        )
        .map_err(anyhow::Error::msg)?;
        Ok(Self { instance, node_key })
    }
}

pub async fn serve(args: ServeArgs, start: Start) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.data)
        .with_context(|| format!("could not create {}", args.data.display()))?;
    let listener = bind(&args.socket)?;

    let verifier = Verifier::new(
        crate::PLUGIN_NAME,
        start.instance,
        start.node_key,
        crate::TOOLS
            .iter()
            .filter(|(_, hint)| *hint == "write")
            .map(|(name, _)| name.to_string())
            .collect(),
    );
    // Applied writes are kept and logged here, as standalone keeps them in
    // its state directory. Previews write nothing.
    let store = Store::open(&args.data).map_err(anyhow::Error::msg)?;
    let handler = Handler::new(Mode::Plugin(Arc::new(Plugin {
        verifier,
        store: Some(store),
    })));
    let ct = tokio_util::sync::CancellationToken::new();
    // The socket is reachable only through the node's private directory, so
    // there is no Host header worth checking.
    let config = StreamableHttpServerConfig::default()
        .disable_allowed_hosts()
        .with_cancellation_token(ct.child_token());
    let service: StreamableHttpService<Handler, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(handler.clone()), Default::default(), config);
    let router = axum::Router::new()
        .route_service("/", service.clone())
        .route_service("/mcp", service);

    eprintln!(
        "sql-review {}: serving the plugin contract on {}",
        crate::VERSION,
        args.socket.display()
    );
    let shutdown = {
        let ct = ct.clone();
        async move {
            shutdown_signal().await;
            ct.cancel();
        }
    };
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await;
    let _ = std::fs::remove_file(&args.socket);
    served.context("the server stopped")
}

fn bind(socket: &Path) -> anyhow::Result<UnixListener> {
    // A socket left by an earlier process would make bind fail.
    if let Ok(meta) = std::fs::symlink_metadata(socket) {
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() {
            bail!("{} exists and is not a socket", socket.display());
        }
        std::fs::remove_file(socket)
            .with_context(|| format!("could not remove the stale socket {}", socket.display()))?;
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("could not bind {}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("could not restrict {}", socket.display()))?;
    Ok(listener)
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = ctrl_c => {}
            _ = async {
                match term.as_mut() {
                    Some(t) => { t.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn start_needs_all_three() {
        let key = hex::encode(
            ed25519_dalek::SigningKey::from_bytes(&[1u8; 32])
                .verifying_key()
                .to_bytes(),
        );
        let ok = Start::from_env(env(&[
            ("TRACON_CONTRACT", "1"),
            ("TRACON_PLUGIN_TOKEN", "tok"),
            ("TRACON_NODE_KEY", &key),
        ]))
        .unwrap();
        assert_eq!(ok.instance, contract::sha256_hex(b"tok"));
        for missing in [
            &[
                ("TRACON_PLUGIN_TOKEN", "tok"),
                ("TRACON_NODE_KEY", key.as_str()),
            ][..],
            &[
                ("TRACON_CONTRACT", "2"),
                ("TRACON_PLUGIN_TOKEN", "tok"),
                ("TRACON_NODE_KEY", &key),
            ],
            &[("TRACON_CONTRACT", "1"), ("TRACON_NODE_KEY", &key)],
            &[("TRACON_CONTRACT", "1"), ("TRACON_PLUGIN_TOKEN", "tok")],
            &[
                ("TRACON_CONTRACT", "1"),
                ("TRACON_PLUGIN_TOKEN", "tok"),
                ("TRACON_NODE_KEY", "zz"),
            ],
        ] {
            assert!(Start::from_env(env(missing)).is_err());
        }
    }
}
