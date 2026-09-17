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
  pub edits: table_edits::TableEdits,
  // Preserve the refresh target if a committed batch loses its follow-up preview.
  pub(super) refresh_table: Option<TableRef>,
  pub(super) database_task: Option<db::Task>,
  // A failed FETCH cannot be retried because the server may already have advanced its cursor.
  pub(super) fetching_page: bool,
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
      edits: table_edits::TableEdits::default(),
      refresh_table: None,
      database_task: None,
      fetching_page: false,
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
    let fetched_page = std::mem::take(&mut self.fetching_page);
    if let Some(state) = response.session_state {
      self.session_state = state;
      // Explicit disconnect closes all cursors; SQL loss alone leaves independent previews intact.
      if state == db::SessionState::Disconnected
        || (state == db::SessionState::Lost
          && self.result.page.as_ref().is_some_and(|page| !page.preview))
      {
        self.result.page = None;
      }
    }
    match response.result {
      Ok(Output::Page { requested, result }) => {
        // Operation IDs and cursor IDs together protect results from stale or misrouted pages.
        if self.result.page.as_ref() != Some(&requested) || self.result.columns != result.columns {
          self.result.page = None;
          self.set_status(
            "Result page no longer matches; refresh or run the query again".into(),
            true,
          );
          return;
        }
        let added = result.rows.len();
        self
          .edits
          .append_page(&mut self.result, result.rows, &mut self.result_row);
        self.result.page = result.page;
        self.result.status = db::page_status(
          self.result.rows.len() - self.edits.new_defaults.len(),
          self.result.page.is_some(),
        );
        self.set_status(
          format!("Loaded {added} more rows; {}", self.result.status),
          false,
        );
      }
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
        self.refresh_table = result.source.as_ref().map(|source| source.table.clone());
        self.edits = table_edits::TableEdits::default();
        self.result = result;
        self.result_row = 0;
        self.result_column = 0;
        self.focus = Focus::Results;
        self.set_status(status, false);
      }
      Ok(Output::Saved(result)) => {
        self.edits = table_edits::TableEdits::default();
        self.result_row = 0;
        self.result_column = 0;
        self.focus = Focus::Results;
        match result {
          Ok(result) => {
            let status = format!("Changes saved; {}", result.status);
            self.result = result;
            self.set_status(status, false);
          }
          Err(error) => {
            // Old row values must not remain editable after a committed write.
            self.result = QueryResult::default();
            if error.is::<db::Cancelled>() {
              self.set_status("Changes saved; refresh cancelled".into(), false);
            } else {
              self.set_status(
                format!(
                  "Changes saved; refresh failed: {}",
                  db::format_error(&error)
                ),
                true,
              );
            }
          }
        }
      }
      Err(error) => {
        if fetched_page {
          self.result.page = None;
          self.set_status(
            format!(
              "Page fetch failed; loaded rows retained. Refresh or run the query again: {}",
              db::format_error(&error)
            ),
            true,
          );
          return;
        }
        // Keep failed batches staged, but never offer a blind retry after an uncertain COMMIT.
        if error.is::<db::UnknownCommit>() {
          self.edits.uncertain = true;
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
      self.edits = table_edits::TableEdits::default();
      self.result = QueryResult::default();
      self.result_row = 0;
      self.result_column = 0;
    }
    if self
      .refresh_table
      .as_ref()
      .is_some_and(|table| table.profile_id == profile_id)
    {
      self.refresh_table = None;
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
