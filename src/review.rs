//! `sql-review review`: the person on the other side of `mcp`. Lists what is
//! waiting, shows each proposal, and applies what they approve on the write
//! account, the stale check running then.

use std::io::{BufRead, Write};

use crate::{
    change::{self, Outcome, Proposal, Status},
    config::Config,
    store::Store,
};

/// One line per pending proposal, oldest first.
pub fn list(store: &Store) -> Result<String, String> {
    let pending = store.pending()?;
    if pending.is_empty() {
        return Ok("nothing is waiting for review\n".into());
    }
    Ok(pending
        .iter()
        .map(|p| {
            let rows = p
                .preview
                .row_count
                .map(|n| format!("{n} row(s)"))
                .unwrap_or_else(|| "not previewable".into());
            format!("{}  {}  {rows}  {}\n", p.id, p.summary(), p.created)
        })
        .collect())
}

/// Claim `id` and apply it with the settings `config` resolves for its
/// profile. Whatever happens is written back as its outcome, so the agent
/// waiting on it hears.
pub async fn approve<F>(store: &Store, id: &str, config: F) -> Result<Proposal, String>
where
    F: Fn(Option<&str>) -> Result<Config, String>,
{
    let p = store.claim(id)?;
    let applied = match config(p.profile.as_deref()) {
        Ok(cfg) => change::apply(&p, &cfg).await,
        Err(e) => Err(change::ApplyError::Invalid(e)),
    };
    store.decide(p, Outcome::of(&applied))
}

pub fn reject(store: &Store, id: &str, reason: Option<String>) -> Result<Proposal, String> {
    let p = store.claim(id)?;
    store.decide(p, Outcome::new(Status::Rejected, None, reason))
}

fn ask(prompt: &str, input: &mut dyn BufRead, out: &mut dyn Write) -> Option<String> {
    let _ = write!(out, "{prompt}");
    let _ = out.flush();
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

/// Go through what is pending, oldest first, asking about each.
pub async fn interactive<F>(
    store: &Store,
    config: F,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<(), String>
where
    F: Fn(Option<&str>) -> Result<Config, String>,
{
    let pending = store.pending()?;
    if pending.is_empty() {
        let _ = writeln!(out, "nothing is waiting for review");
        return Ok(());
    }
    let total = pending.len();
    for (i, p) in pending.iter().enumerate() {
        let _ = write!(out, "\n[{}/{total}]\n{}", i + 1, change::render(p));
        loop {
            let Some(answer) = ask("\n[a]pprove / [r]eject / [s]kip / [q]uit: ", input, out) else {
                return Ok(());
            };
            match answer.as_str() {
                "a" | "approve" => {
                    let said = match approve(store, &p.id, &config).await {
                        Ok(done) => describe(&done),
                        Err(e) => format!("{e}\n"),
                    };
                    let _ = write!(out, "{said}");
                    break;
                }
                "r" | "reject" => {
                    let reason = ask("reason (optional): ", input, out).filter(|r| !r.is_empty());
                    let said = match reject(store, &p.id, reason) {
                        Ok(done) => describe(&done),
                        Err(e) => format!("{e}\n"),
                    };
                    let _ = write!(out, "{said}");
                    break;
                }
                "s" | "skip" => break,
                "q" | "quit" => return Ok(()),
                _ => {
                    let _ = writeln!(out, "a, r, s or q");
                }
            }
        }
    }
    Ok(())
}

/// How a decided proposal turned out, for the reviewer.
pub fn describe(p: &Proposal) -> String {
    match &p.outcome {
        Some(o) => change::describe(p, o),
        None => format!("{} is not decided\n", p.id),
    }
}
