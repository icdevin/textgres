use std::{
  collections::{HashMap, HashSet},
  sync::mpsc::Sender,
  time::{SystemTime, UNIX_EPOCH},
};

use ratatui::{
  crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
  style::Style,
};
use ratatui_textarea::{CursorMove, TextArea};
use tokio::runtime::Handle;

use crate::{
  db::{self, Output, QueryResult, Request, Response, TableRef},
  sql_editor::SqlEditor,
  storage::{ConnectionProfile, Settings, SshConfig, Storage},
  theme::THEME,
};

// Session workspaces and lifecycle actions are separate from pane input handling.
mod sessions;
mod workspace;
// Local table drafts and explicit batch actions do not own SQL sessions.
mod table_edits;
// Explorer lifecycle actions resolve the selected node without changing the active workspace.
mod explorer_sessions;
pub use sessions::SessionAction;
pub use workspace::Workspace;

const MIN_EXPLORER_WIDTH_PERCENT: u16 = 15;
const MAX_EXPLORER_WIDTH_PERCENT: u16 = 50;
const EXPLORER_RESIZE_STEP_PERCENT: i16 = 2;
const MIN_SQL_HEIGHT_PERCENT: u16 = 15;
const MAX_SQL_HEIGHT_PERCENT: u16 = 80;
const SQL_RESIZE_STEP_PERCENT: i16 = 2;

/// The three persistent work areas cycle in this order with Tab.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
  Explorer,
  Sql,
  Results,
}

/// Stable keys track expansion independently of list ordering.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum NodeKey {
  Connection(String),
  Database(String, String),
  Schema(String, String, String),
}

/// A flattened explorer row retains the context required by its action.
#[derive(Clone, Debug)]
pub struct ExplorerRow {
  pub depth: usize,
  pub label: String,
  pub node: ExplorerNode,
}

/// Explorer actions use typed nodes instead of parsing rendered labels.
#[derive(Clone, Debug)]
pub enum ExplorerNode {
  Connection(String),
  Database {
    profile_id: String,
    database: String,
  },
  Schema {
    profile_id: String,
    database: String,
    schema: String,
  },
  Table(TableRef),
}

/// Connection form fields are fixed to keep keyboard behavior predictable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionField {
  Name,
  Host,
  Port,
  Database,
  User,
  Password,
  RequireTls,
  SshTunnel,
  SshHost,
  SshPort,
  SshUser,
  SshIdentityFile,
}

impl ConnectionField {
  pub const ALL: [Self; 12] = [
    Self::Name,
    Self::Host,
    Self::Port,
    Self::Database,
    Self::User,
    Self::Password,
    Self::RequireTls,
    Self::SshTunnel,
    Self::SshHost,
    Self::SshPort,
    Self::SshUser,
    Self::SshIdentityFile,
  ];

  pub const fn label(self) -> &'static str {
    match self {
      Self::Name => "Name",
      Self::Host => "Host",
      Self::Port => "Port",
      Self::Database => "Default database",
      Self::User => "User",
      Self::Password => "Password (saved)",
      Self::RequireTls => "TLS",
      Self::SshTunnel => "Enabled",
      Self::SshHost => "Host",
      Self::SshPort => "Port",
      Self::SshUser => "User",
      Self::SshIdentityFile => "Identity file",
    }
  }

  // Toggle fields live outside the text array, so indexes stay explicit.
  const fn text_index(self) -> Option<usize> {
    match self {
      Self::Name => Some(0),
      Self::Host => Some(1),
      Self::Port => Some(2),
      Self::Database => Some(3),
      Self::User => Some(4),
      Self::Password => Some(5),
      Self::SshHost => Some(6),
      Self::SshPort => Some(7),
      Self::SshUser => Some(8),
      Self::SshIdentityFile => Some(9),
      Self::RequireTls | Self::SshTunnel => None,
    }
  }

  pub const fn is_toggle(self) -> bool {
    matches!(self, Self::RequireTls | Self::SshTunnel)
  }
}

/// The form masks the password while allowing it to be saved with the profile.
#[derive(Clone, Debug)]
pub struct ConnectionForm {
  pub editing_id: Option<String>,
  pub values: [String; 10],
  pub require_tls: bool,
  pub ssh_enabled: bool,
  pub field: usize,
  pub cursor: usize,
}

impl ConnectionForm {
  fn new() -> Self {
    Self {
      editing_id: None,
      values: [
        String::new(),
        "localhost".into(),
        "5432".into(),
        "postgres".into(),
        std::env::var("USER").unwrap_or_default(),
        String::new(),
        String::new(),
        "22".into(),
        std::env::var("USER").unwrap_or_default(),
        String::new(),
      ],
      require_tls: false,
      ssh_enabled: false,
      field: 0,
      cursor: 0,
    }
  }

  fn edit(profile: &ConnectionProfile) -> Self {
    let ssh = profile.ssh.as_ref();
    Self {
      editing_id: Some(profile.id.clone()),
      values: [
        profile.name.clone(),
        profile.host.clone(),
        profile.port.to_string(),
        profile.database.clone(),
        profile.user.clone(),
        // Preload the value so saving an edit preserves the masked password.
        profile.password.clone().unwrap_or_default(),
        ssh.map_or_else(String::new, |ssh| ssh.host.clone()),
        ssh.map_or_else(|| "22".into(), |ssh| ssh.port.to_string()),
        ssh.map_or_else(
          || std::env::var("USER").unwrap_or_default(),
          |ssh| ssh.user.clone(),
        ),
        ssh
          .and_then(|ssh| ssh.identity_file.clone())
          .unwrap_or_default(),
      ],
      require_tls: profile.require_tls,
      ssh_enabled: ssh.is_some(),
      field: 0,
      cursor: profile.name.chars().count(),
    }
  }

  pub fn selected_field(&self) -> ConnectionField {
    ConnectionField::ALL[self.field]
  }

  pub fn value(&self, field: ConnectionField) -> Option<&str> {
    let index = field.text_index()?;
    self.values.get(index).map(String::as_str)
  }

  fn move_field(&mut self, change: isize) {
    self.field =
      (self.field as isize + change).rem_euclid(ConnectionField::ALL.len() as isize) as usize;
    self.cursor = self
      .value(self.selected_field())
      .map_or(0, |value| value.chars().count());
  }

  fn handle_text_key(&mut self, key: KeyEvent) {
    let Some(index) = self.selected_field().text_index() else {
      return;
    };
    let Some(value) = self.values.get_mut(index) else {
      return;
    };
    edit_single_line(value, &mut self.cursor, key);
  }
}

// Expanded row state keeps field edits isolated until the user stages the row.
#[derive(Clone, Debug)]
pub struct RowDetail {
  pub row_index: usize,
  pub columns: Vec<String>,
  pub source: Option<db::TableResultSource>,
  pub original: Vec<Option<String>>,
  pub values: Vec<Option<String>>,
  // Defaults are separate from NULL for new rows with server-generated values.
  pub defaults: Vec<bool>,
  pub is_new: bool,
  pub read_only: bool,
  // Retain the query-specific reason when opening a read-only row.
  pub read_only_reason: Option<String>,
  pub selected: usize,
  pub editing: bool,
  pub editor: TextArea<'static>,
}

impl RowDetail {
  fn new(result: &QueryResult, row_index: usize) -> Option<Self> {
    let values = result.rows.get(row_index)?.clone();
    Some(Self {
      row_index,
      columns: result.columns.clone(),
      source: result.source.clone(),
      original: values.clone(),
      editor: row_value_editor(values.first().and_then(Option::as_deref)),
      defaults: vec![false; values.len()],
      is_new: false,
      read_only: false,
      read_only_reason: result.read_only_reason.clone(),
      values,
      selected: 0,
      editing: false,
    })
  }

