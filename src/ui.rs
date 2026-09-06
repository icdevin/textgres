use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
        Wrap,
    },
};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, ConnectionField, ExplorerNode, Focus, NodeKey, Overlay};

const ACCENT: Color = Color::Rgb(92, 173, 255);
const MUTED: Color = Color::Rgb(130, 140, 150);
const MIN_COLUMN_WIDTH: usize = 8;
const MAX_COLUMN_WIDTH: usize = 48;

/// Draws the whole workspace from application state on each frame.
pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let page = Layout::vertical([Constraint::Min(8), Constraint::Length(1)]).split(frame.area());
    // Give query editing and results more room than the compact connection tree.
    let work = Layout::horizontal([
        Constraint::Percentage(app.explorer_width_percent),
        Constraint::Percentage(100 - app.explorer_width_percent),
    ])
    .split(page[0]);
    let right = Layout::vertical([
        Constraint::Percentage(app.sql_height_percent),
        Constraint::Percentage(100 - app.sql_height_percent),
    ])
    .split(work[1]);

    draw_explorer(frame, app, work[0]);
    draw_sql(frame, app, right[0]);
    draw_results(frame, app, right[1]);
    draw_status(frame, app, page[1]);

    if let Some(overlay) = &app.overlay {
        draw_overlay(frame, overlay, &app.scripts);
    }
}

fn draw_explorer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = app.explorer_rows();
    let items = rows.iter().map(|row| {
        let prefix = match expandable_key(&row.node) {
            Some(key) if app.expanded.contains(&key) => "▾ ",
            Some(_) => "▸ ",
            None => "  ",
        };
        let indentation = "  ".repeat(row.depth);
        ListItem::new(format!("{indentation}{prefix}{}", row.label))
    });
    // An empty connection list has no valid selected row.
    let mut state =
        ListState::default().with_selected((!rows.is_empty()).then_some(app.explorer_selected));
    let list = List::new(items)
        .block(pane_block(" Explorer ", app.focus == Focus::Explorer))
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(42, 52, 64))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_sql(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let target = app
        .active_target
        .as_ref()
        .map(|(profile_id, database)| {
            let name = app
                .profiles
                .iter()
                .find(|profile| profile.id == *profile_id)
                .map_or(profile_id.as_str(), |profile| profile.name.as_str());
            format!(" SQL · {name}/{database} ")
        })
        .unwrap_or_else(|| " SQL · select a database target ".into());
    app.sql
        .set_block(pane_block(&target, app.focus == Focus::Sql));
    frame.render_widget(&app.sql, area);
}

fn draw_results(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = pane_block(" Results ", app.focus == Focus::Results);
    if app.result.columns.is_empty() {
        let text = if app.result.status.is_empty() {
            "Run SQL or select a table to see rows."
        } else {
            app.result.status.as_str()
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(MUTED))
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    // Size columns from visible data while bounding sparse IDs and large text values.
    let available = area.width.saturating_sub(4);
    let start = app
        .result_column
        .min(app.result.columns.len().saturating_sub(1));
    let visible = visible_columns(&app.result, start, available);
    let widths = visible
        .iter()
        .map(|(_, width)| Constraint::Length(*width))
        .collect::<Vec<_>>();
    let header = Row::new(
        visible
            .iter()
            .map(|(index, _)| Cell::from(app.result.columns[*index].as_str())),
    )
    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
    .bottom_margin(1);
    let rows = app.result.rows.iter().map(|values| {
        Row::new(visible.iter().map(|(index, _)| {
            Cell::from(
                values
                    .get(*index)
                    .map_or_else(String::new, |value| one_line(value)),
            )
        }))
    });
    let mut state = TableState::default();
    state.select(Some(app.result_row));
    let viewport_height = area.height.saturating_sub(4) as usize;
    if app.result_row >= viewport_height && viewport_height > 0 {
        *state.offset_mut() = app.result_row - viewport_height + 1;
    }
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(Style::default().bg(Color::Rgb(42, 52, 64)))
        .highlight_symbol("› ");
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let color = if app.status_is_error {
        Color::LightRed
    } else if app.busy.is_some() {
        Color::LightYellow
    } else {
        MUTED
    };
    let prefix = if app.busy.is_some() { "● " } else { "" };
    let shortcuts = shortcuts(app);
    let shortcut_width = shortcuts.width().min(usize::from(area.width)) as u16;
    let sections =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(shortcut_width)]).split(area);
    frame.render_widget(
        Paragraph::new(format!("{prefix}{}", app.status)).style(Style::default().fg(color)),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(shortcuts)
            .alignment(Alignment::Right)
            .style(Style::default().fg(MUTED)),
        sections[1],
    );
}

