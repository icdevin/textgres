use ratatui::{
  Frame,
  layout::{Alignment, Constraint, Direction, Layout, Rect},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{
    Block, Borders, Cell, Clear, List, ListItem, ListState, Padding, Paragraph, Row, Table,
    TableState, Wrap,
  },
};
use unicode_width::UnicodeWidthStr;

use crate::{
  app::{App, ConnectionField, ExplorerNode, ExplorerRow, Focus, NodeKey, Overlay},
  theme::THEME,
};

const MIN_COLUMN_WIDTH: usize = 8;
const MAX_COLUMN_WIDTH: usize = 48;

/// Draws the whole workspace from application state on each frame.
pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
  // Separate messages from controls so neither competes for horizontal space.
  let status_height = status_height(app, frame.area().width);
  let page = Layout::vertical([Constraint::Min(8), Constraint::Length(status_height + 1)])
    .split(frame.area());
  let footer =
    Layout::vertical([Constraint::Length(status_height), Constraint::Length(1)]).split(page[1]);
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
  draw_status(frame, app, footer[0]);
  draw_shortcuts(frame, app, footer[1]);

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
    explorer_item(row, &indentation, prefix)
  });
  // An empty connection list has no valid selected row.
  let mut state =
    ListState::default().with_selected((!rows.is_empty()).then_some(app.explorer_selected));
  let list = List::new(items)
    .block(pane_block(" Explorer ", app.focus == Focus::Explorer))
    .highlight_style(
      Style::default()
        .bg(THEME.selection)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("› ");
  frame.render_stateful_widget(list, area, &mut state);
}

