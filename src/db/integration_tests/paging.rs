// Real servers prove that each fetch advances one execution instead of repeating SELECT/OFFSET.
use super::*;

// Keep one manager across initial execution and later pages, just like an application workspace.
pub(super) async fn execute(
  manager: &SessionManager,
  request: Request,
) -> (anyhow::Result<Output>, Option<SessionState>) {
  manager
    .execute(
      request,
      &TunnelManager::default(),
      &Arc::new(Cancellation::default()),
    )
    .await
}

// Extract both initial and appended row sets while preserving database errors.
pub(super) fn rows(output: anyhow::Result<Output>) -> QueryResult {
  match output.unwrap() {
    Output::Result(result) | Output::Page { result, .. } => result,
    other => panic!("expected rows, got {other:?}"),
  }
}

// SQL reads use the existing session, including temporary objects and session settings.
pub(super) fn query(database: &TestDatabase, sql: &str) -> Request {
  Request::Query {
    profile: database.profile.clone(),
    database: "postgres".into(),
    sql: sql.into(),
  }
}

// A single source identity is shared by all preview pages and edits.
fn table() -> TableRef {
  TableRef {
    profile_id: "test".into(),
    database: "postgres".into(),
    schema: "public".into(),
    name: "page_items".into(),
    kind: "table".into(),
  }
}

// Reading page one must not evaluate an expensive expression in page two; cancellation closes continuation.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_paging_stops_execution_at_page_boundary() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE page_items(id int PRIMARY KEY); INSERT INTO page_items SELECT generate_series(1, 450)").await.unwrap();
  let manager = SessionManager::default();
  let started = Instant::now();
  let (result, state) = execute(
    &manager,
    query(
      &database,
      "SELECT id, CASE WHEN id = 201 THEN pg_sleep(20) END FROM page_items",
    ),
  )
  .await;
  let first = rows(result);
  assert_eq!(first.rows.len(), 200);
  assert!(started.elapsed() < Duration::from_secs(5));
  assert_eq!(
    state,
    Some(SessionState::Connected(TransactionState::Paging))
  );
  let page = first.page.unwrap();
  let (sender, receiver) = mpsc::channel();
  let task = spawn(
    &Handle::current(),
    sender,
    TunnelManager::default(),
    manager.clone(),
    2,
    Request::FetchPage { page: page.clone() },
  );
  let pid = sleeping_backend(&observer, "unused").await;
  task.cancel();
  assert!(response(&receiver).await.unwrap_err().is::<Cancelled>());
  task.shutdown().await.unwrap();
  assert_stopped(&observer, pid).await;
  assert!(
    execute(&manager, Request::FetchPage { page })
      .await
      .0
      .is_err()
  );
  assert_eq!(
    rows(execute(&manager, query(&database, "SELECT 42")).await.0).rows[0][0].as_deref(),
    Some("42")
  );
  manager.close_all();
}

// Cursor snapshots retain row identity even when another connection updates not-yet-fetched rows.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_paging_preserves_snapshot_and_handles_final_empty_page() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE page_items(id int PRIMARY KEY, value text); INSERT INTO page_items SELECT n, 'original' FROM generate_series(1,400) n").await.unwrap();
  let manager = SessionManager::default();
  let first = rows(
    execute(
      &manager,
      query(&database, "SELECT * FROM page_items ORDER BY id;"),
    )
    .await
    .0,
  );
  assert_eq!(first.rows.len(), 200);
  observer.client.batch_execute("UPDATE page_items SET value='new' WHERE id=201; DELETE FROM page_items WHERE id=202; INSERT INTO page_items VALUES (401, 'new')").await.unwrap();
  let second = rows(
    execute(
      &manager,
      Request::FetchPage {
        page: first.page.clone().unwrap(),
      },
    )
    .await
    .0,
  );
  assert_eq!(second.rows.len(), 200);
  assert_eq!(
    second.rows[0],
    vec![Some("201".into()), Some("original".into())]
  );
  assert_eq!(second.rows[1][0].as_deref(), Some("202"));
  assert_eq!(second.rows[199][0].as_deref(), Some("400"));
  let (result, state) = execute(
    &manager,
    Request::FetchPage {
      page: second.page.unwrap(),
    },
  )
  .await;
  let last = rows(result);
  assert!(last.rows.is_empty());
  assert_eq!(last.columns, first.columns);
  assert!(last.page.is_none());
  assert_eq!(state, Some(SessionState::Connected(TransactionState::Idle)));
  assert!(
    execute(
      &manager,
      Request::FetchPage {
        page: first.page.unwrap()
      }
    )
    .await
    .0
    .is_err()
  );
  manager.close_all();
}

