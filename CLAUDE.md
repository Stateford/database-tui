# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`dbtui` (crate `database-tui`): a ratatui terminal UI for exploring and editing PostgreSQL, MySQL/MariaDB and SQLite databases. Rust edition 2024, MSRV 1.94. See README.md for user-facing features, keys and config format.

## Commands

```sh
cargo build                                   # binary is target/debug/dbtui
cargo run -- ./some.db --read-only            # open a SQLite file directly
cargo test                                    # all tests (no external DB needed)
cargo test <name_substring>                   # single test, e.g. cargo test introspection_and_privileges
cargo test db::tests                          # one module's tests
cargo clippy --all-targets -- -D warnings     # CI treats warnings as errors
cargo fmt --check                             # max_width = 120, use_small_heuristics = "Max"
cargo audit                                   # known vulnerabilities in Cargo.lock
```

CI (`.github/workflows/ci.yml`) runs fmt (Linux only), clippy with `-D warnings`, and tests on Linux, macOS and Windows, all with `--locked`, plus a `cargo audit` job (also weekly). It has a read-only token, checkout does not keep credentials, and actions are pinned to commit SHAs (Dependabot updates them): keep it that way when editing the workflow.

Set `DBTUI_CONFIG_DIR` to a scratch directory when running manually so the real config/vault aren't touched.

## Architecture