// Color hierarchy levels by meaning so dense explorer trees remain scannable.
fn explorer_item(row: &ExplorerRow, indentation: &str, prefix: &str) -> ListItem<'static> {
  let mut spans = vec![Span::styled(
    format!("{indentation}{prefix}"),
    Style::default().fg(THEME.muted),
  )];
  match &row.node {
    ExplorerNode::Connection(_) => spans.push(Span::styled(
      row.label.clone(),
      Style::default().fg(THEME.cyan).add_modifier(Modifier::BOLD),
    )),
    ExplorerNode::Database { .. } => spans.push(Span::styled(
      row.label.clone(),
      Style::default().fg(THEME.accent),
    )),
    ExplorerNode::Schema { .. } => spans.push(Span::styled(
      row.label.clone(),
      Style::default().fg(THEME.purple),
    )),
    ExplorerNode::Table(table) => {
      let color = if table.kind.contains("view") {
        Some(THEME.green)
      } else if table.kind == "foreign table" {
        Some(THEME.yellow)
      } else {
        None
      };
      spans.push(Span::styled(
        table.name.clone(),
        color.map_or_else(Style::default, |color| Style::default().fg(color)),
      ));
      spans.push(Span::styled(
        format!("  [{}]", table.kind),
        Style::default().fg(THEME.muted).add_modifier(Modifier::DIM),
      ));
    }
  }
  ListItem::new(Line::from(spans))
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
  app
    .sql
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
        .style(Style::default().fg(THEME.muted))
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
  .style(
    Style::default()
      .fg(THEME.accent)
      .add_modifier(Modifier::BOLD),
  )
  .bottom_margin(1);
  let rows = app.result.rows.iter().map(|values| {
    Row::new(visible.iter().map(|(index, _)| {
      let value = values.get(*index).and_then(Option::as_deref);
      result_cell(value)
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
    .row_highlight_style(Style::default().bg(THEME.selection))
    .highlight_symbol("› ");
  frame.render_stateful_widget(table, area, &mut state);
}

// NULL has distinct semantics and should not look like the literal text "NULL".
fn result_cell(value: Option<&str>) -> Cell<'static> {
  match value {
    Some(value) => Cell::from(one_line(value)),
    None => Cell::from(Line::styled(
      "NULL",
      Style::default()
        .fg(THEME.yellow)
        .add_modifier(Modifier::ITALIC),
    )),
  }
}

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
  let color = if app.status_is_error {
    THEME.red
  } else if app.busy.is_some() {
    THEME.yellow
  } else {
    THEME.green
  };
  let prefix = if app.busy.is_some() { "● " } else { "" };
  frame.render_widget(
    Paragraph::new(format!("{prefix}{}", app.status))
      .style(Style::default().fg(color))
      .wrap(Wrap { trim: false })
      .block(Block::default().padding(Padding::horizontal(1))),
    area,
  );
}

// Grow error diagnostics while bounding their effect on the main workspace.
fn status_height(app: &App, area_width: u16) -> u16 {
  if !app.status_is_error {
    return 1;
  }
  let line_width = usize::from(area_width.saturating_sub(2).max(1));
  app
    .status
    .lines()
    .map(|line| line.width().max(1).div_ceil(line_width))
    .sum::<usize>()
    .clamp(1, 4) as u16
}

fn draw_shortcuts(frame: &mut Frame<'_>, app: &App, area: Rect) {
  let shortcuts = shortcuts(app);
  frame.render_widget(
    Paragraph::new(shortcut_line(&shortcuts))
      .alignment(Alignment::Right)
      .block(Block::default().padding(Padding::horizontal(1))),
    area,
  );
}

// Show only actions that apply to the active pane or modal dialog.
fn shortcuts(app: &App) -> Vec<(&'static str, &'static str)> {
  if let Some(overlay) = &app.overlay {
    return match overlay {
      Overlay::Connection(form) if form.selected_field().is_toggle() => {
        vec![
          ("Space", "toggle"),
          ("Tab", "field"),
          ("^S", "save"),
          ("Esc", "cancel"),
        ]
      }
      Overlay::Connection(_) => {
        vec![("Tab", "field"), ("^S", "save"), ("Esc", "cancel")]
      }
      Overlay::SaveScript { .. } => vec![("Enter", "save"), ("Esc", "cancel")],
      Overlay::LoadScript { .. } => {
        vec![("↑↓", "select"), ("Enter", "load"), ("Esc", "cancel")]
      }
      Overlay::RowDetail(form) if form.editing => {
        vec![("Esc", "done"), ("^N", "toggle NULL"), ("^S", "save row")]
      }
      Overlay::RowDetail(form) if !form.row_is_editable() => {
        vec![("↑↓", "field"), ("Esc", "close")]
      }
      Overlay::RowDetail(_) => vec![
        ("↑↓", "field"),
        ("Enter", "edit"),
        ("^N", "toggle NULL"),
        ("^S", "save row"),
        ("Esc", "close"),
      ],
      Overlay::ConfirmDelete { .. } => {
        vec![("Y/Enter", "delete"), ("N/Esc", "cancel")]
      }
    };
  }

  if app.busy.is_some() {
    // Hide inactive pane actions while one database task owns the workspace.
    return vec![("Esc", "cancel"), ("Tab", "pane"), ("^Q", "quit")];
  }

  match app.focus {
    Focus::Explorer => vec![
      ("↑↓", "move"),
      ("Enter/→", "open"),
      ("←", "close"),
      ("n", "new"),
      ("e", "edit"),
      ("d", "delete"),
      ("^←/^→", "width"),
      ("Tab", "pane"),
      ("^Q", "quit"),
    ],
    Focus::Sql => vec![
      ("F5/^Enter", "run"),
      ("^S", "save"),
      ("^L", "load"),
      ("^↑/^↓", "height"),
      ("Tab", "pane"),
      ("^Q", "quit"),
    ],
    Focus::Results => vec![
      ("↑↓", "rows"),
      ("←→", "columns"),
      ("Enter", "inspect"),
      ("Tab", "pane"),
      ("^Q", "quit"),
    ],
  }
}

// Accent key chords while leaving their action labels quiet.
fn shortcut_line(shortcuts: &[(&'static str, &'static str)]) -> Line<'static> {
  let mut spans = Vec::new();
  for (index, (key, action)) in shortcuts.iter().enumerate() {
    if index > 0 {
      spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(
      *key,
      Style::default()
        .fg(THEME.accent)
        .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(*action, Style::default().fg(THEME.muted)));
  }
  Line::from(spans)
}

fn draw_overlay(frame: &mut Frame<'_>, overlay: &Overlay, scripts: &[String]) {
  match overlay {
    Overlay::Connection(form) => {
      let area = centered(frame.area(), 70, 18);
      frame.render_widget(Clear, area);
      let title = if form.editing_id.is_some() {
        " Edit connection "
      } else {
        " New connection "
      };
      let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.accent))
        .title(title);
      let inner = block.inner(area);
      frame.render_widget(block, area);
      // Compact section boxes separate database and tunnel settings without blank rows.
      let sections = Layout::vertical([Constraint::Length(9), Constraint::Length(7)]).split(inner);
      let database_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.cyan))
        .title(" PostgreSQL ");
      let database_inner = database_block.inner(sections[0]);
      frame.render_widget(database_block, sections[0]);
      let ssh_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.purple))
        .title(" SSH ");
      let ssh_inner = ssh_block.inner(sections[1]);
      frame.render_widget(ssh_block, sections[1]);
      let mut fields = Layout::vertical([Constraint::Length(1); 7])
        .split(database_inner)
        .to_vec();
      fields.extend(
        Layout::vertical([Constraint::Length(1); 5])
          .split(ssh_inner)
          .iter()
          .copied(),
      );
      for (index, field) in ConnectionField::ALL.iter().copied().enumerate() {
        let selected = index == form.field;
        let value = match field {
          ConnectionField::RequireTls => if form.require_tls {
            "required"
          } else {
            "disabled"
          }
          .to_owned(),
          ConnectionField::SshTunnel => if form.ssh_enabled {
            "enabled"
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
          Style::default().fg(Color::White).bg(THEME.selection)
        } else {
          Style::default().fg(THEME.muted)
        };
        let label = format!("{:<24}", format!("{}:", field.label()));
        frame.render_widget(
          Paragraph::new(Line::from(vec![
            Span::styled(label, Style::default().fg(THEME.muted)),
            Span::styled(value, style),
          ])),
          fields[index],
        );
        if selected && !field.is_toggle() {
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
        .border_style(Style::default().fg(THEME.accent))
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
      let items = scripts.iter().map(|name| {
        ListItem::new(Line::styled(
          format!("{name}.sql"),
          Style::default().fg(THEME.cyan),
        ))
      });
      let mut state = ListState::default().with_selected(Some(*selected));
      let list = List::new(items)
        .block(
          Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(THEME.accent))
            .title(" Load script "),
        )
        .highlight_style(Style::default().bg(THEME.selection).fg(Color::White))
        .highlight_symbol("› ");
      frame.render_stateful_widget(list, area, &mut state);
    }
    Overlay::RowDetail(form) => draw_row_detail(frame, form),
    Overlay::ConfirmDelete { name, .. } => {
      let area = centered(frame.area(), 50, 5);
      frame.render_widget(Clear, area);
      frame.render_widget(
        Paragraph::new(Line::from(format!("Delete connection “{name}”?")))
          .alignment(Alignment::Center)
          .block(
            Block::default()
              .borders(Borders::ALL)
              .border_style(Style::default().fg(THEME.red))
              .title(" Confirm delete "),
          ),
        area,
      );
    }
  }
}

