# sql-review

Read-only SQL queries for people and agents, with writes proposed for review before they apply. Oracle, SQL Server and SQLite; a CLI, an MCP server, and a Tracon plugin.

Reads run at once. A write is proposed, and **no write is ever run to preview it**: for a single-table `UPDATE` or `DELETE`, sql-review reads the rows its `WHERE` selects on the read-only account and records them; it applies the write only after review, on the write account, refusing if those rows moved in between. Standalone, that is `propose`, `show`, `apply` and `review` on the command line and a `write` tool in `sql-review mcp`; as a Tracon plugin, `serve` offers `write` with `tracon/preview`, and the node's approval card is the review.

sql-review replaces [consulta](https://github.com/cosmicspork/consulta): the same flags, the same `.env` keys, the same output formats, in one static binary with no Python, no Oracle Instant Client and no ODBC.

## The read-only account is the boundary

Reads always run on the read-only database account (`DB_USER`), inside the backend's read-only mode where it has one, and never commit. **That account is the security boundary.** Give it only the grants you are content for anyone holding this tool to use.

sql-review also parses each statement, but only to classify it: a lone query is a read and runs; anything else (DML, DDL, a locking `SELECT … FOR UPDATE`, `SELECT … INTO`, more than one statement, a PL/SQL or T-SQL block) is a write: `query` refuses it, and `propose` takes it. A statement it cannot parse is refused too. **The classifier is not a security control.** If it ever mistakes a write for a read, the write still runs on the read-only account, in a read-only transaction, and fails there.

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
- `--describe TABLE` lists a table's columns from the data dictionary, through an ordinary read (`DESCRIBE` itself is a client command on most backends). `OWNER.TABLE` describes the table under that owner (Oracle), schema (SQL Server) or attached database (SQLite, e.g. `main.apps`). A bare name searches every schema the account can see; on Oracle and SQL Server its rows lead with an `owner` or `table_schema` column, and when the name is in more than one schema the result keeps every row and a note on stderr says to qualify it. A table that matches nothing is an error (`no table APP4ADM.ADM_APP_DATA visible to this account`), never an empty list. Quoted identifiers (`"MixedCase"`) are not accepted.
- `--param NAME=VALUE` binds a `:NAME` placeholder, as text (repeatable).
- `--limit N` caps rows (`0` = no cap); the default is `DB_FETCH_LIMIT`. When rows were left behind, stderr says `(stopped at --limit N; more rows may exist)`.
- `--format` is `table` (default), `csv`, `tsv`, `json`, `markdown` or `md`. Binary values print as lowercase hex; NULL is `null` in tables and JSON, empty in CSV and Markdown.
- `--output FILE` writes the result to a file instead of stdout.
- `--read-only` offers no write path at all and never reads the write account.
- `--quiet` drops the diagnostic header. Diagnostics go to stderr and results to stdout; the exit code is `0`, or `2` on any error or refusal with `ERROR: …` as the last line of stderr.

### Proposing writes

A write goes through review in two steps, and nothing is committed until the second:

```sh
sql-review propose --sql "UPDATE adm_applications SET status = 'HELD' WHERE strm = :strm" --param strm=1261
sql-review show 01a1f55e-….json      # the statement, the row count and the rows as they are now
sql-review apply 01a1f55e-….json     # commits only if those rows haven't moved
```

`propose` takes SQL as a query does (`--sql`, `--file`, stdin, `--param`, `--profile`, `--env-file`) and writes the proposal to `./<id>.json` (`--out PATH`, or `--out -` for stdout). It never runs the statement, and it needs only the read-only account: the write account is used by `apply` alone. Rolling back is not reliable everywhere (DDL commits implicitly on Oracle and MySQL, MyISAM has no transactions, and sequences, autonomous-transaction triggers and external effects survive a rollback), so nothing is run to see what it would do. What it records depends on the statement:

| Statement | What propose records | What apply checks |
| --- | --- | --- |
| A single-table `UPDATE` or `DELETE` | `SELECT * FROM <table> WHERE <its WHERE>`, run on the read-only account in the backend's read-only transaction: the row count, the first 20 rows as they are now, and a hash of every row | The locked rows' count and hash, then the statement's own row count, all against that count and hash |
| Anything else: `INSERT`, joins, subqueries, `MERGE`, `ORDER BY`/`LIMIT`, DDL, procedure calls, PL/SQL and T-SQL blocks | The statement alone, marked "not previewable", with the reason | Nothing: it runs as written |

The rows shown are the rows as they are, not as they will be: there is no "after". Reading them has a time limit, `DB_PREVIEW_TIMEOUT` seconds (default 10); a read that runs over is stopped and the write proposed as not previewable. One statement per proposal; a read is refused. Proposal files are format 2; a format 1 file, written by 0.2 when propose still ran the statement, is refused, so propose it again.

`apply` takes a proposal file or a stored proposal's id, and needs the write account (`DB_WRITE_USER`, `DB_WRITE_PASSWORD`, with the same profile prefixes). In one transaction on the write account it selects the affected rows locked (`FOR UPDATE` on Oracle, `WITH (UPDLOCK, HOLDLOCK)` on SQL Server; SQLite has no row locks, so the transaction starts with `BEGIN IMMEDIATE`, which takes the database's write lock and keeps every other writer out until it ends), refuses if their count or hash differ from what propose read, runs the statement, and commits only if it changed exactly that many rows. A refusal rolls back, leaves the file as it was and exits 3. A proposal is applied only to the database it was proposed against, and only once: the outcome is written back into the file.