// Show only actions that apply to the active pane or modal dialog.
fn shortcuts(app: &App) -> String {
    if let Some(overlay) = &app.overlay {
        return match overlay {
            Overlay::Connection(form) if form.selected_field() == ConnectionField::RequireTls => {
                "Space toggle  Tab field  ^S save  Esc cancel".into()
            }
            Overlay::Connection(_) => "Tab field  ^S save  Esc cancel".into(),
            Overlay::SaveScript { .. } => "Enter save  Esc cancel".into(),
            Overlay::LoadScript { .. } => "↑↓ select  Enter load  Esc cancel".into(),
            Overlay::ConfirmDelete { .. } => "Y/Enter delete  N/Esc cancel".into(),
        };
    }

    match app.focus {
        Focus::Explorer => {
            "↑↓ move  Enter/→ open  ← close  n new  e edit  d delete  ^←/^→ width  Tab pane  ^Q quit"
                .into()
        }
        Focus::Sql => {
            "F5/^Enter run  ^S save  ^L load  ^↑/^↓ height  Tab pane  ^Q quit".into()
        }
        Focus::Results => "↑↓ rows  ←→ columns  Tab pane  ^Q quit".into(),
    }
}

fn draw_overlay(frame: &mut Frame<'_>, overlay: &Overlay, scripts: &[String]) {
    match overlay {
        Overlay::Connection(form) => {
            let area = centered(frame.area(), 66, 20);
            frame.render_widget(Clear, area);
            let title = if form.editing_id.is_some() {
                " Edit connection "
            } else {
                " New connection "
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(title);
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let fields = Layout::vertical([Constraint::Length(2); 7]).split(inner);
            for (index, field) in ConnectionField::ALL.iter().copied().enumerate() {
                let selected = index == form.field;
                let value = match field {
                    ConnectionField::RequireTls => if form.require_tls {
                        "required"
                    } else {
                        "disabled"
                    }
                    .to_owned(),
                    ConnectionField::Password => {
                        "•".repeat(form.value(field).unwrap_or_default().chars().count())
                    }
                    _ => form.value(field).unwrap_or_default().to_owned(),
                };
                let style = if selected {
                    Style::default().fg(Color::White).bg(Color::Rgb(42, 52, 64))
                } else {
                    Style::default().fg(MUTED)
                };
                let label = format!("{:<24}", format!("{}:", field.label()));
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(label, Style::default().fg(MUTED)),
                        Span::styled(value, style),
                    ])),
                    fields[index],
                );
                if selected && field != ConnectionField::RequireTls {
                    let cursor_x = fields[index]
                        .x
                        .saturating_add(24)
                        .saturating_add(form.cursor as u16)
                        .min(fields[index].right().saturating_sub(1));
                    let cursor_y = fields[index].y;
                    frame.set_cursor_position((cursor_x, cursor_y));
                }
            }
        }
        Overlay::SaveScript { name, cursor } => {
            let area = centered(frame.area(), 50, 5);
            frame.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(" Save script ");
            let inner = block.inner(area);
            frame.render_widget(block, area);
            frame.render_widget(
                Paragraph::new(name.as_str()).block(Block::default().title("Name (without .sql)")),
                inner,
            );
            frame.set_cursor_position((
                inner
                    .x
                    .saturating_add(*cursor as u16)
                    .min(inner.right().saturating_sub(1)),
                inner.y.saturating_add(1),
            ));
        }
        Overlay::LoadScript { selected } => {
            let height = (scripts.len() as u16).saturating_add(2).clamp(5, 20);
            let area = centered(frame.area(), 58, height);
            frame.render_widget(Clear, area);
            let items = scripts
                .iter()
                .map(|name| ListItem::new(format!("{name}.sql")));
            let mut state = ListState::default().with_selected(Some(*selected));
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(ACCENT))
                        .title(" Load script "),
                )
                .highlight_style(Style::default().bg(Color::Rgb(42, 52, 64)).fg(Color::White))
                .highlight_symbol("› ");
            frame.render_stateful_widget(list, area, &mut state);
        }
        Overlay::ConfirmDelete { name, .. } => {
            let area = centered(frame.area(), 50, 5);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(Line::from(format!("Delete connection “{name}”?")))
                    .alignment(Alignment::Center)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::LightRed))
                            .title(" Confirm delete "),
                    ),
                area,
            );
        }
    }
}

fn pane_block(title: &str, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
        .title(title.to_owned())
}

fn expandable_key(node: &ExplorerNode) -> Option<NodeKey> {
    match node {
        ExplorerNode::Connection(id) => Some(NodeKey::Connection(id.clone())),
        ExplorerNode::Database {
            profile_id,
            database,
        } => Some(NodeKey::Database(profile_id.clone(), database.clone())),
        ExplorerNode::Schema {
            profile_id,
            database,
            schema,
        } => Some(NodeKey::Schema(
            profile_id.clone(),
            database.clone(),
            schema.clone(),
        )),
        _ => None,
    }
}