fn draw_row_detail(frame: &mut Frame<'_>, form: &crate::app::RowDetail) {
  let area = centered(
    frame.area(),
    92,
    frame.area().height.saturating_sub(4).max(8),
  );
  frame.render_widget(Clear, area);
  let source = form.source.as_ref();
  let mode = if form.row_is_editable() {
    "editable"
  } else {
    "read-only"
  };
  let source_name = source.map_or_else(
    || "custom result".to_owned(),
    |source| format!("{}.{}", source.table.schema, source.table.name),
  );
  let block = Block::default()
    .borders(Borders::ALL)
    .border_style(Style::default().fg(THEME.accent))
    .title(format!(
      " Row {} · {source_name} · {mode} ",
      form.row_index + 1
    ));
  let inner = block.inner(area);
  frame.render_widget(block, area);
  let panes =
    Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).split(inner);

  let items = form.columns.iter().enumerate().map(|(index, name)| {
    let metadata = source.and_then(|source| source.columns.get(index));
    let marker = if metadata.is_some_and(|column| column.primary_key) {
      "◆ "
    } else if metadata.is_some_and(|column| !column.editable) {
      "· "
    } else {
      "  "
    };
    let value = form.values.get(index).and_then(Option::as_deref);
    let value = value.map_or_else(|| "NULL".to_owned(), one_line);
    ListItem::new(Line::from(vec![
      Span::styled(
        format!("{marker}{name}"),
        Style::default()
          .fg(if metadata.is_some_and(|column| column.primary_key) {
            THEME.accent
          } else {
            THEME.cyan
          })
          .add_modifier(Modifier::BOLD),
      ),
      Span::styled(format!("  {value}"), Style::default().fg(THEME.muted)),
    ]))
  });
  let mut state = ListState::default().with_selected(Some(form.selected));
  let list = List::new(items)
    .block(Block::default().borders(Borders::RIGHT).title(" Columns "))
    .highlight_style(Style::default().bg(THEME.selection).fg(Color::White))
    .highlight_symbol("› ");
  frame.render_stateful_widget(list, panes[0], &mut state);

  let column = form
    .columns
    .get(form.selected)
    .map_or("Value", String::as_str);
  let value_block = Block::default().title(format!(" {column} "));
  let value_area = value_block.inner(panes[1]);
  frame.render_widget(value_block, panes[1]);
  if form.editing {
    frame.render_widget(&form.editor, value_area);
  } else if let Some(value) = form.selected_value() {
    frame.render_widget(Paragraph::new(value).wrap(Wrap { trim: false }), value_area);
  } else {
    frame.render_widget(
      Paragraph::new("NULL").style(
        Style::default()
          .fg(THEME.yellow)
          .add_modifier(Modifier::ITALIC),
      ),
      value_area,
    );
  }
}

