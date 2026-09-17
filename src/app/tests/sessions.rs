// These routing tests use an unpolled runtime: no saved credentials or network are needed.
use super::*;
use db::{SessionState, TransactionState};

// Give each test stable targets with one saved profile and two databases.
fn targets() -> ((String, String), (String, String)) {
  (
    ("local".into(), "postgres".into()),
    ("local".into(), "second".into()),
  )
}

// Dispatch through normal editor handling so operation ownership is tested end to end.
fn start_sql(app: &mut App, sql: &str) -> u64 {
  app.sql = sql_editor(sql);
  app.run_sql();
  app.workspace.database_task.as_ref().unwrap().operation_id()
}

// Targets retain separate results while sharing text, cursor, and undo history.
#[test]
fn switching_preserves_shared_editor_and_separate_results() {
  let (_directory, _runtime, mut app) = preview_app();
  let (first, second) = targets();
  app.switch_workspace(first.clone());
  app.sql = sql_editor("SELECT 1");
  app
    .sql
    .input(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
  app
    .sql
    .input(KeyEvent::new(KeyCode::Char(';'), KeyModifiers::NONE));
  app.workspace.result_column = 2;
  app.workspace.status = "first".into();
  let result = app.workspace.result.clone();
  app.switch_workspace(second.clone());
  assert!(app.workspace.result.rows.is_empty());
  assert_eq!(app.sql.lines(), &["SELECT 1;"]);
  // Insertion at the retained cursor proves switching does not recreate the editor.
  app
    .sql
    .input(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
  app.switch_workspace(first);
  assert_eq!(app.workspace.result, result);
  assert_eq!(app.workspace.result_column, 2);
  assert_eq!(app.workspace.status, "first");
  assert_eq!(app.sql.lines(), &["SELECT 1;2"]);
  app
    .sql
    .input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
  assert_eq!(app.sql.lines(), &["SELECT 1;"]);
  app.switch_workspace(second);
  app
    .sql
    .input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
  assert_eq!(app.sql.lines(), &["SELECT 1"]);
}

// Concurrent replies must never replace the active editor, result, focus, or dialog.
#[test]
fn background_responses_stay_with_their_workspace() {
  let (_directory, _runtime, mut app) = preview_app();
  let (first, second) = targets();
  app.switch_workspace(first.clone());
  let first_id = start_sql(&mut app, "SELECT 1");
  app.switch_workspace(second.clone());
  let second_id = start_sql(&mut app, "SELECT 2");
  app.workspace.focus = Focus::Sql;
  app.workspace.overlay = Some(Overlay::SaveScript {
    name: "second".into(),
    cursor: 6,
  });
  app.handle_database_response(Response {
    operation_id: first_id,
    session_state: Some(SessionState::Connected(TransactionState::Open)),
    result: Ok(Output::Result(QueryResult {
      columns: vec!["first".into()],
      status: "first done".into(),
      ..Default::default()
    })),
  });
  assert_eq!(app.active_target, Some(second));
  assert_eq!(app.sql.lines(), &["SELECT 2"]);
  assert_eq!(app.workspace.focus, Focus::Sql);
  assert!(matches!(
    app.workspace.overlay,
    Some(Overlay::SaveScript { .. })
  ));
  assert!(app.workspace.result.columns.is_empty());
  assert_eq!(
    app.workspace.database_task.as_ref().unwrap().operation_id(),
    second_id
  );
  assert_eq!(app.workspaces[&first].result.columns, vec!["first"]);
  assert_eq!(
    app.workspaces[&first].session_state,
    SessionState::Connected(TransactionState::Open)
  );
  assert!(app.workspaces[&first].database_task.is_none());
  // A duplicate reply cannot alter already completed state or the newer operation.
  app.handle_database_response(Response {
    operation_id: first_id,
    session_state: None,
    result: Err(anyhow::anyhow!("stale")),
  });
  assert_eq!(app.workspaces[&first].status, "first done");
  app.handle_database_response(Response {
    operation_id: second_id,
    session_state: None,
    result: Err(anyhow::anyhow!("second failed")),
  });
  assert!(app.workspace.status.contains("second failed"));
  assert_eq!(app.workspaces[&first].status, "first done");
}

// Recovery of an inactive row edit must not open its dialog over another database.
#[test]
fn background_row_error_restores_only_its_own_form() {
  let (_directory, _runtime, mut app) = pending_row_app();
  let (first, second) = targets();
  app.switch_workspace(first.clone());
  let id = app.workspace.database_task.as_ref().unwrap().operation_id();
  app.switch_workspace(second);
  app.handle_database_response(Response {
    operation_id: id,
    session_state: None,
    result: Err(db::Cancelled.into()),
  });
  assert!(app.workspace.overlay.is_none());
  assert!(matches!(
    app.workspaces[&first].overlay,
    Some(Overlay::RowDetail(_))
  ));
  assert!(app.workspaces[&first].pending_row_edit.is_none());
}

// Explorer activation and keyboard cycling remain usable while a different workspace runs SQL.
#[test]
fn busy_workspace_does_not_block_navigation_or_other_operations() {
  let (_directory, _runtime, mut app) = preview_app();
  let (first, second) = targets();
  app.switch_workspace(first.clone());
  let id = start_sql(&mut app, "SELECT 1");
  app
    .databases
    .insert("local".into(), vec!["postgres".into(), "second".into()]);
  app.expanded.insert(NodeKey::Connection("local".into()));
  app.explorer_selected = 2;
  app.activate_selected();
  assert_eq!(app.active_target, Some(second.clone()));
  assert!(app.workspace.database_task.is_some());
  assert_eq!(
    app.workspaces[&first]
      .database_task
      .as_ref()
      .unwrap()
      .operation_id(),
    id
  );
  app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::CONTROL));
  assert_eq!(app.active_target, Some(first));
  app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
  assert_eq!(app.workspace.status, "Cancelling…");
  assert!(
    app.workspaces[&second]
      .status
      .starts_with("Loading schemas")
  );
}

