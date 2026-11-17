//! Connection settings: consulta's `.env` resolution, carried over unchanged.
//!
//! Standalone, every value comes from the process environment or a `.env`
//! file, and an exported variable wins over the file. With a profile (from
//! `--profile`, else `DB_PROFILE`), a `<PROFILE>_`-prefixed key such as
//! `QA_DB_HOST` is tried before the bare `DB_HOST`, so one file can hold
//! several databases. Under Tracon the same keys arrive per call in a lent
//! credential, without prefixes: the credential's name picks the profile.
//!
//! Keys (all optional unless the backend needs them):
//!
//! ```text
//! DB_BACKEND      oracle | mssql | sqlite            (default: oracle)
//! DB_HOST         hostname
//! DB_PORT         port                               (default: 1521 / 1433)
//! DB_SERVICE      Oracle service name, SQL Server database, SQLite path
//!                 (aliases: DB_DATABASE, DB_SCHEMA)
//! DB_USER         read-only account                  (alias: DB_USERNAME)
//! DB_PASSWORD     its password
//! DB_FETCH_LIMIT  default row cap                    (default: 1000)
//! DATABASE_URL    a whole SQLAlchemy-style URL, overriding the parts above
//! DB_TRUST_SERVER_CERTIFICATE  SQL Server: skip certificate checks (default: false)
//! DB_WRITE_USER / DB_WRITE_PASSWORD  the write account, reserved for
//!                 proposed writes; never read under --read-only
//! ```

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Path, PathBuf},
};

use crate::backend::Kind;

pub const DEFAULT_FETCH_LIMIT: u64 = 1000;

/// The profile that means "the bare keys, no prefix".
pub const DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError(msg.into()))
}

/// A database login.
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    pub user: String,
    pub password: Option<String>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub backend: Kind,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Oracle service, SQL Server database, or SQLite path.
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    /// `DATABASE_URL` as given, or the URL assembled from the parts.
    pub database_url: String,
    pub fetch_limit: u64,
    pub trust_server_certificate: bool,
    /// The write account. Always `None` under `--read-only`, and unused until
    /// writes are proposed for review.
    pub write: Option<Account>,
    pub read_only: bool,
    pub env_path: Option<PathBuf>,
    pub profile: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("backend", &self.backend)
            .field("url", &self.redacted_url())
            .field("fetch_limit", &self.fetch_limit)
            .field("write", &self.write)
            .field("read_only", &self.read_only)
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// The URL with its password shown as `***`, for the header and logs.
    pub fn redacted_url(&self) -> String {
        match parse_url(&self.database_url) {
            Ok(p) if p.password.is_some() => render_url(
                p.backend,
                p.host.as_deref(),
                p.port,
                p.database.as_deref(),
                p.user.as_deref(),
                p.password.as_deref(),
                true,
            ),
            Ok(_) => self.database_url.clone(),
            Err(_) => "(unreadable DATABASE_URL)".into(),
        }
    }

    /// The read-only account.
    pub fn account(&self) -> Account {
        Account {
            user: self.user.clone().unwrap_or_default(),
            password: self.password.clone(),
        }
    }
}

/// Where values come from: a key's value, or `None` when unset or empty.
pub trait Source {
    fn get(&self, key: &str) -> Option<String>;
}

/// The process environment over a `.env` file, as consulta reads them.
#[derive(Debug, Clone, Default)]
pub struct EnvSource {
    pub env: BTreeMap<String, String>,
    pub file: BTreeMap<String, String>,
    pub path: Option<PathBuf>,
}

impl Source for EnvSource {
    fn get(&self, key: &str) -> Option<String> {
        // consulta's `os.environ.get(k) or file.get(k)`: empty is unset.
        [self.env.get(key), self.file.get(key)]
            .into_iter()
            .flatten()
            .find(|v| !v.is_empty())
            .cloned()
    }
}

