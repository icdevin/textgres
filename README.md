# Textgres

Textgres is a PostgreSQL-specific exploration TUI built with Ratatui.

The initial application supports:

- Saved connection profiles and passwords.
- An expandable `connection → database → schema → table` explorer.
- Table previews with an expanded row viewer and primary-key-safe editing.
- Multi-line SQL editing with live syntax highlighting and execution.
- Saved SQL scripts.
- Bounded result rendering with vertical and horizontal navigation.

## Run

Install a current stable Rust toolchain, then run:

```sh
cargo run
```

Textgres stores data in the platform application-data directory. Set
`TEXTGRES_DATA_DIR` to override that location. `PGPASSWORD` provides a password
when a connection does not have a saved password.

## Keys

| Context | Keys | Action |
| --- | --- | --- |
| Global | `Tab` / `Shift+Tab` | Change pane |
| Global | `Ctrl+Q` | Quit |
| Explorer | `j` / `k`, arrows | Move selection |
| Explorer | `Enter`, `Space`, right arrow | Expand or activate |
| Explorer | left arrow | Collapse |
| Explorer | `n` / `e` / `d` | New, edit, or delete connection |
| Explorer | `Ctrl+Left` / `Ctrl+Right` | Resize the explorer |
| SQL | `F5` or `Ctrl+Enter` | Run SQL against the active database |
| SQL | `Ctrl+S` | Save as a script |
| SQL | `Ctrl+L` | Open the saved-script picker |
| SQL | `Ctrl+Up` / `Ctrl+Down` | Resize the SQL editor |
| Results | `j` / `k`, arrows | Move through rows or columns |
| Results | `Enter` | Inspect the selected row |
| Row viewer | arrows, `Enter` | Select and edit a field |
| Row viewer | `Ctrl+N` / `Ctrl+S` | Toggle `NULL` or save the row |

Connection forms use `Tab` to move between fields and `Ctrl+S` to save. The TLS
field uses `Space` to toggle between disabled and required.

`Ctrl+Enter` requires a terminal with enhanced keyboard reporting. Textgres enables
the protocol automatically when the terminal supports it. `F5` works in terminals
that cannot distinguish `Ctrl+Enter` from plain `Enter`.

Konsole does not currently report this combination through that protocol. To map it,
open **Settings → Edit Current Profile → Keyboard → Edit** and add:

```text
key Return+Ctrl : "\E[13;5u"
```

This sends the standard enhanced-key sequence that Textgres reads as `Ctrl+Enter`.

## Safety boundaries

- Passwords are stored as plain text in the connection file. On Unix, Textgres
  restricts this file to the current user (`0600`).
- TLS uses the operating system trust store and verifies server certificates.
- Queries time out after 30 seconds.
- The UI keeps at most 500 rows from a custom query and 200 rows from a table preview.
- Direct table previews are editable only when the table has a primary key. Updates use the
  original key and edited values to detect conflicts. Custom SQL results and views are read-only.
- Custom SQL is unrestricted and can modify data.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
