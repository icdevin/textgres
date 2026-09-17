// Explorer disconnection must follow the selected node without retargeting the visible workspace.
use super::*;
use db::{SessionState, TransactionState};

// Populate a connected profile with two databases while leaving the second database active.
fn connected_workspaces(app: &mut App) -> [(String, String); 2] {
  let first = ("local".into(), "postgres".into());
  let second = ("local".into(), "second".into());
  app.switch_workspace(first.clone());
  app.workspace.session_state = SessionState::Connected(TransactionState::Idle);
  app.switch_workspace(second.clone());
  app.workspace.session_state = SessionState::Connected(TransactionState::Idle);
  app.expanded.insert(NodeKey::Connection("local".into()));
  app
    .databases
    .insert("local".into(), vec!["postgres".into(), "second".into()]);
  app.workspace.focus = Focus::Explorer;
  [first, second]
}

// Confirmed responses must update connection markers only after their owning workers finish.
fn complete_disconnect(app: &mut App, target: &(String, String)) {
  let operation_id = if app.active_target.as_ref() == Some(target) {
    app.workspace.database_task.as_ref()
  } else {
    app.workspaces[target].database_task.as_ref()
  }
  .unwrap()
  .operation_id();
  app.handle_database_response(Response {
    operation_id,
    session_state: Some(SessionState::Disconnected),
    result: Ok(Output::Session),
  });
}

// Selecting the profile closes all its databases, including an inactive one, without losing local state.
#[test]
fn explorer_profile_disconnects_all_databases_and_keeps_drafts() {
  let (_directory, _runtime, mut app) = preview_app();
  let [first, second] = connected_workspaces(&mut app);
  app.workspace.result = editable_result();
  app.add_row().unwrap();
  app.workspace.overlay = None;
  let result = app.workspace.result.clone();
  app.sql = sql_editor("SELECT retained");
  app.explorer_selected = 0;
  app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
  assert_eq!(app.active_target, Some(second.clone()));
  assert!(app.workspaces[&first].database_task.is_some());
  assert!(app.workspace.database_task.is_some());
  complete_disconnect(&mut app, &first);
  assert!(app.explorer_connected(&ExplorerNode::Connection("local".into())));
  complete_disconnect(&mut app, &second);
  assert!(!app.explorer_connected(&ExplorerNode::Connection("local".into())));
  assert_eq!(app.workspace.result, result);
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
  assert_eq!(app.sql.lines(), &["SELECT retained"]);
  assert_eq!(app.workspace.focus, Focus::Explorer);
}

// A selected database closes only that target even when a different database is active.
#[test]
fn explorer_database_disconnect_uses_selection() {
  let (_directory, _runtime, mut app) = preview_app();
  let [first, second] = connected_workspaces(&mut app);
  app.explorer_selected = 1;
  app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
  assert_eq!(app.active_target, Some(second));
  assert!(app.workspace.database_task.is_none());
  assert!(app.workspaces[&first].database_task.is_some());
  complete_disconnect(&mut app, &first);
  assert_eq!(
    app.workspaces[&first].session_state,
    SessionState::Disconnected
  );
  assert!(matches!(
    app.workspace.session_state,
    SessionState::Connected(_)
  ));
}

// Hidden open, failed, or unknown transactions require one confirmation before any database closes.
#[test]
fn explorer_disconnect_confirms_entire_group_before_rollback() {
  for state in [
    TransactionState::Open,
    TransactionState::Failed,
    TransactionState::Unknown,
  ] {
    let (_directory, _runtime, mut app) = preview_app();
    let [first, second] = connected_workspaces(&mut app);
    app.workspaces.get_mut(&first).unwrap().session_state = SessionState::Connected(state);
    app.explorer_selected = 0;
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
    assert!(matches!(
      app.workspace.overlay,
      Some(Overlay::ConfirmExplorerDisconnect { .. })
    ));
    assert!(app.workspace.database_task.is_none());
    assert!(app.workspaces[&first].database_task.is_none());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.workspace.overlay.is_none());
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
    // The dialog owns its target list even if the Explorer selection changes before confirmation.
    app.explorer_selected = 1;
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(app.active_target, Some(second));
    assert!(app.workspace.database_task.is_some());
    assert!(app.workspaces[&first].database_task.is_some());
  }
}

