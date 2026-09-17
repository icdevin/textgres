// Real connection lifetimes verify Explorer group disconnection and rollback scope.
use super::*;
use crate::{
  app::{App, ExplorerNode, Focus},
  sql_editor::SqlEditor,
  storage::Storage,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// Pump a known number of worker replies, including those for inactive workspaces.
async fn finish(app: &mut App, receiver: &Receiver<Response>, count: usize) {
  tokio::time::timeout(Duration::from_secs(5), async {
    for _ in 0..count {
      loop {
        match receiver.try_recv() {
          Ok(response) => {
            assert!(response.result.is_ok(), "{:?}", response.result);
            app.handle_database_response(response);
            break;
          }
          Err(mpsc::TryRecvError::Empty) => tokio::time::sleep(Duration::from_millis(10)).await,
          Err(error) => panic!("worker channel closed: {error}"),
        }
      }
    }
  })
  .await
  .unwrap();
}

// Run SQL through the UI so session state and transaction confirmation use production routing.
async fn sql(app: &mut App, receiver: &Receiver<Response>, text: &str) {
  app.workspace.focus = Focus::Sql;
  app.sql = SqlEditor::new(vec![text.into()]);
  app.handle_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE));
  finish(app, receiver, 1).await;
}

// A profile disconnect closes both databases and rolls back only after consent, leaving another profile live.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_explorer_disconnect_closes_selected_profile_only() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  // CREATE DATABASE must run outside an implicit multi-statement transaction.
  observer
    .client
    .batch_execute("CREATE DATABASE second")
    .await
    .unwrap();
  observer
    .client
    .batch_execute("CREATE TABLE disconnect_probe(id int)")
    .await
    .unwrap();
  let mut other = database.profile.clone();
  other.id = "other".into();
  let directory = tempfile::tempdir().unwrap();
  let (sender, receiver) = mpsc::channel();
  let mut app = App::new(
    Storage::new(directory.path().to_owned()).unwrap(),
    vec![database.profile.clone(), other],
    vec![],
    Handle::current(),
    sender,
  );
  let mut pids = Vec::new();
  for (profile_id, database_name) in [("test", None), ("test", Some("second")), ("other", None)] {
    app.workspace.focus = Focus::Explorer;
    app.explorer_selected = app
      .explorer_rows()
      .iter()
      .position(|row| match (&row.node, database_name) {
        (ExplorerNode::Connection(id), None) => id == profile_id,
        (
          ExplorerNode::Database {
            profile_id: id,
            database,
          },
          Some(name),
        ) => id == profile_id && database == name,
        _ => false,
      })
      .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    finish(&mut app, &receiver, 1).await;
    sql(&mut app, &receiver, "SELECT pg_backend_pid()").await;
    pids.push(
      app.workspace.result.rows[0][0]
        .as_ref()
        .unwrap()
        .parse::<i32>()
        .unwrap(),
    );
    if profile_id == "test" && database_name.is_none() {
      sql(
        &mut app,
        &receiver,
        "BEGIN; INSERT INTO disconnect_probe VALUES (1)",
      )
      .await;
    }
  }
  let active = app.active_target.clone();
  let result = app.workspace.result.clone();
  app.workspace.focus = Focus::Explorer;
  app.explorer_selected = app
    .explorer_rows()
    .iter()
    .position(|row| matches!(&row.node, ExplorerNode::Connection(id) if id == "test"))
    .unwrap();
  app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
  assert!(matches!(
    app.workspace.overlay,
    Some(crate::app::Overlay::ConfirmExplorerDisconnect { .. })
  ));
  app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
  assert!(app.explorer_connected(&ExplorerNode::Connection("test".into())));
  app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
  app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
  finish(&mut app, &receiver, 2).await;
  assert_eq!(app.active_target, active);
  assert_eq!(app.workspace.result, result);
  assert!(!app.explorer_connected(&ExplorerNode::Connection("test".into())));
  assert!(app.explorer_connected(&ExplorerNode::Connection("other".into())));
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      let count: i64 = observer
        .client
        .query_one(
          "SELECT count(*) FROM pg_stat_activity WHERE pid IN ($1, $2)",
          &[&pids[0], &pids[1]],
        )
        .await
        .unwrap()
        .get(0);
      if count == 0 {
        break;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap();
  assert_eq!(
    observer
      .client
      .query_one("SELECT count(*) FROM disconnect_probe", &[])
      .await
      .unwrap()
      .get::<_, i64>(0),
    0
  );
  sql(&mut app, &receiver, "SELECT pg_backend_pid()").await;
  assert_eq!(
    app.workspace.result.rows[0][0].as_deref(),
    Some(pids[2].to_string().as_str())
  );
  // The same key explicitly reopens the selected profile after its sessions were disconnected.
  app.workspace.focus = Focus::Explorer;
  app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
  finish(&mut app, &receiver, 1).await;
  assert_eq!(app.active_target, Some(("test".into(), "postgres".into())));
  assert!(app.selected_connection_is_connected());
  sql(&mut app, &receiver, "SELECT pg_backend_pid()").await;
  assert_ne!(
    app.workspace.result.rows[0][0].as_deref(),
    Some(pids[0].to_string().as_str())
  );
  app.workspace.focus = Focus::Explorer;
  app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
  finish(&mut app, &receiver, 1).await;
  assert!(!app.selected_connection_is_connected());
  assert!(app.explorer_connected(&ExplorerNode::Connection("other".into())));
  app.shutdown().await.unwrap();
}

// Disconnect closes an independent preview cursor even if no persistent SQL session was opened.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_explorer_disconnect_releases_preview_cursor() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE TABLE items AS SELECT generate_series(1,400) AS id")
    .await
    .unwrap();
  let manager = SessionManager::default();
  let tunnels = TunnelManager::default();
  let cancellation = Arc::new(Cancellation::default());
  let request = Request::Preview {
    profile: database.profile.clone(),
    password: None,
    table: TableRef {
      profile_id: "test".into(),
      database: "postgres".into(),
      schema: "public".into(),
      name: "items".into(),
      kind: "table".into(),
    },
  };
  let (result, _) = manager.execute(request, &tunnels, &cancellation).await;
  let Output::Result(result) = result.unwrap() else {
    panic!("expected preview")
  };
  let page = result.page.unwrap();
  manager
    .execute(
      Request::Disconnect {
        profile: database.profile.clone(),
        database: "postgres".into(),
      },
      &tunnels,
      &cancellation,
    )
    .await
    .0
    .unwrap();
  assert!(
    manager
      .execute(Request::FetchPage { page }, &tunnels, &cancellation)
      .await
      .0
      .is_err()
  );
  // AccessExclusive proves the old cursor no longer retains its table lock.
  observer
    .client
    .batch_execute("SET statement_timeout = '2s'; ALTER TABLE items ADD COLUMN extra text")
    .await
    .unwrap();
  manager.close_all();
}