Every proposal and decision is logged to `log.jsonl` in the state directory (below) and on stderr: the statement, its parameters, row counts and hashes, never a credential.

### MCP server on stdio

`sql-review mcp` serves two tools to any MCP client:

- `query {sql, params?, limit?, profile?}`: one read. `params` binds `:name` placeholders to strings, integers, booleans or null.
- `describe {table, profile?}`: a table's columns, as `--describe` takes them (`TABLE` or `OWNER.TABLE`).

Each answers with JSON text: `{"columns": [...], "rows": [{...}], "row_count": n, "truncated": bool}`, rows keyed in column order. A `describe` of a bare name found in more than one schema adds `"notice"` saying so. A refused write or a database error comes back as a tool error result, so an agent can correct itself.

Unless `--read-only` is given, it also serves two write tools:

- `write {sql, params?, profile?}`: propose one write, as `sql-review propose` does. It answers at once with a `proposal_id`, the rendered proposal, and `status: "pending"`; nothing is run.
- `proposal_status {id, wait_secs?}`: wait up to `wait_secs` (at most 45, default 30) for a person to decide. `status` is `pending`, `approved` (with `row_count`), `rejected` (with the reviewer's `reason`), `stale` (the rows moved; propose again) or `failed`.

A person decides in a terminal with `sql-review review`, which goes through what is waiting, oldest first, showing each proposal and asking to approve, reject, skip or quit. `review --list` lists them, `review --approve ID` and `review --reject ID [--reason TEXT]` decide one, and `sql-review apply ID` and `sql-review show ID` work on stored proposals too. Approving applies it on the write account, with the checks above. Proposals wait in `pending/` and move to `decided/` under the state directory: `SQL_REVIEW_STATE`, else `$XDG_STATE_HOME/sql-review`, else `~/Library/Application Support/sql-review` on macOS and `~/.local/state/sql-review` elsewhere.

`profile` is an enum of the profiles configured in the environment and `.env`, and is required when there are several and no default. `--profile NAME` (repeatable) offers only those; `--env-file` works as on the CLI. `--read-only` offers no write tools, never reads the write account, and needs no state directory. Settings are read again on every call.

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

Under [Tracon](https://github.com/cosmicspork/tracon), the node installs a release archive and runs `bin/sql-review serve --socket <path> --data <dir>`. It serves `query`, `describe` and `write` over MCP (streamable HTTP) on that Unix socket, with credentials lent per call by the node, never read from the environment, a `.env` file or the OS keyring. A `write` runs only once the operator approves its card. See [The plugin contract](#the-plugin-contract) below.

## Configuration

Standalone, every value comes from the process environment or a `.env` file: `--env-file PATH`, else the first `.env` found walking up from the working directory. An exported variable wins over the file, and an empty value counts as unset. A password left unset by both can come from the OS keyring (below).

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
| `DB_WRITE_USER`, `DB_WRITE_PASSWORD` | The write account, used only to apply a proposed write, never to preview one. `--read-only` never reads it. SQLite has no logins: setting `DB_WRITE_USER` to anything is what lets `apply` open the file read-write. |
| `DB_PREVIEW_TIMEOUT` | Seconds reading a write's rows at propose may take, on the read-only account (default 10) |

**Profiles.** With `--profile NAME` (or `DB_PROFILE`), a `NAME_`-prefixed key such as `QA_DB_HOST` is tried before the bare `DB_HOST`, key by key, so one `.env` holds several databases. `--profile default` means the bare keys. For `sql-review mcp`, the profiles are every prefix with a `<P>_DB_*` or `<P>_DATABASE_URL` key, lowercased, plus `default` when bare keys configure one.

**The OS keyring.** In the CLI and `sql-review mcp`, a password that neither the environment nor `.env` gives (`DB_PASSWORD`, or a `DATABASE_URL` without one) is read from the OS keyring: service `sql-review`, account `<profile>` (`default` without a profile; the profile is `--profile`, else `DB_PROFILE`, lowercased). The write account's password, when `DB_WRITE_USER` is set and `DB_WRITE_PASSWORD` isn't, is account `<profile>-write`; under `--read-only` it is never looked up. SQLite has no logins and never asks. Nothing stored means no password, as before; a keyring that can't be read (no Secret Service running, say) is an error that names the entry and the key to set instead.

```sh
sql-review keyring set                      # prompts, unechoed; stores sql-review/default
sql-review keyring set --profile qa --write < qa-write-password.txt
sql-review keyring delete --profile qa
```

`keyring set` reads the password from stdin (one trailing newline is dropped), or prompts for it without echo on a terminal. Without `--profile` it uses `DB_PROFILE` from the environment or `.env`, so it stores what a query run in the same place reads. The keyring is the platform's own: the Secret Service on Linux (GNOME Keyring, KWallet; spoken to over D-Bus in pure Rust, so the static Linux build needs no system libraries), the login Keychain on macOS, and the Credential Manager on Windows. It is opened only when a password is looked up.

`serve` never opens the keyring: under Tracon every credential arrives with its call, and nothing on the host is read for one.

The keyring is the `keyring` Cargo feature, on by default. `cargo build --no-default-features` leaves it out (and its D-Bus dependencies with it); such a build answers `keyring set` with an error and never finds a password there.

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
lend = "approved"    # lent only to a call dispatched from an approved card

[[tool]]
name = "query"
hint = "read"

[[tool]]
name = "describe"
hint = "read"

[[tool]]
name = "write"
hint = "write"
consequential = true
```

### Start

The node runs `exec` with `--socket <path> --data <dir>` and an empty environment except `TRACON_PLUGIN_TOKEN`, `TRACON_NODE_KEY` (the node's ed25519 public key, lowercase hex) and `TRACON_CONTRACT=1`. `serve` refuses to start without all three. It keeps only the sha256 of the token, replaces a stale socket, makes the socket mode 0600, creates the data directory, and answers `initialize` at once: nothing connects to a database until a call.

### Credentials

Each call's credentials arrive in its caller context. `sql-read` is the default profile and `sql-read-<profile>` any other; their fields are the bare `DB_*` keys above (no prefixes). A call that names a profile whose credential wasn't lent fails with `credential sql-read-<profile> was not lent to this call`.

`sql-write` (and `sql-write-<profile>`) is the write account, and is read only by an approved `write`. Its fields are the same `DB_*` keys: `DB_USER` and `DB_PASSWORD` (or a `DATABASE_URL`'s login) are the write account. Connection fields it leaves out are taken from the profile's `sql-read`; any it gives must name the same database, or the write is refused. `query`, `describe` and every preview read with `sql-read` only, whatever else is lent.

### Writes: `tracon/preview` and an approved `write`

When an approval card for a `write` opens, the node sends `tracon/preview` with the call's `name`, `arguments` and a caller context (`approval_id` null; checks 1 to 6 apply, 7 does not). sql-review proposes the write as `propose` does, on `sql-read`, without running it, and answers:

```json
{ "previewable": true,
  "summary": "UPDATE apps SET score = 0 WHERE strm = '1261' (2 row(s))",
  "body": "<Markdown: the statement in a sql code block, the row count, and the rows as they are now as a table>",
  "token": "{\"count\":2,\"rows_hash\":\"<sha256 hex>\"}" }
```

Anything but a single-table `UPDATE` or `DELETE` answers `previewable: false`, `token: null`, and a body with the statement and why it can't be previewed. `query` and `describe` answer `previewable: false` with a one-line body. A preview sends nothing to the database but that read and writes nothing, not even the plugin's log. One that cannot be taken (no `sql-read` lent, a read refused as a write, the read failing) is error `-32003` with the reason; an unknown tool is `-32602`; any other `tracon/…` method is `-32601`.

The approved `tools/call` of `write` carries `approval_id` (check 7 refuses it without one), the `sql-write` credential, and `preview_token` when the preview returned one. With a token, sql-review applies against it as `apply` does: one transaction on the write account, the rows locked, refused if their count or hash moved, committed only if the statement changed that many rows. A refusal because they moved is JSON-RPC error `-32002` with `data: {"stale": true}`, and nothing is committed. Without a token (not previewable), it runs the statement in one transaction on the write account and commits. A token that doesn't parse, or one sent with a statement that has no rows to check, is `-32602`. Each applied or refused write is kept in the data directory as `decided/<id>.json` and logged to `log.jsonl` there, as standalone keeps them.

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
  "preview_token": "<the token of the preview the operator approved; optional>",
  "signature": "<lowercase hex ed25519 signature>"
}
```

`preview_token` is present only on a call dispatched from an approved card whose preview returned a token; absent and `null` mean the same. `caller.kind = "plugin"` is reserved for plugin-to-plugin calls through the node; it is accepted, and nothing sends it yet.

**Signing bytes:** the ASCII label `tracon/v1/plugin-call`, then a big-endian u32 length and the canonical JSON of the whole object with `signature` removed (the convention of `proto/src/admin.rs` in Tracon). `signature` is ed25519 over those bytes with the node key. sql-review computes them on the object as received, so a field it doesn't know is still covered.

**Canonical JSON:** object keys sorted by byte order, no whitespace, strings escaped as `serde_json` escapes them, `null` written out (absent and null differ), integers only. A float anywhere has no canonical form and refuses the call, so pass decimal parameters as strings. A call without `arguments` hashes as `{}`.

**Checks, in order.** Any failure is a JSON-RPC error (with `data.check`, the check's number) and no side effect:

1. `v == 1`, and `plugin` is `sql-review`.
2. `instance` is the sha256 of this process's `TRACON_PLUGIN_TOKEN`, so a context for an earlier spawn is refused.
3. `tool` and `arguments_sha256` match the call being served.
4. The signature verifies under `TRACON_NODE_KEY` (strict ed25519).
5. `issued_ms - 5000 <= now <= expires_ms + 5000`, and `expires_ms - issued_ms <= 60000`.
6. `nonce` is 32 lowercase hex characters and hasn't been seen; it is remembered until `expires_ms + 5000`. A call refused by checks 1–5 does not use up its nonce.
7. A write tool (`write`) needs a non-null `approval_id`. A `tracon/preview` runs checks 1 to 6 only.

A call without `_meta.tracon` is refused. A call that passes has its caller (kind, session, channel) logged on stderr, never its credentials or parameters.

There is no separate header check of the token: the instance hash inside the signed context carries it.

**Test vectors:** `tests/contract_vectors.json` is jira-review's file, copied verbatim, so both plugins and Tracon's node check against the same bytes: a fixed seed and its public key, one valid context with its signing bytes and signature, and refused contexts covering every check, each with the check that must refuse it. Its contexts are addressed to jira-review; the tests build sql-review's verifier for that plugin name. A signed field the vectors don't carry, such as `preview_token`, is covered by the signature and accepted.

## Checking Oracle

Nothing in this repository's tests reaches an Oracle database. To verify a profile:

```sh
sql-review check-oracle --profile prd            # or scripts/oracle-check.sh --profile prd
```

It connects, runs `SELECT 1 FROM DUAL`, runs a query of typed values through every format, describes a table (`--table`, default `DUAL`) by its bare name and then as `OWNER.TABLE` (the owner `--table` gives, else the first one the bare name found), runs a query inside `SET TRANSACTION READ ONLY` and rolls back, and binds a parameter, printing `PASS` or `FAIL` for each step. It exits 0 only if every step passes, and writes nothing.

## What is verified and what isn't

- **SQLite**: verified end to end in the test suite: the CLI (every format, describe, params, limits, profiles, `.env` discovery, `--read-only`, refusals), `mcp` over stdio, and `serve` over a Unix socket with signed contexts and lent credentials, including a write sent past the classifier failing on the read-only connection. Proposed writes are verified end to end on SQLite in `tests/writes.rs` and the `mcp` tests: an `UPDATE` proposed with its rows as they are now and applied; propose working with no write account on a database file nobody may write; a change to those rows after propose making apply refuse as stale (and a new matching row); apply checking the count read at propose; an `ALTER`, an `INSERT` and DML with a subquery proposed as not previewable, left unrun, and applied as written; the read's time limit; a write sent straight to the read path failing on the read-only connection with the write account configured; `review` approving, rejecting and refusing stale, interactively and by flag; and `mcp --read-only` listing no write tools. The plugin's `write` is verified in `tests/serve.rs`: an `UPDATE` previewed with its rows, count and token and then applied; a change between preview and apply refused as stale (`-32002`, `data.stale`); an `ALTER` not previewable and applied once approved; `write` refused without an approval; a preview that works on a read-only file with an unusable `sql-write` lent and fails with only `sql-write` lent; a write sent through `query` with `sql-write` lent still refused and failing read-only; malformed tokens; and `sql-write` naming another database.
- **The contract**: verified against jira-review's test vectors (shared with Tracon's node) and the `serve` tests. The node side is being written in parallel; until it calls this build, the wire format is checked only against itself.
- **SQL Server**: integration tests in `tests/mssql.rs` run only when `SQL_REVIEW_TEST_MSSQL_URL` points at a server; they have not been run yet. The `:name` placeholder rewrite is unit-tested. Proposed writes are written (the rows read on the read-only login at `READ COMMITTED` in a rolled-back transaction, the read's time limit by dropping the connection, then the write login, `XACT_ABORT ON` and `UPDLOCK, HOLDLOCK` at apply) and compile, but are **unverified**: no test reaches a server. The statement's row count comes from the server's done tokens, so a trigger that changes other rows may make the counts differ, and apply then refuses as stale rather than commit.
- **Oracle**: built on Oracle's pure-Rust driver (`oracledb`, pinned to `26.0.0-beta.6`) and unit-tested where it can be (connect strings, number mapping), but **unverified against a database** until `check-oracle` is run against one. Proposed writes (the rows read on the read-only account inside `SET TRANSACTION READ ONLY` under the driver's call timeout, then the write account with autocommit off and `FOR UPDATE` at apply) compile and their queries are unit-tested. One single-table `UPDATE` was proposed, applied, refused as stale and reverted on a test database with 0.2, when propose still ran the statement; this build's propose, which never runs it, has not yet been run on Oracle. Row-level security that shows the read-only account fewer rows than the write account would make apply refuse as stale.
- **The OS keyring**: the lookup order, profile and `-write` accounts, `--read-only`, SQLite, and `keyring set`/`delete` are tested against keyring-core's in-memory mock store, and a test shows a plugin call resolving the same unlent password to none while standalone reads it from the keyring. The Linux Secret Service store was exercised by hand (set, a query's header showing the password found, delete) with a throwaway entry, from both the glibc build and the static musl build. The macOS Keychain and Windows stores are unverified; sql-review does not build for Windows today (`serve` needs Unix sockets).
- **Release builds**: both Linux targets have been built locally the way the release workflow builds them (`cargo zigbuild`, zig 0.15.2) and come out statically linked; the x86_64 binary was run, the aarch64 one only built. The `aarch64-apple-darwin` archive is unverified until the first tag builds it.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test
```

## Releases

Releases are cut with [release-please](https://github.com/googleapis/release-please). Each merge to `main` updates a release PR that bumps the version in `Cargo.toml`, `Cargo.lock` and `tracon-plugin.toml` together and writes `CHANGELOG.md` from the conventional-commit PR titles (`feat` bumps the minor version, `fix` the patch, and before 1.0 a breaking change bumps the minor). Merging the release PR tags `v<version>`, creates the GitHub release, and the release workflow attaches `sql-review-<version>-<target>.tar.gz` for each target with `checksums.txt` and build provenance attestations.

## Licence

MIT.