// Closing any open, failed, or unknown transaction needs explicit Y; Enter is not consent.
#[test]
fn disconnect_and_reconnect_require_transaction_confirmation() {
  for state in [
    TransactionState::Open,
    TransactionState::Failed,
    TransactionState::Unknown,
  ] {
    for action in [SessionAction::Disconnect, SessionAction::Reconnect] {
      let (_directory, _runtime, mut app) = preview_app();
      app.switch_workspace(targets().0);
      app.workspace.session_state = SessionState::Connected(state);
      app.request_session_action(action);
      assert!(
        matches!(app.workspace.overlay, Some(Overlay::ConfirmSession(found)) if found == action)
      );
      assert!(app.workspace.database_task.is_none());
      app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
      assert!(app.workspace.database_task.is_none());
      app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
      assert!(app.workspace.overlay.is_none());
      app.request_session_action(action);
      app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
      assert!(app.workspace.overlay.is_none());
      assert!(app.workspace.database_task.is_some());
    }
  }
}

// Quit considers inactive transactions and running operations, not just the selected database.
#[test]
fn quit_protects_inactive_transactions_and_workers() {
  for busy in [false, true] {
    let (_directory, _runtime, mut app) = preview_app();
    let (first, second) = targets();
    app.switch_workspace(first);
    if busy {
      start_sql(&mut app, "SELECT 1");
    } else {
      app.workspace.session_state = SessionState::Connected(TransactionState::Open);
    }
    app.switch_workspace(second);
    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
    assert!(!app.should_quit);
    assert!(matches!(
      app.workspace.overlay,
      Some(Overlay::ConfirmSession(SessionAction::Quit))
    ));
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(!app.should_quit);
    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(app.should_quit);
  }
}

// A busy workspace refuses lifecycle actions without cancelling or replacing its worker.
#[test]
fn lifecycle_rejects_running_workspace() {
  let (_directory, _runtime, mut app) = preview_app();
  app.switch_workspace(targets().0);
  let id = start_sql(&mut app, "SELECT 1");
  for key in [6, 7, 8] {
    app.handle_key(KeyEvent::new(KeyCode::F(key), KeyModifiers::NONE));
    assert_eq!(
      app.workspace.database_task.as_ref().unwrap().operation_id(),
      id
    );
    assert!(app.workspace.overlay.is_none());
    assert!(app.workspace.status.contains("Cancel or wait"));
  }
}

// Profile mutation must respect running work in every database, including metadata operations.
#[test]
fn profile_change_rejects_inactive_work_and_invalidates_all_previews() {
  let (_directory, _runtime, mut app) = preview_app();
  let (first, second) = targets();
  app.switch_workspace(first.clone());
  let form = ConnectionForm::edit(&app.profiles[0]);
  let id = start_sql(&mut app, "SELECT 1");
  app.switch_workspace(second);
  assert!(
    app
      .save_connection(&form)
      .unwrap_err()
      .contains("all operations")
  );
  app.delete_connection("local");
  assert_eq!(app.profiles.len(), 1);
  app.handle_database_response(Response {
    operation_id: id,
    session_state: None,
    result: Err(db::Cancelled.into()),
  });
  let old_form = RowDetail::new(&app.workspaces[&first].result, 0).unwrap();
  app.workspaces.get_mut(&first).unwrap().overlay = Some(Overlay::RowDetail(Box::new(old_form)));
  app.workspace.result = editable_result();
  app.save_connection(&form).unwrap();
  assert!(app.workspaces[&first].result.source.is_none());
  assert!(app.workspaces[&first].overlay.is_none());
  app.switch_workspace(first);
  assert_eq!(app.sql.lines(), &["SELECT 1"]);
}