  pub fn selected_value(&self) -> Option<&str> {
    self.values.get(self.selected).and_then(Option::as_deref)
  }

  pub fn selected_is_editable(&self) -> bool {
    self.row_is_editable()
      && self.source.as_ref().is_some_and(|source| {
        source.columns.get(self.selected).is_some_and(|column| {
          if self.is_new {
            column.insertable
          } else {
            column.editable
          }
        })
      })
  }

  pub fn row_is_editable(&self) -> bool {
    !self.read_only
      && self.read_only_reason.is_none()
      && self.source.as_ref().is_some_and(|source| {
        matches!(source.table.kind.as_str(), "table" | "partitioned table")
          && (self.is_new || source.columns.iter().any(|column| column.primary_key))
      })
  }

  fn move_selection(&mut self, change: isize) {
    self.selected = (self.selected as isize + change)
      .clamp(0, self.columns.len().saturating_sub(1) as isize) as usize;
    self.editor = row_value_editor(self.selected_value());
  }

  fn begin_edit(&mut self) {
    if self.selected_is_editable() {
      // Entering a NULL field starts an intentional empty-string edit.
      self.defaults[self.selected] = false;
      self.values[self.selected].get_or_insert_default();
      self.editor = row_value_editor(self.selected_value());
      self.editing = true;
    }
  }

  fn finish_edit(&mut self) {
    if self.editing && self.values[self.selected].is_some() {
      self.values[self.selected] = Some(self.editor.lines().join("\n"));
    }
    self.editing = false;
  }

  fn toggle_null(&mut self) {
    if !self.selected_is_editable() {
      return;
    }
    // DEFAULT is a third state: its first NULL toggle must choose NULL, not empty text.
    self.values[self.selected] = if self.defaults[self.selected] {
      None
    } else {
      match self.values[self.selected] {
        Some(_) => None,
        None => Some(String::new()),
      }
    };
    self.defaults[self.selected] = false;
    self.editor = row_value_editor(self.selected_value());
  }
}

/// Only one overlay exists at once, which prevents conflicting key handlers.
#[derive(Clone, Debug)]
pub enum Overlay {
  // Edit a copy so Escape leaves both the Explorer and saved preferences unchanged.
  Settings {
    draft: Settings,
    selected: usize,
  },
  Connection(Box<ConnectionForm>),
  SaveScript {
    name: String,
    cursor: usize,
  },
  LoadScript {
    selected: usize,
  },
  // Keep the confirmed name stable and restore the picker position after cancellation.
  ConfirmDeleteScript {
    name: String,
    selected: usize,
  },
  RowDetail(Box<RowDetail>),
  ConfirmSession(SessionAction),
  ConfirmExplorerDisconnect {
    targets: Vec<(String, String)>,
    label: String,
  },
  ConfirmRefresh,
  ConfirmDelete {
    profile_id: String,
    name: String,
  },
}

/// Central application state; database work is sent to independent Tokio tasks.
pub struct App {
  pub settings: Settings,
  pub should_quit: bool,
  pub profiles: Vec<ConnectionProfile>,
  pub scripts: Vec<String>,
  pub expanded: HashSet<NodeKey>,
  pub databases: HashMap<String, Vec<String>>,
  pub schemas: HashMap<(String, String), Vec<String>>,
  pub tables: HashMap<(String, String, String), Vec<TableRef>>,
  pub explorer_selected: usize,
  pub explorer_width_percent: u16,
  pub sql_height_percent: u16,
  pub workspace: Workspace,
  workspaces: HashMap<(String, String), Workspace>,
  // One editor lets the same SQL run against different targets without losing undo state.
  pub sql: SqlEditor,
  sessions: db::SessionManager,
  pub active_target: Option<(String, String)>,
  next_operation_id: u64,
  storage: Storage,
  runtime: Handle,
  database_tx: Sender<Response>,
  tunnels: db::TunnelManager,
}

impl App {
  /// Creates an immediately usable empty workspace.
  pub fn new(
    storage: Storage,
    profiles: Vec<ConnectionProfile>,
    scripts: Vec<String>,
    runtime: Handle,
    database_tx: Sender<Response>,
  ) -> Self {
    Self {
      settings: Settings::default(),
      should_quit: false,
      profiles,
      scripts,
      expanded: HashSet::new(),
      databases: HashMap::new(),
      schemas: HashMap::new(),
      tables: HashMap::new(),
      explorer_selected: 0,
      explorer_width_percent: 25,
      sql_height_percent: 34,
      workspace: Workspace::default(),
      workspaces: HashMap::new(),
      sql: sql_editor(""),
      sessions: db::SessionManager::default(),
      active_target: None,
      next_operation_id: 1,
      storage,
      runtime,
      database_tx,
      tunnels: db::TunnelManager::default(),
    }
  }

  /// Flattens only expanded branches for both rendering and keyboard actions.
  pub fn explorer_rows(&self) -> Vec<ExplorerRow> {
    let mut rows = Vec::new();
    for profile in &self.profiles {
      rows.push(ExplorerRow {
        depth: 0,
        label: profile.name.clone(),
        node: ExplorerNode::Connection(profile.id.clone()),
      });
      if self
        .expanded
        .contains(&NodeKey::Connection(profile.id.clone()))
      {
        for database in self.databases.get(&profile.id).into_iter().flatten() {
          // Filter at display time so cached and late metadata responses use the latest settings.
          if !self.settings.show_all_databases && database != &profile.database {
            continue;
          }
          rows.push(ExplorerRow {
            depth: 1,
            label: database.clone(),
            node: ExplorerNode::Database {
              profile_id: profile.id.clone(),
              database: database.clone(),
            },
          });
          if self
            .expanded
            .contains(&NodeKey::Database(profile.id.clone(), database.clone()))
          {
            for schema in self
              .schemas
              .get(&(profile.id.clone(), database.clone()))
              .into_iter()
              .flatten()
            {
              if !self.settings.shows_schema(schema) {
                continue;
              }
              rows.push(ExplorerRow {
                depth: 2,
                label: schema.clone(),
                node: ExplorerNode::Schema {
                  profile_id: profile.id.clone(),
                  database: database.clone(),
                  schema: schema.clone(),
                },
              });
              if self.expanded.contains(&NodeKey::Schema(
                profile.id.clone(),
                database.clone(),
                schema.clone(),
              )) {
                for table in self
                  .tables
                  .get(&(profile.id.clone(), database.clone(), schema.clone()))
                  .into_iter()
                  .flatten()
                {
                  rows.push(ExplorerRow {
                    depth: 3,
                    label: format!("{}  [{}]", table.name, table.kind),
                    node: ExplorerNode::Table(table.clone()),
                  });
                }
              }
            }
          }
        }
      }
    }
    rows
  }

  /// Routes keys to the active overlay or pane before any global action.
  pub fn handle_key(&mut self, key: KeyEvent) {
    if self.workspace.overlay.is_some() {
      self.handle_overlay_key(key);
      return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
      self.request_session_action(SessionAction::Quit);
      return;
    }
    // Workspace navigation stays available while another database is busy.
    if key.modifiers.contains(KeyModifiers::CONTROL)
      && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
    {
      self.cycle_workspace(key.code == KeyCode::PageDown);
      return;
    }
    match key.code {
      KeyCode::F(2) => {
        self.workspace.overlay = Some(Overlay::Settings {
          draft: self.settings,
          selected: 0,
        });
        return;
      }
      KeyCode::F(6) => {
        self.request_session_action(SessionAction::Connect);
        return;
      }
      KeyCode::F(7) => {
        if self.workspace.focus == Focus::Explorer {
          self.disconnect_selected(false, None);
        } else {
          self.request_session_action(SessionAction::Disconnect);
        }
        return;
      }
      KeyCode::F(8) => {
        self.request_session_action(SessionAction::Reconnect);
        return;
      }
      _ => {}
    }
    if key.code == KeyCode::Esc && self.workspace.database_task.is_some() {
      self.cancel_database_operation();
      return;
    }
    match key.code {
      KeyCode::Tab => {
        self.workspace.focus = match self.workspace.focus {
          Focus::Explorer => Focus::Sql,
          Focus::Sql => Focus::Results,
          Focus::Results => Focus::Explorer,
        };
        return;
      }
      KeyCode::BackTab => {
        self.workspace.focus = match self.workspace.focus {
          Focus::Explorer => Focus::Results,
          Focus::Sql => Focus::Explorer,
          Focus::Results => Focus::Sql,
        };
        return;
      }
      _ => {}
    }

    match self.workspace.focus {
      Focus::Explorer => self.handle_explorer_key(key),
      Focus::Sql => self.handle_sql_key(key),
      Focus::Results => self.handle_results_key(key),
    }
  }

