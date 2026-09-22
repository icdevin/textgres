// Exercise the settings dialog through real key handling and durable storage.
use super::*;

// Cached metadata must use current preferences without closing a hidden active database.
#[test]
fn settings_save_filters_cached_metadata_and_preserves_workspace() {
  let (_directory, _runtime, mut app) = preview_app();
  app.expanded.insert(NodeKey::Connection("local".into()));
  app
    .expanded
    .insert(NodeKey::Database("local".into(), "postgres".into()));
  app
    .databases
    .insert("local".into(), vec!["postgres".into(), "second".into()]);
  app.schemas.insert(
    ("local".into(), "postgres".into()),
    [
      "public",
      "pg_catalog",
      "information_schema",
      "pg_temp_1",
      "pg_toast",
      "pgxtempx1",
    ]
    .map(String::from)
    .to_vec(),
  );
  app.active_target = Some(("local".into(), "second".into()));
  app.workspace.session_state = db::SessionState::Connected(db::TransactionState::Open);
  let result = app.workspace.result.clone();
  app.explorer_selected = app.explorer_rows().len() - 1;
  app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
  app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
  // Saving the draft hides the second database but keeps its workspace usable.
  app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
  assert_eq!(app.storage.load_settings().unwrap(), app.settings);
  assert!(app.workspace.overlay.is_none());
  assert!(!app.settings.show_all_databases);
  assert_eq!(
    app
      .explorer_rows()
      .iter()
      .map(|row| row.label.as_str())
      .collect::<Vec<_>>(),
    ["Local", "postgres", "public", "pgxtempx1"]
  );
  assert!(app.explorer_selected < app.explorer_rows().len());
  assert_eq!(app.active_target, Some(("local".into(), "second".into())));
  assert_eq!(app.workspace.result, result);
  assert_eq!(
    app.workspace.session_state,
    db::SessionState::Connected(db::TransactionState::Open)
  );

  app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
  app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
  app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
  app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
  let labels: Vec<_> = app
    .explorer_rows()
    .into_iter()
    .map(|row| row.label)
    .collect();
  assert!(labels.contains(&"pg_temp_1".into()));
  assert!(labels.contains(&"pg_toast".into()));
  assert!(!labels.contains(&"pg_catalog".into()));
  app.settings.show_system_schemas = true;
  assert!(
    app
      .explorer_rows()
      .iter()
      .any(|row| row.label == "information_schema")
  );
}

// Escape discards edits, while a failed write retains the draft and reports the failure.
#[test]
fn settings_cancel_and_save_failure_do_not_apply_draft() {
  let (directory, _runtime, mut app) = preview_app();
  let original = app.settings;
  app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
  app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
  app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
  assert_eq!(app.settings, original);
  assert!(!directory.path().join("settings.toml").exists());
  std::fs::create_dir(directory.path().join("settings.toml")).unwrap();
  app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
  app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
  app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
  assert_eq!(app.settings, original);
  assert!(
    matches!(app.workspace.overlay, Some(Overlay::Settings { draft, .. }) if !draft.show_all_databases)
  );
  assert!(app.workspace.status_is_error);
  assert!(app.workspace.status.contains("Could not save settings"));
}