// Busy databases block the whole group so a root disconnect cannot silently close only some sessions.
#[test]
fn explorer_disconnect_rejects_busy_group_without_partial_disconnect() {
  let (_directory, _runtime, mut app) = preview_app();
  let [first, second] = connected_workspaces(&mut app);
  app.switch_workspace(first.clone());
  app.sql = sql_editor("SELECT 1");
  app.run_sql();
  let original_operation = app.workspace.database_task.as_ref().unwrap().operation_id();
  app.switch_workspace(second);
  app.explorer_selected = 0;
  app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
  assert!(app.workspace.database_task.is_none());
  assert!(app.workspace.status_is_error);
  assert_eq!(
    app.workspaces[&first]
      .database_task
      .as_ref()
      .unwrap()
      .operation_id(),
    original_operation
  );
}

// Descendants resolve their database, and repeated disconnects do not create unnecessary workers.
#[test]
fn explorer_descendants_disconnect_and_clear_preview_continuation() {
  let (_directory, _runtime, mut app) = preview_app();
  let [first, second] = connected_workspaces(&mut app);
  app
    .expanded
    .insert(NodeKey::Database(first.0.clone(), first.1.clone()));
  app.schemas.insert(first.clone(), vec!["public".into()]);
  app.explorer_selected = 2;
  assert!(matches!(
    app.explorer_rows()[2].node,
    ExplorerNode::Schema { .. }
  ));
  app.workspaces.get_mut(&first).unwrap().result.page = Some(db::PageRef {
    id: 42,
    profile_id: first.0.clone(),
    database: first.1.clone(),
    preview: true,
  });
  app.handle_key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE));
  complete_disconnect(&mut app, &first);
  assert!(app.workspaces[&first].result.page.is_none());
  assert!(!app.workspaces[&first].result.rows.is_empty());
  app.handle_key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE));
  assert!(app.workspaces[&first].database_task.is_none());
  assert!(app.workspace.status.contains("already disconnected"));
  assert_eq!(app.active_target, Some(second));
}

// The toggle connects a disconnected selected database rather than disconnecting the active one.
#[test]
fn explorer_toggle_connects_selected_database_and_preserves_other_workspace() {
  let (_directory, _runtime, mut app) = preview_app();
  let [first, second] = connected_workspaces(&mut app);
  app.workspaces.get_mut(&first).unwrap().session_state = SessionState::Disconnected;
  app.workspace.result = editable_result();
  app.sql = sql_editor("SELECT retained");
  app.explorer_selected = 1;
  assert!(!app.selected_connection_is_connected());
  app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
  assert_eq!(app.active_target, Some(second.clone()));
  assert!(app.workspace.database_task.is_none());
  app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
  assert_eq!(app.active_target, Some(first.clone()));
  let operation_id = app.workspace.database_task.as_ref().unwrap().operation_id();
  app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
  assert_eq!(
    app.workspace.database_task.as_ref().unwrap().operation_id(),
    operation_id
  );
  app.handle_database_response(Response {
    operation_id,
    session_state: Some(SessionState::Connected(TransactionState::Idle)),
    result: Ok(Output::Session),
  });
  assert!(app.selected_connection_is_connected());
  assert!(matches!(
    app.workspaces[&second].session_state,
    SessionState::Connected(_)
  ));
  assert_eq!(app.workspaces[&second].result, editable_result());
  assert_eq!(app.sql.lines(), &["SELECT retained"]);
  app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
  complete_disconnect(&mut app, &first);
  assert!(!app.selected_connection_is_connected());
}

// A collapsed profile connects its default database, including after a lost session.
#[test]
fn explorer_toggle_connects_profile_default_database() {
  for state in [SessionState::Disconnected, SessionState::Lost] {
    let (_directory, _runtime, mut app) = preview_app();
    let target = ("local".into(), "postgres".into());
    app.switch_workspace(target.clone());
    app.workspace.session_state = state;
    app.explorer_selected = 0;
    assert!(!app.selected_connection_is_connected());
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
    assert_eq!(app.active_target, Some(target));
    assert!(app.workspace.database_task.is_some());
    assert!(app.workspace.overlay.is_none());
    assert!(app.workspace.busy.as_ref().unwrap().contains("Connect"));
  }
}