  fn handle_explorer_key(&mut self, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
      match key.code {
        KeyCode::Left => {
          self.resize_explorer(-EXPLORER_RESIZE_STEP_PERCENT);
          return;
        }
        KeyCode::Right => {
          self.resize_explorer(EXPLORER_RESIZE_STEP_PERCENT);
          return;
        }
        _ => {}
      }
    }
    let row_count = self.explorer_rows().len();
    match key.code {
      KeyCode::Up | KeyCode::Char('k') => {
        self.explorer_selected = self.explorer_selected.saturating_sub(1)
      }
      KeyCode::Down | KeyCode::Char('j') => {
        self.explorer_selected = (self.explorer_selected + 1).min(row_count.saturating_sub(1))
      }
      KeyCode::Home | KeyCode::Char('g') => self.explorer_selected = 0,
      KeyCode::End | KeyCode::Char('G') => self.explorer_selected = row_count.saturating_sub(1),
      KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right => self.activate_selected(),
      KeyCode::Left => self.collapse_selected(),
      KeyCode::Char('n') => {
        // Keep the larger multi-section form outside the compact overlay enum.
        self.workspace.overlay = Some(Overlay::Connection(Box::new(ConnectionForm::new())));
      }
      KeyCode::Char('e') => self.edit_selected_connection(),
      KeyCode::Char('d') => self.confirm_delete_selected_connection(),
      // Modified keys such as Ctrl+C must not toggle a connection.
      KeyCode::Char('c') if key.modifiers.is_empty() => self.toggle_selected_connection(),
      _ => {}
    }
  }

  fn handle_sql_key(&mut self, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
      match key.code {
        KeyCode::Up => {
          self.resize_sql(-SQL_RESIZE_STEP_PERCENT);
          return;
        }
        KeyCode::Down => {
          self.resize_sql(SQL_RESIZE_STEP_PERCENT);
          return;
        }
        _ => {}
      }
    }
    if is_run_key(key) {
      self.run_sql();
      return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
      match key.code {
        KeyCode::Char('s') => {
          self.workspace.overlay = Some(Overlay::SaveScript {
            name: String::new(),
            cursor: 0,
          });
          return;
        }
        KeyCode::Char('l') => {
          self.open_script_picker();
          return;
        }
        _ => {}
      }
    }
    self.sql.input(key);
  }

  fn handle_results_key(&mut self, key: KeyEvent) {
    // Result actions stage locally; only Ctrl+S sends the batch to PostgreSQL.
    let action = match (key.code, key.modifiers.contains(KeyModifiers::CONTROL)) {
      (KeyCode::Insert, _) | (KeyCode::Char('n'), false) => Some(self.add_row()),
      (KeyCode::Delete, _) | (KeyCode::Char('d'), false) => Some(self.delete_row()),
      (KeyCode::Char('s'), true) => Some(self.save_changes()),
      (KeyCode::Char('z'), true) => {
        self.discard_changes();
        return;
      }
      (KeyCode::F(5), _) => Some(self.refresh_results(false)),
      _ => None,
    };
    if let Some(result) = action {
      if let Err(error) = result {
        self.set_status(error, true);
      }
      return;
    }
    match key.code {
      KeyCode::Up | KeyCode::Char('k') => {
        self.workspace.result_row = self.workspace.result_row.saturating_sub(1)
      }
      KeyCode::Down | KeyCode::Char('j') => {
        self.workspace.result_row =
          (self.workspace.result_row + 1).min(self.workspace.result.rows.len().saturating_sub(1))
      }
      KeyCode::Left | KeyCode::Char('h') => {
        self.workspace.result_column = self.workspace.result_column.saturating_sub(1)
      }
      KeyCode::Right | KeyCode::Char('l') => {
        self.workspace.result_column = (self.workspace.result_column + 1)
          .min(self.workspace.result.columns.len().saturating_sub(1))
      }
      KeyCode::PageDown => {
        self.workspace.result_row =
          (self.workspace.result_row + 20).min(self.workspace.result.rows.len().saturating_sub(1));
      }
      KeyCode::PageUp => self.workspace.result_row = self.workspace.result_row.saturating_sub(20),
      KeyCode::Home | KeyCode::Char('g') => self.workspace.result_row = 0,
      KeyCode::End | KeyCode::Char('G') => {
        self.workspace.result_row = self.workspace.result.rows.len().saturating_sub(1)
      }
      KeyCode::Enter | KeyCode::Char('e') => self.open_row_detail(),
      _ => {}
    }
    // One boundary event requests one page; End never drains the whole result automatically.
    if matches!(
      key.code,
      KeyCode::Down | KeyCode::Char('j') | KeyCode::PageDown | KeyCode::End | KeyCode::Char('G')
    ) && self.workspace.result_row >= self.workspace.result.rows.len().saturating_sub(1)
      && self.workspace.database_task.is_none()
      && let Some(page) = self.workspace.result.page.clone()
    {
      self.dispatch("Loading next 200 rows".into(), Request::FetchPage { page });
    }
  }

  fn handle_overlay_key(&mut self, key: KeyEvent) {
    let Some(mut overlay) = self.workspace.overlay.take() else {
      return;
    };
    match &mut overlay {
      Overlay::Settings { draft, selected } => {
        if key.code == KeyCode::Esc {
          return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
          match self.storage.save_settings(draft) {
            Ok(()) => {
              self.settings = *draft;
              // Changing visibility must not change the SQL target or close hidden sessions.
              self.explorer_selected = self
                .explorer_selected
                .min(self.explorer_rows().len().saturating_sub(1));
              self.set_status("Settings saved".into(), false);
            }
            Err(error) => {
              self.set_status(format!("Could not save settings: {error:#}"), true);
              self.workspace.overlay = Some(overlay);
            }
          }
          return;
        }
        match key.code {
          KeyCode::Down | KeyCode::Tab => *selected = (*selected + 1) % 3,
          KeyCode::Up | KeyCode::BackTab => *selected = (*selected + 2) % 3,
          KeyCode::Enter | KeyCode::Char(' ') => {
            let value = match *selected {
              0 => &mut draft.show_all_databases,
              1 => &mut draft.show_system_schemas,
              _ => &mut draft.show_utility_schemas,
            };
            *value = !*value;
          }
          _ => {}
        }
        self.workspace.overlay = Some(overlay);
      }
      Overlay::Connection(form) => {
        if key.code == KeyCode::Esc {
          return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
          if let Err(error) = self.save_connection(form) {
            self.set_status(error, true);
            self.workspace.overlay = Some(overlay);
          }
          return;
        }
        match key.code {
          KeyCode::Tab | KeyCode::Down | KeyCode::Enter => form.move_field(1),
          KeyCode::BackTab | KeyCode::Up => form.move_field(-1),
          KeyCode::Char(' ') if form.selected_field() == ConnectionField::RequireTls => {
            form.require_tls = !form.require_tls
          }
          KeyCode::Char(' ') if form.selected_field() == ConnectionField::SshTunnel => {
            form.ssh_enabled = !form.ssh_enabled
          }
          _ => form.handle_text_key(key),
        }
        self.workspace.overlay = Some(overlay);
      }
      Overlay::SaveScript { name, cursor } => {
        if key.code == KeyCode::Esc {
          return;
        }
        if key.code == KeyCode::Enter {
          let sql = self.sql.lines().join("\n");
          match self.storage.save_script(name, &sql) {
            Ok(()) => {
              match self.storage.list_scripts() {
                Ok(scripts) => self.scripts = scripts,
                Err(error) => {
                  self.set_status(format!("Saved, but refresh failed: {error:#}"), true);
                  return;
                }
              }
              self.set_status(format!("Saved {name}.sql"), false);
            }
            Err(error) => {
              self.set_status(format!("Could not save script: {error:#}"), true);
              self.workspace.overlay = Some(overlay);
            }
          }
          return;
        }
        edit_single_line(name, cursor, key);
        self.workspace.overlay = Some(overlay);
      }
      Overlay::LoadScript { selected } => {
        if key.code == KeyCode::Esc {
          return;
        }
        match key.code {
          KeyCode::Up | KeyCode::Char('k') => {
            *selected = selected.saturating_sub(1);
          }
          KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1).min(self.scripts.len().saturating_sub(1));
          }
          KeyCode::Home | KeyCode::Char('g') => *selected = 0,
          KeyCode::End | KeyCode::Char('G') => {
            *selected = self.scripts.len().saturating_sub(1);
          }
          // Deletion requires a separate confirmation before touching the saved file.
          KeyCode::Char('d') | KeyCode::Delete => {
            let Some(name) = self.scripts.get(*selected).cloned() else {
              self.set_status("No saved scripts".into(), true);
              return;
            };
            self.workspace.overlay = Some(Overlay::ConfirmDeleteScript {
              name,
              selected: *selected,
            });
            return;
          }
          KeyCode::Enter => {
            let Some(name) = self.scripts.get(*selected).cloned() else {
              self.set_status("No saved scripts".into(), true);
              return;
            };
            match self.storage.load_script(&name) {
              Ok(script) => {
                self.sql = sql_editor(&script.sql);
                self.workspace.focus = Focus::Sql;
                self.set_status(format!("Loaded {}.sql", script.name), false);
              }
              Err(error) => {
                self.set_status(format!("Could not load script: {error:#}"), true);
                self.workspace.overlay = Some(overlay);
              }
            }
            return;
          }
          _ => {}
        }
        self.workspace.overlay = Some(overlay);
      }
      Overlay::ConfirmDeleteScript { name, selected } => match key.code {
        KeyCode::Char('y') | KeyCode::Enter => {
          if let Err(error) = self.storage.delete_script(name) {
            self.set_status(format!("Could not delete script: {error:#}"), true);
            self.workspace.overlay = Some(overlay);
            return;
          }
          // Only remove the cached entry after disk success; keep unsaved editor text intact.
          self.scripts.retain(|script| script != name);
          self.set_status(format!("Deleted {name}.sql"), false);
          if !self.scripts.is_empty() {
            self.workspace.overlay = Some(Overlay::LoadScript {
              selected: (*selected).min(self.scripts.len() - 1),
            });
          }
        }
        KeyCode::Char('n') | KeyCode::Esc => {
          self.workspace.overlay = Some(Overlay::LoadScript {
            selected: *selected,
          });
        }
        _ => self.workspace.overlay = Some(overlay),
      },
      Overlay::RowDetail(form) => {
        if form.is_new
          && form.selected_is_editable()
          && key.modifiers.contains(KeyModifiers::CONTROL)
          && key.code == KeyCode::Char('d')
        {
          form.defaults[form.selected] = true;
          form.values[form.selected] = None;
          form.editing = false;
          form.editor = row_value_editor(None);
          self.workspace.overlay = Some(overlay);
          return;
        }
        if form.editing {
          if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            form.finish_edit();
            if let Err(error) = self.stage_row(form) {
              self.set_status(error, true);
              self.workspace.overlay = Some(overlay);
            }
            return;
          } else if key.code == KeyCode::Esc {
            form.finish_edit();
          } else if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n')
          {
            form.toggle_null();
          } else {
            form.editor.input(key);
          }
          self.workspace.overlay = Some(overlay);
          return;
        }
        if key.code == KeyCode::Esc {
          return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
          if let Err(error) = self.stage_row(form) {
            self.set_status(error, true);
            self.workspace.overlay = Some(overlay);
          }
          return;
        }
        match key.code {
          KeyCode::Up | KeyCode::Char('k') => form.move_selection(-1),
          KeyCode::Down | KeyCode::Char('j') => form.move_selection(1),
          KeyCode::Home | KeyCode::Char('g') => form.move_selection(-(form.selected as isize)),
          KeyCode::End | KeyCode::Char('G') => form.move_selection(form.columns.len() as isize),
          KeyCode::Enter => {
            if form.selected_is_editable() {
              form.begin_edit();
            } else {
              self.set_status(row_read_only_reason(form), true);
            }
          }
          KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => form.toggle_null(),
          _ => {}
        }
        self.workspace.overlay = Some(overlay);
      }
      Overlay::ConfirmRefresh => match key.code {
        KeyCode::Char('y') => {
          if let Err(error) = self.refresh_results(true) {
            self.set_status(error, true);
          }
        }
        KeyCode::Char('n') | KeyCode::Esc => {}
        _ => self.workspace.overlay = Some(overlay),
      },
      Overlay::ConfirmSession(action) => match key.code {
        KeyCode::Char('y') => self.perform_session_action(*action),
        KeyCode::Char('n') | KeyCode::Esc => {}
        _ => self.workspace.overlay = Some(overlay),
      },
      Overlay::ConfirmExplorerDisconnect { targets, .. } => match key.code {
        KeyCode::Char('y') => self.disconnect_selected(true, Some(targets.clone())),
        KeyCode::Char('n') | KeyCode::Esc => {}
        _ => self.workspace.overlay = Some(overlay),
      },
      Overlay::ConfirmDelete { profile_id, .. } => match key.code {
        KeyCode::Char('y') | KeyCode::Enter => {
          self.delete_connection(profile_id);
        }
        KeyCode::Char('n') | KeyCode::Esc => {}
        _ => self.workspace.overlay = Some(overlay),
      },
    }
  }

  fn activate_selected(&mut self) {
    let Some(row) = self.explorer_rows().get(self.explorer_selected).cloned() else {
      return;
    };
    // Switch first so a busy database never blocks navigation to another workspace.
    let target = match &row.node {
      ExplorerNode::Connection(id) => self
        .profile(id)
        .map(|profile| (id.clone(), profile.database.clone())),
      ExplorerNode::Database {
        profile_id,
        database,
      }
      | ExplorerNode::Schema {
        profile_id,
        database,
        ..
      } => Some((profile_id.clone(), database.clone())),
      ExplorerNode::Table(table) => Some((table.profile_id.clone(), table.database.clone())),
    };
    let Some(target) = target else {
      return;
    };
    self.switch_workspace(target);
    if self.workspace.database_task.is_some() {
      return;
    }
    match row.node {
      ExplorerNode::Connection(profile_id) => {
        let key = NodeKey::Connection(profile_id.clone());
        if self.expanded.remove(&key) {
          return;
        }
        self.expanded.insert(key);
        // Expansion ensures a persistent session even when metadata was cached.
        if let Some(profile) = self.profile(&profile_id).cloned() {
          self.dispatch(
            format!("Connecting to {}", profile.name),
            Request::Databases {
              password: self.password(&profile_id),
              profile,
            },
          );
        }
      }
      ExplorerNode::Database {
        profile_id,
        database,
      } => {
        let key = NodeKey::Database(profile_id.clone(), database.clone());
        if self.expanded.remove(&key) {
          return;
        }
        self.expanded.insert(key);
        // Refresh through a separate metadata connection, preserving the SQL transaction.
        if let Some(profile) = self.profile(&profile_id).cloned() {
          self.dispatch(
            format!("Loading schemas from {database}"),
            Request::Schemas {
              password: self.password(&profile_id),
              profile,
              database,
            },
          );
        }
      }
      ExplorerNode::Schema {
        profile_id,
        database,
        schema,
      } => {
        let key = NodeKey::Schema(profile_id.clone(), database.clone(), schema.clone());
        if self.expanded.remove(&key) {
          return;
        }
        self.expanded.insert(key);
        if !self
          .tables
          .contains_key(&(profile_id.clone(), database.clone(), schema.clone()))
          && let Some(profile) = self.profile(&profile_id).cloned()
        {
          self.dispatch(
            format!("Loading relations from {schema}"),
            Request::Tables {
              password: self.password(&profile_id),
              profile,
              database,
              schema,
            },
          );
        }
      }
      ExplorerNode::Table(table) => {
        if self.workspace.edits.count(&self.workspace.result) > 0 || self.workspace.edits.uncertain
        {
          self.set_status("Save or discard table changes before opening another table; refresh any unknown save outcome first".into(), true);
          return;
        }
        if let Some(profile) = self.profile(&table.profile_id).cloned() {
          self.dispatch(
            format!("Previewing {}.{}", table.schema, table.name),
            Request::Preview {
              password: self.password(&table.profile_id),
              profile,
              table,
            },
          );
        }
      }
    }
  }

  fn collapse_selected(&mut self) {
    let Some(row) = self.explorer_rows().get(self.explorer_selected).cloned() else {
      return;
    };
    let key = match row.node {
      ExplorerNode::Connection(id) => Some(NodeKey::Connection(id)),
      ExplorerNode::Database {
        profile_id,
        database,
      } => Some(NodeKey::Database(profile_id, database)),
      ExplorerNode::Schema {
        profile_id,
        database,
        schema,
      } => Some(NodeKey::Schema(profile_id, database, schema)),
      _ => None,
    };
    if let Some(key) = key {
      self.expanded.remove(&key);
    }
  }

  fn run_sql(&mut self) {
    // Replacing a preview must never discard an unsaved table batch.
    if self.workspace.edits.count(&self.workspace.result) > 0 || self.workspace.edits.uncertain {
      self.set_status(
        "Save or discard table changes before running SQL; refresh any unknown save outcome first"
          .into(),
        true,
      );
      return;
    }
    if self.workspace.busy.is_some() {
      return;
    }
    let sql = self.sql.lines().join("\n");
    if sql.trim().is_empty() {
      self.set_status("SQL is empty".into(), true);
      return;
    }
    let Some((profile_id, database)) = self.active_target.clone() else {
      self.set_status(
        "Expand a connection or database before running SQL".into(),
        true,
      );
      return;
    };
    let Some(profile) = self.profile(&profile_id).cloned() else {
      self.set_status("The active connection no longer exists".into(), true);
      return;
    };
    self.dispatch(
      format!("Running SQL on {database}"),
      Request::Query {
        profile,
        database,
        sql,
      },
    );
  }

  fn open_row_detail(&mut self) {
    if let Some(mut form) = RowDetail::new(&self.workspace.result, self.workspace.result_row) {
      // Row editing reflects the staged row while retaining its original visible snapshot.
      if let Some(defaults) = self.workspace.edits.defaults(form.row_index) {
        form.is_new = true;
        form.defaults = defaults.clone();
      }
      // Query edits require the same live session and an application-owned transaction.
      let query_unavailable = form
        .source
        .as_ref()
        .is_some_and(|source| source.query.is_some())
        && !matches!(
          self.workspace.session_state,
          db::SessionState::Connected(db::TransactionState::Idle | db::TransactionState::Paging)
        );
      form.read_only = query_unavailable
        || self.workspace.edits.uncertain
        || self.workspace.database_task.is_some()
        || self.workspace.edits.deleted.contains(&form.row_index);
      // Keep the large multiline editor outside the compact overlay enum.
      self.workspace.overlay = Some(Overlay::RowDetail(Box::new(form)));
    } else {
      self.set_status("Select a result row to inspect it".into(), true);
    }
  }

  fn dispatch(&mut self, description: String, request: Request) {
    // One operation at a time keeps cancellation and response ownership exact.
    if self.workspace.database_task.is_some() {
      self.set_status("Wait for this workspace's operation to finish".into(), true);
      return;
    }
    self.workspace.fetching_page = matches!(request, Request::FetchPage { .. });
    // The backend retires query authority even if replacement SQL or reconnect later fails.
    if matches!(
      request,
      Request::Query { .. }
        | Request::Preview { .. }
        | Request::Disconnect { .. }
        | Request::Connect {
          reconnect: true,
          ..
        }
    ) {
      self.workspace.retire_query_result(
        "Query result is no longer current; press F5 in Results to rerun the original query",
      );
    }
    // Starting a replacement retires the old cursor even if the new operation later fails.
    if matches!(
      request,
      Request::Query { .. } | Request::Preview { .. } | Request::SaveChanges { .. }
    ) {
      self.workspace.result.page = None;
    }
    self.workspace.busy = Some(description.clone());
    self.set_status(description, false);
    self.workspace.database_task = Some(self.start_database_task(request));
  }

  // All workspaces share one operation-ID sequence, including background disconnects.
  fn start_database_task(&mut self, request: Request) -> db::Task {
    let operation_id = self.next_operation_id;
    self.next_operation_id = self.next_operation_id.wrapping_add(1);
    db::spawn(
      &self.runtime,
      self.database_tx.clone(),
      self.tunnels.clone(),
      self.sessions.clone(),
      operation_id,
      request,
    )
  }

  fn cancel_database_operation(&mut self) {
    let Some(task) = self.workspace.database_task.as_ref() else {
      return;
    };
    // Keep the operation and pending edits until the worker confirms its outcome.
    task.cancel();
    self.set_status("Cancelling…".into(), false);
  }

  fn save_connection(&mut self, form: &ConnectionForm) -> Result<(), String> {
    if self.workspace.busy.is_some() {
      return Err("Wait for the current database operation to finish".into());
    }
    let port = form.values[2]
      .parse::<u16>()
      .map_err(|_| "Port must be an integer from 1 through 65535".to_owned())?;
    if form.values[..5].iter().any(|value| value.trim().is_empty()) {
      return Err("Name, host, port, database, and user are required".into());
    }
    let ssh = if form.ssh_enabled {
      if form.values[6].trim().is_empty() || form.values[8].trim().is_empty() {
        return Err("SSH host and user are required".into());
      }
      let ssh_port = form.values[7]
        .parse::<u16>()
        .map_err(|_| "SSH port must be an integer from 1 through 65535".to_owned())?;
      Some(SshConfig {
        host: form.values[6].trim().to_owned(),
        port: ssh_port,
        user: form.values[8].trim().to_owned(),
        identity_file: (!form.values[9].trim().is_empty())
          .then(|| form.values[9].trim().to_owned()),
      })
    } else {
      None
    };
    let id = form.editing_id.clone().unwrap_or_else(new_id);
    self.prepare_profile_change(&id)?;
    self
      .tunnels
      .invalidate(&id)
      .map_err(|error| format!("Could not close the previous SSH tunnel: {error:#}"))?;
    let profile = ConnectionProfile {
      id: id.clone(),
      name: form.values[0].trim().to_owned(),
      host: form.values[1].trim().to_owned(),
      port,
      database: form.values[3].trim().to_owned(),
      user: form.values[4].trim().to_owned(),
      password: (!form.values[5].is_empty()).then(|| form.values[5].clone()),
      require_tls: form.require_tls,
      ssh,
    };
    // Persist a candidate first so a disk error cannot leave memory and disk divergent.
    let mut profiles = self.profiles.clone();
    if let Some(index) = profiles.iter().position(|item| item.id == id) {
      profiles[index] = profile;
    } else {
      profiles.push(profile);
    }
    self
      .storage
      .save_connections(&profiles)
      .map_err(|error| format!("Could not save connection: {error:#}"))?;
    self.profiles = profiles;

    // Endpoint changes invalidate every object loaded through the old profile.
    self.invalidate_all_previews(&id);
    self.databases.remove(&id);
    self.schemas.retain(|(profile_id, _), _| profile_id != &id);
    self
      .tables
      .retain(|(profile_id, _, _), _| profile_id != &id);
    if self
      .active_target
      .as_ref()
      .is_some_and(|(profile_id, _)| profile_id == &id)
    {
      self.park_workspace();
    }
    self.workspace.overlay = None;
    self.set_status("Connection saved".into(), false);
    Ok(())
  }

  fn edit_selected_connection(&mut self) {
    let Some(row) = self.explorer_rows().get(self.explorer_selected).cloned() else {
      return;
    };
    let ExplorerNode::Connection(profile_id) = row.node else {
      self.set_status("Select a connection to edit it".into(), true);
      return;
    };
    if let Some(profile) = self.profile(&profile_id) {
      self.workspace.overlay = Some(Overlay::Connection(Box::new(ConnectionForm::edit(profile))));
    }
  }

  fn confirm_delete_selected_connection(&mut self) {
    let Some(row) = self.explorer_rows().get(self.explorer_selected).cloned() else {
      return;
    };
    let ExplorerNode::Connection(profile_id) = row.node else {
      self.set_status("Select a connection to delete it".into(), true);
      return;
    };
    if let Some(profile) = self.profile(&profile_id) {
      self.workspace.overlay = Some(Overlay::ConfirmDelete {
        profile_id,
        name: profile.name.clone(),
      });
    }
  }

  fn delete_connection(&mut self, profile_id: &str) {
    if self.workspace.busy.is_some() {
      self.set_status(
        "Wait for the current database operation to finish".into(),
        true,
      );
      return;
    }
    if let Err(error) = self.prepare_profile_change(profile_id) {
      self.set_status(error, true);
      return;
    }
    if let Err(error) = self.tunnels.invalidate(profile_id) {
      self.set_status(format!("Could not close the SSH tunnel: {error:#}"), true);
      return;
    }
    let previous = self.profiles.clone();
    self.profiles.retain(|profile| profile.id != profile_id);
    if let Err(error) = self.storage.save_connections(&self.profiles) {
      self.profiles = previous;
      self.set_status(format!("Could not delete connection: {error:#}"), true);
      return;
    }
    // Deleted profiles must not leave a preview that still appears editable.
    self.invalidate_all_previews(profile_id);
    self.databases.remove(profile_id);
    self.schemas.retain(|(id, _), _| id != profile_id);
    self.tables.retain(|(id, _, _), _| id != profile_id);
    if self
      .active_target
      .as_ref()
      .is_some_and(|(id, _)| id == profile_id)
    {
      self.park_workspace();
    }
    self.explorer_selected = self
      .explorer_selected
      .min(self.explorer_rows().len().saturating_sub(1));
    self.set_status("Connection deleted".into(), false);
  }

  // Keep saved scripts out of the database hierarchy while making loading immediate.
  fn open_script_picker(&mut self) {
    if self.scripts.is_empty() {
      self.set_status("No saved scripts".into(), true);
      return;
    }
    self.workspace.overlay = Some(Overlay::LoadScript { selected: 0 });
  }

  // Bound user resizing so every pane remains usable on ordinary terminals.
  fn resize_explorer(&mut self, change: i16) {
    self.explorer_width_percent = (self.explorer_width_percent as i16 + change).clamp(
      MIN_EXPLORER_WIDTH_PERCENT as i16,
      MAX_EXPLORER_WIDTH_PERCENT as i16,
    ) as u16;
    self.set_status(
      format!("Explorer width: {}%", self.explorer_width_percent),
      false,
    );
  }

  // Keep enough vertical space for both SQL editing and result inspection.
  fn resize_sql(&mut self, change: i16) {
    self.sql_height_percent = (self.sql_height_percent as i16 + change)
      .clamp(MIN_SQL_HEIGHT_PERCENT as i16, MAX_SQL_HEIGHT_PERCENT as i16)
      as u16;
    self.set_status(format!("SQL height: {}%", self.sql_height_percent), false);
  }

  fn profile(&self, id: &str) -> Option<&ConnectionProfile> {
    self.profiles.iter().find(|profile| profile.id == id)
  }

  fn password(&self, id: &str) -> Option<String> {
    self
      .profile(id)
      .and_then(|profile| profile.password.clone())
  }

  fn set_status(&mut self, status: String, is_error: bool) {
    self.workspace.status = status;
    self.workspace.status_is_error = is_error;
  }
}

