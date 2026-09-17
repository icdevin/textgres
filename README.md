# Textgres

![Textgres connection explorer, SQL editor, and query results](assets/textgres.png)

Textgres is a PostgreSQL exploration TUI built with
[Ratatui](https://ratatui.rs/). It provides a connection tree, SQL editor,
result table, expanded row viewer, saved scripts, and optional SSH access.
The installed command is `tg`.

Textgres is an early-stage project. It currently supports:

- Saved PostgreSQL connection profiles and passwords.
- Optional persistent SSH connections through the local OpenSSH client.
- An expandable `connection → database → schema → table` explorer.
- Table previews with primary-key-safe row editing.
- Multi-line SQL editing with live syntax highlighting.
- Multiple persistent SQL sessions, with a workspace for each profile/database.
- Saved SQL scripts.
- Bounded result tables with measured column widths.

## Requirements

- A current stable Rust toolchain.
- Network access to a PostgreSQL server.
- The OpenSSH `ssh` command when SSH is enabled.

## Install

Install the latest version from GitHub:

```sh
cargo install --git https://github.com/icdevin/textgres --locked
```

On Linux, this uses the system OpenSSL library. To compile and embed OpenSSL
instead:

```sh
cargo install --git https://github.com/icdevin/textgres --locked \
  --features vendored-openssl
```

Both commands install the `tg` executable. The vendored feature has no effect on
macOS, where the operating system TLS framework is used.

## Run from source

Run from the repository:

```sh
cargo run --release
```

Or install the executable locally:

```sh
cargo install --path .
tg
```

## Getting started

1. Press `n` in the Explorer to create a connection.
2. Enter the PostgreSQL settings. Use `Tab` to change fields and `Ctrl+S` to save.
3. Press `Enter` on the connection, database, and schema to expand them.
4. Press `Enter` on a table to load its first 200 rows.
5. Use `Tab` to move between the Explorer, SQL editor, and Results.
6. Press `F5` in the SQL editor to run SQL against the active database.

First expanding a connection opens a persistent SQL session for its default
database. First expanding another database opens its own session. Collapsing
a branch leaves its sessions open.

### SQL sessions and workspaces

The SQL editor is shared across all targets. Changing the connection or database
keeps the text, cursor, selection, and undo history, so you can run the same SQL
against multiple databases. Each profile/database pair keeps its own SQL session,
results, result selection, and operation status. Select a target again in the
Explorer, or use `Ctrl+PageUp` / `Ctrl+PageDown` to cycle through visited workspaces. In the Explorer, `●` and bold
text indicate a connected profile or database; `○` and regular text indicate no
open SQL session. A profile stays marked connected while any of its database
sessions remains open. Transaction state stays in the SQL title.
Workspaces stay in memory until exit; save SQL scripts with `Ctrl+S` to keep them
across launches.

Expansion keeps the SQL connection open. Running SQL on a new target also opens
its session if needed. `BEGIN`, `COMMIT`, `ROLLBACK`, temporary tables, and session
settings therefore work across separate
executions. Different workspaces can run operations concurrently. Each workspace
allows one operation at a time; `Esc` cancels only that workspace's operation.

- `F6`: Connect. An existing connection is kept.
- `F7`: Disconnect the selected SQL session; keep its editor and results.
- `F8`: Reconnect with a new connection. Temporary tables and session settings
  are lost.

After a disconnect or connection failure, SQL execution does not reconnect
automatically, including when expanding the Explorer again. Use `F6` or `F8`.
Failed connection attempts also require an explicit retry. A lost connection cannot restore an uncommitted transaction.
After an uncertain write outcome, check the data before retrying.

The SQL title shows the selected target, with an additional label for an open,
failed, or unknown transaction, or a disconnected/lost session. Normal autocommit
has no extra label. Disconnect, reconnect, and quit require confirmation
if a transaction is open, failed, or unknown. Quit also checks running operations
in inactive workspaces. Confirm with `Y`; `N` or `Esc` keeps working. Closing a
connection rolls back its open transaction. To commit instead, cancel the dialog
and execute `COMMIT` first. A failed transaction requires `ROLLBACK` or recovery
to a valid savepoint.

Explorer queries, table previews, and row edits use separate connections. **Row
edits commit independently of the SQL editor's transaction.** Previews do not see
its uncommitted changes or temporary tables. Use SQL in the same workspace to
inspect or change data within that transaction. Disconnect all SQL sessions for a
profile and finish its operations before editing or deleting the profile.

Transaction state is read from `pg_catalog.pg_stat_activity` through a short-lived
observer connection after SQL operations. This avoids adding statements to the
user's transaction, including a failed transaction. If observation fails or
activity tracking is disabled, the UI reports unknown state and requires
confirmation before closing. Allow capacity for these additional connections.
Persistent session features require a direct PostgreSQL connection or a proxy
that preserves backend affinity; transaction pooling cannot provide that guarantee.

## Connections

The PostgreSQL section contains the database host, port, default database, user,
password, and TLS setting. When no password is saved, Textgres uses `PGPASSWORD`
when that environment variable exists.

TLS uses the operating system trust store and verifies the PostgreSQL server
certificate. The TLS toggle does not configure client certificates.

### SSH

Enable the SSH section when PostgreSQL is reachable through an SSH host:

- **Host**: SSH server or an alias from `~/.ssh/config`.
- **Port**: SSH server port, usually `22`.
- **User**: SSH account name.
- **Identity file**: Optional path to a private key, such as
  `~/.ssh/id_ed25519`. Do not select the `.pub` file.

The host and port in the PostgreSQL section remain the PostgreSQL endpoint as seen
from the SSH server. Do not replace them with the local forwarded address.

Textgres starts one OpenSSH connection per profile and reuses it until the profile
changes, is deleted, or Textgres exits. OpenSSH configuration such as `ProxyJump`
applies when the SSH host is a configured alias.

SSH is non-interactive. Authentication must already work through `ssh-agent`,
OpenSSH configuration, or the selected identity file. Encrypted keys must be loaded
into `ssh-agent`. SSH password and key-passphrase prompts are not supported.

Textgres requires an existing trusted host key. Connect with `ssh` once before using
the profile if the SSH host is not yet in `known_hosts`.

## Keyboard controls

The bottom bar shows controls for the active pane or dialog. `^` means `Ctrl`.

| Context | Keys | Action |
| --- | --- | --- |
| Global | `Tab` / `Shift+Tab` | Select the next or previous pane |
| Global | `Ctrl+Q` | Quit |
| Global | `Ctrl+PageUp` / `Ctrl+PageDown` | Previous or next visited workspace |
| Global | `F6` / `F7` / `F8` | Connect / disconnect / reconnect selected SQL session |
| Database operation | `Esc` | Cancel the current database operation |
| Explorer | `↑` / `↓`, `j` / `k` | Move selection |
| Explorer | `Home` / `End`, `g` / `G` | Select the first or last item |
| Explorer | `Enter`, `Space`, `→` | Expand or activate |
| Explorer | `←` | Collapse |
| Explorer | `n` / `e` / `d` | New, edit, or delete a connection |
| Explorer | `Ctrl+Left` / `Ctrl+Right` | Resize the Explorer |
| SQL | `F5` or `Ctrl+Enter` | Run SQL |
| SQL | `Ctrl+S` | Save the editor contents as a script |
| SQL | `Ctrl+L` | Load a saved script |
| SQL | `Ctrl+Up` / `Ctrl+Down` | Resize the SQL editor |
| Results | `↑` / `↓`, `j` / `k` | Move through rows |
| Results | `←` / `→`, `h` / `l` | Scroll through columns |
| Results | `Home` / `End`, `g` / `G` | Select the first or last row |
| Results | `Enter` | Open the selected row |
| Row viewer | `↑` / `↓`, `j` / `k` | Select a field |
| Row viewer | `Enter` | Edit an editable field |
| Row viewer | `Ctrl+N` | Toggle the selected value between text and `NULL` |
| Row viewer | `Ctrl+S` | Save changes and refresh the table preview |
| Row viewer | `Esc` | Finish field editing or close the viewer |

Connection forms use `Tab`, `Enter`, or `Down` for the next field and
`Shift+Tab` or `Up` for the previous field. Use `Space` on the TLS and SSH
**Enabled** fields. `Ctrl+S` saves; `Esc` cancels.

The script picker uses the arrow keys and `Enter`. Script names can contain only
letters, numbers, `-`, and `_`.

`Ctrl+Enter` requires enhanced keyboard reporting. Textgres enables that protocol
when supported. Use `F5` when `Ctrl+Enter` is not distinguishable from `Enter`.

## Data storage

Textgres stores `connections.toml` and a `scripts` directory in the platform
application-data directory. Set `TEXTGRES_DATA_DIR` to use another location:

```sh
TEXTGRES_DATA_DIR=/path/to/textgres-data tg
```

Saved PostgreSQL passwords are plain text in `connections.toml`. On Unix, Textgres
sets that file to owner-only permissions (`0600`). SSH private keys are not copied;
only the configured identity-file path is saved.

## Behavior and safety

- Connections start with a 30-second statement timeout. SQL sessions can change
  their own timeout with `SET statement_timeout`; previews and row edits keep the
  default timeout.
- Custom SQL results keep at most 500 rows.
- Table previews keep at most 200 rows.
- Table and view previews show `Results · schema.object` in the pane title.
  Free-form SQL uses `SQL results` because it may have no single source object.
- Direct table previews are editable only when the table has a primary key.
- Saving or deleting a connection clears its table preview and row edits. Load
  the table again after a connection change before editing rows.
- Generated columns, views, tables without primary keys, and custom SQL results are
  read-only in the row viewer.
- Row updates use typed parameters and match the original key and edited values.
  A conflicting concurrent change causes the update to fail instead of being
  overwritten.
- Custom SQL is unrestricted and can modify or delete data.

Press `Esc` to request cancellation. Textgres displays `Cancelling…` and waits
for the query outcome. If the query finishes first, its success is retained.
If cancellation cannot be confirmed within five seconds, Textgres closes the
connection and reports that the write outcome is unknown. Check the data before
retrying a write with an unknown outcome.

Cancellation does not undo earlier committed statements. If a row update has
already committed and only its preview refresh is cancelled, Textgres reports
`Row saved; refresh cancelled`. A cancelled statement inside an explicit transaction
usually leaves it failed; execute `ROLLBACK` before continuing. Normal exit requests
cancellation for every running workspace, waits for their outcomes, then closes
all SQL connections.

The 500-row limit bounds displayed data. Textgres continues reading the response
to receive later errors and determine whether the query completed.

## SSH troubleshooting

If an SSH connection fails:

- Confirm the system `ssh` command is installed.
- Textgres cannot use interactive SSH password authentication.
- Confirm the private key is loaded with `ssh-add -l`, or set its full path in the
  Identity file field.
- Confirm `ssh user@host` succeeds without an interactive prompt.
- Confirm the PostgreSQL host and port are reachable from the SSH server.
- Read the complete SSH or PostgreSQL diagnostic in the Textgres status area.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Database regression tests create disposable local PostgreSQL clusters. Install
`initdb`, `pg_ctl`, and `postgres` on `PATH`, then run these tests as a non-root
user. The TLS test also requires the `openssl` command:

```sh
cargo test postgres_ -- --ignored --test-threads=1
```

These tests use temporary directories and local ports. They do not connect to
saved profiles or existing databases.

Session tests cover transaction persistence and recovery, metadata isolation,
concurrent databases, cancellation, timeout, connection loss, reconnect, and
application shutdown. UI tests cover workspace ownership, background responses,
draft and undo preservation, profile changes, and transaction confirmation.

Session ownership is in `src/db/sessions.rs`. Workspace state and result handling
are in `src/app/workspace.rs`; switching and lifecycle actions are in
`src/app/sessions.rs`.

Textgres is licensed under [GPL-3.0-only](LICENSE).
