// Exercise persistent sessions against real PostgreSQL protocol and transaction behavior.
use super::*;
use crate::db::sessions::TransactionState;

// Keep session ownership across requests while using the production execution path.
async fn query(
  manager: &SessionManager,
  profile: &ConnectionProfile,
  database: &str,
  sql: &str,
) -> (anyhow::Result<Output>, Option<SessionState>) {
  manager
    .execute(
      Request::Query {
        profile: profile.clone(),
        database: database.into(),
        sql: sql.into(),
      },
      &TunnelManager::default(),
      &Arc::new(Cancellation::default()),
    )
    .await
}

// Extract the first value without hiding SQL errors or unexpected output types.
fn value(output: anyhow::Result<Output>) -> String {
  let Output::Result(result) = output.unwrap() else {
    panic!("expected SQL output")
  };
  result.rows[0][0].clone().unwrap()
}

// Lifecycle operations use exactly the same manager as query execution.
async fn lifecycle(
  manager: &SessionManager,
  profile: &ConnectionProfile,
  reconnect: Option<bool>,
) -> SessionState {
  let request = match reconnect {
    Some(reconnect) => Request::Connect {
      profile: profile.clone(),
      database: "postgres".into(),
      reconnect,
    },
    None => Request::Disconnect {
      profile: profile.clone(),
      database: "postgres".into(),
    },
  };
  let (result, state) = manager
    .execute(
      request,
      &TunnelManager::default(),
      &Arc::new(Cancellation::default()),
    )
    .await;
  assert!(matches!(result.unwrap(), Output::Session));
  state.unwrap()
}

// BEGIN, session settings, temporary tables, COMMIT, and ROLLBACK survive separate executions.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_sessions_preserve_connection_and_transactions() {
  let database = TestDatabase::start();
  let manager = SessionManager::default();
  let profile = &database.profile;
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE TABLE persisted (id int)")
    .await
    .unwrap();
  let pid = value(
    query(&manager, profile, "postgres", "SELECT pg_backend_pid()")
      .await
      .0,
  );
  let (result, state) = query(
    &manager,
    profile,
    "postgres",
    "CREATE TEMP TABLE scratch (id int); SET application_name = 'session test'; BEGIN",
  )
  .await;
  result.unwrap();
  assert_eq!(state, Some(SessionState::Connected(TransactionState::Open)));
  query(
    &manager,
    profile,
    "postgres",
    "INSERT INTO persisted VALUES (1); INSERT INTO scratch VALUES (42)",
  )
  .await
  .0
  .unwrap();
  let count: i64 = observer
    .client
    .query_one("SELECT count(*) FROM persisted", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(count, 0);
  assert_eq!(
    value(
      query(&manager, profile, "postgres", "SELECT pg_backend_pid()")
        .await
        .0
    ),
    pid
  );
  assert_eq!(
    value(
      query(&manager, profile, "postgres", "SHOW application_name")
        .await
        .0
    ),
    "session test"
  );
  let (_, state) = query(&manager, profile, "postgres", "COMMIT").await;
  assert_eq!(state, Some(SessionState::Connected(TransactionState::Idle)));
  assert_eq!(
    value(
      query(&manager, profile, "postgres", "SELECT id FROM scratch")
        .await
        .0
    ),
    "42"
  );
  query(
    &manager,
    profile,
    "postgres",
    "BEGIN; INSERT INTO persisted VALUES (2)",
  )
  .await
  .0
  .unwrap();
  query(&manager, profile, "postgres", "ROLLBACK")
    .await
    .0
    .unwrap();
  let count: i64 = observer
    .client
    .query_one("SELECT count(*) FROM persisted", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(count, 1);
  manager.close_all();
}

// Error recovery must not insert a SET or health probe into an aborted transaction.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_sessions_recover_failed_and_read_only_transactions() {
  let database = TestDatabase::start();
  let manager = SessionManager::default();
  let profile = &database.profile;
  let (result, state) = query(
    &manager,
    profile,
    "postgres",
    "BEGIN READ ONLY; SAVEPOINT before_error",
  )
  .await;
  result.unwrap();
  assert_eq!(state, Some(SessionState::Connected(TransactionState::Open)));
  let (result, state) = query(&manager, profile, "postgres", "SELECT 1/0").await;
  assert!(format_error(&result.unwrap_err()).contains("22012"));
  assert_eq!(
    state,
    Some(SessionState::Connected(TransactionState::Failed))
  );
  let (result, state) = query(
    &manager,
    profile,
    "postgres",
    "ROLLBACK TO before_error; SELECT 7",
  )
  .await;
  assert_eq!(value(result), "7");
  assert_eq!(state, Some(SessionState::Connected(TransactionState::Open)));
  let (result, state) = query(&manager, profile, "postgres", "ROLLBACK").await;
  result.unwrap();
  assert_eq!(state, Some(SessionState::Connected(TransactionState::Idle)));
  manager.close_all();
}