// Keeps the SQL editor configuration consistent after loading a script.
fn sql_editor(sql: &str) -> SqlEditor {
  let lines = if sql.is_empty() {
    vec![String::new()]
  } else {
    sql.split('\n').map(str::to_owned).collect()
  };
  SqlEditor::new(lines)
}

// Row values use a plain multiline editor because their contents are not SQL.
fn row_value_editor(value: Option<&str>) -> TextArea<'static> {
  let lines = value.map_or_else(
    || vec![String::new()],
    |value| value.split('\n').map(str::to_owned).collect(),
  );
  let mut editor = TextArea::new(lines);
  editor.set_cursor_line_style(Style::default().bg(THEME.cursor_line));
  editor.move_cursor(CursorMove::Bottom);
  editor.move_cursor(CursorMove::End);
  editor
}

fn row_read_only_reason(form: &RowDetail) -> String {
  // A frozen or deleted row must not be described as a generated-column restriction.
  if let Some(reason) = &form.read_only_reason {
    return reason.clone();
  }
  if form.read_only {
    return "This row is read-only while deleted, busy, or awaiting save verification".into();
  }
  let Some(source) = &form.source else {
    return "Custom SQL results are read-only".into();
  };
  if !matches!(source.table.kind.as_str(), "table" | "partitioned table") {
    return "This object is read-only".into();
  }
  if !form.is_new && !source.columns.iter().any(|column| column.primary_key) {
    return "This table has no primary key; the row is read-only".into();
  }
  "This generated or computed column is read-only".into()
}

