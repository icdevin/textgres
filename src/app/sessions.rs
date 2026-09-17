// Workspace routing keeps background results and lifecycle decisions with their database.
use super::*;

// The same actions drive keyboard handling, confirmation, and request dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionAction {
  Connect,
  Disconnect,
  Reconnect,
  Quit,
}

impl SessionAction {
  // Confirmation never commits SQL; users can cancel and explicitly COMMIT first.
  pub fn confirmation(self) -> &'static str {
    match self {
      Self::Quit => {
        "Quit all sessions? Running operations will be cancelled and open transactions rolled back. Write outcomes may be unknown. Unsaved SQL text will be lost. Y: quit · N/Esc: keep working"
      }
      Self::Reconnect => {
        "Reconnect this SQL session? Any open transaction will be rolled back and session settings lost. Y: reconnect · N/Esc: keep working"
      }
      _ => {
        "Disconnect this SQL session? Any open transaction will be rolled back. Y: disconnect · N/Esc: keep working"
      }
    }
  }
}

impl App {
  // A profile stays connected while any database session under it remains open.
  pub fn explorer_connected(&self, node: &ExplorerNode) -> bool {
    let (profile_id, database) = match node {
      ExplorerNode::Connection(id) => (id, None),
      ExplorerNode::Database {
        profile_id,
        database,
      } => (profile_id, Some(database)),
      _ => return false,
    };
    self
      .workspaces
      .iter()
      .chain(
        self
          .active_target
          .as_ref()
          .map(|target| (target, &self.workspace)),
      )
      .any(|((id, name), workspace)| {
        id == profile_id
          && database.is_none_or(|database| database == name)
          && matches!(workspace.session_state, db::SessionState::Connected(_))
      })
  }

  // Switch database results and operations while leaving the shared SQL editor untouched.
  pub(super) fn switch_workspace(&mut self, target: (String, String)) {
    if self.active_target.as_ref() == Some(&target) {
      return;
    }
    let next = self.workspaces.remove(&target);
    if let Some(previous) = self.active_target.replace(target) {
      let previous_workspace = std::mem::replace(&mut self.workspace, next.unwrap_or_default());
      self.workspaces.insert(previous, previous_workspace);
    } else if let Some(next) = next {
      self.workspace = next;
    }
    // Preserve Explorer navigation when selecting a database.
    self.workspace.focus = Focus::Explorer;
  }

  // Profile changes park database state under its original target; SQL input stays shared.
  pub(super) fn park_workspace(&mut self) {
    if let Some(target) = self.active_target.take() {
      self
        .workspaces
        .insert(target, std::mem::take(&mut self.workspace));
    }
  }

  // Deterministic order makes keyboard switching independent of HashMap iteration order.
  pub(super) fn cycle_workspace(&mut self, forward: bool) {
    let mut targets: Vec<_> = self
      .workspaces
      .keys()
      .filter(|(id, _)| self.profile(id).is_some())
      .cloned()
      .map(Some)
      .collect();
    targets.push(self.active_target.clone());
    targets.sort();
    targets.dedup();
    let index = targets
      .iter()
      .position(|target| target == &self.active_target)
      .unwrap_or(0);
    let next = if forward {
      (index + 1) % targets.len()
    } else {
      (index + targets.len() - 1) % targets.len()
    };
    let focus = self.workspace.focus;
    if let Some(target) = &targets[next] {
      self.switch_workspace(target.clone());
    } else {
      self.park_workspace();
    }
    self.workspace.focus = focus;
  }

  // A response can only update the workspace that owns its unique operation ID.
  pub fn handle_database_response(&mut self, response: Response) {
    let owns_operation = |workspace: &&mut Workspace| {
      workspace
        .database_task
        .as_ref()
        .is_some_and(|task| task.operation_id() == response.operation_id)
    };
    let Some(workspace) = std::iter::once(&mut self.workspace)
      .chain(self.workspaces.values_mut())
      .find(owns_operation)
    else {
      return;
    };
    // Explorer caches are shared; editor, result, and error state remain local.
    match &response.result {
      Ok(Output::Databases { profile_id, names }) => {
        self.databases.insert(profile_id.clone(), names.clone());
      }
      Ok(Output::Schemas {
        profile_id,
        database,
        names,
      }) => {
        self
          .schemas
          .insert((profile_id.clone(), database.clone()), names.clone());
      }
      Ok(Output::Tables {
        profile_id,
        database,
        schema,
        tables,
      }) => {
        self.tables.insert(
          (profile_id.clone(), database.clone(), schema.clone()),
          tables.clone(),
        );
      }
      _ => {}
    }
    workspace.apply_database_response(response);
  }

