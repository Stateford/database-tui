# dbtui

A terminal UI for exploring and editing SQL databases, built with Rust and
[ratatui](https://ratatui.rs). It supports **PostgreSQL**, **MySQL / MariaDB** and **SQLite**.

![Browsing and editing a table](docs/screenshots/data.png)

## Features

- **Explore schemas**: schemas, tables, views and materialized views in a filterable tree.
  The Structure tab lists columns, types, defaults, keys and indexes.
- **Browse data**: a fast spreadsheet-style grid with paging, sorting, SQL `WHERE` filters,
  a full-value viewer (JSON is pretty-printed) and copy to the system clipboard (OSC 52).
- **Edit data safely**: edit cells, set `NULL`, add rows and delete rows. Changes are *staged*
  and highlighted, and you can undo them. Committing first shows a preview of the generated SQL,
  then runs everything in **one transaction**. Each update or delete targets exactly one row by
  primary key, and if any statement would touch a different number of rows, the whole commit is
  rolled back.
- **SQL editor**: a multi-line editor with a result grid. You can run the whole buffer or only
  the selection.
- **Vim or standard keys**: arrows/F-keys/Ctrl shortcuts by default, or a vim-style scheme
  (`hjkl`, `gg`/`G`, `dd`, `yy`, `p`, `u`, `:w`, `:q`, a modal SQL editor…). Switch with
  `Ctrl-K`, `:set vim` / `:set novim` or `--vim`. The choice is remembered.
- **Secure credentials**: passwords are never written to the config file. They go to the OS
  keyring (macOS Keychain, Windows Credential Manager, Secret Service on Linux). When no keyring
  is available (servers, containers, WSL), they go to an encrypted vault instead. The vault uses
  Argon2id for the key and XChaCha20-Poly1305 for the data, and is unlocked once per session
  with a master password.
- **Permission-aware**: the status bar always shows your access level and your privileges on the
  open table. Read-only users can browse and query, but cannot even start an edit. See below.

## Permissions

When you connect, dbtui works out what you are allowed to do:

| Database   | Session-wide read-only when…                                             | Per-table privileges from              |
|------------|--------------------------------------------------------------------------|----------------------------------------|
| PostgreSQL | `default_transaction_read_only` is on, or the server is a hot standby    | `has_table_privilege` / schema `USAGE` |
| MySQL      | `super_read_only`, `read_only` (for non-SUPER users), read-only sessions | `SHOW GRANTS` (including active roles) |
| SQLite     | the file is not writable, or `PRAGMA query_only` is set                  | everything if the file is writable     |

A profile can also be marked **Read-only**. The server then enforces it too:
`default_transaction_read_only` on PostgreSQL, `SET SESSION TRANSACTION READ ONLY` on MySQL, and
a read-only open on SQLite.

The status bar shows the result:

- `🔒 READ-ONLY`, `● READ-WRITE` or `● SUPERUSER` for the session;
- `SEL INS UPD DEL` for the open table, with privileges you don't have crossed out;
- a `ro` tag next to every table you cannot modify in the explorer.

Without the right privilege, the edit, delete and insert keys do nothing except show a short
explanation, and their hints disappear from the footer. Rows can only be edited or deleted when
the table has a primary key. In the SQL editor, a fully read-only session refuses any statement
that writes before sending it. Statements also run inside a read-only transaction as a second
safeguard.

## Screenshots

| Connection manager | Reviewing staged changes before commit |
|---|---|
| ![Connections](docs/screenshots/connections.png) | ![Commit preview](docs/screenshots/commit.png) |

A read-only account in the SQL editor: queries run, writes are refused before reaching the server.

![Read-only SQL editor](docs/screenshots/sql-read-only.png)

## Install

You need a recent stable Rust toolchain (1.94+).

```sh
cargo install --path .
# or
cargo build --release && ./target/release/dbtui
```

## Usage

```sh
dbtui                 # connection manager
dbtui "prod replica"  # open a saved connection by name
dbtui ./app.db        # open a SQLite file directly (add --read-only to be safe)
dbtui --vim           # use (and remember) vim keys
```

Press `n` in the connection manager to add a connection. `Ctrl-T` tests it and `Ctrl-S` saves it.
Press `?` or `F1` anywhere for the full key reference.

### Keys at a glance

| Action             | Standard            | Vim                  |
|--------------------|---------------------|----------------------|
| Move               | arrows, PgUp/PgDn   | `hjkl`, `Ctrl-d/u`   |
| First / last row   | `Ctrl-Home/End`     | `gg` / `G`           |
| Switch panel / tab | `Tab`, `1` `2` `3`  | `Tab`, `Ctrl-h/l`, `gt` |
| Next / prev page   | `]` / `[`           | `]` / `[`            |
| Filter / sort      | `/` / `Ctrl-O`      | `/` / `s`            |
| View / copy cell   | `Space` / `Ctrl-C`  | `v` / `yy`           |
| Edit cell          | `Enter`, `F2`       | `i`, `Enter`         |
| Set NULL           | `Backspace`         | `x`                  |
| New row / delete   | `Insert` / `Delete` | `o` / `dd`           |
| Undo staged change | `Ctrl-Z`            | `u`                  |
| Commit             | `Ctrl-S`            | `:w`                 |
| Run SQL            | `F5`, `Ctrl-E`      | `F5`, `Ctrl-E`       |
| Quit               | `Ctrl-Q`            | `:q`                 |

## Configuration

Settings live in `config.toml` in the platform config directory (for example
`~/.config/dbtui/` on Linux). Set `DBTUI_CONFIG_DIR` to use a different directory. The file is
created with `0600` permissions and never contains passwords.

```toml
keymap = "vim"            # or "standard"
page_size = 200           # rows per page
secret_backend = "auto"   # "auto" (keyring, then vault), "keyring" or "vault"

[[connection]]
id = "3f9c2a7e1b4d5c60"   # stable id, used as the credential key
name = "local postgres"
driver = "postgres"       # "postgres", "mysql" or "sqlite"
host = "localhost"
port = 5432
user = "app"
database = "app"
ssl_mode = "prefer"       # disable, prefer, require, verify-ca, verify-full
read_only = false
credential = "keyring"    # where the password is stored: none, keyring or vault
```

The encrypted vault (`vault.toml`) sits next to the config file.

## Development

```sh
cargo test     # unit tests plus end-to-end tests against temporary SQLite databases
cargo clippy --all-targets
cargo fmt
```

The code is organised as:

- `src/db/`: drivers, introspection, permission detection and SQL generation
- `src/app/`: application state, key handling, the staged-edit grid and the SQL editor
- `src/ui/`: rendering, with a Tokyo Night–inspired theme
- `src/secrets.rs`: OS keyring access and the encrypted vault
- `src/keymap.rs`: the standard and vim key schemes