impl EnvSource {
    /// Read the environment and the `.env` file: `env_file` if given (it
    /// must exist), else the first `.env` walking up from `cwd`.
    pub fn load(
        env: BTreeMap<String, String>,
        env_file: Option<&Path>,
        cwd: &Path,
    ) -> Result<Self, ConfigError> {
        let path = match env_file {
            Some(p) => {
                let p = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    cwd.join(p)
                };
                if !p.is_file() {
                    return err(format!(".env file not found: {}", p.display()));
                }
                Some(p.canonicalize().unwrap_or(p))
            }
            None => discover_env(cwd),
        };
        let mut file = BTreeMap::new();
        if let Some(p) = &path {
            let iter = dotenvy::from_path_iter(p)
                .map_err(|e| ConfigError(format!("could not read {}: {e}", p.display())))?;
            for item in iter {
                let (k, v) =
                    item.map_err(|e| ConfigError(format!("could not read {}: {e}", p.display())))?;
                file.insert(k, v);
            }
        }
        Ok(Self { env, file, path })
    }

    /// The profiles this environment configures, lowercased: each `<P>` with
    /// a `<P>_DB_*` or `<P>_DATABASE_URL` key, and `default` when bare keys
    /// configure one.
    pub fn profiles(&self) -> Vec<String> {
        let mut out = std::collections::BTreeSet::new();
        for key in self.env.keys().chain(self.file.keys()) {
            if self.get(key).is_none() {
                continue;
            }
            if DEFAULT_MARKERS.contains(&key.as_str()) {
                out.insert(DEFAULT_PROFILE.to_string());
                continue;
            }
            for k in PROFILE_KEYS {
                if let Some(prefix) = key.strip_suffix(k).and_then(|p| p.strip_suffix('_'))
                    && !prefix.is_empty()
                {
                    out.insert(prefix.to_ascii_lowercase());
                }
            }
        }
        out.into_iter().collect()
    }
}

/// Keys whose bare presence configures the default profile.
const DEFAULT_MARKERS: &[&str] = &[
    "DB_BACKEND",
    "DB_HOST",
    "DB_SERVICE",
    "DB_DATABASE",
    "DB_SCHEMA",
    "DATABASE_URL",
];

/// Keys that, prefixed, name a profile.
const PROFILE_KEYS: &[&str] = &[
    "DB_BACKEND",
    "DB_HOST",
    "DB_PORT",
    "DB_SERVICE",
    "DB_DATABASE",
    "DB_SCHEMA",
    "DB_USER",
    "DB_USERNAME",
    "DB_PASSWORD",
    "DB_FETCH_LIMIT",
    "DB_TRUST_SERVER_CERTIFICATE",
    "DATABASE_URL",
];

fn discover_env(start: &Path) -> Option<PathBuf> {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    start
        .ancestors()
        .map(|d| d.join(".env"))
        .find(|p| p.is_file())
}

/// A credential lent by Tracon for one call: bare `DB_*` fields, no prefixes.
#[derive(Debug, Clone, Default)]
pub struct CredentialSource(pub BTreeMap<String, String>);

impl Source for CredentialSource {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).filter(|v| !v.is_empty()).cloned()
    }
}

/// How to resolve: which profile, and whether prefixed keys apply.
#[derive(Debug, Clone, Default)]
pub struct Resolve<'a> {
    /// `--profile`. `None` falls back to `DB_PROFILE` when `prefixed`.
    pub profile: Option<&'a str>,
    /// Try `<PROFILE>_KEY` before `KEY` (standalone). A lent credential is
    /// already one profile's, so it has no prefixes.
    pub prefixed: bool,
    /// Never read the write account.
    pub read_only: bool,
    pub env_path: Option<PathBuf>,
}

