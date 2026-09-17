// Paging responses must append to their owning workspace without changing staged edits.
use super::*;

// Cursor IDs are deliberately distinct from operation IDs in these fixtures.
fn page(id: u64) -> db::PageRef {
  db::PageRef {
    id,
    profile_id: "local".into(),
    database: "postgres".into(),
    preview: true,
  }
}

// One boundary key starts one fetch, and repeated keys while busy must not enqueue duplicates.
#[test]
fn scrolling_fetches_once_and_appends_without_resetting_selection() {
  let (_directory, _runtime, mut app) = preview_app();
  app.workspace.result.page = Some(page(1));
  app.workspace.focus = Focus::Results;
  app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
  let operation_id = app.workspace.database_task.as_ref().unwrap().operation_id();
  app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
  assert_eq!(
    app.workspace.database_task.as_ref().unwrap().operation_id(),
    operation_id
  );
  let next = vec![vec![Some("8".into()), Some("next".into()), None]];
  app.handle_database_response(Response {
    operation_id,
    session_state: None,
    result: Ok(Output::Page {
      requested: page(1),
      result: QueryResult {
        columns: app.workspace.result.columns.clone(),
        rows: next.clone(),
        ..Default::default()
      },
    }),
  });
  assert_eq!(app.workspace.result.rows.len(), 2);
  assert_eq!(app.workspace.result.rows[1], next[0]);
  assert_eq!(app.workspace.result_row, 0);
  assert!(app.workspace.result.source.is_some());
  assert!(app.workspace.result.page.is_none());
  app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
  assert!(app.workspace.database_task.is_none());
  assert_eq!(app.workspace.result_row, 1);
}

// Appended original rows go before new drafts; discard must retain every fetched original.
#[test]
fn later_pages_preserve_modified_deleted_and_inserted_rows() {
  let (_directory, _runtime, mut app) = preview_app();
  app.open_row_detail();
  let Some(Overlay::RowDetail(mut form)) = app.workspace.overlay.take() else {
    panic!("missing editor")
  };
  form.values[1] = Some("modified".into());
  app.stage_row(&form).unwrap();
  app.delete_row().unwrap();
  app.add_row().unwrap();
  app.workspace.overlay = None;
  app.workspace.result.page = Some(page(1));
  app.workspace.focus = Focus::Results;
  app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
  let operation_id = app.workspace.database_task.as_ref().unwrap().operation_id();
  let next = vec![Some("8".into()), Some("next".into()), None];
  app.handle_database_response(Response {
    operation_id,
    session_state: None,
    result: Ok(Output::Page {
      requested: page(1),
      result: QueryResult {
        columns: app.workspace.result.columns.clone(),
        rows: vec![next.clone()],
        ..Default::default()
      },
    }),
  });
  assert_eq!(app.workspace.result.rows.len(), 3);
  assert_eq!(app.workspace.result_row, 2);
  assert_eq!(app.workspace.edits.defaults(2), Some(&vec![true; 3]));
  assert!(app.workspace.edits.defaults(1).is_none());
  assert!(app.workspace.edits.deleted.contains(&0));
  assert_eq!(app.workspace.edits.count(&app.workspace.result), 2);
  app.discard_changes();
  assert_eq!(app.workspace.result.rows.len(), 2);
  assert_eq!(app.workspace.result.rows[0][1].as_deref(), Some("before"));
  assert_eq!(app.workspace.result.rows[1], next);
}

// Fetch errors retain loaded rows and drafts but disable a continuation that may have advanced.
#[test]
fn failed_page_fetch_and_wrong_cursor_preserve_loaded_values() {
  for wrong_cursor in [false, true] {
    let (_directory, _runtime, mut app) = preview_app();
    app.workspace.result.page = Some(page(1));
    let original = app.workspace.result.rows.clone();
    app.workspace.focus = Focus::Results;
    app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    let operation_id = app.workspace.database_task.as_ref().unwrap().operation_id();
    let result = if wrong_cursor {
      Ok(Output::Page {
        requested: page(2),
        result: QueryResult::default(),
      })
    } else {
      Err(db::Cancelled.into())
    };
    app.handle_database_response(Response {
      operation_id,
      session_state: None,
      result,
    });
    assert_eq!(app.workspace.result.rows, original);
    assert!(app.workspace.result.page.is_none());
    assert!(app.workspace.status_is_error);
  }
}

// A background fetch appends only to its original database's results.
#[test]
fn fetched_pages_follow_their_workspace() {
  let (_directory, _runtime, mut app) = preview_app();
  let first = ("local".into(), "postgres".into());
  app.switch_workspace(first.clone());
  app.workspace.result.page = Some(page(1));
  app.workspace.focus = Focus::Results;
  app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
  let operation_id = app.workspace.database_task.as_ref().unwrap().operation_id();
  let columns = app.workspace.result.columns.clone();
  app.switch_workspace(("local".into(), "second".into()));
  app.handle_database_response(Response {
    operation_id,
    session_state: None,
    result: Ok(Output::Page {
      requested: page(1),
      result: QueryResult {
        columns,
        rows: vec![vec![Some("8".into()), None, None]],
        ..Default::default()
      },
    }),
  });
  assert!(app.workspace.result.rows.is_empty());
  app.switch_workspace(first);
  assert_eq!(app.workspace.result.rows.len(), 2);
}