// Paging inside BEGIN must neither commit nor roll back earlier writes, even when results are replaced.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_paging_preserves_user_transactions_and_temp_tables() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE TABLE commit_probe(id int)")
    .await
    .unwrap();
  let manager = SessionManager::default();
  execute(&manager, query(&database, "BEGIN; INSERT INTO commit_probe VALUES (1); CREATE TEMP TABLE scratch AS SELECT generate_series(1,450) AS id")).await.0.unwrap();
  let (result, state) = execute(
    &manager,
    query(&database, "SELECT * FROM scratch ORDER BY id"),
  )
  .await;
  let first = rows(result);
  assert_eq!(state, Some(SessionState::Connected(TransactionState::Open)));
  assert_eq!(first.rows.len(), 200);
  assert_eq!(
    observer
      .client
      .query_one("SELECT count(*) FROM commit_probe", &[])
      .await
      .unwrap()
      .get::<_, i64>(0),
    0
  );
  execute(&manager, query(&database, "COMMIT"))
    .await
    .0
    .unwrap();
  assert_eq!(
    observer
      .client
      .query_one("SELECT count(*) FROM commit_probe", &[])
      .await
      .unwrap()
      .get::<_, i64>(0),
    1
  );
  assert!(
    execute(
      &manager,
      Request::FetchPage {
        page: first.page.unwrap()
      }
    )
    .await
    .0
    .is_err()
  );
  let first = rows(
    execute(&manager, query(&database, "SELECT * FROM scratch"))
      .await
      .0,
  );
  assert_eq!(first.rows.len(), 200);
  manager.close_all();
}

// Preview cursors page independently and reject stale tokens after replacement or shutdown.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_paging_previews_retain_metadata_and_release_cursors() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE page_items(id int PRIMARY KEY); INSERT INTO page_items SELECT generate_series(1,450)").await.unwrap();
  let manager = SessionManager::default();
  let request = Request::Preview {
    profile: database.profile.clone(),
    password: None,
    table: table(),
  };
  let first = rows(execute(&manager, request.clone()).await.0);
  assert_eq!(first.rows.len(), 200);
  assert_eq!(first.source.as_ref().unwrap().table, table());
  let second = rows(
    execute(
      &manager,
      Request::FetchPage {
        page: first.page.clone().unwrap(),
      },
    )
    .await
    .0,
  );
  let third = rows(
    execute(
      &manager,
      Request::FetchPage {
        page: second.page.unwrap(),
      },
    )
    .await
    .0,
  );
  assert_eq!(third.rows.len(), 50);
  assert!(third.page.is_none());
  let replaced = rows(execute(&manager, request.clone()).await.0);
  let current = rows(execute(&manager, request).await.0);
  assert!(
    execute(
      &manager,
      Request::FetchPage {
        page: replaced.page.unwrap()
      }
    )
    .await
    .0
    .is_err()
  );
  assert_eq!(
    rows(
      execute(
        &manager,
        Request::FetchPage {
          page: current.page.clone().unwrap()
        }
      )
      .await
      .0
    )
    .rows
    .len(),
    200
  );
  manager.close_all();
  assert!(
    execute(
      &manager,
      Request::FetchPage {
        page: current.page.unwrap()
      }
    )
    .await
    .0
    .is_err()
  );
}

// RETURNING and writable CTEs execute once to completion rather than leaving partial writes behind a page.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_paging_keeps_write_and_script_semantics() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE TABLE page_items(id int PRIMARY KEY)")
    .await
    .unwrap();
  let manager = SessionManager::default();
  let result = rows(
    execute(
      &manager,
      query(
        &database,
        "INSERT INTO page_items SELECT generate_series(1,650) RETURNING *",
      ),
    )
    .await
    .0,
  );
  assert!(result.page.is_none());
  assert!(result.truncated);
  assert_eq!(
    observer
      .client
      .query_one("SELECT count(*) FROM page_items", &[])
      .await
      .unwrap()
      .get::<_, i64>(0),
    650
  );
  let result = rows(
    execute(
      &manager,
      query(
        &database,
        "WITH gone AS (DELETE FROM page_items RETURNING *) SELECT * FROM gone",
      ),
    )
    .await
    .0,
  );
  assert!(result.page.is_none());
  assert_eq!(
    observer
      .client
      .query_one("SELECT count(*) FROM page_items", &[])
      .await
      .unwrap()
      .get::<_, i64>(0),
    0
  );
  assert!(
    execute(
      &manager,
      query(&database, "SELECT generate_series(1,650); SELECT 1/0")
    )
    .await
    .0
    .is_err()
  );
  manager.close_all();
}