// Edits at Unicode scalar boundaries so non-ASCII input cannot corrupt a field.
fn edit_single_line(value: &mut String, cursor: &mut usize, key: KeyEvent) {
  match key.code {
    KeyCode::Char(character)
      if !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
    {
      let byte = char_to_byte(value, *cursor);
      value.insert(byte, character);
      *cursor += 1;
    }
    KeyCode::Backspace if *cursor > 0 => {
      let start = char_to_byte(value, *cursor - 1);
      let end = char_to_byte(value, *cursor);
      value.replace_range(start..end, "");
      *cursor -= 1;
    }
    KeyCode::Delete if *cursor < value.chars().count() => {
      let start = char_to_byte(value, *cursor);
      let end = char_to_byte(value, *cursor + 1);
      value.replace_range(start..end, "");
    }
    KeyCode::Left => *cursor = cursor.saturating_sub(1),
    KeyCode::Right => *cursor = (*cursor + 1).min(value.chars().count()),
    KeyCode::Home => *cursor = 0,
    KeyCode::End => *cursor = value.chars().count(),
    _ => {}
  }
}

fn char_to_byte(value: &str, character_index: usize) -> usize {
  value
    .char_indices()
    .nth(character_index)
    .map_or(value.len(), |(index, _)| index)
}

