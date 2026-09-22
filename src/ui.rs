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
  db::{SessionState, TransactionState},
  theme::THEME,
};

const MIN_COLUMN_WIDTH: usize = 8;
const MAX_COLUMN_WIDTH: usize = 48;

/// Draws the whole workspace from application state on each frame.
pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
  // Separate messages from controls so neither competes for horizontal space.
  let status_height = status_height(app, frame.area().width);
  let controls = shortcut_line(&shortcuts(app));
  let shortcut_height = controls
    .width()
    .div_ceil(usize::from(frame.area().width.saturating_sub(2).max(1)))
    .clamp(1, 3) as u16;
  let page = Layout::vertical([
    Constraint::Min(8),
    Constraint::Length(status_height + shortcut_height),
  ])
  .split(frame.area());
  let footer = Layout::vertical([
    Constraint::Length(status_height),
    Constraint::Length(shortcut_height),
  ])
  .split(page[1]);
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

  if let Some(overlay) = &app.workspace.overlay {
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
    explorer_item(row, &indentation, prefix, app.explorer_connected(&row.node))
  });
  // An empty connection list has no valid selected row.
  let mut state =
    ListState::default().with_selected((!rows.is_empty()).then_some(app.explorer_selected));
  let list = List::new(items)
    .block(pane_block(
      " Explorer ",
      app.workspace.focus == Focus::Explorer,
    ))
    .highlight_style(Style::default().bg(THEME.selection).fg(Color::White))
    .highlight_symbol("› ");
  frame.render_stateful_widget(list, area, &mut state);
}

// Color hierarchy levels by meaning so dense explorer trees remain scannable.
fn explorer_item(
  row: &ExplorerRow,
  indentation: &str,
  prefix: &str,
  connected: bool,
) -> ListItem<'static> {
  let mut spans = vec![Span::styled(
    format!("{indentation}{prefix}"),
    Style::default().fg(THEME.muted),
  )];
  match &row.node {
    ExplorerNode::Connection(_) | ExplorerNode::Database { .. } => {
      // Shape conveys connection state even when terminal color or bold is unavailable.
      spans.push(Span::styled(
        if connected { "● " } else { "○ " },
        Style::default().fg(if connected { THEME.green } else { THEME.muted }),
      ));
      let color = if matches!(row.node, ExplorerNode::Connection(_)) {
        THEME.cyan
      } else {
        THEME.accent
      };
      let style = Style::default().fg(color);
      spans.push(Span::styled(
        row.label.clone(),
        if connected {
          style.add_modifier(Modifier::BOLD)
        } else {
          style
        },
      ));
    }
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
      // Routine connection and paging state do not need title labels.
      match app.workspace.session_state {
        SessionState::Connected(TransactionState::Idle | TransactionState::Paging) => {
          format!(" SQL · {name}/{database} ")
        }
        state => format!(" SQL · {name}/{database} · {} ", state.label()),
      }
    })
    .unwrap_or_else(|| " SQL · select a database target ".into());
  app
    .sql
    .set_block(pane_block(&target, app.workspace.focus == Focus::Sql));
  frame.render_widget(&app.sql, area);
}

