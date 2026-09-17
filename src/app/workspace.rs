// Each database owns its results, dialogs, and in-flight operation; SQL input is shared.
use super::*;

// Keeping all local state together makes switching and background response routing atomic.
pub struct Workspace {
  pub focus: Focus,
  pub result: QueryResult,
  pub result_row: usize,
  pub result_column: usize,
  pub overlay: Option<Overlay>,
  pub busy: Option<String>,
  pub status: String,
  pub status_is_error: bool,
  pub session_state: db::SessionState,
  pub(super) pending_row_edit: Option<RowDetail>,
  pub(super) database_task: Option<db::Task>,
}

impl Default for Workspace {
  // A new target starts empty; existing targets retain their complete workspace.
  fn default() -> Self {
    Self {
      focus: Focus::Explorer,
      result: QueryResult::default(),
      result_row: 0,
      result_column: 0,
      overlay: None,
      busy: None,
      status: "Ready".into(),
      status_is_error: false,
      session_state: db::SessionState::Disconnected,
      pending_row_edit: None,
      database_task: None,
    }
  }
}

impl Workspace {
  /// Applies output only when it belongs to the active operation.
  pub(super) fn apply_database_response(&mut self, response: Response) {
    if self
      .database_task
      .as_ref()
      .is_none_or(|task| task.operation_id() != response.operation_id)
    {
      // Ignore stale responses without discarding the active worker's actual outcome.
      return;
    }
    self.database_task = None;
    self.busy = None;
    if let Some(state) = response.session_state {
      self.session_state = state;
    }
    match response.result {
      Ok(Output::Session) => self.set_status(self.session_state.label().into(), false),
      Ok(Output::Databases { names, .. }) => {
        let count = names.len();
        self.set_status(format!("Loaded {count} database(s)"), false);
      }
      Ok(Output::Schemas { names, .. }) => {
        let count = names.len();
        self.set_status(format!("Loaded {count} schema(s)"), false);
      }
      Ok(Output::Tables { tables, .. }) => {
        let count = tables.len();
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
        self.pending_row_edit = None;
        self.result_row = 0;
        self.result_column = 0;
        self.focus = Focus::Results;
        match result {
          Ok(result) => {
            let status = format!("Row saved; {}", result.status);
            self.result = result;
            self.set_status(status, false);
          }
          Err(error) => {
            // Old row values must not remain editable after a committed write.
            self.result = QueryResult::default();
            if error.is::<db::Cancelled>() {
              self.set_status("Row saved; refresh cancelled".into(), false);
            } else {
              self.set_status(
                format!("Row saved; refresh failed: {}", db::format_error(&error)),
                true,
              );
            }
          }
        }
      }
      Err(error) => {
        if let Some(form) = self.pending_row_edit.take() {
          self.overlay = Some(Overlay::RowDetail(Box::new(form)));
        }
        if error.is::<db::Cancelled>() {
          self.set_status(error.to_string(), false);
        } else {
          self.set_status(db::format_error(&error), true);
        }
      }
    }
  }

  // Remove editable state before the same profile ID can resolve to another endpoint.
  pub(super) fn invalidate_connection_preview(&mut self, profile_id: &str) {
    let from_profile = |source: &Option<db::TableResultSource>| {
      source
        .as_ref()
        .is_some_and(|source| source.table.profile_id == profile_id)
    };
    if from_profile(&self.result.source) {
      self.result = QueryResult::default();
      self.result_row = 0;
      self.result_column = 0;
    }
    if self
      .pending_row_edit
      .as_ref()
      .is_some_and(|form| from_profile(&form.source))
    {
      self.pending_row_edit = None;
    }
    if matches!(&self.overlay, Some(Overlay::RowDetail(form)) if from_profile(&form.source)) {
      self.overlay = None;
    }
  }

  // Status belongs to the operation's workspace, including when that workspace is hidden.
  fn set_status(&mut self, status: String, is_error: bool) {
    self.status = status;
    self.status_is_error = is_error;
  }
}
