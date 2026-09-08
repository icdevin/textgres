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
  storage::{ConnectionProfile, SshConfig, Storage},
  theme::THEME,
};

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

/// Expanded row state keeps edits isolated until the user explicitly saves.
#[derive(Clone, Debug)]
pub struct RowDetail {
  pub row_index: usize,
  pub columns: Vec<String>,
  pub source: Option<db::TableResultSource>,
  pub original: Vec<Option<String>>,
  pub values: Vec<Option<String>>,
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
        source
          .columns
          .get(self.selected)
          .is_some_and(|column| column.editable)
      })
  }

  pub fn row_is_editable(&self) -> bool {
    self.source.as_ref().is_some_and(|source| {
      source.columns.iter().any(|column| column.primary_key)
        && source.columns.iter().any(|column| column.editable)
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
    self.values[self.selected] = match self.values[self.selected] {
      Some(_) => None,
      None => Some(String::new()),
    };
    self.editor = row_value_editor(self.selected_value());
  }
}

/// Only one overlay exists at once, which prevents conflicting key handlers.
#[derive(Clone, Debug)]
pub enum Overlay {
  Connection(Box<ConnectionForm>),
  SaveScript { name: String, cursor: usize },
  LoadScript { selected: usize },
  RowDetail(Box<RowDetail>),
  ConfirmDelete { profile_id: String, name: String },
}

/// Central application state; database work is sent to independent Tokio tasks.
pub struct App {
  pub should_quit: bool,
  pub focus: Focus,
  pub profiles: Vec<ConnectionProfile>,
  pub scripts: Vec<String>,
  pub expanded: HashSet<NodeKey>,
  pub databases: HashMap<String, Vec<String>>,
  pub schemas: HashMap<(String, String), Vec<String>>,
  pub tables: HashMap<(String, String, String), Vec<TableRef>>,
  pub explorer_selected: usize,
  pub explorer_width_percent: u16,
  pub sql_height_percent: u16,
  pub sql: SqlEditor,
  pub result: QueryResult,
  pub result_row: usize,
  pub result_column: usize,
  pub active_target: Option<(String, String)>,
  pub overlay: Option<Overlay>,
  pub busy: Option<String>,
  pub status: String,
  pub status_is_error: bool,
  pending_row_edit: Option<RowDetail>,
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
      should_quit: false,
      focus: Focus::Explorer,
      profiles,
      scripts,
      expanded: HashSet::new(),
      databases: HashMap::new(),
      schemas: HashMap::new(),
      tables: HashMap::new(),
      explorer_selected: 0,
      explorer_width_percent: 25,
      sql_height_percent: 34,
      sql: sql_editor(""),
      result: QueryResult::default(),
      result_row: 0,
      result_column: 0,
      active_target: None,
      overlay: None,
      busy: None,
      status: "Ready".into(),
      status_is_error: false,
      pending_row_edit: None,
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
    if self.overlay.is_some() {
      self.handle_overlay_key(key);
      return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
      self.should_quit = true;
      return;
    }
    match key.code {
      KeyCode::Tab => {
        self.focus = match self.focus {
          Focus::Explorer => Focus::Sql,
          Focus::Sql => Focus::Results,
          Focus::Results => Focus::Explorer,
        };
        return;
      }
      KeyCode::BackTab => {
        self.focus = match self.focus {
          Focus::Explorer => Focus::Results,
          Focus::Sql => Focus::Explorer,
          Focus::Results => Focus::Sql,
        };
        return;
      }
      _ => {}
    }

    match self.focus {
      Focus::Explorer => self.handle_explorer_key(key),
      Focus::Sql => self.handle_sql_key(key),
      Focus::Results => self.handle_results_key(key),
    }
  }

  /// Applies typed worker output and always clears the busy state.
  pub fn handle_database_response(&mut self, response: Response) {
    self.busy = None;
    match response.result {
      Ok(Output::Databases { profile_id, names }) => {
        let count = names.len();
        self.databases.insert(profile_id, names);
        self.set_status(format!("Loaded {count} database(s)"), false);
      }
      Ok(Output::Schemas {
        profile_id,
        database,
        names,
      }) => {
        let count = names.len();
        self.schemas.insert((profile_id, database), names);
        self.set_status(format!("Loaded {count} schema(s)"), false);
      }
      Ok(Output::Tables {
        profile_id,
        database,
        schema,
        tables,
      }) => {
        let count = tables.len();
        self.tables.insert((profile_id, database, schema), tables);
        self.set_status(format!("Loaded {count} relation(s)"), false);
      }
      Ok(Output::Result(result)) => {
        let status = result.status.clone();
        self.result = result;
        self.result_row = 0;
        self.result_column = 0;
        self.focus = Focus::Results;
        self.set_status(status, false);
      }
      Ok(Output::Updated(result)) => {
        let status = format!("Row updated; {}", result.status);
        self.pending_row_edit = None;
        self.result = result;
        self.result_row = 0;
        self.result_column = 0;
        self.focus = Focus::Results;
        self.set_status(status, false);
      }
      Err(error) => {
        if let Some(form) = self.pending_row_edit.take() {
          self.overlay = Some(Overlay::RowDetail(Box::new(form)));
        }
        self.set_status(db::format_error(&error), true);
      }
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
        self.overlay = Some(Overlay::Connection(Box::new(ConnectionForm::new())));
      }
      KeyCode::Char('e') => self.edit_selected_connection(),
      KeyCode::Char('d') => self.confirm_delete_selected_connection(),
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
          self.overlay = Some(Overlay::SaveScript {
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
    match key.code {
      KeyCode::Up | KeyCode::Char('k') => self.result_row = self.result_row.saturating_sub(1),
      KeyCode::Down | KeyCode::Char('j') => {
        self.result_row = (self.result_row + 1).min(self.result.rows.len().saturating_sub(1))
      }
      KeyCode::Left | KeyCode::Char('h') => {
        self.result_column = self.result_column.saturating_sub(1)
      }
      KeyCode::Right | KeyCode::Char('l') => {
        self.result_column =
          (self.result_column + 1).min(self.result.columns.len().saturating_sub(1))
      }
      KeyCode::Home | KeyCode::Char('g') => self.result_row = 0,
      KeyCode::End | KeyCode::Char('G') => {
        self.result_row = self.result.rows.len().saturating_sub(1)
      }
      KeyCode::Enter => self.open_row_detail(),
      _ => {}
    }
  }

  fn handle_overlay_key(&mut self, key: KeyEvent) {
    let Some(mut overlay) = self.overlay.take() else {
      return;
    };
    match &mut overlay {
      Overlay::Connection(form) => {
        if key.code == KeyCode::Esc {
          return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
          if let Err(error) = self.save_connection(form) {
            self.set_status(error, true);
            self.overlay = Some(overlay);
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
        self.overlay = Some(overlay);
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
              self.overlay = Some(overlay);
            }
          }
          return;
        }
        edit_single_line(name, cursor, key);
        self.overlay = Some(overlay);
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
          KeyCode::Enter => {
            let Some(name) = self.scripts.get(*selected).cloned() else {
              self.set_status("No saved scripts".into(), true);
              return;
            };
            match self.storage.load_script(&name) {
              Ok(script) => {
                self.sql = sql_editor(&script.sql);
                self.focus = Focus::Sql;
                self.set_status(format!("Loaded {}.sql", script.name), false);
              }
              Err(error) => {
                self.set_status(format!("Could not load script: {error:#}"), true);
                self.overlay = Some(overlay);
              }
            }
            return;
          }
          _ => {}
        }
        self.overlay = Some(overlay);
      }
      Overlay::RowDetail(form) => {
        if form.editing {
          if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            form.finish_edit();
            if let Err(error) = self.save_row(form) {
              self.set_status(error, true);
              self.overlay = Some(overlay);
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
          self.overlay = Some(overlay);
          return;
        }
        if key.code == KeyCode::Esc {
          return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
          if let Err(error) = self.save_row(form) {
            self.set_status(error, true);
            self.overlay = Some(overlay);
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
        self.overlay = Some(overlay);
      }
      Overlay::ConfirmDelete { profile_id, .. } => match key.code {
        KeyCode::Char('y') | KeyCode::Enter => {
          self.delete_connection(profile_id);
        }
        KeyCode::Char('n') | KeyCode::Esc => {}
        _ => self.overlay = Some(overlay),
      },
    }
  }

  fn activate_selected(&mut self) {
    if self.busy.is_some() {
      return;
    }
    let Some(row) = self.explorer_rows().get(self.explorer_selected).cloned() else {
      return;
    };
    match row.node {
      ExplorerNode::Connection(profile_id) => {
        self.active_target = self
          .profile(&profile_id)
          .map(|profile| (profile_id.clone(), profile.database.clone()));
        let key = NodeKey::Connection(profile_id.clone());
        if self.expanded.remove(&key) {
          return;
        }
        self.expanded.insert(key);
        if !self.databases.contains_key(&profile_id)
          && let Some(profile) = self.profile(&profile_id).cloned()
        {
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
        self.active_target = Some((profile_id.clone(), database.clone()));
        let key = NodeKey::Database(profile_id.clone(), database.clone());
        if self.expanded.remove(&key) {
          return;
        }
        self.expanded.insert(key);
        if !self
          .schemas
          .contains_key(&(profile_id.clone(), database.clone()))
          && let Some(profile) = self.profile(&profile_id).cloned()
        {
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
        self.active_target = Some((profile_id.clone(), database.clone()));
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
        self.active_target = Some((table.profile_id.clone(), table.database.clone()));
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
    if self.busy.is_some() {
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
        password: self.password(&profile_id),
        profile,
        database,
        sql,
      },
    );
  }

  fn open_row_detail(&mut self) {
    if let Some(form) = RowDetail::new(&self.result, self.result_row) {
      // Keep the large multiline editor outside the compact overlay enum.
      self.overlay = Some(Overlay::RowDetail(Box::new(form)));
    } else {
      self.set_status("Select a result row to inspect it".into(), true);
    }
  }

  fn save_row(&mut self, form: &RowDetail) -> Result<(), String> {
    if self.busy.is_some() {
      return Err("Wait for the current database operation to finish".into());
    }
    let source = form
      .source
      .clone()
      .ok_or_else(|| "Custom SQL results are read-only".to_owned())?;
    if !source.columns.iter().any(|column| column.primary_key) {
      return Err("This table has no primary key; the row is read-only".into());
    }
    if form.original == form.values {
      return Err("No row values changed".into());
    }
    let profile_id = &source.table.profile_id;
    let profile = self
      .profile(profile_id)
      .cloned()
      .ok_or_else(|| "The source connection no longer exists".to_owned())?;
    self.pending_row_edit = Some(form.clone());
    self.dispatch(
      format!(
        "Updating row in {}.{}",
        source.table.schema, source.table.name
      ),
      Request::UpdateRow {
        password: self.password(profile_id),
        profile,
        source,
        original: form.original.clone(),
        values: form.values.clone(),
      },
    );
    Ok(())
  }

  fn dispatch(&mut self, description: String, request: Request) {
    self.busy = Some(description.clone());
    self.set_status(description, false);
    db::spawn(
      &self.runtime,
      self.database_tx.clone(),
      self.tunnels.clone(),
      request,
    );
  }

  fn save_connection(&mut self, form: &ConnectionForm) -> Result<(), String> {
    if self.busy.is_some() {
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
      self.active_target = None;
    }
    self.overlay = None;
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
      self.overlay = Some(Overlay::Connection(Box::new(ConnectionForm::edit(profile))));
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
      self.overlay = Some(Overlay::ConfirmDelete {
        profile_id,
        name: profile.name.clone(),
      });
    }
  }

  fn delete_connection(&mut self, profile_id: &str) {
    if self.busy.is_some() {
      self.set_status(
        "Wait for the current database operation to finish".into(),
        true,
      );
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
    self.databases.remove(profile_id);
    self.schemas.retain(|(id, _), _| id != profile_id);
    self.tables.retain(|(id, _, _), _| id != profile_id);
    if self
      .active_target
      .as_ref()
      .is_some_and(|(id, _)| id == profile_id)
    {
      self.active_target = None;
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
    self.overlay = Some(Overlay::LoadScript { selected: 0 });
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
    self.status = status;
    self.status_is_error = is_error;
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
  let Some(source) = &form.source else {
    return "Custom SQL results are read-only".into();
  };
  if !source.columns.iter().any(|column| column.primary_key) {
    return "This table has no primary key; the row is read-only".into();
  }
  "This generated column is read-only".into()
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
            primary_key: true,
          },
          db::ResultColumn {
            name: "name".into(),
            type_name: "text".into(),
            editable: true,
            primary_key: false,
          },
          db::ResultColumn {
            name: "slug".into(),
            type_name: "text".into(),
            editable: false,
            primary_key: false,
          },
        ],
      }),
      ..Default::default()
    }
  }
}
