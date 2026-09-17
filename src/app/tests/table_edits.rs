// Application tests exercise staging, ownership, and error recovery without database I/O.
use super::*;

// Stage a changed cell through the same form path used by the row editor.
fn modify(app: &mut App, row: usize, value: &str) {
  app.workspace.result_row = row;
  app.open_row_detail();
  let Some(Overlay::RowDetail(mut form)) = app.workspace.overlay.take() else {
    panic!("missing row editor")
  };
  form.values[1] = Some(value.into());
  app.stage_row(&form).unwrap();
}

// Staging multiple rows is local; cancelling restores all original values and removes inserts.
#[test]
fn batch_staging_and_discard_preserve_original_preview() {
  let (_directory, _runtime, mut app) = preview_app();
  let original = app.workspace.result.clone();
  modify(&mut app, 0, "changed");
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
  assert!(
    app
      .workspace
      .edits
      .cell_changed(&app.workspace.result, 0, 1)
  );
  assert!(
    !app
      .workspace
      .edits
      .cell_changed(&app.workspace.result, 0, 0)
  );
  app.workspace.result_row = 0;
  app.delete_row().unwrap();
  assert!(app.workspace.edits.deleted.contains(&0));
  app.delete_row().unwrap();
  assert_eq!(app.workspace.result.rows[0][1].as_deref(), Some("changed"));
  app.add_row().unwrap();
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 2);
  assert_eq!(app.workspace.edits.defaults(1), Some(&vec![true; 3]));
  assert!(app.workspace.database_task.is_none());
  app.workspace.overlay = None;
  app.discard_changes();
  assert_eq!(app.workspace.result, original);
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 0);
}

// Repeated edits compare against the loaded row; reverting to the original removes the change.
#[test]
fn reverting_cell_and_deleting_new_row_remove_changes() {
  let (_directory, _runtime, mut app) = preview_app();
  modify(&mut app, 0, "first");
  modify(&mut app, 0, "second");
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
  modify(&mut app, 0, "before");
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 0);
  app.add_row().unwrap();
  app.workspace.overlay = None;
  app.add_row().unwrap();
  app.workspace.overlay = None;
  app.workspace.result_row = 1;
  app.delete_row().unwrap();
  assert_eq!(app.workspace.result.rows.len(), 2);
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
  assert!(app.workspace.edits.defaults(1).is_some());
  app.delete_row().unwrap();
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 0);
}

// New-row field editing distinguishes DEFAULT, NULL, and an intentional empty string.
#[test]
fn insert_form_tracks_defaults_and_explicit_nulls() {
  let (_directory, _runtime, mut app) = preview_app();
  app.add_row().unwrap();
  let Some(Overlay::RowDetail(mut form)) = app.workspace.overlay.take() else {
    panic!("missing row editor")
  };
  assert!(form.is_new);
  assert!(form.defaults.iter().all(|value| *value));
  form.selected = 1;
  form.toggle_null();
  assert_eq!(form.values[1], None);
  assert!(!form.defaults[1]);
  form.begin_edit();
  form.finish_edit();
  assert_eq!(form.values[1], Some(String::new()));
  assert!(!form.defaults[1]);
  form.toggle_null();
  assert_eq!(form.values[1], None);
  assert!(!form.defaults[1]);
  app.workspace.overlay = Some(Overlay::RowDetail(form));
  app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
  let Some(Overlay::RowDetail(form)) = app.workspace.overlay.take() else {
    panic!("missing row editor")
  };
  assert!(form.defaults[1]);
  assert!(form.selected_is_editable());
  app.stage_row(&form).unwrap();
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
}

// Refresh requires consent and retains changes if its worker fails or is cancelled.
#[test]
fn refresh_preserves_draft_until_success() {
  let (_directory, _runtime, mut app) = preview_app();
  let original = app.workspace.result.clone();
  modify(&mut app, 0, "changed");
  app.workspace.focus = Focus::Results;
  app.handle_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE));
  assert!(matches!(
    app.workspace.overlay,
    Some(Overlay::ConfirmRefresh)
  ));
  assert!(app.workspace.database_task.is_none());
  app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
  app.refresh_results(true).unwrap();
  let id = app.workspace.database_task.as_ref().unwrap().operation_id();
  app.handle_database_response(Response {
    operation_id: id,
    session_state: None,
    result: Err(db::Cancelled.into()),
  });
  assert_eq!(app.workspace.result.rows[0][1].as_deref(), Some("changed"));
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
  app.refresh_results(true).unwrap();
  let id = app.workspace.database_task.as_ref().unwrap().operation_id();
  app.handle_database_response(Response {
    operation_id: id,
    session_state: None,
    result: Ok(Output::Result(original.clone())),
  });
  assert_eq!(app.workspace.result, original);
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 0);
}

