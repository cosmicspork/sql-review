//! Proposals waiting on a reviewer, as files: `pending/<id>.json` until one
//! is decided, then `decided/<id>.json` with its outcome. Claiming a
//! proposal is an atomic rename out of `pending/`, so two reviewers cannot
//! both apply it.
//!
//! `log.jsonl` beside them records every proposal and decision: the
//! statement, row counts and hashes, never a credential.

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use crate::change::{Outcome, Proposal};

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// Where a proposal stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Pending,
    Decided,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, String> {
        let root = root.into();
        for dir in ["pending", "decided"] {
            std::fs::create_dir_all(root.join(dir))
                .map_err(|e| format!("{}: {e}", root.join(dir).display()))?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, place: Place, id: &str) -> Result<PathBuf, String> {
        // An id names a file; it may not name a path.
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!("{id:?} is not a proposal id"));
        }
        let dir = match place {
            Place::Pending => "pending",
            Place::Decided => "decided",
        };
        Ok(self.root.join(dir).join(format!("{id}.json")))
    }

    /// Write `p` whole or not at all: to a temporary file, then renamed.
    fn write(&self, place: Place, p: &Proposal) -> Result<(), String> {
        let path = self.path(place, &p.id)?;
        write_atomic(&path, &p.to_json())
    }

    /// Put a new proposal up for review.
    pub fn put(&self, p: &Proposal) -> Result<(), String> {
        self.write(Place::Pending, p)?;
        self.log_proposed(p);
        Ok(())
    }

    /// The proposals waiting on a reviewer, oldest first. Ids start with a
    /// v7 UUID, so their order is the order they were proposed in.
    pub fn pending(&self) -> Result<Vec<Proposal>, String> {
        let dir = self.root.join("pending");
        let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        names.sort();
        Ok(names
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .filter_map(|s| Proposal::from_json(&s).ok())
            .collect())
    }

    /// A proposal by id, wherever it is.
    pub fn get(&self, id: &str) -> Result<Option<(Place, Proposal)>, String> {
        for place in [Place::Decided, Place::Pending] {
            let path = self.path(place, id)?;
            match std::fs::read_to_string(&path) {
                Ok(s) => return Ok(Some((place, Proposal::from_json(&s)?))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        Ok(None)
    }

    /// Take a pending proposal for deciding. Only one caller gets it: the
    /// rename out of `pending/` either happens once or fails.
    pub fn claim(&self, id: &str) -> Result<Proposal, String> {
        let from = self.path(Place::Pending, id)?;
        let to = self.path(Place::Decided, id)?;
        match std::fs::rename(&from, &to) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(match self.get(id)? {
                    Some((Place::Decided, _)) => format!("proposal {id} was already decided"),
                    _ => format!("no proposal {id}"),
                });
            }
            Err(e) => return Err(format!("{}: {e}", from.display())),
        }
        let s = std::fs::read_to_string(&to).map_err(|e| format!("{}: {e}", to.display()))?;
        Proposal::from_json(&s)
    }

    /// Write a claimed proposal's outcome.
    pub fn decide(&self, mut p: Proposal, outcome: Outcome) -> Result<Proposal, String> {
        p.outcome = Some(outcome);
        self.write(Place::Decided, &p)?;
        self.log_decided(&p);
        Ok(p)
    }

    /// Keep a proposal that was decided as it was made: a plugin `write`,
    /// whose review was the node's approval card.
    pub fn record(&self, p: &Proposal) -> Result<(), String> {
        self.write(Place::Decided, p)?;
        self.log_decided(p);
        Ok(())
    }

    /// Record a proposal written to a file of its own.
    pub fn log_proposed(&self, p: &Proposal) {
        self.log(json!({
            "event": "proposed",
            "id": p.id,
            "backend": p.backend,
            "profile": p.profile,
            "target": p.target,
            "statement": p.statement,
            "params": p.params,
            "preview": p.preview.kind,
            "row_count": p.preview.row_count,
            "rows_hash": p.preview.rows_hash,
        }));
    }

    /// Record how a proposal was decided.
    pub fn log_decided(&self, p: &Proposal) {
        let Some(o) = &p.outcome else { return };
        self.log(json!({
            "event": o.status.name(),
            "id": p.id,
            "backend": p.backend,
            "profile": p.profile,
            "target": p.target,
            "statement": p.statement,
            "proposed_row_count": p.preview.row_count,
            "rows_hash": p.preview.rows_hash,
            "row_count": o.row_count,
            "reason": o.reason,
        }));
    }

    /// Append one line to `log.jsonl`, and say it on stderr. A log that
    /// can't be written is reported, not fatal: the decision already stands.
    fn log(&self, mut entry: Value) {
        entry["time"] = json!(crate::change::now());
        let line = format!("{entry}\n");
        let path = self.root.join("log.jsonl");
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = written {
            eprintln!("sql-review: could not write {}: {e}", path.display());
        }
        eprintln!(
            "sql-review: {} {} row_count={} rows_hash={}",
            entry["event"].as_str().unwrap_or_default(),
            entry["id"].as_str().unwrap_or_default(),
            entry["row_count"],
            entry["rows_hash"].as_str().unwrap_or("-"),
        );
    }
}

/// Write `body` to `path` whole or not at all.
pub fn write_atomic(path: &Path, body: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::now_v7().simple()));
    let mut f = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(body.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Where proposals and the log are kept: `SQL_REVIEW_STATE`, else
/// `$XDG_STATE_HOME/sql-review`, else `~/Library/Application
/// Support/sql-review` on macOS and `~/.local/state/sql-review` elsewhere.
pub fn default_root(env: &BTreeMap<String, String>) -> Result<PathBuf, String> {
    let var = |k: &str| env.get(k).filter(|v| !v.is_empty());
    if let Some(dir) = var("SQL_REVIEW_STATE") {
        return Ok(dir.into());
    }
    if let Some(dir) = var("XDG_STATE_HOME") {
        return Ok(PathBuf::from(dir).join("sql-review"));
    }
    let home = var("HOME").ok_or("no HOME to keep proposals under; set SQL_REVIEW_STATE")?;
    let home = PathBuf::from(home);
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/sql-review")
    } else {
        home.join(".local/state/sql-review")
    })
}