**Event loop** (`src/main.rs`): a single `tokio::select!` over crossterm key/paste events, an `mpsc` channel of `AppEvent`s, a 100 ms tick and stop signals (SIGTERM/SIGHUP). Every iteration redraws via `ui::draw(frame, &mut app)`. The UI thread never awaits database work. After the loop, everything dbtui still waits for (the keyring, closing the pool, bounded by `CLOSE_TIMEOUT` after the terminal is restored) goes through `Signals::run_until`, so SIGTERM/SIGHUP keep ending the process; the runtime is shut down with a timeout so blocking work cannot hold up exit. `src/terminal.rs` sets the terminal up and always restores it (quit, signal, or a panic on the UI thread, via dbtui's own panic hook); a panic on another thread leaves the screen alone and is reported by the app.

**App state** (`src/app/mod.rs`): `App` owns everything — `screen` (Connections | Browser), an optional modal `popup` (an enum of every dialog), the vim `:` command line, toast, keymap state, the unlocked `Vault`, and `browser: Option<Browser>` once connected. Key routing order in `on_key`: popup → command line → current screen. Database operations are started with `tokio::spawn`, which clone the `Database` and send their result back as an `AppEvent`, handled in `on_app_event`. Wrap the work in `guarded` (`terminal::catch_panic`) so that a panic still sends the event, as an error; otherwise the app waits for it forever. Connect attempts carry an attempt id (`App::connecting`; stale `Connected` pools are closed). Every request on a connection carries the connection's `Browser::generation` plus a monotonically increasing `seq` (`Browser::seq`, `schema_seq`, `commit_seq`, `QueryState::seq`); responses whose generation or `seq` is stale are dropped, so always carry and check both when adding new async fetches.

**Keymaps** (`src/keymap.rs`): raw keys are never matched directly in feature code. `Keymap::resolve(mode, Context, key)` maps a key to an `Action` per context (Connections, Sidebar, Grid, Structure, …) for both the Standard and Vim schemes, including multi-key sequences (`gg`, `dd`, `yy`). `hints()` produces the footer hints and hides actions the current `Privileges` don't allow. Adding a feature usually means: new `Action` variant → bindings for both schemes → hint → handling in `App::*_action` → entry in `src/ui/help.rs`.

**Staged edits** (`src/app/grid.rs`): `Grid` holds the fetched rows plus an `Edits` overlay (changed cells, deleted rows, inserted rows) with an undo stack. Nothing hits the database until commit: `build_statements` turns the overlay into `Statement`s, the Commit popup previews them, and `Database::apply` runs them in one transaction. Updates/deletes are keyed by primary key and flagged `expect_one`; if any affects ≠ 1 row the whole transaction rolls back (`check_affected`). Key values are bound as they were read (`ResultSet::kinds` / `CellKind` → `sql::Value`: SQLite storage classes, bytes), never as display text. Edits are refused while the table is loading or a commit runs (`App::table_busy`); the cell editor targets a row by key (`EditTarget`), and the Commit popup and in-flight commit carry a `CommitSnapshot` (generation, table, `Grid::id`, edits) so only what was previewed is committed and cleared.

**Database layer** (`src/db/`):
- `mod.rs`: `Database` is an enum over sqlx pools (`Postgres`, `MySql`, `Sqlite`) with driver-dispatching methods (`connect`, `session_info`, `schemas`, `structure`, `fetch_page`, `count_rows`, `run_query`, `apply`). Values are decoded per driver via the `CellText` trait to `Option<String>` plus a `CellKind` (what the text stands for: SQLite storage class, bytes, not exact, or `Truncated`: only the first `CELL_BYTES` were kept, less past `RESULT_BYTES` per result; such a cell is never edited or used as a key); user-typed parameters are bound as text and cast by the server, key values with their own type (`sql::Value`).
- `postgres.rs` / `mysql.rs` / `sqlite.rs`: per-driver introspection and permission detection (session read-only state, per-table privileges).
- `grants.rs`: parses MySQL `SHOW GRANTS` output.
- `sql.rs`: pure SQL generation and inspection (identifier quoting per driver and dialect settings, page/count/insert/update/delete builders, `write_keyword(driver, sql)` statement classification, filter validation). Kept DB-free so it is unit-testable.

**Permissions are enforced in layers**: profile `read_only` (and `--read-only`, `App::read_only_run`, for every connection of the run) is applied server-side at connect; `SessionInfo`/`Privileges` gate edit actions in the app and hints in the UI; in the SQL editor a read-only session rejects write statements via `sql::write_keyword(driver, sql)` before sending; and `run_query`, `fetch_page` and `count_rows` (table filters are user SQL) take `read_only` (`Browser::fully_read_only`) and then run inside a read-only transaction that is rolled back (or `PRAGMA query_only` on SQLite). Preserve all layers when changing related code, and pass `read_only` from any new fetch path.

**UI** (`src/ui/`): stateless rendering of `App` (`ui::draw`), split by screen (connections, browser, grid) plus `popups.rs`, `help.rs`, and `theme.rs` (Tokyo Night–style palette). Every string that comes from a database, a server or the config (names, values, error text, versions) is drawn through the display-safety helpers in `src/util.rs`, never as is: `display_safe` / `ui::safe_line` (one line), `display_safe_lines` / `ui::safe_text`, `ident` for names (adds the `⚠` flag), `fit` to cut to a width without reading a whole large value. They show control, zero-width and bidi characters visibly instead of letting the terminal drop or apply them. `modal()` titles are made safe for you. Text going to the system clipboard goes through `clipboard::copy`, which escapes control characters.

**Config and secrets**: `src/config.rs` loads `config.toml` (0600, never contains passwords); each `ConnectionProfile` has a stable `id` used as the credential key. Other dbtui instances may share the files, so every change goes through `App::update_config` / `Config::update`, which re-read the file under a lock before applying it (the vault does the same for its entries). `src/secrets.rs` stores passwords in the OS keyring or, as fallback, an Argon2id + XChaCha20-Poly1305 vault (`vault.toml`) unlocked once per session (off the UI thread). Each stored password is bound to its `Endpoint` (driver, host, port, user) and must not be used for another one (`StoredSecret::password_for`). One saved by an older version (`unbound_password`), or one for a remote server the SSL mode does not authenticate (`ConnectionProfile::tls_warning`), is only sent after the user confirms (`App::send_saved_password`); re-pointing a profile in the form forgets its password. Keyring calls go through `Keyring`, which runs them in order on its own thread; results come back as `AppEvent`s whenever the keyring answers, and `CredentialOps` generations tell stale ones apart (they are cleaned up, not recorded). Passwords are held in `Zeroizing<String>`.

## Tests

- Unit tests live inline (`#[cfg(test)]`) in most modules, notably `db/sql.rs`, `keymap.rs`, `app/grid.rs`, `db/grants.rs`, `config.rs`, `secrets.rs`.
- `src/db/tests.rs`: end-to-end tests of the `Database` layer against a temporary SQLite file.
- `src/app/tests.rs`: drives the real `App` with synthetic key events against a temp SQLite DB, pumps `AppEvent`s with `wait_for`, and renders headlessly with ratatui's `TestBackend` to assert on screen text. Use this pattern for UI/interaction tests.
- PostgreSQL/MySQL paths have no live-server tests; their pure logic (SQL generation, grant parsing) is unit-tested. `src/db/transport_tests.rs` has wire-level fake servers (`fake_pg_server`, `fake_mysql_server`) that record what the client sends (TLS request, startup parameters, password) and then fail the login; app tests reuse `fake_pg_server` to check where saved passwords go. They never complete a connection: to test what happens after one works, inject `AppEvent::Connected` with `App::connecting` and a SQLite `Database` standing in.
- Credential tests use `app_in(dir, config)` (config and vault in a temp dir) with `secrets::fake::FakeKeyring` (in-memory; `hold`/`release` simulate a keyring waiting for its unlock prompt), never the OS keyring or the real config directory.
- Process-global state (environment variables, the panic hook, signals) is tested by re-running the test binary on that one test in a child process (see `pg_environment_is_never_used`, `panic_hook_restores_the_terminal_only_for_the_ui`). Paths in panic locations use the platform separator, and Windows consoles may not emit escape sequences: keep such assertions portable or `#[cfg(unix)]`.
