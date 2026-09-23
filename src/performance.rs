// Manual release-mode probes measure full frames without terminal I/O or database latency.
use std::{hint::black_box, sync::mpsc, time::Instant};

use ratatui::{
  Terminal,
  backend::TestBackend,
  crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
};

use crate::{app::App, sql_editor::SqlEditor, storage::Storage, ui};

// Large schemas stress explorer flattening and list construction separately from result rendering.
#[test]
#[ignore = "manual benchmark: cargo test --release performance:: -- --ignored --nocapture --test-threads=1"]
fn explorer_workloads() {
  use crate::{app::NodeKey, db::TableRef, storage::ConnectionProfile};
  let directory = tempfile::tempdir().unwrap();
  let storage = Storage::new(directory.path().to_owned()).unwrap();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .build()
    .unwrap();
  let (sender, _receiver) = mpsc::channel();
  let profile = ConnectionProfile {
    id: "local".into(),
    name: "Local".into(),
    host: "localhost".into(),
    port: 5432,
    database: "postgres".into(),
    user: "postgres".into(),
    password: None,
    require_tls: false,
    ssh: None,
  };
  let mut app = App::new(
    storage,
    vec![profile],
    vec![],
    runtime.handle().clone(),
    sender,
  );
  app
    .databases
    .insert("local".into(), vec!["postgres".into()]);
  app
    .schemas
    .insert(("local".into(), "postgres".into()), vec!["public".into()]);
  app.expanded.extend([
    NodeKey::Connection("local".into()),
    NodeKey::Database("local".into(), "postgres".into()),
    NodeKey::Schema("local".into(), "postgres".into(), "public".into()),
  ]);
  let mut terminal = Terminal::new(TestBackend::new(160, 48)).unwrap();
  for count in [1_000, 10_000] {
    app.tables.insert(
      ("local".into(), "postgres".into(), "public".into()),
      (0..count)
        .map(|index| TableRef {
          profile_id: "local".into(),
          database: "postgres".into(),
          schema: "public".into(),
          name: format!("table_{index}"),
          kind: "table".into(),
        })
        .collect(),
    );
    app.explorer_selected = count + 2;
    measure(&format!("explorer/{count}-tables"), 20, || {
      terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
      black_box(terminal.backend().buffer());
    });
  }
}

// Report a median across batches so scheduler noise does not dominate small redraws.
fn measure(label: &str, iterations: usize, mut operation: impl FnMut()) {
  operation();
  let mut samples = Vec::new();
  for _ in 0..7 {
    let start = Instant::now();
    for _ in 0..iterations {
      operation();
    }
    samples.push(start.elapsed().as_secs_f64() * 1_000_000.0 / iterations as f64);
  }
  samples.sort_by(f64::total_cmp);
  eprintln!(
    "{label}: {:.1} us/op (median of 7 x {iterations})",
    samples[3]
  );
}

// Exercise realistic page accumulation, large cells, and SQL navigation in the complete layout.
#[test]
#[ignore = "manual benchmark: cargo test --release performance:: -- --ignored --nocapture --test-threads=1"]
fn redraw_workloads() {
  let directory = tempfile::tempdir().unwrap();
  let storage = Storage::new(directory.path().to_owned()).unwrap();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .build()
    .unwrap();
  let (sender, _receiver) = mpsc::channel();
  let mut app = App::new(storage, vec![], vec![], runtime.handle().clone(), sender);
  let mut terminal = Terminal::new(TestBackend::new(160, 48)).unwrap();
  for count in [200, 10_000, 50_000] {
    app.workspace.result.columns = vec!["id".into(), "name".into(), "email".into()];
    app.workspace.result.rows = (0..count)
      .map(|index| {
        vec![
          Some(index.to_string()),
          Some(format!("User {index}")),
          Some(format!("user{index}@example.com")),
        ]
      })
      .collect();
    app.workspace.result_row = count - 1;
    measure(&format!("results/{count}"), 20, || {
      terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
      black_box(terminal.backend().buffer());
    });
  }
  app.workspace.result.columns = vec!["payload".into()];
  app.workspace.result.rows = vec![vec![Some("value\twith\nlines\r".repeat(512))]; 2_000];
  app.workspace.result_row = 1_999;
  measure("results/2000-large-cells", 10, || {
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    black_box(terminal.backend().buffer());
  });
  app.workspace.result = Default::default();
  for count in [1, 1_000] {
    app.sql = SqlEditor::new(vec![
      "SELECT id, name FROM users WHERE id > 42; -- inspect"
        .into();
      count
    ]);
    measure(&format!("sql/{count}-lines-navigation"), 20, || {
      app
        .sql
        .input(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
      terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
      black_box(terminal.backend().buffer());
    });
    measure(&format!("sql/{count}-lines-edit"), 10, || {
      app
        .sql
        .input(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
      terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
      black_box(terminal.backend().buffer());
    });
  }
}
