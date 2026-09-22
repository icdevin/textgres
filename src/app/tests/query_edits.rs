// Exercise custom-result capability checks and refresh without a live SQL connection.
use super::*;

// Reuse table data while assigning a unique custom-query execution and projection.
fn query_app() -> (tempfile::TempDir, tokio::runtime::Runtime, App) {
  let (directory, runtime, mut app) = preview_app();
  let source = app.workspace.result.source.as_mut().unwrap();
  source.query = Some(Box::new(db::QuerySource {
    sql: "SELECT id, name, slug FROM items WHERE id=7".into(),
    execution_id: 1,
    table_oid: 42,
    table_columns: source.columns.clone(),
    attribute_ids: vec![1, 2, 3],
    mapping: vec![Some(0), Some(1), Some(2)],
  }));
  app.workspace.refresh_query = Some((
    source.table.clone(),
    source.query.as_ref().unwrap().sql.clone(),
  ));
  app.workspace.refresh_table = None;
  app.workspace.session_state = db::SessionState::Connected(db::TransactionState::Idle);
  (directory, runtime, app)
}

// Custom results stage ordinary edits but never enable insertion, deletion, or expression changes.
#[test]
fn custom_result_capabilities_are_separate() {
  let (_directory, _runtime, mut app) = query_app();
  assert!(app.add_row().unwrap_err().contains("updates only"));
  assert!(app.delete_row().unwrap_err().contains("updates only"));
  app.open_row_detail();
  let Some(Overlay::RowDetail(mut form)) = app.workspace.overlay.take() else {
    panic!("missing editor")
  };
  assert!(form.row_is_editable());
  form.values[2] = Some("computed change".into());
  assert!(app.stage_row(&form).is_err());
  form.values[2] = form.original[2].clone();
  form.values[1] = Some("after".into());
  app.stage_row(&form).unwrap();
  app.discard_changes();
  assert_eq!(app.workspace.result.rows[0][1].as_deref(), Some("before"));
  app.workspace.session_state = db::SessionState::Lost;
  assert!(app.stage_row(&form).is_err());
  app.open_row_detail();
  let Some(Overlay::RowDetail(form)) = &app.workspace.overlay else {
    panic!("missing editor")
  };
  assert!(!form.row_is_editable());
}

// Committed custom updates retire write authority and preserve the explicit original-query refresh.
#[test]
fn custom_save_marks_results_stale_without_automatic_rerun() {
  let (_directory, _runtime, mut app) = query_app();
  app.open_row_detail();
  let Some(Overlay::RowDetail(mut form)) = app.workspace.overlay.take() else {
    panic!("missing editor")
  };
  form.values[1] = Some("after".into());
  app.stage_row(&form).unwrap();
  app.save_changes().unwrap();
  let operation_id = app.workspace.database_task.as_ref().unwrap().operation_id();
  app.handle_database_response(Response {
    operation_id,
    session_state: Some(db::SessionState::Connected(db::TransactionState::Idle)),
    result: Ok(Output::QuerySaved),
  });
  assert!(app.workspace.database_task.is_none());
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 0);
  assert_eq!(app.workspace.result.rows[0][1].as_deref(), Some("after"));
  assert!(app.workspace.result.source.is_none());
  assert!(app.workspace.result.page.is_none());
  assert!(app.workspace.refresh_table.is_none());
  app.open_row_detail();
  let Some(Overlay::RowDetail(form)) = app.workspace.overlay.take() else {
    panic!("missing editor")
  };
  assert!(!form.row_is_editable());
  assert!(row_read_only_reason(&form).contains("stale"));
  app.sql = sql_editor("DELETE FROM items");
  assert_eq!(
    app.workspace.refresh_query.as_ref().unwrap().1,
    "SELECT id, name, slug FROM items WHERE id=7"
  );
  app.refresh_results(false).unwrap();
  assert!(app.workspace.database_task.is_some());
  assert_eq!(app.workspace.status, "Rerunning original query");
}

// Unknown outcomes cannot be retried, and an explicit refresh needs confirmation before replacing edits.
#[test]
fn custom_unknown_save_requires_verification() {
  let (_directory, _runtime, mut app) = query_app();
  app.workspace.edits.uncertain = true;
  assert!(app.save_changes().unwrap_err().contains("unknown"));
  app.refresh_results(false).unwrap();
  assert!(matches!(
    app.workspace.overlay,
    Some(Overlay::ConfirmRefresh)
  ));
  assert!(app.workspace.database_task.is_none());
  app.refresh_results(true).unwrap();
  assert!(app.workspace.database_task.is_some());
}

// Missing keys must be explained in both the viewer and save failure rather than silently disabling edits.
#[test]
fn custom_missing_key_reason_reaches_the_row_editor() {
  let (_directory, _runtime, mut app) = query_app();
  app.workspace.result.source = None;
  app.workspace.result.read_only_reason =
    Some("Include primary key column(s) id to edit rows".into());
  app.open_row_detail();
  let Some(Overlay::RowDetail(form)) = app.workspace.overlay.take() else {
    panic!("missing editor")
  };
  assert!(!form.row_is_editable());
  assert!(row_read_only_reason(&form).contains("id"));
  assert!(
    app
      .save_changes()
      .unwrap_err()
      .contains("Include primary key")
  );
}

// A failed rerun preserves drafts for inspection but cannot restore retired write authority.
#[test]
fn custom_failed_refresh_preserves_drafts_but_requires_rerun() {
  let (_directory, _runtime, mut app) = query_app();
  app.open_row_detail();
  let Some(Overlay::RowDetail(mut form)) = app.workspace.overlay.take() else {
    panic!("missing editor")
  };
  form.values[1] = Some("draft".into());
  app.stage_row(&form).unwrap();
  app.refresh_results(true).unwrap();
  let operation_id = app.workspace.database_task.as_ref().unwrap().operation_id();
  app.handle_database_response(Response {
    operation_id,
    session_state: Some(db::SessionState::Connected(db::TransactionState::Idle)),
    result: Err(anyhow::anyhow!("query failed")),
  });
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 1);
  assert_eq!(app.workspace.result.rows[0][1].as_deref(), Some("draft"));
  assert!(
    app
      .save_changes()
      .unwrap_err()
      .contains("no longer current")
  );
  app.open_row_detail();
  let Some(Overlay::RowDetail(form)) = app.workspace.overlay.take() else {
    panic!("missing editor")
  };
  assert!(!form.row_is_editable());
  app.discard_changes();
  assert_eq!(app.workspace.result.rows[0][1].as_deref(), Some("before"));
  assert!(app.workspace.refresh_query.is_some());
}