  // Poll only idle sessions; running workers publish their final state with their response.
  pub fn poll_sessions(&mut self) -> bool {
    let mut changed = false;
    for (target, state) in self.sessions.states() {
      let workspace = if self.active_target.as_ref() == Some(&target) {
        Some(&mut self.workspace)
      } else {
        self.workspaces.get_mut(&target)
      };
      if let Some(workspace) = workspace
        && workspace.database_task.is_none()
        && workspace.session_state != state
      {
        workspace.session_state = state;
        if state == db::SessionState::Lost {
          workspace.status = "SQL session lost; reconnect explicitly. Any uncommitted transaction is lost; a recent write outcome may be unknown.".into();
          workspace.status_is_error = true;
        }
        changed = true;
      }
    }
    changed
  }

  // Include inactive workspaces in quit safety and profile mutation checks.
  pub(super) fn request_session_action(&mut self, action: SessionAction) {
    self.poll_sessions();
    let confirm = if action == SessionAction::Quit {
      std::iter::once(&self.workspace)
        .chain(self.workspaces.values())
        .any(|workspace| {
          workspace.database_task.is_some() || workspace.session_state.needs_confirmation()
        })
    } else {
      if self.workspace.database_task.is_some() {
        self.set_status(
          "Cancel or wait for this workspace's operation first".into(),
          true,
        );
        return;
      }
      action != SessionAction::Connect && self.workspace.session_state.needs_confirmation()
    };
    if confirm {
      self.workspace.overlay = Some(Overlay::ConfirmSession(action));
    } else {
      self.perform_session_action(action);
    }
  }

  // Lifecycle requests use the same operation ownership and error handling as SQL.
  pub(super) fn perform_session_action(&mut self, action: SessionAction) {
    if action == SessionAction::Quit {
      self.should_quit = true;
      return;
    }
    let Some((profile_id, database)) = self.active_target.clone() else {
      self.set_status("Select a database first".into(), true);
      return;
    };
    let Some(profile) = self.profile(&profile_id).cloned() else {
      self.set_status("The connection no longer exists".into(), true);
      return;
    };
    let request = match action {
      SessionAction::Disconnect => Request::Disconnect { profile, database },
      _ => Request::Connect {
        profile,
        database,
        reconnect: action == SessionAction::Reconnect,
      },
    };
    self.dispatch(format!("{action:?} SQL session"), request);
  }

  // Check all operations before invalidating a shared profile or SSH tunnel.
  pub(super) fn prepare_profile_change(&self, profile_id: &str) -> Result<(), String> {
    let active_busy = self
      .active_target
      .as_ref()
      .is_some_and(|(id, _)| id == profile_id)
      && self.workspace.database_task.is_some();
    if active_busy
      || self
        .workspaces
        .iter()
        .any(|((id, _), workspace)| id == profile_id && workspace.database_task.is_some())
    {
      return Err("Wait for all operations on this profile to finish".into());
    }
    self
      .sessions
      .invalidate_profile(profile_id)
      .map_err(|error| error.to_string())
  }

  // Stale row forms in inactive workspaces must not survive profile changes either.
  pub(super) fn invalidate_all_previews(&mut self, profile_id: &str) {
    for workspace in std::iter::once(&mut self.workspace).chain(self.workspaces.values_mut()) {
      workspace.invalidate_connection_preview(profile_id);
    }
  }

  // Signal all workers before awaiting any of them, then close every persistent socket.
  pub async fn shutdown(&mut self) -> anyhow::Result<()> {
    let tasks: Vec<_> = std::iter::once(&mut self.workspace)
      .chain(self.workspaces.values_mut())
      .filter_map(|workspace| workspace.database_task.take())
      .collect();
    for task in &tasks {
      task.cancel();
    }
    let results = futures_util::future::join_all(tasks.into_iter().map(db::Task::shutdown)).await;
    self.sessions.close_all();
    for result in results {
      result?;
    }
    Ok(())
  }
}