fn pane_block(title: &str, focused: bool) -> Block<'static> {
  Block::default()
    .borders(Borders::ALL)
    .border_style(Style::default().fg(if focused { THEME.accent } else { THEME.muted }))
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
    .map(|value| {
      value
        .as_deref()
        .map_or("NULL".width(), |value| one_line(value).width())
    })
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
  use crate::{
    db::{ResultColumn, TableRef, TableResultSource},
    storage::ConnectionProfile,
  };

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
      ssh: None,
    };
    let mut app = App::new(
      storage,
      vec![profile],
      vec!["inspect_users".into()],
      runtime.handle().clone(),
      sender,
    );
    app.expanded.insert(NodeKey::Connection("local".into()));
    app
      .expanded
      .insert(NodeKey::Database("local".into(), "postgres".into()));
    app.expanded.insert(NodeKey::Schema(
      "local".into(),
      "postgres".into(),
      "public".into(),
    ));
    app
      .databases
      .insert("local".into(), vec!["postgres".into()]);
    app
      .schemas
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
    app.explorer_selected = 1;
    app.result.columns = vec!["id".into(), "email".into(), "note".into()];
    app.result.rows = vec![vec![Some("1".into()), Some("dev@example.com".into()), None]];
    // Source metadata makes the table-preview row safe to edit.
    app.result.source = Some(TableResultSource {
      table: TableRef {
        profile_id: "local".into(),
        database: "postgres".into(),
        schema: "public".into(),
        name: "users".into(),
        kind: "table".into(),
      },
      columns: vec![
        ResultColumn {
          name: "id".into(),
          type_name: "integer".into(),
          editable: true,
          primary_key: true,
        },
        ResultColumn {
          name: "email".into(),
          type_name: "text".into(),
          editable: true,
          primary_key: false,
        },
        ResultColumn {
          name: "note".into(),
          type_name: "text".into(),
          editable: true,
          primary_key: false,
        },
      ],
    });

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
    assert!(screen.contains("NULL"));
    assert!(screen.contains("^←/^→ width"));
    let cells = terminal.backend().buffer().content();
    assert!(cells.iter().any(|cell| {
      cell.symbol() == "L" && cell.fg == THEME.cyan && cell.modifier.contains(Modifier::BOLD)
    }));
    assert!(cells.iter().any(|cell| {
      cell.symbol() == "N" && cell.fg == THEME.yellow && cell.modifier.contains(Modifier::ITALIC)
    }));
    assert!(cells.iter().any(|cell| {
      cell.symbol() == "^" && cell.fg == THEME.accent && cell.modifier.contains(Modifier::BOLD)
    }));

    // Enter opens a full row viewer with source and edit state visible.
    app.focus = Focus::Results;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let row_detail = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|cell| cell.symbol())
      .collect::<String>();
    assert!(row_detail.contains("Row 1 · public.users · editable"));
    assert!(row_detail.contains("dev@example.com"));
    assert!(row_detail.contains("◆ id"));
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    // Explorer resize keys adjust the Ratatui layout without collapsing nodes.
    app.focus = Focus::Explorer;
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
    assert!(form.contains("Enabled:"));
    assert!(form.contains("Identity file:"));
    assert!(!form.contains("secret"));
    // The SSH section uses a distinct outline from PostgreSQL settings.
    assert!(
      terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .any(|cell| { matches!(cell.symbol(), "│" | "─") && cell.fg == THEME.purple })
    );

    // Errors and shortcuts occupy independent full-width footer rows.
    app.overlay = None;
    app.status = "SQL Error [42P01]: ERROR: relation \"derp\" does not exist\nPosition: 52".into();
    app.status_is_error = true;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let error = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|cell| cell.symbol())
      .collect::<String>();
    assert!(error.contains("SQL Error [42P01]: ERROR: relation \"derp\" does not exist"));
    assert!(error.contains("Position: 52"));
    assert!(error.contains("^Q"));
    assert_eq!(status_height(&app, 120), 2);
  }

  #[test]
  fn measures_result_columns_with_sane_bounds() {
    let result = crate::db::QueryResult {
      columns: vec!["id".into(), "description".into(), "payload".into()],
      rows: vec![vec![
        Some("1".into()),
        Some("medium value".into()),
        Some("x".repeat(100)),
      ]],
      ..Default::default()
    };

    assert_eq!(column_width(&result, 0), MIN_COLUMN_WIDTH);
    assert_eq!(column_width(&result, 1), "medium value".width() + 1);
    assert_eq!(column_width(&result, 2), MAX_COLUMN_WIDTH);
    assert_eq!(visible_columns(&result, 0, 30).len(), 2);
  }
}