fn new_id() -> String {
  let nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_nanos();
  format!("connection-{nanos:x}")
}

// F5 works on legacy terminals; Ctrl+Enter works with enhanced keyboard reporting.
fn is_run_key(key: KeyEvent) -> bool {
  key.code == KeyCode::F(5)
    || (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
}

#[cfg(test)]
mod tests {
  // Session tests share the deterministic, network-free application fixtures.
  mod sessions;
  // Table drafts share the deterministic application fixtures.
  mod table_edits;
  // Result continuation regressions use the shared workspace fixtures.
  mod paging;
  // Explorer disconnect regressions share the workspace fixtures.
  mod explorer_sessions;
  // Saved script tests exercise confirmation and disk failures without a database.
  mod scripts;
  // Query editing tests cover the UI's capability and explicit-refresh boundaries.
  mod query_edits;
  // Settings tests cover persistence and visibility without changing session ownership.
  mod settings;
  use super::*;
  use ratatui::crossterm::event::KeyEvent;

  #[test]
  fn single_line_edit_preserves_unicode_boundaries() {
    let mut value = "åb".to_owned();
    let mut cursor = 1;

    edit_single_line(
      &mut value,
      &mut cursor,
      KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );

    assert_eq!(value, "b");
    assert_eq!(cursor, 0);
  }

  #[test]
  fn recognizes_portable_and_enhanced_run_keys() {
    assert!(is_run_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)));
    assert!(is_run_key(KeyEvent::new(
      KeyCode::Enter,
      KeyModifiers::CONTROL
    )));
    assert!(!is_run_key(KeyEvent::new(
      KeyCode::Enter,
      KeyModifiers::NONE
    )));
  }

  // Escape requests cancellation but must still accept a success queued before it.
  #[test]
  fn escape_waits_for_the_actual_database_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::new(directory.path().to_owned()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
      .build()
      .unwrap();
    let (sender, _receiver) = std::sync::mpsc::channel();
    let profile = ConnectionProfile {
      id: "local".into(),
      name: "Local".into(),
      host: "192.0.2.1".into(),
      port: 5432,
      database: "postgres".into(),
      user: "postgres".into(),
      password: None,
      require_tls: false,
      ssh: None,
    };
    let mut app = App::new(
      storage,
      vec![profile.clone()],
      Vec::new(),
      runtime.handle().clone(),
      sender,
    );
    app.dispatch(
      "Connecting to Local".into(),
      Request::Databases {
        profile,
        password: None,
      },
    );
    let operation_id = app.workspace.database_task.as_ref().unwrap().operation_id();

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(app.workspace.busy.is_some());
    assert!(app.workspace.database_task.is_some());
    assert_eq!(app.workspace.status, "Cancelling…");
    app.handle_database_response(Response {
      session_state: None,
      operation_id: operation_id + 1,
      result: Err(db::Cancelled.into()),
    });
    assert!(app.workspace.busy.is_some());
    app.handle_database_response(Response {
      session_state: None,
      operation_id,
      result: Ok(Output::Databases {
        profile_id: "local".into(),
        names: vec!["completed_before_cancel".into()],
      }),
    });
    assert_eq!(app.databases["local"], vec!["completed_before_cancel"]);
    assert!(app.workspace.busy.is_none());
    assert!(app.workspace.database_task.is_none());
    assert_eq!(app.workspace.status, "Loaded 1 database(s)");
  }

  // Unsaved values remain recoverable only after the worker confirms cancellation.
  #[test]
  fn confirmed_cancellation_preserves_staged_changes() {
    let (_directory, _runtime, mut app) = pending_row_app();
    let operation_id = app.workspace.database_task.as_ref().unwrap().operation_id();
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.workspace.overlay.is_none());
    assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
    app.handle_database_response(Response {
      session_state: None,
      operation_id,
      result: Err(db::Cancelled.into()),
    });
    assert!(app.workspace.overlay.is_none());
    assert_eq!(app.workspace.result.rows[0][1].as_deref(), Some("changed"));
    assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
    assert!(!app.workspace.status_is_error);
    assert!(app.workspace.busy.is_none());
  }

  // A saved row must not be offered for retry when only its preview was cancelled.
  #[test]
  fn cancelled_refresh_preserves_the_committed_write_outcome() {
    let (_directory, _runtime, mut app) = pending_row_app();
    let operation_id = app.workspace.database_task.as_ref().unwrap().operation_id();
    app.handle_database_response(Response {
      session_state: None,
      operation_id,
      result: Ok(Output::Saved(Err(db::Cancelled.into()))),
    });
    assert_eq!(app.workspace.status, "Changes saved; refresh cancelled");
    assert_eq!(app.workspace.edits.count(&app.workspace.result), 0);
    assert!(app.workspace.overlay.is_none());
    assert!(app.workspace.result.source.is_none());
    assert!(app.workspace.result.rows.is_empty());
  }

  // Server and connection failures during refresh have the same committed-write boundary.
  #[test]
  fn failed_refresh_does_not_restore_an_already_saved_edit() {
    let (_directory, _runtime, mut app) = pending_row_app();
    let operation_id = app.workspace.database_task.as_ref().unwrap().operation_id();
    app.handle_database_response(Response {
      session_state: None,
      operation_id,
      result: Ok(Output::Saved(Err(anyhow::anyhow!("connection closed")))),
    });
    assert!(
      app
        .workspace
        .status
        .starts_with("Changes saved; refresh failed:")
    );
    assert!(app.workspace.status_is_error);
    assert_eq!(app.workspace.edits.count(&app.workspace.result), 0);
    assert!(app.workspace.overlay.is_none());
  }

  // Saving a changed endpoint must prevent the original preview from targeting that endpoint.
  #[test]
  fn connection_edit_invalidates_its_preview_and_rejects_old_row_forms() {
    for active_profile in ["local", "other"] {
      let (_directory, _runtime, mut app) = preview_app();
      let mut other = app.profiles[0].clone();
      other.id = "other".into();
      app.profiles.push(other);
      app.active_target = Some((active_profile.into(), "postgres".into()));
      app.workspace.result.rows.push(vec![
        Some("8".into()),
        Some("second".into()),
        Some("generated".into()),
      ]);
      app.workspace.result_row = 1;
      app.workspace.result_column = 2;
      let mut old_form = RowDetail::new(&app.workspace.result, 1).unwrap();
      old_form.values[1] = Some("changed".into());

      app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
      let Some(Overlay::Connection(form)) = &mut app.workspace.overlay else {
        panic!("connection editor must open");
      };
      form.values[1] = "new-server".into();
      app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

      assert_eq!(app.profile("local").unwrap().host, "new-server");
      assert_eq!(app.storage.load_connections().unwrap(), app.profiles);
      assert_eq!(app.workspace.result, QueryResult::default());
      assert_eq!(
        (app.workspace.result_row, app.workspace.result_column),
        (0, 0)
      );
      assert!(
        app
          .stage_row(&old_form)
          .unwrap_err()
          .contains("reload the table")
      );
      assert!(app.workspace.database_task.is_none());
      assert_eq!(app.workspace.edits.count(&app.workspace.result), 0);
      app.workspace.focus = Focus::Results;
      app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
      assert!(app.workspace.overlay.is_none());
    }
  }

  // Invalidation follows preview provenance, not the selected SQL target or another profile's edits.
  #[test]
  fn changing_another_connection_preserves_the_current_preview() {
    let (_directory, _runtime, mut app) = preview_app();
    let result = app.workspace.result.clone();
    let mut other = app.profiles[0].clone();
    other.id = "other".into();
    app.profiles.push(other.clone());
    app.active_target = Some(("local".into(), "postgres".into()));
    app.workspace.result_column = 2;
    let mut connection_form = ConnectionForm::edit(&other);
    connection_form.values[1] = "new-server".into();

    app.save_connection(&connection_form).unwrap();
    assert_eq!(app.workspace.result, result);
    assert_eq!(app.workspace.result_column, 2);
    app.delete_connection("other");
    assert_eq!(app.workspace.result, result);
    assert_eq!(app.workspace.result_column, 2);
    let mut row_form = RowDetail::new(&app.workspace.result, 0).unwrap();
    row_form.values[1] = Some("changed".into());
    app.stage_row(&row_form).unwrap();
    app.save_changes().unwrap();
    assert!(app.workspace.database_task.is_some());
  }

  // A failed disk save leaves the old endpoint and its editable preview valid.
  #[test]
  fn failed_connection_save_preserves_the_profile_and_preview() {
    let (_directory, _runtime, mut app) = preview_app();
    app.storage.save_connections(&app.profiles).unwrap();
    let profiles = app.profiles.clone();
    let result = app.workspace.result.clone();
    app.active_target = Some(("local".into(), "postgres".into()));
    let target = app.active_target.clone();
    let mut connection_form = ConnectionForm::edit(&app.profiles[0]);
    connection_form.values[1] = "new-server".into();
    // A directory at the temporary-file path forces a write failure without permission assumptions.
    std::fs::create_dir(app.storage.root().join("connections.toml.tmp")).unwrap();

    assert!(app.save_connection(&connection_form).is_err());
    assert_eq!(app.profiles, profiles);
    assert_eq!(app.storage.load_connections().unwrap(), profiles);
    assert_eq!(app.workspace.result, result);
    assert_eq!(app.active_target, target);
    let mut row_form = RowDetail::new(&app.workspace.result, 0).unwrap();
    row_form.values[1] = Some("changed".into());
    app.stage_row(&row_form).unwrap();
    app.save_changes().unwrap();
    assert!(app.workspace.database_task.is_some());
  }

  // A deleted source must not leave editable rows or permit a retained form to dispatch a write.
  #[test]
  fn connection_deletion_invalidates_its_preview() {
    let (_directory, _runtime, mut app) = preview_app();
    let mut old_form = RowDetail::new(&app.workspace.result, 0).unwrap();
    old_form.values[1] = Some("changed".into());
    app.workspace.result_column = 2;
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

    assert!(app.profiles.is_empty());
    assert!(app.storage.load_connections().unwrap().is_empty());
    assert_eq!(app.workspace.result, QueryResult::default());
    assert_eq!(
      (app.workspace.result_row, app.workspace.result_column),
      (0, 0)
    );
    assert!(app.workspace.overlay.is_none());
    assert!(app.stage_row(&old_form).is_err());
    assert!(app.workspace.database_task.is_none());
  }

  // Failed deletion must preserve the preview along with the restored connection profile.
  #[test]
  fn failed_connection_deletion_preserves_the_preview() {
    let (_directory, _runtime, mut app) = preview_app();
    app.storage.save_connections(&app.profiles).unwrap();
    let profiles = app.profiles.clone();
    let result = app.workspace.result.clone();
    std::fs::create_dir(app.storage.root().join("connections.toml.tmp")).unwrap();

    app.delete_connection("local");
    assert!(app.workspace.status_is_error);
    assert_eq!(app.profiles, profiles);
    assert_eq!(app.storage.load_connections().unwrap(), profiles);
    assert_eq!(app.workspace.result, result);
  }

  // An unpolled runtime keeps profile and preview tests independent of network access.
  fn preview_app() -> (tempfile::TempDir, tokio::runtime::Runtime, App) {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::new(directory.path().to_owned()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
      .build()
      .unwrap();
    let (sender, _receiver) = std::sync::mpsc::channel();
    let profile = ConnectionProfile {
      id: "local".into(),
      name: "Local".into(),
      host: "192.0.2.1".into(),
      port: 5432,
      database: "postgres".into(),
      user: "postgres".into(),
      password: None,
      require_tls: false,
      ssh: None,
    };
    let mut app = App::new(
      storage,
      vec![profile],
      vec![],
      runtime.handle().clone(),
      sender,
    );
    app.workspace.result = editable_result();
    (directory, runtime, app)
  }

  // Reuse a valid preview so cancellation tests start with a dispatched row edit.
  fn pending_row_app() -> (tempfile::TempDir, tokio::runtime::Runtime, App) {
    let (directory, runtime, mut app) = preview_app();
    let mut form = RowDetail::new(&app.workspace.result, 0).unwrap();
    form.values[1] = Some("changed".into());
    app.stage_row(&form).unwrap();
    app.save_changes().unwrap();
    (directory, runtime, app)
  }

  #[test]
  fn edits_and_nulls_supported_row_values() {
    // Direct table metadata enables edits but keeps generated fields read-only.
    let result = editable_result();
    let mut form = RowDetail::new(&result, 0).unwrap();

    assert!(form.row_is_editable());
    assert!(form.selected_is_editable());
    form.move_selection(1);
    assert_eq!(form.selected_value(), Some("before"));
    form.begin_edit();
    form
      .editor
      .input(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    form.finish_edit();
    assert_eq!(form.selected_value(), Some("beforea"));

    form.toggle_null();
    assert_eq!(form.selected_value(), None);
    form.move_selection(1);
    assert!(!form.selected_is_editable());
  }

  #[test]
  fn custom_query_rows_are_read_only() {
    // Custom SQL has no stable table identity for a safe generated UPDATE.
    let result = QueryResult {
      columns: vec!["value".into()],
      rows: vec![vec![Some("42".into())]],
      ..Default::default()
    };
    let form = RowDetail::new(&result, 0).unwrap();

    assert!(!form.row_is_editable());
    assert!(!form.selected_is_editable());
    assert_eq!(
      row_read_only_reason(&form),
      "Custom SQL results are read-only"
    );
  }

  #[test]
  fn connection_form_preserves_ssh_settings() {
    // Editing must not silently remove tunnel authentication settings.
    let profile = ConnectionProfile {
      id: "remote".into(),
      name: "Remote".into(),
      host: "db.internal".into(),
      port: 5432,
      database: "postgres".into(),
      user: "postgres".into(),
      password: None,
      require_tls: true,
      ssh: Some(SshConfig {
        host: "gateway.example.com".into(),
        port: 2222,
        user: "devin".into(),
        identity_file: Some("~/.ssh/work".into()),
      }),
    };

    let form = ConnectionForm::edit(&profile);

    assert!(form.ssh_enabled);
    assert_eq!(
      form.value(ConnectionField::SshHost),
      Some("gateway.example.com")
    );
    assert_eq!(form.value(ConnectionField::SshPort), Some("2222"));
    assert_eq!(form.value(ConnectionField::SshUser), Some("devin"));
    assert_eq!(
      form.value(ConnectionField::SshIdentityFile),
      Some("~/.ssh/work")
    );
  }

  // Keeps row-editor tests compact while retaining real PostgreSQL metadata.
  fn editable_result() -> QueryResult {
    QueryResult {
      columns: vec!["id".into(), "name".into(), "slug".into()],
      rows: vec![vec![
        Some("7".into()),
        Some("before".into()),
        Some("generated".into()),
      ]],
      source: Some(db::TableResultSource {
        query: None,
        table: TableRef {
          profile_id: "local".into(),
          database: "postgres".into(),
          schema: "public".into(),
          name: "items".into(),
          kind: "table".into(),
        },
        columns: vec![
          db::ResultColumn {
            name: "id".into(),
            type_name: "integer".into(),
            editable: true,
            insertable: true,
            primary_key: true,
          },
          db::ResultColumn {
            name: "name".into(),
            type_name: "text".into(),
            editable: true,
            insertable: true,
            primary_key: false,
          },
          db::ResultColumn {
            name: "slug".into(),
            type_name: "text".into(),
            editable: false,
            insertable: false,
            primary_key: false,
          },
        ],
      }),
      ..Default::default()
    }
  }
}