pub fn resolve(source: &dyn Source, how: Resolve<'_>) -> Result<Config, ConfigError> {
    let profile = match how.profile {
        Some(p) => Some(p.to_string()),
        None if how.prefixed => source.get("DB_PROFILE"),
        None => None,
    };
    let prefix = profile
        .as_deref()
        .filter(|p| how.prefixed && *p != DEFAULT_PROFILE)
        .map(|p| format!("{}_", p.to_ascii_uppercase()));
    let get = |name: &str| {
        prefix
            .as_ref()
            .and_then(|p| source.get(&format!("{p}{name}")))
            .or_else(|| source.get(name))
    };
    let get_first = |names: &[&str]| names.iter().find_map(|n| get(n));

    let backend_name = get("DB_BACKEND")
        .unwrap_or_else(|| "oracle".into())
        .to_ascii_lowercase();
    let Some(mut backend) = Kind::from_name(&backend_name) else {
        return err(format!(
            "unknown DB_BACKEND {backend_name:?}; known backends: {}",
            Kind::NAMES.join(", ")
        ));
    };

    let mut host = get("DB_HOST");
    let mut database = get_first(&["DB_SERVICE", "DB_DATABASE", "DB_SCHEMA"]);
    let mut user = get_first(&["DB_USER", "DB_USERNAME"]);
    let mut password = get("DB_PASSWORD");
    let mut port = match get("DB_PORT") {
        None => backend.default_port(),
        Some(raw) => Some(
            raw.parse::<u16>()
                .map_err(|_| ConfigError(format!("DB_PORT must be an integer, got {raw:?}")))?,
        ),
    };
    let fetch_limit = match get("DB_FETCH_LIMIT") {
        None => DEFAULT_FETCH_LIMIT,
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|_| ConfigError(format!("DB_FETCH_LIMIT must be an integer, got {raw:?}")))?,
    };
    let trust_server_certificate = match get("DB_TRUST_SERVER_CERTIFICATE") {
        None => false,
        Some(raw) => parse_bool(&raw).ok_or_else(|| {
            ConfigError(format!(
                "DB_TRUST_SERVER_CERTIFICATE must be true or false, got {raw:?}"
            ))
        })?,
    };

    let database_url = match get("DATABASE_URL") {
        Some(url) => {
            let p = parse_url(&url)?;
            backend = p.backend;
            host = p.host;
            port = p.port.or(backend.default_port());
            database = p.database;
            user = p.user;
            password = p.password;
            url
        }
        None => {
            if backend != Kind::Sqlite {
                let missing: Vec<&str> = [("DB_HOST", &host), ("DB_USER", &user)]
                    .into_iter()
                    .filter(|(_, v)| v.is_none())
                    .map(|(n, _)| n)
                    .collect();
                if !missing.is_empty() {
                    let place = profile
                        .as_deref()
                        .map(|p| format!(" (profile {p})"))
                        .unwrap_or_default();
                    return err(format!(
                        "missing required {} for backend {}{place}",
                        missing.join(", "),
                        backend.name()
                    ));
                }
            }
            render_url(
                backend,
                host.as_deref(),
                port,
                database.as_deref(),
                user.as_deref(),
                password.as_deref(),
                false,
            )
        }
    };
    if backend == Kind::Sqlite && database.is_none() {
        database = Some(":memory:".into());
    }

    let write = if how.read_only {
        None
    } else {
        get("DB_WRITE_USER").map(|user| Account {
            user,
            password: get("DB_WRITE_PASSWORD"),
        })
    };

    Ok(Config {
        backend,
        host,
        port,
        database,
        user,
        password,
        database_url,
        fetch_limit,
        trust_server_certificate,
        write,
        read_only: how.read_only,
        env_path: how.env_path,
        profile,
    })
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The parts of a `DATABASE_URL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    pub backend: Kind,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
}

/// Read a SQLAlchemy-style URL: `oracle[+driver]://u:p@h:port/?service_name=S`
/// (or `/S`), `mssql[+driver]://u:p@h:port/db`, `sqlite:///relative.db` or
/// `sqlite:////absolute.db`.
pub fn parse_url(raw: &str) -> Result<UrlParts, ConfigError> {
    let Some((scheme, rest)) = raw.split_once("://") else {
        return err("DATABASE_URL is not a URL (expected scheme://…)");
    };
    let base = scheme
        .split('+')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let backend = match base.as_str() {
        "oracle" => Kind::Oracle,
        "mssql" => Kind::MsSql,
        "sqlite" => Kind::Sqlite,
        "postgresql" | "postgres" | "mysql" | "mariadb" => {
            return err(format!("DATABASE_URL backend {base} is not supported yet"));
        }
        other => {
            return err(format!(
                "unknown DATABASE_URL backend {other:?}; known backends: {}",
                Kind::NAMES.join(", ")
            ));
        }
    };
    if backend == Kind::Sqlite {
        // `sqlite:///x` is the relative path `x`; `sqlite:////x` is `/x`.
        let path = rest.split('?').next().unwrap_or_default();
        let path = path.strip_prefix('/').unwrap_or(path);
        let path = decode(path);
        return Ok(UrlParts {
            backend,
            host: None,
            port: None,
            database: Some(if path.is_empty() {
                ":memory:".into()
            } else {
                path
            }),
            user: None,
            password: None,
        });
    }
    let url = url::Url::parse(raw).map_err(|e| ConfigError(format!("DATABASE_URL: {e}")))?;
    let service = url
        .query_pairs()
        .find(|(k, _)| k == "service_name")
        .map(|(_, v)| v.into_owned());
    let path = decode(url.path().trim_start_matches('/'));
    Ok(UrlParts {
        backend,
        host: url.host_str().map(|h| decode(h.trim_matches(['[', ']']))),
        port: url.port(),
        database: service.or((!path.is_empty()).then_some(path)),
        user: Some(decode(url.username())).filter(|u| !u.is_empty()),
        password: url.password().map(decode),
    })
}