// Disconnect rolls back and preserves a tombstone; reconnect opens a fresh backend explicitly.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_sessions_disconnect_and_reconnect_explicitly() {
  let database = TestDatabase::start();
  let profile = &database.profile;
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE TABLE persisted (id int)")
    .await
    .unwrap();
  let manager = SessionManager::default();
  // Even disconnecting a never-connected workspace disables lazy connection.
  assert_eq!(
    lifecycle(&manager, profile, None).await,
    SessionState::Disconnected
  );
  assert!(
    query(&manager, profile, "postgres", "SELECT 1")
      .await
      .0
      .unwrap_err()
      .to_string()
      .contains("explicitly")
  );
  lifecycle(&manager, profile, Some(false)).await;
  let pid = value(
    query(&manager, profile, "postgres", "SELECT pg_backend_pid()")
      .await
      .0,
  );
  query(
    &manager,
    profile,
    "postgres",
    "CREATE TEMP TABLE scratch(id int); BEGIN; INSERT INTO persisted VALUES(1)",
  )
  .await
  .0
  .unwrap();
  lifecycle(&manager, profile, Some(true)).await;
  assert_ne!(
    value(
      query(&manager, profile, "postgres", "SELECT pg_backend_pid()")
        .await
        .0
    ),
    pid
  );
  assert!(
    query(&manager, profile, "postgres", "SELECT * FROM scratch")
      .await
      .0
      .is_err()
  );
  let count: i64 = observer
    .client
    .query_one("SELECT count(*) FROM persisted", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(count, 0);
  query(
    &manager,
    profile,
    "postgres",
    "BEGIN; INSERT INTO persisted VALUES(2)",
  )
  .await
  .0
  .unwrap();
  assert_eq!(
    lifecycle(&manager, profile, None).await,
    SessionState::Disconnected
  );
  assert!(
    query(&manager, profile, "postgres", "SELECT 1")
      .await
      .0
      .is_err()
  );
  let count: i64 = observer
    .client
    .query_one("SELECT count(*) FROM persisted", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(count, 0);
}

// An idle backend loss is visible before another query, and SQL cannot silently create a new session.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_sessions_require_reconnect_after_idle_loss() {
  let database = TestDatabase::start();
  let manager = SessionManager::default();
  let profile = &database.profile;
  let pid: i32 = value(
    query(&manager, profile, "postgres", "SELECT pg_backend_pid()")
      .await
      .0,
  )
  .parse()
  .unwrap();
  let observer = database.connect().await;
  observer
    .client
    .query_one("SELECT pg_terminate_backend($1)", &[&pid])
    .await
    .unwrap();
  tokio::time::timeout(Duration::from_secs(2), async {
    while manager.states()[0].1 != SessionState::Lost {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap();
  let (result, state) = query(&manager, profile, "postgres", "SELECT 99").await;
  assert!(result.unwrap_err().to_string().contains("explicitly"));
  assert_eq!(state, Some(SessionState::Lost));
  lifecycle(&manager, profile, Some(true)).await;
  assert_ne!(
    value(
      query(&manager, profile, "postgres", "SELECT pg_backend_pid()")
        .await
        .0
    ),
    pid.to_string()
  );
  manager.close_all();
}

// One profile can run two databases concurrently; cancellation stays with its original session.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_sessions_run_independently_and_reuse_after_cancellation() {
  let database = TestDatabase::start();
  let manager = SessionManager::default();
  let profile = &database.profile;
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE DATABASE second")
    .await
    .unwrap();
  query(&manager, profile, "postgres", "BEGIN")
    .await
    .0
    .unwrap();
  let sql = "SELECT pg_sleep(20)";
  let (sender, receiver) = mpsc::channel();
  let task = spawn(
    &Handle::current(),
    sender,
    TunnelManager::default(),
    manager.clone(),
    1,
    Request::Query {
      profile: profile.clone(),
      database: "postgres".into(),
      sql: sql.into(),
    },
  );
  let pid = sleeping_backend(&observer, sql).await;
  let second = tokio::time::timeout(
    Duration::from_secs(2),
    query(&manager, profile, "second", "SELECT 42"),
  )
  .await
  .unwrap();
  assert_eq!(value(second.0), "42");
  // Reject another operation on the busy session instead of queuing SQL behind a transaction.
  assert!(
    query(&manager, profile, "postgres", "SELECT 2")
      .await
      .0
      .unwrap_err()
      .to_string()
      .contains("active operation")
  );
  assert!(manager.invalidate_profile(&profile.id).is_err());
  task.cancel();
  assert!(response(&receiver).await.unwrap_err().is::<Cancelled>());
  task.shutdown().await.unwrap();
  assert!(manager.states().contains(&(
    (profile.id.clone(), "postgres".into()),
    SessionState::Connected(TransactionState::Failed)
  )));
  query(&manager, profile, "postgres", "ROLLBACK")
    .await
    .0
    .unwrap();
  assert_eq!(
    value(
      query(&manager, profile, "postgres", "SELECT pg_backend_pid()")
        .await
        .0
    ),
    pid.to_string()
  );
  assert_eq!(
    value(query(&manager, profile, "second", "SELECT 43").await.0),
    "43"
  );
  manager.close_all();
}

// Metadata uses its own autocommit connection, even when the SQL session is aborted.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_sessions_keep_metadata_and_profiles_isolated() {
  let database = TestDatabase::start();
  let profile = &database.profile;
  let manager = SessionManager::default();
  let pid = value(
    query(&manager, profile, "postgres", "SELECT pg_backend_pid()")
      .await
      .0,
  );
  query(&manager, profile, "postgres", "BEGIN; SELECT 1/0")
    .await
    .0
    .unwrap_err();
  let (result, state) = manager
    .execute(
      Request::Schemas {
        profile: profile.clone(),
        password: None,
        database: "postgres".into(),
      },
      &TunnelManager::default(),
      &Arc::new(Cancellation::default()),
    )
    .await;
  assert!(matches!(result.unwrap(), Output::Schemas { .. }));
  assert_eq!(
    state,
    Some(SessionState::Connected(TransactionState::Failed))
  );
  assert_eq!(
    manager.states()[0].1,
    SessionState::Connected(TransactionState::Failed)
  );
  let mut other = profile.clone();
  other.id = "other".into();
  assert_ne!(
    value(
      query(&manager, &other, "postgres", "SELECT pg_backend_pid()")
        .await
        .0
    ),
    pid
  );
  assert_eq!(manager.states().len(), 2);
  let mut changed = profile.clone();
  changed.host = "invalid.invalid".into();
  assert!(
    query(&manager, &changed, "postgres", "SELECT 1")
      .await
      .0
      .unwrap_err()
      .to_string()
      .contains("settings changed")
  );
  assert!(manager.invalidate_profile(&profile.id).is_err());
  lifecycle(&manager, profile, None).await;
  manager.invalidate_profile(&profile.id).unwrap();
  assert_eq!(manager.states().len(), 1);
  manager.close_all();
}

// Disabled activity tracking must produce a conservative state instead of guessing from SQL.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_sessions_report_unknown_transaction_state() {
  let database = TestDatabase::start();
  let manager = SessionManager::default();
  let (result, state) = query(
    &manager,
    &database.profile,
    "postgres",
    "SET track_activities = off; BEGIN",
  )
  .await;
  result.unwrap();
  assert_eq!(
    state,
    Some(SessionState::Connected(TransactionState::Unknown))
  );
  assert!(state.unwrap().needs_confirmation());
  manager.close_all();
}

