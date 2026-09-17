// Explorer connection controls follow the selected tree node, not the currently displayed SQL target.
use super::*;

impl App {
  // Inspect inactive workspaces without switching their focus, result selection, or dialogs.
  fn target_workspace(&self, target: &(String, String)) -> Option<&Workspace> {
    if self.active_target.as_ref() == Some(target) {
      Some(&self.workspace)
    } else {
      self.workspaces.get(target)
    }
  }

  // Normalize descendants to their database so the shortcut label and action use the same target.
  fn selected_connection_scope(&self) -> Result<(String, Option<String>), String> {
    let row = self
      .explorer_rows()
      .get(self.explorer_selected)
      .cloned()
      .ok_or("Select a connection or database first")?;
    Ok(match row.node {
      ExplorerNode::Connection(id) => (id, None),
      ExplorerNode::Database {
        profile_id,
        database,
      }
      | ExplorerNode::Schema {
        profile_id,
        database,
        ..
      } => (profile_id, Some(database)),
      ExplorerNode::Table(table) => (table.profile_id, Some(table.database)),
    })
  }

  // A profile is connected while any of its databases is connected, matching its Explorer marker.
  pub fn selected_connection_is_connected(&self) -> bool {
    self
      .selected_connection_scope()
      .is_ok_and(|(profile_id, database)| {
        let node = match database {
          None => ExplorerNode::Connection(profile_id),
          Some(database) => ExplorerNode::Database {
            profile_id,
            database,
          },
        };
        self.explorer_connected(&node)
      })
  }

  // Explicit connect also recovers disconnected/lost sessions and selects their SQL workspace.
  pub(super) fn toggle_selected_connection(&mut self) {
    self.poll_sessions();
    if self.selected_connection_is_connected() {
      self.disconnect_selected(false, None);
      return;
    }
    let (profile_id, database) = match self.selected_connection_scope() {
      Ok(scope) => scope,
      Err(error) => {
        self.set_status(error, true);
        return;
      }
    };
    let Some(profile) = self.profile(&profile_id) else {
      self.set_status("The connection no longer exists".into(), true);
      return;
    };
    let database = database.unwrap_or_else(|| profile.database.clone());
    self.switch_workspace((profile_id, database));
    self.request_session_action(SessionAction::Connect);
  }

  // A profile row owns every database beneath it; descendants identify one database session.
  fn selected_disconnect_targets(&self) -> Result<(Vec<(String, String)>, String), String> {
    let (profile_id, database) = self.selected_connection_scope()?;
    let profile = self
      .profile(&profile_id)
      .ok_or("The connection no longer exists")?;
    let label = database.as_ref().map_or_else(
      || format!("{} (all databases)", profile.name),
      |database| format!("{}/{database}", profile.name),
    );
    let mut targets: Vec<_> = self
      .workspaces
      .keys()
      .chain(self.active_target.iter())
      .filter(|(id, name)| {
        id == &profile_id && database.as_ref().is_none_or(|database| database == name)
      })
      .cloned()
      .collect();
    targets.sort();
    targets.dedup();
    Ok((targets, label))
  }

  // Validate the entire selection before closing anything; confirmation freezes the target list.
  pub(super) fn disconnect_selected(
    &mut self,
    confirmed: bool,
    targets: Option<Vec<(String, String)>>,
  ) {
    self.poll_sessions();
    let (targets, label) = if let Some(targets) = targets {
      (targets, String::new())
    } else {
      match self.selected_disconnect_targets() {
        Ok(selection) => selection,
        Err(error) => {
          self.set_status(error, true);
          return;
        }
      }
    };
    if targets.iter().any(|target| {
      self
        .target_workspace(target)
        .is_some_and(|workspace| workspace.database_task.is_some())
    }) {
      self.set_status(
        "Cancel or wait for all operations on the selected connection before disconnecting".into(),
        true,
      );
      return;
    }
    let targets: Vec<_> = targets
      .into_iter()
      .filter(|target| {
        self.target_workspace(target).is_some_and(|workspace| {
          workspace.session_state != db::SessionState::Disconnected
            || workspace.result.page.is_some()
        })
      })
      .collect();
    if targets.is_empty() {
      self.set_status(
        "The selected connection is already disconnected".into(),
        false,
      );
      return;
    }
    if !confirmed
      && targets.iter().any(|target| {
        self
          .target_workspace(target)
          .is_some_and(|workspace| workspace.session_state.needs_confirmation())
      })
    {
      self.workspace.overlay = Some(Overlay::ConfirmExplorerDisconnect { targets, label });
      return;
    }
    // Resolve all profiles first so a missing profile cannot cause a partial group disconnect.
    let requests: Result<Vec<_>, _> = targets
      .iter()
      .map(|(id, database)| {
        self
          .profile(id)
          .cloned()
          .map(|profile| Request::Disconnect {
            profile,
            database: database.clone(),
          })
          .ok_or("The connection no longer exists")
      })
      .collect();
    let requests = match requests {
      Ok(requests) => requests,
      Err(error) => {
        self.set_status(error.into(), true);
        return;
      }
    };
    self.set_status(
      format!(
        "Disconnect requested for {} database session(s)",
        targets.len()
      ),
      false,
    );
    for (target, request) in targets.into_iter().zip(requests) {
      let task = self.start_database_task(request);
      let workspace = if self.active_target.as_ref() == Some(&target) {
        &mut self.workspace
      } else {
        self
          .workspaces
          .get_mut(&target)
          .expect("validated workspace")
      };
      workspace.busy = Some("Disconnecting SQL session".into());
      workspace.status = "Disconnecting SQL session".into();
      workspace.status_is_error = false;
      workspace.fetching_page = false;
      workspace.database_task = Some(task);
    }
  }
}
