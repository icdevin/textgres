use std::{
    collections::{HashMap, HashSet},
    sync::mpsc::Sender,
    time::{SystemTime, UNIX_EPOCH},
};

use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    style::Style,
};
use ratatui_textarea::TextArea;
use tokio::runtime::Handle;

use crate::{
    db::{self, Output, QueryResult, Request, Response, TableRef},
    storage::{ConnectionProfile, Storage},
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
}

impl ConnectionField {
    pub const ALL: [Self; 7] = [
        Self::Name,
        Self::Host,
        Self::Port,
        Self::Database,
        Self::User,
        Self::Password,
        Self::RequireTls,
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
        }
    }
}

/// The form masks the password while allowing it to be saved with the profile.
#[derive(Clone, Debug)]
pub struct ConnectionForm {
    pub editing_id: Option<String>,
    pub values: [String; 6],
    pub require_tls: bool,
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
            ],
            require_tls: false,
            field: 0,
            cursor: 0,
        }
    }

    fn edit(profile: &ConnectionProfile) -> Self {
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
            ],
            require_tls: profile.require_tls,
            field: 0,
            cursor: profile.name.chars().count(),
        }
    }

    pub fn selected_field(&self) -> ConnectionField {
        ConnectionField::ALL[self.field]
    }

    pub fn value(&self, field: ConnectionField) -> Option<&str> {
        let index = ConnectionField::ALL
            .iter()
            .position(|item| *item == field)?;
        self.values.get(index).map(String::as_str)
    }

    fn move_field(&mut self, change: isize) {
        self.field =
            (self.field as isize + change).rem_euclid(ConnectionField::ALL.len() as isize) as usize;
        self.cursor = self
            .values
            .get(self.field)
            .map_or(0, |value| value.chars().count());
    }

    fn handle_text_key(&mut self, key: KeyEvent) {
        let Some(value) = self.values.get_mut(self.field) else {
            return;
        };
        edit_single_line(value, &mut self.cursor, key);
    }
}

/// Only one overlay exists at once, which prevents conflicting key handlers.
#[derive(Clone, Debug)]
pub enum Overlay {
    Connection(ConnectionForm),
    SaveScript { name: String, cursor: usize },
    LoadScript { selected: usize },
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
    pub sql: TextArea<'static>,
    pub result: QueryResult,
    pub result_row: usize,
    pub result_column: usize,
    pub active_target: Option<(String, String)>,
    pub overlay: Option<Overlay>,
    pub busy: Option<String>,
    pub status: String,
    pub status_is_error: bool,
    storage: Storage,
    runtime: Handle,
    database_tx: Sender<Response>,
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
            storage,
            runtime,
            database_tx,
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
            Err(error) => self.set_status(format!("Database error: {error:#}"), true),
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
                self.explorer_selected =
                    (self.explorer_selected + 1).min(row_count.saturating_sub(1))
            }
            KeyCode::Home | KeyCode::Char('g') => self.explorer_selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.explorer_selected = row_count.saturating_sub(1)
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right => self.activate_selected(),
            KeyCode::Left => self.collapse_selected(),
            KeyCode::Char('n') => self.overlay = Some(Overlay::Connection(ConnectionForm::new())),
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
                self.result_row =
                    (self.result_row + 1).min(self.result.rows.len().saturating_sub(1))
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
                                    self.set_status(
                                        format!("Saved, but refresh failed: {error:#}"),
                                        true,
                                    );
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
                if !self.tables.contains_key(&(
                    profile_id.clone(),
                    database.clone(),
                    schema.clone(),
                )) && let Some(profile) = self.profile(&profile_id).cloned()
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

    fn dispatch(&mut self, description: String, request: Request) {
        self.busy = Some(description.clone());
        self.set_status(description, false);
        db::spawn(&self.runtime, self.database_tx.clone(), request);
    }

    fn save_connection(&mut self, form: &ConnectionForm) -> Result<(), String> {
        let port = form.values[2]
            .parse::<u16>()
            .map_err(|_| "Port must be an integer from 1 through 65535".to_owned())?;
        if form.values[..5].iter().any(|value| value.trim().is_empty()) {
            return Err("Name, host, port, database, and user are required".into());
        }
        let id = form.editing_id.clone().unwrap_or_else(new_id);
        let profile = ConnectionProfile {
            id: id.clone(),
            name: form.values[0].trim().to_owned(),
            host: form.values[1].trim().to_owned(),
            port,
            database: form.values[3].trim().to_owned(),
            user: form.values[4].trim().to_owned(),
            password: (!form.values[5].is_empty()).then(|| form.values[5].clone()),
            require_tls: form.require_tls,
        };
        // Persist a candidate first so a disk error cannot leave memory and disk divergent.
        let mut profiles = self.profiles.clone();
        if let Some(index) = profiles.iter().position(|item| item.id == id) {
            profiles[index] = profile;
        } else {
            profiles.push(profile);
        }
        self.storage
            .save_connections(&profiles)
            .map_err(|error| format!("Could not save connection: {error:#}"))?;
        self.profiles = profiles;

        // Endpoint changes invalidate every object loaded through the old profile.
        self.databases.remove(&id);
        self.schemas.retain(|(profile_id, _), _| profile_id != &id);
        self.tables
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
            self.overlay = Some(Overlay::Connection(ConnectionForm::edit(profile)));
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
        self.profile(id)
            .and_then(|profile| profile.password.clone())
    }

    fn set_status(&mut self, status: String, is_error: bool) {
        self.status = status;
        self.status_is_error = is_error;
    }
}

// Keeps the SQL editor configuration consistent after loading a script.
fn sql_editor(sql: &str) -> TextArea<'static> {
    let lines = if sql.is_empty() {
        vec![String::new()]
    } else {
        sql.split('\n').map(str::to_owned).collect()
    };
    let mut editor = TextArea::new(lines);
    editor.set_line_number_style(Style::default().fg(THEME.muted));
    editor.set_cursor_line_style(Style::default().bg(THEME.cursor_line));
    editor
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
}