// A statement timeout leaves the same session recoverable, including an aborted transaction.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_sessions_recover_after_statement_timeout() {
  let database = TestDatabase::start();
  let manager = SessionManager::default();
  let profile = &database.profile;
  assert_eq!(
    value(
      query(&manager, profile, "postgres", "SHOW statement_timeout")
        .await
        .0
    ),
    "30s"
  );
  query(
    &manager,
    profile,
    "postgres",
    "SET statement_timeout = '50ms'; BEGIN",
  )
  .await
  .0
  .unwrap();
  let (result, state) = query(&manager, profile, "postgres", "SELECT pg_sleep(20)").await;
  assert!(format_error(&result.unwrap_err()).contains("57014"));
  assert_eq!(
    state,
    Some(SessionState::Connected(TransactionState::Failed))
  );
  query(&manager, profile, "postgres", "ROLLBACK")
    .await
    .0
    .unwrap();
  assert_eq!(
    value(query(&manager, profile, "postgres", "SELECT 1").await.0),
    "1"
  );
  manager.close_all();
}

// Shutdown cancels active SQL on every database and rolls back their transactions.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_sessions_app_shutdown_closes_all_workspaces() {
  use crate::{
    app::{App, Focus, NodeKey},
    sql_editor::SqlEditor,
    storage::Storage,
  };
  use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE DATABASE second")
    .await
    .unwrap();
  observer
    .client
    .batch_execute("CREATE TABLE shutdown_probe(id int)")
    .await
    .unwrap();
  let data = tempfile::tempdir().unwrap();
  let (sender, receiver) = mpsc::channel();
  let mut app = App::new(
    Storage::new(data.path().to_owned()).unwrap(),
    vec![database.profile.clone()],
    vec![],
    Handle::current(),
    sender,
  );
  app.active_target = Some((database.profile.id.clone(), "postgres".into()));
  app.workspace.focus = Focus::Sql;
  let first = "BEGIN; INSERT INTO shutdown_probe VALUES (1); SELECT pg_sleep(20)";
  app.sql = SqlEditor::new(vec![first.into()]);
  app.handle_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE));
  let first_pid = sleeping_backend(&observer, first).await;
  // Expanding the second database must connect independently of the first running query.
  app.databases.insert(
    database.profile.id.clone(),
    vec!["postgres".into(), "second".into()],
  );
  app
    .schemas
    .insert((database.profile.id.clone(), "second".into()), vec![]);
  app
    .expanded
    .insert(NodeKey::Connection(database.profile.id.clone()));
  app.workspace.focus = Focus::Explorer;
  app.explorer_selected = 2;
  app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
  tokio::time::timeout(Duration::from_secs(3), async {
    while app.workspace.busy.is_some() {
      while let Ok(response) = receiver.try_recv() {
        app.handle_database_response(response);
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap();
  assert!(!app.workspace.status_is_error);
  app.workspace.focus = Focus::Sql;
  let second = "BEGIN; SELECT pg_sleep(19)";
  app.sql = SqlEditor::new(vec![second.into()]);
  app.handle_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE));
  let second_pid = sleeping_backend(&observer, second).await;
  tokio::time::timeout(Duration::from_secs(7), app.shutdown())
    .await
    .unwrap()
    .unwrap();
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      let count: i64 = observer
        .client
        .query_one(
          "SELECT count(*) FROM pg_stat_activity WHERE pid IN ($1, $2)",
          &[&first_pid, &second_pid],
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
  let count: i64 = observer
    .client
    .query_one("SELECT count(*) FROM shutdown_probe", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(count, 0);
}

// A fatal response must poison the retained session rather than leave it available for retries.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_sessions_reject_reuse_after_active_connection_loss() {
  let database = TestDatabase::start();
  let manager = SessionManager::default();
  let observer = database.connect().await;
  let sql = "SELECT pg_sleep(20)";
  let (sender, receiver) = mpsc::channel();
  let task = spawn(
    &Handle::current(),
    sender,
    TunnelManager::default(),
    manager.clone(),
    1,
    Request::Query {
      profile: database.profile.clone(),
      database: "postgres".into(),
      sql: sql.into(),
    },
  );
  let pid = sleeping_backend(&observer, sql).await;
  observer
    .client
    .query_one("SELECT pg_terminate_backend($1)", &[&pid])
    .await
    .unwrap();
  assert!(
    response(&receiver)
      .await
      .unwrap_err()
      .to_string()
      .contains("write outcome is unknown")
  );
  assert_eq!(manager.states()[0].1, SessionState::Lost);
  assert!(
    query(&manager, &database.profile, "postgres", "SELECT 1")
      .await
      .0
      .unwrap_err()
      .to_string()
      .contains("explicitly")
  );
  assert!(task.shutdown().await.is_err());
  manager.close_all();
}

// Expansion opens the default database's persistent session without running user SQL first.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_expansion_opens_and_retains_sql_session() {
  let database = TestDatabase::start();
  let profile = &database.profile;
  let manager = SessionManager::default();
  let expand = Request::Databases {
    profile: profile.clone(),
    password: None,
  };
  let (output, state) = manager
    .execute(
      expand.clone(),
      &TunnelManager::default(),
      &Arc::new(Cancellation::default()),
    )
    .await;
  assert!(matches!(output.unwrap(), Output::Databases { .. }));
  assert_eq!(state, Some(SessionState::Connected(TransactionState::Idle)));
  let pid = value(
    query(&manager, profile, "postgres", "SELECT pg_backend_pid()")
      .await
      .0,
  );
  query(&manager, profile, "postgres", "BEGIN")
    .await
    .0
    .unwrap();
  let (output, state) = manager
    .execute(
      expand.clone(),
      &TunnelManager::default(),
      &Arc::new(Cancellation::default()),
    )
    .await;
  output.unwrap();
  assert_eq!(state, Some(SessionState::Connected(TransactionState::Open)));
  assert_eq!(
    value(
      query(&manager, profile, "postgres", "SELECT pg_backend_pid()")
        .await
        .0
    ),
    pid
  );
  lifecycle(&manager, profile, None).await;
  let (output, state) = manager
    .execute(
      expand,
      &TunnelManager::default(),
      &Arc::new(Cancellation::default()),
    )
    .await;
  assert!(output.unwrap_err().to_string().contains("explicitly"));
  assert_eq!(state, Some(SessionState::Disconnected));
}

// Opening another database establishes a distinct SQL session while preserving the default session.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_database_expansion_opens_its_own_session() {
  let database = TestDatabase::start();
  let profile = &database.profile;
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE DATABASE second")
    .await
    .unwrap();
  let manager = SessionManager::default();
  let first_pid = value(
    query(&manager, profile, "postgres", "SELECT pg_backend_pid()")
      .await
      .0,
  );
  let expand = Request::Schemas {
    profile: profile.clone(),
    password: None,
    database: "second".into(),
  };
  let (output, state) = manager
    .execute(
      expand.clone(),
      &TunnelManager::default(),
      &Arc::new(Cancellation::default()),
    )
    .await;
  assert!(matches!(output.unwrap(), Output::Schemas { .. }));
  assert_eq!(state, Some(SessionState::Connected(TransactionState::Idle)));
  assert_eq!(manager.states().len(), 2);
  let second_pid: i32 = value(
    query(&manager, profile, "second", "SELECT pg_backend_pid()")
      .await
      .0,
  )
  .parse()
  .unwrap();
  assert_ne!(first_pid, second_pid.to_string());
  observer
    .client
    .query_one("SELECT pg_terminate_backend($1)", &[&second_pid])
    .await
    .unwrap();
  tokio::time::timeout(Duration::from_secs(2), async {
    while !manager
      .states()
      .iter()
      .any(|(target, state)| target.1 == "second" && *state == SessionState::Lost)
    {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap();
  let (output, state) = manager
    .execute(
      expand,
      &TunnelManager::default(),
      &Arc::new(Cancellation::default()),
    )
    .await;
  assert!(output.unwrap_err().to_string().contains("explicitly"));
  assert_eq!(state, Some(SessionState::Lost));
  assert_eq!(
    value(
      query(&manager, profile, "postgres", "SELECT pg_backend_pid()")
        .await
        .0
    ),
    first_pid
  );
  manager.close_all();
}

// The unchanged editor executes on each selected database/profile while results stay local.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_shared_editor_runs_against_selected_targets() {
  use crate::{
    app::{App, ExplorerNode, Focus},
    sql_editor::SqlEditor,
    storage::Storage,
  };
  use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

  // Pump actual worker replies so navigation and execution follow the terminal event loop.
  async fn finish(app: &mut App, receiver: &Receiver<Response>) {
    tokio::time::timeout(Duration::from_secs(3), async {
      while app.workspace.busy.is_some() {
        while let Ok(response) = receiver.try_recv() {
          app.handle_database_response(response);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .unwrap();
    assert!(!app.workspace.status_is_error, "{}", app.workspace.status);
  }

  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE DATABASE second")
    .await
    .unwrap();
  let mut other = database.profile.clone();
  other.id = "other".into();
  let data = tempfile::tempdir().unwrap();
  let (sender, receiver) = mpsc::channel();
  let mut app = App::new(
    Storage::new(data.path().to_owned()).unwrap(),
    vec![database.profile.clone(), other.clone()],
    vec![],
    Handle::current(),
    sender,
  );
  let sql = "SELECT current_database(), pg_backend_pid()";
  app.sql = SqlEditor::new(vec![sql.into()]);
  let mut backend_ids = std::collections::HashSet::new();
  for (node, expected_database) in [
    (
      ExplorerNode::Connection(database.profile.id.clone()),
      "postgres",
    ),
    (
      ExplorerNode::Database {
        profile_id: database.profile.id.clone(),
        database: "second".into(),
      },
      "second",
    ),
    (ExplorerNode::Connection(other.id), "postgres"),
  ] {
    app.workspace.focus = Focus::Explorer;
    app.explorer_selected = app
      .explorer_rows()
      .iter()
      .position(|row| match (&row.node, &node) {
        (ExplorerNode::Connection(left), ExplorerNode::Connection(right)) => left == right,
        (
          ExplorerNode::Database {
            profile_id: left_id,
            database: left_db,
          },
          ExplorerNode::Database {
            profile_id: right_id,
            database: right_db,
          },
        ) => left_id == right_id && left_db == right_db,
        _ => false,
      })
      .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    finish(&mut app, &receiver).await;
    assert_eq!(app.sql.lines(), &[sql]);
    app.workspace.focus = Focus::Sql;
    app.handle_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE));
    finish(&mut app, &receiver).await;
    assert_eq!(app.sql.lines(), &[sql]);
    assert_eq!(
      app.workspace.result.rows[0][0].as_deref(),
      Some(expected_database)
    );
    assert!(backend_ids.insert(app.workspace.result.rows[0][1].clone().unwrap()));
  }
  // Cycling restores the first result without replacing or rerunning the shared SQL.
  app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::CONTROL));
  assert_eq!(
    app.active_target,
    Some((database.profile.id.clone(), "postgres".into()))
  );
  assert_eq!(app.workspace.result.rows[0][0].as_deref(), Some("postgres"));
  assert_eq!(app.sql.lines(), &[sql]);
  app.shutdown().await.unwrap();
}