fn decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Assemble the URL consulta (SQLAlchemy) would have for these parts.
pub fn render_url(
    backend: Kind,
    host: Option<&str>,
    port: Option<u16>,
    database: Option<&str>,
    user: Option<&str>,
    password: Option<&str>,
    redact: bool,
) -> String {
    if backend == Kind::Sqlite {
        return format!("sqlite:///{}", database.unwrap_or(":memory:"));
    }
    let mut out = format!("{}://", backend.url_scheme());
    if let Some(u) = user {
        out.push_str(&encode(u));
        if let Some(p) = password {
            out.push(':');
            out.push_str(&if redact { "***".into() } else { encode(p) });
        }
        out.push('@');
    }
    out.push_str(host.unwrap_or_default());
    if let Some(p) = port {
        let _ = write!(out, ":{p}");
    }
    match (backend, database) {
        (Kind::Oracle, Some(s)) => {
            let _ = write!(out, "/?service_name={}", encode(s));
        }
        (_, Some(d)) => {
            let _ = write!(out, "/{}", encode(d));
        }
        (_, None) => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_source(body: &str) -> (tempfile::TempDir, EnvSource) {
        file_source_with_env(body, &[])
    }

    fn file_source_with_env(body: &str, env: &[(&str, &str)]) -> (tempfile::TempDir, EnvSource) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, body).unwrap();
        let env = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let src = EnvSource::load(env, Some(&path), dir.path()).unwrap();
        (dir, src)
    }

    fn load(src: &EnvSource, profile: Option<&str>) -> Result<Config, ConfigError> {
        resolve(
            src,
            Resolve {
                profile,
                prefixed: true,
                read_only: false,
                env_path: src.path.clone(),
            },
        )
    }

    // consulta's test_config.py, one for one.

    #[test]
    fn oracle_url_assembled_from_parts() {
        let (_d, src) = file_source(
            "DB_BACKEND=oracle\nDB_HOST=h.example.edu\nDB_PORT=2982\n\
             DB_SERVICE=prduno.db\nDB_USER=app4adm\nDB_PASSWORD=s3cr3t\n",
        );
        let cfg = load(&src, None).unwrap();
        assert_eq!(cfg.backend, Kind::Oracle);
        assert_eq!(cfg.host.as_deref(), Some("h.example.edu"));
        assert_eq!(cfg.port, Some(2982));
        assert!(cfg.database_url.contains("service_name=prduno.db"));
        assert!(cfg.database_url.contains("app4adm"));
    }

    #[test]
    fn redacted_url_hides_password() {
        let (_d, src) = file_source(
            "DB_BACKEND=oracle\nDB_HOST=h\nDB_SERVICE=svc\nDB_USER=u\nDB_PASSWORD=s3cr3t\n",
        );
        let cfg = load(&src, None).unwrap();
        assert!(!cfg.redacted_url().contains("s3cr3t"));
        assert!(cfg.redacted_url().contains("***"));
        assert!(!format!("{cfg:?}").contains("s3cr3t"));
    }

    #[test]
    fn profile_prefix_overrides_bare_key() {
        let (_d, src) = file_source(
            "DB_BACKEND=oracle\nDB_HOST=prd.example.edu\nDB_SERVICE=prduno.db\n\
             DB_USER=app4adm\nDB_PASSWORD=p\n\
             QA_DB_HOST=qa.example.edu\nQA_DB_SERVICE=qauno.db\n",
        );
        let cfg = load(&src, Some("qa")).unwrap();
        assert_eq!(cfg.host.as_deref(), Some("qa.example.edu"));
        assert!(cfg.database_url.contains("service_name=qauno.db"));
        assert_eq!(cfg.user.as_deref(), Some("app4adm"));
        assert_eq!(cfg.profile.as_deref(), Some("qa"));
    }

    #[test]
    fn missing_required_keys_raise() {
        let (_d, src) = file_source("DB_BACKEND=oracle\nDB_PORT=2982\n");
        let e = load(&src, None).unwrap_err();
        assert!(e.0.contains("DB_HOST"), "{e}");
        let (_d, src) = file_source("DB_BACKEND=oracle\n");
        let e = load(&src, Some("qa")).unwrap_err();
        assert!(e.0.contains("(profile qa)"), "{e}");
    }

    #[test]
    fn unknown_backend_raises() {
        let (_d, src) = file_source("DB_BACKEND=cassandra\n");
        let e = load(&src, None).unwrap_err();
        assert!(e.0.contains("unknown DB_BACKEND"), "{e}");
        assert!(e.0.contains("mssql, oracle, sqlite"), "{e}");
    }

    #[test]
    fn database_url_override_passthrough() {
        let (_d, src) = file_source("DB_BACKEND=sqlite\nDATABASE_URL=sqlite:///explicit.db\n");
        let cfg = load(&src, None).unwrap();
        assert_eq!(cfg.database_url, "sqlite:///explicit.db");
        assert_eq!(cfg.database.as_deref(), Some("explicit.db"));
    }

    #[test]
    fn process_env_overrides_file() {
        let (_d, src) = file_source_with_env(
            "DB_BACKEND=oracle\nDB_HOST=file.example.edu\nDB_SERVICE=svc\nDB_USER=u\nDB_PASSWORD=p\n",
            &[("DB_HOST", "env.example.edu")],
        );
        assert_eq!(
            load(&src, None).unwrap().host.as_deref(),
            Some("env.example.edu")
        );
    }

    #[test]
    fn bad_port_raises() {
        let (_d, src) = file_source("DB_BACKEND=oracle\nDB_HOST=h\nDB_USER=u\nDB_PORT=abc\n");
        let e = load(&src, None).unwrap_err();
        assert!(e.0.contains("DB_PORT"), "{e}");
    }

    // Beyond consulta's tests.

    #[test]
    fn empty_env_value_counts_as_unset() {
        let (_d, src) = file_source_with_env(
            "DB_BACKEND=sqlite\nDB_SERVICE=from_file.db\n",
            &[("DB_SERVICE", "")],
        );
        assert_eq!(
            load(&src, None).unwrap().database.as_deref(),
            Some("from_file.db")
        );
    }

    #[test]
    fn db_profile_comes_from_the_file() {
        let (_d, src) = file_source(
            "DB_PROFILE=qa\nDB_BACKEND=sqlite\nDB_SERVICE=prd.db\nQA_DB_SERVICE=qa.db\n",
        );
        let cfg = load(&src, None).unwrap();
        assert_eq!(cfg.profile.as_deref(), Some("qa"));
        assert_eq!(cfg.database.as_deref(), Some("qa.db"));
        // Naming the default profile means the bare keys, whatever DB_PROFILE says.
        let cfg = load(&src, Some("default")).unwrap();
        assert_eq!(cfg.database.as_deref(), Some("prd.db"));
    }

    #[test]
    fn env_file_is_discovered_walking_up() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "DB_BACKEND=sqlite\n").unwrap();
        let deep = dir.path().join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        let src = EnvSource::load(BTreeMap::new(), None, &deep).unwrap();
        assert_eq!(
            src.path.unwrap(),
            dir.path().canonicalize().unwrap().join(".env")
        );
    }

    #[test]
    fn missing_env_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let e =
            EnvSource::load(BTreeMap::new(), Some(Path::new("nope.env")), dir.path()).unwrap_err();
        assert!(e.0.starts_with(".env file not found: "), "{e}");
    }

    #[test]
    fn read_only_never_reads_the_write_account() {
        let (_d, src) = file_source(
            "DB_BACKEND=sqlite\nDB_WRITE_USER=writer\nDB_WRITE_PASSWORD=w\nQA_DB_WRITE_USER=qawriter\n",
        );
        let cfg = load(&src, None).unwrap();
        assert_eq!(
            cfg.write,
            Some(Account {
                user: "writer".into(),
                password: Some("w".into())
            })
        );
        assert_eq!(
            load(&src, Some("qa")).unwrap().write.unwrap().user,
            "qawriter"
        );
        let ro = resolve(
            &src,
            Resolve {
                read_only: true,
                prefixed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ro.write, None);
        assert!(ro.read_only);
    }

    #[test]
    fn database_url_schemes() {
        let p = parse_url("oracle+oracledb://u:p%40ss@h.example:1522/?service_name=PRD").unwrap();
        assert_eq!(
            p,
            UrlParts {
                backend: Kind::Oracle,
                host: Some("h.example".into()),
                port: Some(1522),
                database: Some("PRD".into()),
                user: Some("u".into()),
                password: Some("p@ss".into()),
            }
        );
        assert_eq!(
            parse_url("oracle://u:p@h/SVC").unwrap().database.as_deref(),
            Some("SVC")
        );
        let p = parse_url("mssql+pyodbc://sa:pw@sql.example:1444/Analytics").unwrap();
        assert_eq!(p.backend, Kind::MsSql);
        assert_eq!(p.port, Some(1444));
        assert_eq!(p.database.as_deref(), Some("Analytics"));
        assert_eq!(
            parse_url("sqlite:///rel.db").unwrap().database.as_deref(),
            Some("rel.db")
        );
        assert_eq!(
            parse_url("sqlite:////abs/x.db")
                .unwrap()
                .database
                .as_deref(),
            Some("/abs/x.db")
        );
        let e = parse_url("postgresql://u@h/db").unwrap_err();
        assert!(e.0.contains("not supported yet"), "{e}");
    }

    #[test]
    fn database_url_sets_the_backend_and_parts() {
        let (_d, src) = file_source("DATABASE_URL=mssql://sa:pw@h/db\n");
        let cfg = load(&src, None).unwrap();
        assert_eq!(cfg.backend, Kind::MsSql);
        assert_eq!(cfg.port, Some(1433));
        assert_eq!(cfg.password.as_deref(), Some("pw"));
        assert_eq!(cfg.redacted_url(), "mssql://sa:***@h/db");
    }

    #[test]
    fn sqlite_defaults_to_memory() {
        let (_d, src) = file_source("DB_BACKEND=sqlite\n");
        let cfg = load(&src, None).unwrap();
        assert_eq!(cfg.database.as_deref(), Some(":memory:"));
        assert_eq!(cfg.database_url, "sqlite:///:memory:");
    }

    #[test]
    fn profiles_are_listed() {
        let (_d, src) = file_source_with_env(
            "DB_BACKEND=sqlite\nQA_DB_SERVICE=qa.db\nTST_DATABASE_URL=sqlite:///t.db\n",
            &[
                ("MY_APP_DB_HOST", "h"),
                ("UNRELATED_THING", "x"),
                ("EMPTY_DB_HOST", ""),
            ],
        );
        assert_eq!(src.profiles(), vec!["default", "my_app", "qa", "tst"]);
    }

    #[test]
    fn a_credential_has_no_prefixes() {
        let cred = CredentialSource(
            [
                ("DB_BACKEND", "sqlite"),
                ("DB_SERVICE", "x.db"),
                ("QA_DB_SERVICE", "ignored.db"),
                ("DB_WRITE_USER", "w"),
            ]
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect(),
        );
        let cfg = resolve(
            &cred,
            Resolve {
                profile: Some("qa"),
                prefixed: false,
                read_only: true,
                env_path: None,
            },
        )
        .unwrap();
        assert_eq!(cfg.database.as_deref(), Some("x.db"));
        assert_eq!(cfg.write, None);
    }

    #[test]
    fn trust_server_certificate() {
        let (_d, src) = file_source(
            "DB_BACKEND=mssql\nDB_HOST=h\nDB_USER=u\nDB_TRUST_SERVER_CERTIFICATE=yes\n",
        );
        assert!(load(&src, None).unwrap().trust_server_certificate);
        let (_d, src) = file_source(
            "DB_BACKEND=mssql\nDB_HOST=h\nDB_USER=u\nDB_TRUST_SERVER_CERTIFICATE=maybe\n",
        );
        assert!(load(&src, None).is_err());
    }
}
