// Exercise deletion through real key events and temporary script files.
use super::*;
use ratatui::{Terminal, backend::TestBackend};

// Keep selection and persistence checks independent of database sessions.
fn script_app() -> (tempfile::TempDir, tokio::runtime::Runtime, App) {
  let (directory, runtime, mut app) = preview_app();
  for name in ["first", "second"] {
    app.storage.save_script(name, "SELECT 1;").unwrap();
  }
  app.scripts = app.storage.list_scripts().unwrap();
  app.sql = sql_editor("SELECT unsaved;");
  app.workspace.focus = Focus::Sql;
  app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
  app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
  (directory, runtime, app)
}

// Both deletion keys must show the target and preserve files until explicit confirmation.
#[test]
fn script_deletion_requires_confirmation_and_supports_cancel() {
  let (_directory, _runtime, mut app) = script_app();
  let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
  for (delete, cancel) in [
    (KeyCode::Char('d'), KeyCode::Char('n')),
    (KeyCode::Delete, KeyCode::Esc),
  ] {
    app.handle_key(KeyEvent::new(delete, KeyModifiers::NONE));
    terminal
      .draw(|frame| crate::ui::draw(frame, &mut app))
      .unwrap();
    let screen = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|cell| cell.symbol())
      .collect::<String>();
    assert!(screen.contains("Delete script “second.sql”?"));
    assert!(screen.contains("Y/Enter"));
    assert!(screen.contains("N/Esc"));
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    assert!(matches!(
      app.workspace.overlay,
      Some(Overlay::ConfirmDeleteScript { .. })
    ));
    assert_eq!(app.storage.list_scripts().unwrap(), ["first", "second"]);
    app.handle_key(KeyEvent::new(cancel, KeyModifiers::NONE));
    assert!(matches!(
      app.workspace.overlay,
      Some(Overlay::LoadScript { selected: 1 })
    ));
  }
  assert_eq!(app.sql.lines(), &["SELECT unsaved;"]);
}

// Deleting the last entry clamps selection; deleting all entries closes the picker.
#[test]
fn confirmed_script_deletion_updates_picker_and_keeps_editor() {
  let (_directory, _runtime, mut app) = script_app();
  app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
  app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
  assert_eq!(app.scripts, ["first"]);
  assert_eq!(app.storage.list_scripts().unwrap(), ["first"]);
  assert!(matches!(
    app.workspace.overlay,
    Some(Overlay::LoadScript { selected: 0 })
  ));
  assert_eq!(app.workspace.status, "Deleted second.sql");
  assert!(!app.workspace.status_is_error);

  app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
  app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
  assert!(app.scripts.is_empty());
  assert!(app.storage.list_scripts().unwrap().is_empty());
  assert!(app.workspace.overlay.is_none());
  assert_eq!(app.sql.lines(), &["SELECT unsaved;"]);
  app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
  assert!(app.workspace.overlay.is_none());
  assert_eq!(app.workspace.status, "No saved scripts");
}

// A disk failure must show an error and leave the entry available for retry or cancellation.
#[test]
fn failed_script_deletion_keeps_confirmation_and_cached_entry() {
  let (_directory, _runtime, mut app) = script_app();
  app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
  app.storage.delete_script("second").unwrap();
  app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
  assert!(app.workspace.status_is_error);
  assert!(app.workspace.status.starts_with("Could not delete script:"));
  assert_eq!(app.scripts, ["first", "second"]);
  assert!(matches!(
    app.workspace.overlay,
    Some(Overlay::ConfirmDeleteScript { .. })
  ));
  assert_eq!(app.sql.lines(), &["SELECT unsaved;"]);

  app.storage.save_script("second", "SELECT 2;").unwrap();
  app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
  assert_eq!(app.scripts, ["first"]);
  assert!(!app.workspace.status_is_error);
}