// Local batches survive target changes and prevent silent replacement, profile changes, and quit.
#[test]
fn pending_batches_guard_navigation_and_quit() {
  let (_directory, _runtime, mut app) = preview_app();
  let first = ("local".into(), "postgres".into());
  app.switch_workspace(first.clone());
  modify(&mut app, 0, "changed");
  app.sql = sql_editor("SELECT 1");
  app.run_sql();
  assert!(app.workspace.database_task.is_none());
  let form = ConnectionForm::edit(&app.profiles[0]);
  app.switch_workspace(("local".into(), "other".into()));
  assert!(
    app
      .save_connection(&form)
      .unwrap_err()
      .contains("table changes")
  );
  app.request_session_action(SessionAction::Quit);
  assert!(!app.should_quit);
  assert!(matches!(
    app.workspace.overlay,
    Some(Overlay::ConfirmSession(SessionAction::Quit))
  ));
  app.workspace.overlay = None;
  app.switch_workspace(first);
  assert_eq!(app.workspace.result.rows[0][1].as_deref(), Some("changed"));
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
}

// An uncertain COMMIT blocks edits and retries even after local discard, until refresh succeeds.
#[test]
fn unknown_commit_requires_refresh_before_retry() {
  let (_directory, _runtime, mut app) = pending_row_app();
  let id = app.workspace.database_task.as_ref().unwrap().operation_id();
  app.handle_database_response(Response {
    operation_id: id,
    session_state: None,
    result: Err(db::UnknownCommit(anyhow::anyhow!("connection lost")).into()),
  });
  assert!(app.workspace.edits.uncertain);
  assert!(app.save_changes().unwrap_err().contains("unknown"));
  assert!(app.add_row().is_err());
  assert!(app.delete_row().is_err());
  app.discard_changes();
  assert!(app.workspace.edits.uncertain);
  assert!(app.save_changes().is_err());
  app.refresh_results(false).unwrap();
  let id = app.workspace.database_task.as_ref().unwrap().operation_id();
  app.handle_database_response(Response {
    operation_id: id,
    session_state: None,
    result: Ok(Output::Result(editable_result())),
  });
  assert!(!app.workspace.edits.uncertain);
  modify(&mut app, 0, "retry after verification");
  app.save_changes().unwrap();
}

// A committed batch cannot be retried when only its refresh failed; F5 still knows its source.
#[test]
fn committed_batch_keeps_refresh_target_after_preview_failure() {
  let (_directory, _runtime, mut app) = pending_row_app();
  let id = app.workspace.database_task.as_ref().unwrap().operation_id();
  app.handle_database_response(Response {
    operation_id: id,
    session_state: None,
    result: Ok(Output::Saved(Err(anyhow::anyhow!("refresh failed")))),
  });
  assert!(app.workspace.result.rows.is_empty());
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 0);
  app.refresh_results(false).unwrap();
  assert!(app.workspace.database_task.is_some());
}

// An active save owns an immutable batch; local actions cannot change or discard its values.
#[test]
fn running_save_freezes_local_changes() {
  let (_directory, _runtime, mut app) = pending_row_app();
  let staged = app.workspace.result.clone();
  assert!(app.add_row().is_err());
  assert!(app.delete_row().is_err());
  assert!(app.save_changes().is_err());
  assert!(app.refresh_results(true).is_err());
  app.discard_changes();
  app.open_row_detail();
  let Some(Overlay::RowDetail(form)) = app.workspace.overlay.take() else {
    panic!("missing row viewer")
  };
  assert!(!form.row_is_editable());
  assert!(app.stage_row(&form).is_err());
  assert_eq!(app.workspace.result, staged);
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
}

// Read-only result types reject writes; keyless base tables still permit inserts.
#[test]
fn batch_permissions_follow_table_metadata() {
  let (_directory, _runtime, mut app) = preview_app();
  app.workspace.result.source.as_mut().unwrap().table.kind = "view".into();
  assert!(app.add_row().is_err());
  assert!(app.delete_row().is_err());
  app.workspace.result = editable_result();
  for column in &mut app.workspace.result.source.as_mut().unwrap().columns {
    column.primary_key = false;
  }
  assert!(app.delete_row().is_err());
  app.add_row().unwrap();
  app.workspace.overlay = None;
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
  app.workspace.result = QueryResult::default();
  assert!(app.refresh_results(false).is_err());
}
