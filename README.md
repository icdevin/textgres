# Textgres

A PostgreSQL client for your terminal. Browse databases, run SQL, edit rows,
and save scripts. Supports multiple connections and SSH tunnels.

![Textgres connection explorer, SQL editor, and query results](assets/textgres.png)

## Install

Requires a stable Rust toolchain.

```sh
cargo install --git https://github.com/icdevin/textgres --locked
tg
```

On Linux, install the OpenSSL development libraries or add
`--features vendored-openssl` to the install command.

## Get started

1. Press `n` in the Explorer to add a connection. Use `Tab` to move between
   fields and `Ctrl+S` to save.
2. Press `Enter` to expand connections, databases, and schemas. Select a table
   to preview its rows.
3. Use `Tab` to switch panes. In the SQL editor, enter a query and press `F5`.
4. Scroll to the bottom of a table preview or supported SELECT result to load
   the next 200 rows.

Switching databases keeps your SQL text. Each database keeps its own results.
Connected entries show `●` and bold text. Use `c` to connect or disconnect;
on a connection row, disconnect closes all its database sessions.

## Shortcuts

The bottom bar shows available actions. `^` means `Ctrl`.

| Where | Key | Action |
| --- | --- | --- |
| Anywhere | `Tab` / `Shift+Tab` | Switch panes |
| Anywhere | `Ctrl+PageUp` / `Ctrl+PageDown` | Switch database workspaces |
| Anywhere | `Ctrl+Q` | Quit |
| Running operation | `Esc` | Cancel |
| Explorer | `n` / `e` / `d` | Add / edit / delete a connection |
| Explorer | `c` | Connect / disconnect |
| SQL | `F5` | Run SQL |
| SQL | `Ctrl+S` / `Ctrl+L` | Save / load a script |
| Results | `n` / `e` / `d` | Add / edit / mark a row for deletion |
| Results | `Ctrl+S` / `Ctrl+Z` | Save / discard all pending changes |
| Results | `F5` | Refresh the table preview |

## Edit rows

Open a table from the Explorer. In the row editor, press `Enter` to edit a field
and `Ctrl+S` to stage the row. Back in Results, press `Ctrl+S` to save the whole
batch, or `Ctrl+Z` to discard it.

New rows are **green**, deleted rows **red**, and changed cells **yellow**.
Use `Ctrl+N` for `NULL`, or `Ctrl+D` for a new field's database default.
Updates and deletes require a primary key; views and custom SQL results are read-only.
Table edits save separately from transactions you start in the SQL editor.

## Connections

For SSH, configure a host, user, and optional private key in the connection form.
Make sure `ssh` works without a password prompt and the host key is already trusted.

Saved passwords are stored in plain text in `connections.toml`. Leave the password
blank to use `PGPASSWORD`. Set `TEXTGRES_DATA_DIR` to choose a different directory
for saved connections and scripts.

[GPL-3.0-only](LICENSE)