fn centered(area: Rect, percent_x: u16, height: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height.min(area.height)),
        Constraint::Fill(1),
    ])
    .split(area);
    Layout::new(
        Direction::Horizontal,
        [
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ],
    )
    .split(vertical[1])[1]
}

// Database values may contain newlines, but each table row must remain one terminal row.
fn one_line(value: &str) -> String {
    value
        .replace('\n', "↵")
        .replace('\r', "")
        .replace('\t', "⇥")
}

// Returns as many measured columns as fit, always preserving one for narrow terminals.
fn visible_columns(
    result: &crate::db::QueryResult,
    start: usize,
    available: u16,
) -> Vec<(usize, u16)> {
    let mut visible = Vec::new();
    let mut used = 0usize;
    let available = usize::from(available.max(1));
    for index in start..result.columns.len() {
        let measured = column_width(result, index);
        let spacing = usize::from(!visible.is_empty());
        if !visible.is_empty() && used + spacing + measured > available {
            break;
        }

        // The first column shrinks below its minimum only when the pane is very narrow.
        let width = if visible.is_empty() {
            measured.min(available)
        } else {
            measured
        };
        used += spacing + width;
        visible.push((index, width as u16));
    }
    visible
}

// Measures terminal cells, not bytes, and adds one cell of visual breathing room.
fn column_width(result: &crate::db::QueryResult, index: usize) -> usize {
    let header_width = result.columns[index].width();
    let value_width = result
        .rows
        .iter()
        .filter_map(|row| row.get(index))
        .map(|value| one_line(value).width())
        .max()
        .unwrap_or(0);
    header_width
        .max(value_width)
        .saturating_add(1)
        .clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{db::TableRef, storage::ConnectionProfile};

    #[test]
    fn renders_the_complete_exploration_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let storage = crate::storage::Storage::new(directory.path().to_owned()).unwrap();
        storage.save_script("inspect_users", "select 42;").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (sender, _receiver) = mpsc::channel();
        let profile = ConnectionProfile {
            id: "local".into(),
            name: "Local PostgreSQL".into(),
            host: "localhost".into(),
            port: 5432,
            database: "postgres".into(),
            user: "postgres".into(),
            password: Some("secret".into()),
            require_tls: false,
        };
        let mut app = App::new(
            storage,
            vec![profile],
            vec!["inspect_users".into()],
            runtime.handle().clone(),
            sender,
        );
        app.expanded.insert(NodeKey::Connection("local".into()));
        app.expanded
            .insert(NodeKey::Database("local".into(), "postgres".into()));
        app.expanded.insert(NodeKey::Schema(
            "local".into(),
            "postgres".into(),
            "public".into(),
        ));
        app.databases
            .insert("local".into(), vec!["postgres".into()]);
        app.schemas
            .insert(("local".into(), "postgres".into()), vec!["public".into()]);
        app.tables.insert(
            ("local".into(), "postgres".into(), "public".into()),
            vec![TableRef {
                profile_id: "local".into(),
                database: "postgres".into(),
                schema: "public".into(),
                name: "users".into(),
                kind: "table".into(),
            }],
        );
        app.result.columns = vec!["id".into(), "email".into()];
        app.result.rows = vec![vec!["1".into(), "dev@example.com".into()]];

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(screen.contains("Local PostgreSQL"));
        assert!(screen.contains("users  [table]"));
        assert!(!screen.contains("Saved scripts"));
        assert!(!screen.contains("inspect_users.sql"));
        assert!(screen.contains("dev@example.com"));
        assert!(screen.contains("^←/^→ width"));

        // Explorer resize keys adjust the Ratatui layout without collapsing nodes.
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(app.explorer_width_percent, 23);
        assert!(app.expanded.contains(&NodeKey::Connection("local".into())));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.explorer_width_percent, 25);

        // Saved scripts appear only in the SQL pane's load dialog.
        app.focus = Focus::Sql;
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL));
        assert_eq!(app.sql_height_percent, 36);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.sql_height_percent, 34);
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let picker = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(picker.contains("Load script"));
        assert!(picker.contains("inspect_users.sql"));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.sql.lines(), &["select 42;"]);

        // The connection form must show defaults instead of covering them with labels.
        app.focus = Focus::Explorer;
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let form = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(form.contains("localhost"));
        assert!(form.contains("Password (saved):"));
        assert!(!form.contains("secret"));
    }

    #[test]
    fn measures_result_columns_with_sane_bounds() {
        let result = crate::db::QueryResult {
            columns: vec!["id".into(), "description".into(), "payload".into()],
            rows: vec![vec!["1".into(), "medium value".into(), "x".repeat(100)]],
            ..Default::default()
        };

        assert_eq!(column_width(&result, 0), MIN_COLUMN_WIDTH);
        assert_eq!(column_width(&result, 1), "medium value".width() + 1);
        assert_eq!(column_width(&result, 2), MAX_COLUMN_WIDTH);
        assert_eq!(visible_columns(&result, 0, 30).len(), 2);
    }
}