fn draw_results(frame: &mut Frame<'_>, app: &App, area: Rect) {
  // Use the result's known object, never the currently selected Explorer row or SQL text.
  let mut title = app.workspace.result.source.as_ref().map_or_else(
    || " SQL results ".to_owned(),
    |source| {
      format!(
        " {} · {}.{} ",
        if source.query.is_some() {
          "SQL results"
        } else {
          "Results"
        },
        source.table.schema,
        source.table.name
      )
    },
  );
  // Pending rows remain visible until the complete batch is saved or discarded.
  let changes = app.workspace.edits.count(&app.workspace.result);
  if changes > 0 {
    title = format!("{} · {changes} pending ", title.trim_end());
  }
  if app.workspace.edits.uncertain {
    title = format!("{} · save outcome unknown ", title.trim_end());
  }
  if app.workspace.result.page.is_some() {
    title = format!(
      "{} · {}+ rows ",
      title.trim_end(),
      app.workspace.result.rows.len() - app.workspace.edits.new_defaults.len()
    );
  }
  let block = pane_block(&title, app.workspace.focus == Focus::Results);
  if app.workspace.result.columns.is_empty() {
    let text = if app.workspace.result.status.is_empty() {
      "Run SQL or select a table to see rows."
    } else {
      app.workspace.result.status.as_str()
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
  // The result ordinal stays fixed at the left while database columns scroll horizontally.
  let number_width = app
    .workspace
    .result
    .rows
    .len()
    .max(1)
    .to_string()
    .len()
    .max(3) as u16;
  let available = area.width.saturating_sub(5 + number_width);
  let start = app
    .workspace
    .result_column
    .min(app.workspace.result.columns.len().saturating_sub(1));
  let visible = visible_columns(&app.workspace.result, start, available);
  let widths = std::iter::once(Constraint::Length(number_width))
    .chain(visible.iter().map(|(_, width)| Constraint::Length(*width)))
    .collect::<Vec<_>>();
  let header = Row::new(
    // Leave the row-number gutter untitled to distinguish it from database columns.
    std::iter::once(Cell::from("")).chain(
      visible
        .iter()
        .map(|(index, _)| Cell::from(app.workspace.result.columns[*index].as_str())),
    ),
  )
  .style(
    Style::default()
      .fg(THEME.accent)
      .add_modifier(Modifier::BOLD),
  )
  .bottom_margin(1);
  let rows = app
    .workspace
    .result
    .rows
    .iter()
    .enumerate()
    .map(|(row, values)| {
      Row::new(
        std::iter::once(Cell::from((row + 1).to_string()).style(Style::default().fg(THEME.muted)))
          .chain(visible.iter().map(|(index, _)| {
            let value = values.get(*index).and_then(Option::as_deref);
            // Keep cell foreground colors when the selection row changes its background.
            let edits = &app.workspace.edits;
            let defaults = edits.defaults(row);
            let color = if edits.deleted.contains(&row) {
              Some(THEME.red)
            } else if defaults.is_some() {
              Some(THEME.green)
            } else if edits.cell_changed(&app.workspace.result, row, *index) {
              Some(THEME.yellow)
            } else {
              None
            };
            if let Some(color) = color {
              let text = if defaults.is_some_and(|defaults| defaults[*index]) {
                "DEFAULT".into()
              } else {
                value.map_or_else(|| "NULL".into(), one_line)
              };
              Cell::from(text).style(Style::default().fg(color).add_modifier(Modifier::BOLD))
            } else {
              result_cell(value)
            }
          })),
      )
    });
  let mut state = TableState::default();
  state.select(Some(app.workspace.result_row));
  let viewport_height = area.height.saturating_sub(4) as usize;
  if app.workspace.result_row >= viewport_height && viewport_height > 0 {
    *state.offset_mut() = app.workspace.result_row - viewport_height + 1;
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
  let color = if app.workspace.status_is_error {
    THEME.red
  } else if app.workspace.busy.is_some() {
    THEME.yellow
  } else {
    THEME.green
  };
  let prefix = if app.workspace.busy.is_some() {
    "● "
  } else {
    ""
  };
  frame.render_widget(
    Paragraph::new(format!("{prefix}{}", app.workspace.status))
      .style(Style::default().fg(color))
      .wrap(Wrap { trim: false })
      .block(Block::default().padding(Padding::horizontal(1))),
    area,
  );
}

// Grow error diagnostics while bounding their effect on the main workspace.
fn status_height(app: &App, area_width: u16) -> u16 {
  if !app.workspace.status_is_error {
    return 1;
  }
  let line_width = usize::from(area_width.saturating_sub(2).max(1));
  app
    .workspace
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
      .wrap(Wrap { trim: true })
      .alignment(Alignment::Right)
      .block(Block::default().padding(Padding::horizontal(1))),
    area,
  );
}

// Show only actions that apply to the active pane or modal dialog.
fn shortcuts(app: &App) -> Vec<(&'static str, &'static str)> {
  if let Some(overlay) = &app.workspace.overlay {
    return match overlay {
      Overlay::Settings { .. } => vec![
        ("↑↓", "setting"),
        ("Space", "toggle"),
        ("^S", "save"),
        ("Esc", "cancel"),
      ],
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
        // Advertise deletion where the saved script is selected.
        vec![
          ("↑↓", "select"),
          ("Enter", "load"),
          ("d/Del", "delete"),
          ("Esc", "cancel"),
        ]
      }
      Overlay::RowDetail(form) if form.is_new && form.row_is_editable() => vec![
        ("Enter", "edit"),
        ("^N", "NULL"),
        ("^D", "DEFAULT"),
        ("^S", "stage row"),
        ("Esc", "done"),
      ],
      Overlay::RowDetail(form) if form.editing => {
        vec![("Esc", "done"), ("^N", "toggle NULL"), ("^S", "stage row")]
      }
      Overlay::RowDetail(form) if !form.row_is_editable() => {
        vec![("↑↓", "field"), ("Esc", "close")]
      }
      Overlay::RowDetail(_) => vec![
        ("↑↓", "field"),
        ("Enter", "edit"),
        ("^N", "toggle NULL"),
        ("^S", "stage row"),
        ("Esc", "close"),
      ],
      Overlay::ConfirmDelete { .. } | Overlay::ConfirmDeleteScript { .. } => {
        vec![("Y/Enter", "delete"), ("N/Esc", "cancel")]
      }
      Overlay::ConfirmRefresh => vec![("Y", "discard and refresh"), ("N/Esc", "cancel")],
      Overlay::ConfirmSession(_) | Overlay::ConfirmExplorerDisconnect { .. } => {
        vec![("Y", "confirm"), ("N/Esc", "cancel")]
      }
    };
  }

  if app.workspace.busy.is_some() {
    // Hide inactive pane actions while one database task owns the workspace.
    return vec![
      ("Esc", "cancel"),
      ("^PgUp/Down", "session"),
      ("Tab", "pane"),
      ("^Q", "quit"),
      ("F2", "settings"),
    ];
  }

  match app.workspace.focus {
    Focus::Explorer => vec![
      ("↑↓", "move"),
      ("Enter/→", "open"),
      ("←", "close"),
      ("n", "new"),
      ("e", "edit"),
      ("d", "delete"),
      (
        "c",
        if app.selected_connection_is_connected() {
          "disconnect"
        } else {
          "connect"
        },
      ),
      ("^←/^→", "width"),
      ("F2", "settings"),
      ("Tab", "pane"),
      ("^Q", "quit"),
    ],
    Focus::Sql => vec![
      ("F5/^Enter", "run"),
      ("F6", "connect"),
      ("F7", "disconnect"),
      ("F8", "reconnect"),
      ("^PgUp/Down", "session"),
      ("^S", "save"),
      ("^L", "load"),
      ("^↑/^↓", "height"),
      ("F2", "settings"),
      ("Tab", "pane"),
      ("^Q", "quit"),
    ],
    // Custom results expose update and explicit rerun actions without insertion/deletion hints.
    Focus::Results
      if app.workspace.refresh_query.is_some()
        || app
          .workspace
          .result
          .source
          .as_ref()
          .is_some_and(|source| source.query.is_some()) =>
    {
      vec![
        ("PgUp/Dn", "scroll"),
        ("↑↓", "rows"),
        ("←→", "columns"),
        ("e", "edit"),
        ("^S", "save all"),
        ("^Z", "discard all"),
        ("F5", "rerun query"),
        ("F2", "settings"),
        ("Tab", "pane"),
        ("^Q", "quit"),
      ]
    }
    Focus::Results => vec![
      ("PgUp/Dn", "scroll"),
      ("↑↓", "rows"),
      ("←→", "columns"),
      // Show short aliases to keep the bar compact; alternate bindings remain available.
      ("n", "add"),
      ("d", "delete"),
      ("e", "edit"),
      ("^S", "save all"),
      ("^Z", "discard all"),
      ("F5", "refresh"),
      ("F2", "settings"),
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
    Overlay::Settings { draft, selected } => {
      let area = centered(frame.area(), 80, 12);
      frame.render_widget(Clear, area);
      // Each option explains its scope; the list scrolls when terminal height is limited.
      let options = [
        (
          draft.show_all_databases,
          "Show all databases",
          "Off: only the database saved in each connection.",
        ),
        (
          draft.show_system_schemas,
          "Show system schemas",
          "pg_catalog, information_schema, other pg_* schemas.",
        ),
        (
          draft.show_utility_schemas,
          "Show utility schemas",
          "pg_temp_* and pg_toast* schemas.",
        ),
      ];
      let items = options.into_iter().map(|(enabled, label, description)| {
        ListItem::new(vec![
          Line::from(format!("[{}] {label}", if enabled { "x" } else { " " })),
          Line::styled(
            format!("    {description}"),
            Style::default().fg(THEME.muted),
          ),
          Line::from(""),
        ])
      });
      let list = List::new(items)
        .block(pane_block(" Settings · all connections ", true))
        .highlight_style(Style::default().bg(THEME.selection).fg(Color::White))
        .highlight_symbol("› ");
      let mut state = ListState::default().with_selected(Some(*selected));
      frame.render_stateful_widget(list, area, &mut state);
    }
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
    Overlay::ConfirmRefresh => {
      // Refresh must not silently replace staged values with database values.
      let area = centered(frame.area(), 70, 7);
      frame.render_widget(Clear, area);
      frame.render_widget(Paragraph::new("Discard pending changes and refresh the original results? If the last save outcome was unknown, verify the refreshed data before editing. Y: refresh · N/Esc: keep changes").wrap(Wrap { trim: false }).block(pane_block(" Refresh table? ", true).padding(Padding::uniform(1))), area);
    }
    Overlay::ConfirmExplorerDisconnect { label, .. } => {
      let area = centered(frame.area(), 70, 8);
      frame.render_widget(Clear, area);
      frame.render_widget(
        Paragraph::new(format!("Disconnect {label}? Open transactions will be rolled back; temporary tables and session settings will be lost. Y: disconnect · N/Esc: cancel"))
          .wrap(Wrap { trim: true })
          .block(pane_block(" Disconnect ", true)),
        area,
      );
    }
    Overlay::ConfirmSession(action) => {
      // Require an explicit Y so Enter cannot accidentally discard a transaction.
      let area = centered(frame.area(), 70, 9);
      frame.render_widget(Clear, area);
      frame.render_widget(
        Paragraph::new(action.confirmation())
          .wrap(Wrap { trim: false })
          .block(pane_block(" Close SQL session? ", true).padding(Padding::uniform(1))),
        area,
      );
    }
    Overlay::ConfirmDelete { name, .. } | Overlay::ConfirmDeleteScript { name, .. } => {
      // Use the same confirmation layout while identifying the exact deletion target.
      let prompt = match overlay {
        Overlay::ConfirmDeleteScript { .. } => format!("Delete script “{name}.sql”?"),
        _ => format!("Delete connection “{name}”?"),
      };
      let area = centered(frame.area(), 50, 5);
      frame.render_widget(Clear, area);
      frame.render_widget(
        Paragraph::new(prompt)
          .wrap(Wrap { trim: false })
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
    let value = if form.defaults[index] {
      "DEFAULT".to_owned()
    } else {
      value.map_or_else(|| "NULL".to_owned(), one_line)
    };
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
  } else if form.defaults[form.selected] {
    // DEFAULT is not NULL: the server supplies a generated value or column default on insert.
    frame.render_widget(
      Paragraph::new("DEFAULT").style(Style::default().fg(THEME.green)),
      value_area,
    );
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

  // Both standard and narrow terminals must show every setting and the save/cancel controls.
  #[test]
  fn renders_settings_dialog_and_shortcuts() {
    let directory = tempfile::tempdir().unwrap();
    let storage = crate::storage::Storage::new(directory.path().to_owned()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
      .build()
      .unwrap();
    let (sender, _receiver) = mpsc::channel();
    let mut app = App::new(storage, vec![], vec![], runtime.handle().clone(), sender);
    app.workspace.overlay = Some(Overlay::Settings {
      draft: app.settings,
      selected: 2,
    });
    for (width, height) in [(80, 24), (40, 20)] {
      let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
      terminal.draw(|frame| draw(frame, &mut app)).unwrap();
      let screen = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
      // Wrapped shortcut labels remain readable even when separated by terminal padding.
      let screen = screen.split_whitespace().collect::<Vec<_>>().join(" ");
      for label in [
        "[x] Show all databases",
        "[ ] Show system schemas",
        "[ ] Show utility schemas",
        "^S save",
        "Esc cancel",
      ] {
        assert!(screen.contains(label), "missing {label} at {width} columns");
      }
    }
  }

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
    app.active_target = Some(("local".into(), "postgres".into()));
    app.workspace.session_state =
      crate::db::SessionState::Connected(crate::db::TransactionState::Idle);
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
    app.workspace.result.columns = vec!["id".into(), "email".into(), "note".into()];
    app.workspace.result.rows = vec![vec![Some("1".into()), Some("dev@example.com".into()), None]];
    // Source metadata makes the table-preview row safe to edit.
    app.workspace.result.source = Some(TableResultSource {
      query: None,
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
          insertable: true,
          primary_key: true,
        },
        ResultColumn {
          name: "email".into(),
          type_name: "text".into(),
          editable: true,
          insertable: true,
          primary_key: false,
        },
        ResultColumn {
          name: "note".into(),
          type_name: "text".into(),
          editable: true,
          insertable: true,
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
    // Name the displayed result even while a different Explorer row is selected.
    assert!(screen.contains("Results · public.users"));
    assert!(!screen.contains("edits autocommit"));
    assert!(!screen.contains("Saved scripts"));
    assert!(!screen.contains("inspect_users.sql"));
    assert!(screen.contains("dev@example.com"));
    assert!(screen.contains("NULL"));
    assert!(screen.contains("^←/^→ width"));
    assert!(screen.contains("c disconnect"));
    // The action follows the selected node's connection state, not a static disconnect label.
    app.workspace.session_state = crate::db::SessionState::Disconnected;
    assert!(shortcuts(&app).contains(&("c", "connect")));
    assert!(!shortcuts(&app).contains(&("c", "disconnect")));
    app.workspace.session_state =
      crate::db::SessionState::Connected(crate::db::TransactionState::Idle);
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
    app.workspace.focus = Focus::Results;
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
    app.workspace.focus = Focus::Explorer;
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
    assert_eq!(app.explorer_width_percent, 23);
    assert!(app.expanded.contains(&NodeKey::Connection("local".into())));
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
    assert_eq!(app.explorer_width_percent, 25);

    // Saved scripts appear only in the SQL pane's load dialog.
    app.workspace.focus = Focus::Sql;
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
    app.workspace.focus = Focus::Explorer;
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
    app.workspace.overlay = None;
    app.workspace.status =
      "SQL Error [42P01]: ERROR: relation \"derp\" does not exist\nPosition: 52".into();
    app.workspace.status_is_error = true;
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

  // Both the marker and name weight distinguish connected rows without relying on color.
  #[test]
  fn explorer_connection_markers_and_bold_agree() {
    let row = ExplorerRow {
      depth: 0,
      label: "Local".into(),
      node: ExplorerNode::Connection("local".into()),
    };
    for connected in [false, true] {
      let mut terminal = Terminal::new(TestBackend::new(30, 3)).unwrap();
      terminal
        .draw(|frame| {
          frame.render_widget(
            List::new([explorer_item(&row, "", "▸ ", connected)]),
            frame.area(),
          );
        })
        .unwrap();
      let cells = terminal.backend().buffer().content();
      let marker = if connected { "●" } else { "○" };
      assert!(cells.iter().any(|cell| cell.symbol() == marker));
      let name = cells.iter().find(|cell| cell.symbol() == "L").unwrap();
      assert_eq!(name.modifier.contains(Modifier::BOLD), connected);
    }
  }

  // Session identity, lifecycle shortcuts, and rollback consent stay visible at common terminal sizes.
  #[test]
  fn renders_session_state_and_confirmation() {
    use crate::{
      app::SessionAction,
      db::{SessionState, TransactionState},
    };
    let directory = tempfile::tempdir().unwrap();
    let storage = crate::storage::Storage::new(directory.path().to_owned()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
      .build()
      .unwrap();
    let (sender, _receiver) = mpsc::channel();
    let mut app = App::new(storage, vec![], vec![], runtime.handle().clone(), sender);
    app.active_target = Some(("Local".into(), "postgres".into()));
    app.workspace.focus = Focus::Sql;
    app.workspace.session_state = SessionState::Connected(TransactionState::Open);
    for width in [80, 120] {
      let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
      terminal.draw(|frame| draw(frame, &mut app)).unwrap();
      let screen = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
      assert!(screen.contains("SQL · Local/postgres · transaction open"));
      assert!(screen.contains("SQL results"));
      assert!(!screen.contains("[transaction open]"));
      assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "┌");
      for shortcut in [
        "F6 connect",
        "F7 disconnect",
        "F8 reconnect",
        "^PgUp/Down session",
      ] {
        assert!(
          screen.contains(shortcut),
          "missing {shortcut} at width {width}"
        );
      }
      for action in [
        SessionAction::Disconnect,
        SessionAction::Reconnect,
        SessionAction::Quit,
      ] {
        app.workspace.overlay = Some(Overlay::ConfirmSession(action));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let screen = terminal
          .backend()
          .buffer()
          .content()
          .iter()
          .map(|cell| cell.symbol())
          .collect::<String>();
        assert!(
          screen.contains("rolled back"),
          "missing rollback notice at width {width}"
        );
        assert!(screen.contains("Y confirm"));
        assert!(screen.contains("N/Esc cancel"));
      }
      app.workspace.overlay = None;
    }
    // Normal sessions show only the target; exceptional states keep their diagnostics.
    let mut terminal = Terminal::new(TestBackend::new(160, 24)).unwrap();
    for state in [
      SessionState::Connected(TransactionState::Idle),
      SessionState::Connected(TransactionState::Paging),
      SessionState::Connected(TransactionState::Failed),
      SessionState::Connected(TransactionState::Unknown),
      SessionState::Disconnected,
      SessionState::Lost,
    ] {
      app.workspace.session_state = state;
      terminal.draw(|frame| draw(frame, &mut app)).unwrap();
      let screen = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
      assert!(!screen.contains("autocommit"));
      assert!(!screen.contains("result cursor open"));
      assert!(screen.contains("SQL · Local/postgres"));
      if !matches!(
        state,
        SessionState::Connected(TransactionState::Idle | TransactionState::Paging)
      ) {
        assert!(screen.contains(state.label()));
      }
    }
  }

  // Pending colors must survive row selection and override NULL styling only on changed rows.
  #[test]
  fn renders_pending_rows_cells_and_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let storage = crate::storage::Storage::new(directory.path().to_owned()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
      .build()
      .unwrap();
    let (sender, _receiver) = mpsc::channel();
    let mut app = App::new(storage, vec![], vec![], runtime.handle().clone(), sender);
    app.workspace.focus = Focus::Results;
    app.workspace.result.columns = vec!["id".into(), "value".into()];
    app.workspace.result.rows = vec![
      vec![Some("1".into()), Some("old".into())],
      vec![Some("2".into()), None],
    ];
    app.workspace.result.source = Some(TableResultSource {
      query: None,
      table: TableRef {
        profile_id: "local".into(),
        database: "postgres".into(),
        schema: "public".into(),
        name: "items".into(),
        kind: "table".into(),
      },
      columns: ["id", "value"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| ResultColumn {
          name: name.into(),
          type_name: "text".into(),
          editable: true,
          insertable: true,
          primary_key: index == 0,
        })
        .collect(),
    });
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Some(Overlay::RowDetail(form)) = &mut app.workspace.overlay else {
      panic!("missing row editor")
    };
    form.values[1] = Some("changed".into());
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    app.workspace.result_row = 1;
    app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Insert, KeyModifiers::NONE));
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let screen = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|cell| cell.symbol())
      .collect::<String>();
    assert!(screen.contains("DEFAULT"));
    assert!(screen.contains("^D DEFAULT"));
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(app.workspace.overlay.is_none());
    assert_eq!(app.workspace.edits.count(&app.workspace.result), 3);
    // Render only Results to make cell coordinates independent of the surrounding layout.
    for selected in 0..3 {
      app.workspace.result_row = selected;
      terminal
        .draw(|frame| draw_results(frame, &app, frame.area()))
        .unwrap();
      let buffer = terminal.backend().buffer();
      let text = buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
      assert!(text.contains("3 pending"));
      for (needle, color) in [
        ("changed", THEME.yellow),
        ("NULL", THEME.red),
        ("DEFAULT", THEME.green),
      ] {
        let (x, y) = (0..buffer.area.height)
          .find_map(|y| {
            let line = (0..buffer.area.width)
              .map(|x| buffer[(x, y)].symbol())
              .collect::<String>();
            // All text before these cells is single-width, including the pane border and selector.
            line
              .find(needle)
              .map(|byte| (line[..byte].chars().count() as u16, y))
          })
          .expect("pending value missing");
        for offset in 0..needle.len() as u16 {
          assert_eq!(buffer[(x + offset, y)].fg, color, "{needle}");
          assert!(buffer[(x + offset, y)].modifier.contains(Modifier::BOLD));
        }
      }
      let unchanged_key = buffer
        .content()
        .iter()
        .find(|cell| cell.symbol() == "1")
        .unwrap();
      assert_ne!(unchanged_key.fg, THEME.yellow);
    }
    for action in [("^S", "save all"), ("^Z", "discard all"), ("F5", "refresh")] {
      assert!(shortcuts(&app).contains(&action));
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
    assert_eq!(app.workspace.result.rows.len(), 2);
    assert_eq!(app.workspace.result.rows[0][1].as_deref(), Some("old"));
    // Ordinals stay fixed when columns scroll and continue past the first 200-row page.
    app.workspace.result.rows = (1..=205)
      .map(|_| vec![Some("key".into()), Some("value".into())])
      .collect();
    app.workspace.result_column = 1;
    app.workspace.result_row = 204;
    terminal
      .draw(|frame| draw_results(frame, &app, frame.area()))
      .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(3, 1)].symbol(), " ");
    let last_number = (3..6).map(|x| buffer[(x, 28)].symbol()).collect::<String>();
    assert_eq!(last_number, "205");
    assert_eq!(buffer[(3, 28)].fg, THEME.muted);
  }

  // Wide cells remain bounded so the result viewport can still show neighboring columns.
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
