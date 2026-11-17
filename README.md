# sql-review

Read-only SQL queries for people and agents, with writes proposed for review before they apply. Oracle, SQL Server and SQLite; a CLI, an MCP server, and a Tracon plugin.

This version runs reads. A statement that writes is refused, with a message saying writes are not available yet; proposing writes for review comes in a later version.

sql-review replaces [consulta](https://github.com/cosmicspork/consulta): the same flags, the same `.env` keys, the same output formats, in one static binary with no Python, no Oracle Instant Client and no ODBC.

## The read-only account is the boundary

Reads always run on the read-only database account (`DB_USER`), inside the backend's read-only mode where it has one, and never commit. **That account is the security boundary.** Give it only the grants you are content for anyone holding this tool to use.

sql-review also parses each statement, but only to classify it: a lone query is a read and runs; anything else (DML, DDL, a locking `SELECT … FOR UPDATE`, `SELECT … INTO`, more than one statement, a PL/SQL or T-SQL block) is a write and is refused. A statement it cannot parse is refused too. **The classifier is not a security control.** If it ever mistakes a write for a read, the write still runs on the read-only account, in a read-only transaction, and fails there.

What each backend enforces on its own:

| Backend | Read-only mode | What it guarantees |
| --- | --- | --- |
| Oracle | `SET TRANSACTION READ ONLY`, then always `ROLLBACK` | Oracle refuses DML inside the transaction. DDL commits implicitly and is not stopped by it; the account's grants are what stop it. |
| SQLite | The file is opened read-only and the connection set to `PRAGMA query_only` | SQLite refuses every write. |
| SQL Server | `BEGIN TRANSACTION` … always `ROLLBACK`, `READ UNCOMMITTED`, and `ApplicationIntent=ReadOnly` | SQL Server has no read-only transaction. The rollback undoes transactional writes only, and ApplicationIntent matters only on an availability-group secondary. The read-only login is all that stands between a write and the database. |

## Install

From a release: download `sql-review-<version>-<target>.tar.gz` for your platform from the [releases](https://github.com/cosmicspork/sql-review/releases), check it against `checksums.txt`, and put `bin/sql-review` on your `PATH`. Targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and `aarch64-apple-darwin`. The Linux binaries are fully static (built against musl, named for the platform), so they run on any Linux of that architecture.

From source:

```sh
cargo install --git https://github.com/cosmicspork/sql-review
```

## Three modes

One binary, useful with or without Tracon.

### CLI

```sh
cp .env.example .env          # then fill in host/port/service/user/password
sql-review --check            # verify the connection

sql-review --sql "SELECT COUNT(*) FROM adm_applications"
sql-review --file reports/held_queue.sql --param strm=1261
sql-review --describe adm_applications
sql-review --sql "SELECT * FROM adm_fraud_rules" --format markdown
sql-review --sql "SELECT * FROM adm_applications" --limit 0 --format csv --output apps.csv
echo "SELECT 1 FROM dual" | sql-review --quiet
sql-review --profile qa --check
```

- SQL comes from `--sql`, else `--file`, else piped stdin.
- `--describe TABLE` lists a table's columns from the data dictionary, through an ordinary read (`DESCRIBE` itself is a client command on most backends).
- `--param NAME=VALUE` binds a `:NAME` placeholder, as text (repeatable).
- `--limit N` caps rows (`0` = no cap); the default is `DB_FETCH_LIMIT`. When rows were left behind, stderr says `(stopped at --limit N; more rows may exist)`.
- `--format` is `table` (default), `csv`, `tsv`, `json`, `markdown` or `md`. Binary values print as lowercase hex; NULL is `null` in tables and JSON, empty in CSV and Markdown.
- `--output FILE` writes the result to a file instead of stdout.
- `--read-only` offers no write path at all and never reads the write account.
- `--quiet` drops the diagnostic header. Diagnostics go to stderr and results to stdout; the exit code is `0`, or `2` on any error or refusal with `ERROR: …` as the last line of stderr.

### MCP server on stdio

`sql-review mcp` serves two tools to any MCP client:

- `query {sql, params?, limit?, profile?}`: one read. `params` binds `:name` placeholders to strings, integers, booleans or null.
- `describe {table, profile?}`: a table's columns.

Each answers with JSON text: `{"columns": [...], "rows": [{...}], "row_count": n, "truncated": bool}`, rows keyed in column order. A refused write or a database error comes back as a tool error result, so an agent can correct itself.

`profile` is an enum of the profiles configured in the environment and `.env`, and is required when there are several and no default. `--profile NAME` (repeatable) offers only those; `--env-file` and `--read-only` work as on the CLI. Settings are read again on every call.

```json
{
  "mcpServers": {
    "sql": {
      "command": "sql-review",
      "args": ["mcp", "--read-only", "--env-file", "/home/me/db/.env"]
    }
  }
}
```

### Tracon plugin

Under [Tracon](https://github.com/cosmicspork/tracon), the node installs a release archive and runs `bin/sql-review serve --socket <path> --data <dir>`. It serves the same two tools over MCP (streamable HTTP) on that Unix socket, with credentials lent per call by the node, never read from the environment or a `.env` file. See [The plugin contract](#the-plugin-contract) below.

## Configuration

Standalone, every value comes from the process environment or a `.env` file: `--env-file PATH`, else the first `.env` found walking up from the working directory. An exported variable wins over the file, and an empty value counts as unset.

| Key | Meaning |
| --- | --- |
| `DB_BACKEND` | `oracle`, `mssql` or `sqlite` (default `oracle`) |
| `DB_HOST` | Host name (required for Oracle and SQL Server) |
| `DB_PORT` | Port (default 1521 for Oracle, 1433 for SQL Server) |
| `DB_SERVICE` | Oracle service name, SQL Server database, or SQLite file path (aliases `DB_DATABASE`, `DB_SCHEMA`; SQLite defaults to `:memory:`) |
| `DB_USER` | The read-only account (alias `DB_USERNAME`; required for Oracle and SQL Server) |
| `DB_PASSWORD` | Its password |
| `DB_FETCH_LIMIT` | Default row cap (default 1000) |
| `DATABASE_URL` | A whole URL, overriding the parts: `oracle[+driver]://user:pass@host:port/?service_name=SVC` (or `/SVC`), `mssql[+driver]://user:pass@host:port/db`, `sqlite:///relative.db`, `sqlite:////absolute.db` |
| `DB_TRUST_SERVER_CERTIFICATE` | SQL Server: accept the server's certificate unchecked (default `false`) |
| `DB_WRITE_USER`, `DB_WRITE_PASSWORD` | The write account, reserved for writes proposed for review. Nothing uses it yet, and `--read-only` never reads it. |

**Profiles.** With `--profile NAME` (or `DB_PROFILE`), a `NAME_`-prefixed key such as `QA_DB_HOST` is tried before the bare `DB_HOST`, key by key, so one `.env` holds several databases. `--profile default` means the bare keys. For `sql-review mcp`, the profiles are every prefix with a `<P>_DB_*` or `<P>_DATABASE_URL` key, lowercased, plus `default` when bare keys configure one.

The OS keyring is planned as a third source and is not available yet.

### Moving from consulta

The flags, `.env` keys, profiles and output formats are consulta's, so an existing `.env` works unchanged: replace `consulta` with `sql-review` in your commands. Under Tracon, the credential is renamed from `consulta` to `sql-read` (and `consulta-<profile>` to `sql-read-<profile>`); the node handles that.

Differences: `--param` values and SQL Server placeholders are bound by sql-review itself rather than SQLAlchemy; Oracle NUMBER values that are whole and fit 64 bits come back as integers, other NUMBERs as exact decimal strings; and where consulta refused, sql-review classifies, so a refusal says the statement is a write.

## The plugin contract

### Manifest

`tracon-plugin.toml`, at the root of every release archive:

```toml
name = "sql-review"
version = "0.1.0"
contract = 1
exec = ["bin/sql-review", "serve"]
egress = []          # database hosts depend on each deployment

[[credential]]
name = "sql-read"
pattern = "sql-read-*"

[[credential]]
name = "sql-write"
pattern = "sql-write-*"
lend = "approved"    # declared now; nothing reads it until proposed writes ship

[[tool]]
name = "query"
hint = "read"

[[tool]]
name = "describe"
hint = "read"
```

### Start

The node runs `exec` with `--socket <path> --data <dir>` and an empty environment except `TRACON_PLUGIN_TOKEN`, `TRACON_NODE_KEY` (the node's ed25519 public key, lowercase hex) and `TRACON_CONTRACT=1`. `serve` refuses to start without all three. It keeps only the sha256 of the token, replaces a stale socket, makes the socket mode 0600, creates the data directory, and answers `initialize` at once: nothing connects to a database until a call.

### Credentials

Each call's credentials arrive in its caller context. `sql-read` is the default profile and `sql-read-<profile>` any other; their fields are the bare `DB_*` keys above (no prefixes). A call that names a profile whose credential wasn't lent fails with `credential sql-read-<profile> was not lent to this call`. `sql-write` is never read in this version.

### Caller context, wire format v1

Provisional, and shared with jira-review and Tracon's node. Every `tools/call` carries `params._meta.tracon`:

```json
{
  "v": 1,
  "plugin": "<manifest name>",
  "instance": "<lowercase hex sha256 of TRACON_PLUGIN_TOKEN>",
  "tool": "<tool name, unprefixed>",
  "arguments_sha256": "<lowercase hex sha256 of the canonical JSON of params.arguments>",
  "caller": { "kind": "operator" | "agent" | "plugin", "session": "<id or null>", "harness": "<id or null>" },
  "channel": "<channel>",
  "approval_id": "<id>" | null,
  "issued_ms": 0,
  "expires_ms": 0,
  "nonce": "<32 lowercase hex chars (16 random bytes)>",
  "credentials": { "<credential name>": { "<FIELD>": "<value>" } },
  "signature": "<lowercase hex ed25519 signature>"
}
```

`caller.kind = "plugin"` is reserved for plugin-to-plugin calls through the node; it is accepted, and nothing sends it yet.

**Signing bytes:** the ASCII label `tracon/v1/plugin-call`, then a big-endian u32 length and the canonical JSON of the whole object with `signature` removed (the convention of `proto/src/admin.rs` in Tracon). `signature` is ed25519 over those bytes with the node key. sql-review computes them on the object as received, so a field it doesn't know is still covered.

**Canonical JSON:** object keys sorted by byte order, no whitespace, strings escaped as `serde_json` escapes them, `null` written out (absent and null differ), integers only. A float anywhere has no canonical form and refuses the call, so pass decimal parameters as strings. A call without `arguments` hashes as `{}`.

**Checks, in order.** Any failure is a JSON-RPC error (with `data.check`, the check's number) and no side effect:

1. `v == 1`, and `plugin` is `sql-review`.
2. `instance` is the sha256 of this process's `TRACON_PLUGIN_TOKEN`, so a context for an earlier spawn is refused.
3. `tool` and `arguments_sha256` match the call being served.
4. The signature verifies under `TRACON_NODE_KEY` (strict ed25519).
5. `issued_ms - 5000 <= now <= expires_ms + 5000`, and `expires_ms - issued_ms <= 60000`.
6. `nonce` is 32 lowercase hex characters and hasn't been seen; it is remembered until `expires_ms + 5000`. A call refused by checks 1–5 does not use up its nonce.
7. A write tool needs a non-null `approval_id`. sql-review has no write tools yet.

A call without `_meta.tracon` is refused. A call that passes has its caller (kind, session, channel) logged on stderr, never its credentials or parameters.

There is no separate header check of the token: the instance hash inside the signed context carries it.

**Test vectors:** `tests/contract_vectors.json` holds a fixed seed and its public key, one valid context with its signing bytes and signature, and one spoiled context per check with the check that must refuse it. Until jira-review publishes its own file in this shape, this repository's is checked in; Tracon's node can check its signer against either.

## Checking Oracle

Nothing in this repository's tests reaches an Oracle database. To verify a profile:

```sh
sql-review check-oracle --profile prd            # or scripts/oracle-check.sh --profile prd
```

It connects, runs `SELECT 1 FROM DUAL`, runs a query of typed values through every format, describes a table (`--table`, default `DUAL`), runs a query inside `SET TRANSACTION READ ONLY` and rolls back, and binds a parameter, printing `PASS` or `FAIL` for each step. It exits 0 only if every step passes, and writes nothing.

## What is verified and what isn't

- **SQLite**: verified end to end in the test suite: the CLI (every format, describe, params, limits, profiles, `.env` discovery, `--read-only`, refusals), `mcp` over stdio, and `serve` over a Unix socket with signed contexts and lent credentials, including a write sent past the classifier failing on the read-only connection.
- **The contract**: verified against this repository's own test vectors and the `serve` tests. The node side is being written in parallel; until it calls this build, the wire format is checked only against itself.
- **SQL Server**: integration tests in `tests/mssql.rs` run only when `SQL_REVIEW_TEST_MSSQL_URL` points at a server; they have not been run yet. The `:name` placeholder rewrite is unit-tested.
- **Oracle**: built on Oracle's pure-Rust driver (`oracledb`, pinned to `26.0.0-beta.6`) and unit-tested where it can be (connect strings, number mapping), but **unverified against a database** until `check-oracle` is run against one.
- **Release builds**: both Linux targets have been built locally the way the release workflow builds them (`cargo zigbuild`, zig 0.15.2) and come out statically linked; the x86_64 binary was run, the aarch64 one only built. The `aarch64-apple-darwin` archive is unverified until the first tag builds it.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Licence

MIT.
