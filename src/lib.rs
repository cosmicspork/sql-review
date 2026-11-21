//! sql-review: read-only SQL for people and agents, with writes proposed
//! for review.
//!
//! One binary, three modes: a CLI ([`cli`]), an MCP server on stdio
//! ([`mcp`]), and the Tracon plugin contract on a Unix socket ([`serve`]).
//! The database account is the boundary: reads, and the reads that preview
//! a write, run on the read-only account; the write account is used only
//! to apply an approved write. The classifier ([`classify`]) only sorts
//! reads from writes.

pub mod backend;
pub mod change;
pub mod check_oracle;
pub mod classify;
pub mod cli;
pub mod config;
pub mod contract;
pub mod keyring;
pub mod mcp;
pub mod render;
pub mod review;
pub mod run;
pub mod serve;
pub mod store;
pub mod value;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The plugin's name in `tracon-plugin.toml` and in every caller context.
pub const PLUGIN_NAME: &str = "sql-review";

/// The tools and their hints, as `tracon-plugin.toml` declares them.
pub const TOOLS: &[(&str, &str)] = &[
    (mcp::QUERY, "read"),
    (mcp::DESCRIBE, "read"),
    (mcp::WRITE, "write"),
];