// Explicit lifecycle results update state while preserving useful drafts and previous results.
#[test]
fn lifecycle_response_preserves_workspace_contents() {
  let (_directory, _runtime, mut app) = preview_app();
  app.switch_workspace(targets().0);
  app.sql = sql_editor("BEGIN");
  let result = app.workspace.result.clone();
  app.handle_key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE));
  let id = app.workspace.database_task.as_ref().unwrap().operation_id();
  app.handle_database_response(Response {
    operation_id: id,
    session_state: Some(SessionState::Connected(TransactionState::Idle)),
    result: Ok(Output::Session),
  });
  assert_eq!(app.sql.lines(), &["BEGIN"]);
  assert_eq!(app.workspace.result, result);
  assert_eq!(app.workspace.status, "autocommit");
}

// SQL written before selecting a target or after a profile change remains shared.
#[test]
fn targetless_input_survives_restoring_and_cycling_workspaces() {
  let (_directory, _runtime, mut app) = preview_app();
  let (first, second) = targets();
  app.sql = sql_editor("SELECT original");
  app.switch_workspace(first.clone());
  assert_eq!(app.sql.lines(), &["SELECT original"]);
  app.park_workspace();
  app.sql = sql_editor("SELECT revised");
  app.switch_workspace(first.clone());
  assert_eq!(app.sql.lines(), &["SELECT revised"]);
  app.switch_workspace(second);
  app.cycle_workspace(false);
  assert_eq!(app.active_target, Some(first));
  assert_eq!(app.sql.lines(), &["SELECT revised"]);
}

// A profile is connected if any of its databases is connected, including an inactive workspace.
#[test]
fn explorer_markers_follow_live_sessions() {
  let (_directory, _runtime, mut app) = preview_app();
  let (first, second) = targets();
  let profile = ExplorerNode::Connection("local".into());
  let first_node = ExplorerNode::Database {
    profile_id: first.0.clone(),
    database: first.1.clone(),
  };
  let second_node = ExplorerNode::Database {
    profile_id: second.0.clone(),
    database: second.1.clone(),
  };
  app.switch_workspace(first.clone());
  assert!(!app.explorer_connected(&profile));
  app.workspace.session_state = SessionState::Connected(TransactionState::Open);
  app.switch_workspace(second);
  assert!(app.explorer_connected(&profile));
  assert!(app.explorer_connected(&first_node));
  assert!(!app.explorer_connected(&second_node));
  app.workspaces.get_mut(&first).unwrap().session_state = SessionState::Lost;
  assert!(!app.explorer_connected(&profile));
  app.workspace.session_state = SessionState::Connected(TransactionState::Unknown);
  assert!(app.explorer_connected(&profile));
  assert!(app.explorer_connected(&second_node));
  assert!(!app.explorer_connected(&ExplorerNode::Connection("other".into())));
  app.workspace.session_state = SessionState::Disconnected;
  assert!(!app.explorer_connected(&profile));
}

// Collapse only changes the tree; expanding cached metadata still verifies the persistent session.
#[test]
fn expansion_connects_and_collapse_keeps_session() {
  let (_directory, _runtime, mut app) = preview_app();
  app.activate_selected();
  let id = app.workspace.database_task.as_ref().unwrap().operation_id();
  assert!(!app.explorer_connected(&ExplorerNode::Connection("local".into())));
  app.handle_database_response(Response {
    operation_id: id,
    session_state: Some(SessionState::Connected(TransactionState::Idle)),
    result: Ok(Output::Databases {
      profile_id: "local".into(),
      names: vec!["postgres".into()],
    }),
  });
  app.activate_selected();
  assert!(!app.expanded.contains(&NodeKey::Connection("local".into())));
  assert!(app.explorer_connected(&ExplorerNode::Connection("local".into())));
  assert!(app.workspace.database_task.is_none());
  app.activate_selected();
  assert!(app.workspace.database_task.is_some());
  assert!(app.expanded.contains(&NodeKey::Connection("local".into())));
}
