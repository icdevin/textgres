// Real PostgreSQL verifies projected identity, session ownership, and atomic custom-result writes.
use super::paging::{execute, query, rows};
use super::*;

// Preserve the metadata and original row exactly as the result editor sends them.
fn update(
  database: &TestDatabase,
  result: &QueryResult,
  row: usize,
  column: usize,
  value: &str,
) -> Request {
  let original = result.rows[row].clone();
  let mut values = original.clone();
  values[column] = Some(value.into());
  Request::SaveChanges {
    profile: database.profile.clone(),
    password: None,
    source: result.source.clone().expect("editable result"),
    changes: vec![RowChange::Update { original, values }],
  }
}

// Aliases, dropped columns, composite keys, and expressions must address only their physical columns.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_custom_updates_map_columns_and_preserve_session() {
  let database = TestDatabase::start();
  let manager = SessionManager::default();
  execute(&manager, query(&database, "CREATE SCHEMA custom; CREATE TABLE custom.items (tenant int, removed text, id int, name text, run_on timestamptz, PRIMARY KEY(tenant,id)); ALTER TABLE custom.items DROP COLUMN removed; INSERT INTO custom.items VALUES (1,7,'before',now()), (2,7,'other',now()); SET search_path = custom; SET textgres.marker = 'retained'; CREATE FUNCTION stamp() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.name = NEW.name || current_setting('textgres.marker'); RETURN NEW; END $$; CREATE TRIGGER stamp BEFORE UPDATE ON items FOR EACH ROW EXECUTE FUNCTION stamp();")).await.0.unwrap();
  let result = rows(execute(&manager, query(&database, "SELECT name AS label, id + 1 AS computed, id AS key, tenant FROM items WHERE tenant = 1 ORDER BY id LIMIT 1")).await.0);
  let source = result.source.as_ref().unwrap();
  assert_eq!(source.table.schema, "custom");
  assert_eq!(source.columns[0].name, "name");
  assert!(!source.columns[1].editable);
  assert!(
    execute(&manager, update(&database, &result, 0, 1, "99"))
      .await
      .0
      .is_err()
  );
  let save = update(&database, &result, 0, 0, "after");
  let (saved, state) = execute(&manager, save.clone()).await;
  assert!(matches!(saved.unwrap(), Output::QuerySaved));
  assert_eq!(state, Some(SessionState::Connected(TransactionState::Idle)));
  assert!(execute(&manager, save).await.0.is_err());
  let observer = database.connect().await;
  let values = run_query(&observer, "SELECT name FROM custom.items ORDER BY tenant")
    .await
    .unwrap();
  assert_eq!(
    values.rows,
    vec![
      vec![Some("afterretained".into())],
      vec![Some("other".into())]
    ]
  );
  // A source from a replaced session cannot silently reconnect to perform a write.
  let result = rows(
    execute(
      &manager,
      query(&database, "SELECT id, tenant, name FROM items"),
    )
    .await
    .0,
  );
  let save = update(&database, &result, 0, 2, "stale");
  manager.close_all();
  assert!(execute(&manager, save).await.0.is_err());
}

// Missing key components and unsupported shapes stay readable while returning a useful reason.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_custom_updates_require_complete_unambiguous_keys() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE items(tenant int, id int, name text, PRIMARY KEY(tenant,id)); INSERT INTO items VALUES (1,1,'one'); CREATE TABLE no_key(name text); INSERT INTO no_key VALUES ('one'); CREATE VIEW item_view AS SELECT * FROM items;").await.unwrap();
  let manager = SessionManager::default();
  for (sql, reason) in [
    ("SELECT name FROM items", "tenant, id"),
    ("SELECT id, name FROM items", "tenant"),
    (
      "SELECT id, tenant, name, name AS other FROM items",
      "more than once",
    ),
    ("SELECT * FROM no_key", "no primary key"),
    ("SELECT * FROM item_view", "physical table"),
    ("SELECT DISTINCT tenant, id FROM items", "single-table"),
    (
      "SELECT tenant, id FROM items GROUP BY tenant, id",
      "single-table",
    ),
    (
      "SELECT a.* FROM items a JOIN items b USING(tenant,id)",
      "single-table",
    ),
    (
      "SELECT tenant,id,generate_series(1,2) FROM items",
      "single-table",
    ),
  ] {
    let result = rows(execute(&manager, query(&database, sql)).await.0);
    assert!(result.source.is_none(), "{sql}");
    assert!(
      result.read_only_reason.as_deref().unwrap().contains(reason),
      "{sql}: {:?}",
      result.read_only_reason
    );
  }
  // An explicit transaction stays open and cannot be accidentally committed by a result save.
  execute(
    &manager,
    query(&database, "BEGIN; UPDATE items SET name = 'uncommitted'"),
  )
  .await
  .0
  .unwrap();
  let (result, state) = execute(&manager, query(&database, "SELECT * FROM items")).await;
  let result = rows(result);
  assert!(result.source.is_none());
  assert!(
    result
      .read_only_reason
      .unwrap()
      .contains("explicit transaction")
  );
  assert_eq!(state, Some(SessionState::Connected(TransactionState::Open)));
  execute(&manager, query(&database, "ROLLBACK"))
    .await
    .0
    .unwrap();
  assert_eq!(
    run_query(&observer, "SELECT name FROM items")
      .await
      .unwrap()
      .rows[0][0]
      .as_deref(),
    Some("one")
  );
  manager.close_all();
}

// Conflict and DDL checks roll back the entire batch; paging keeps the original identity.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_custom_updates_validate_schema_conflicts_and_paging() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE items(id int PRIMARY KEY, name text); INSERT INTO items SELECT n, 'before' FROM generate_series(1,401) n").await.unwrap();
  let manager = SessionManager::default();
  let result = rows(
    execute(
      &manager,
      query(&database, "SELECT name, id FROM items ORDER BY id"),
    )
    .await
    .0,
  );
  assert!(result.source.is_some());
  let page = result.page.clone().unwrap();
  let next = rows(
    execute(&manager, Request::FetchPage { page: page.clone() })
      .await
      .0,
  );
  let mut loaded = result.clone();
  loaded.rows.extend(next.rows);
  observer
    .client
    .batch_execute("UPDATE items SET name='concurrent' WHERE id=201")
    .await
    .unwrap();
  let mut request = update(&database, &loaded, 0, 0, "first");
  if let Request::SaveChanges { changes, .. } = &mut request {
    let Request::SaveChanges { changes: other, .. } =
      update(&database, &loaded, 200, 0, "conflict")
    else {
      unreachable!()
    };
    changes.extend(other);
  }
  assert!(
    execute(&manager, request)
      .await
      .0
      .unwrap_err()
      .to_string()
      .contains("Batch not committed")
  );
  assert_eq!(
    run_query(&observer, "SELECT name FROM items WHERE id=1")
      .await
      .unwrap()
      .rows[0][0]
      .as_deref(),
    Some("before")
  );
  assert!(
    execute(&manager, Request::FetchPage { page })
      .await
      .0
      .is_err()
  );
  // A later-page row remains editable after its cursor has closed on a rolled-back save.
  assert!(matches!(
    execute(&manager, update(&database, &loaded, 201, 0, "after"))
      .await
      .0
      .unwrap(),
    Output::QuerySaved
  ));
  assert_eq!(
    run_query(&observer, "SELECT name FROM items WHERE id=202")
      .await
      .unwrap()
      .rows[0][0]
      .as_deref(),
    Some("after")
  );
  let result = rows(
    execute(
      &manager,
      query(&database, "SELECT name, id FROM items LIMIT 1"),
    )
    .await
    .0,
  );
  observer
    .client
    .batch_execute("ALTER TABLE items ADD COLUMN extra text")
    .await
    .unwrap();
  let error = execute(&manager, update(&database, &result, 0, 0, "bad"))
    .await
    .0
    .unwrap_err();
  assert!(format!("{error:#}").contains("Table definition changed"));
  manager.close_all();
}

// A replaced query or attempted insert/delete must not reuse a custom result's write authority.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_custom_updates_reject_other_writes_and_stale_results() {
  let database = TestDatabase::start();
  let manager = SessionManager::default();
  execute(&manager, query(&database, "CREATE TEMP TABLE items(id int PRIMARY KEY, name text); INSERT INTO items VALUES(1,'before')")).await.0.unwrap();
  let result = rows(
    execute(&manager, query(&database, "SELECT * FROM items"))
      .await
      .0,
  );
  let save = update(&database, &result, 0, 1, "after");
  for change in [
    RowChange::Delete {
      original: result.rows[0].clone(),
    },
    RowChange::Insert {
      values: result.rows[0].clone(),
      defaults: vec![false; 2],
    },
  ] {
    let mut request = save.clone();
    if let Request::SaveChanges { changes, .. } = &mut request {
      *changes = vec![change];
    }
    assert!(execute(&manager, request).await.0.is_err());
  }
  assert!(matches!(
    execute(&manager, save).await.0.unwrap(),
    Output::QuerySaved
  ));
  let result = rows(
    execute(&manager, query(&database, "SELECT * FROM items"))
      .await
      .0,
  );
  let save = update(&database, &result, 0, 1, "stale");
  execute(&manager, query(&database, "SELECT 1/0"))
    .await
    .0
    .unwrap_err();
  assert!(execute(&manager, save).await.0.is_err());
  manager.close_all();
}

// Cancellation rolls back all updates and leaves the original SQL session usable.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_custom_updates_cancel_and_recover_in_same_session() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE items(id int PRIMARY KEY, name text); INSERT INTO items VALUES(1,'before'); CREATE FUNCTION delay_update() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(20); RETURN NEW; END $$; CREATE TRIGGER delay_update BEFORE UPDATE ON items FOR EACH ROW EXECUTE FUNCTION delay_update()").await.unwrap();
  let manager = SessionManager::default();
  let result = rows(
    execute(&manager, query(&database, "SELECT id, name FROM items"))
      .await
      .0,
  );
  let request = update(&database, &result, 0, 1, "after");
  let Request::SaveChanges {
    source, changes, ..
  } = &request
  else {
    unreachable!()
  };
  let RowChange::Update { original, values } = &changes[0] else {
    unreachable!()
  };
  let sql = build_update(source, original, values).unwrap().0;
  let (sender, receiver) = mpsc::channel();
  let task = spawn(
    &Handle::current(),
    sender,
    TunnelManager::default(),
    manager.clone(),
    1,
    request.clone(),
  );
  let pid = sleeping_backend(&observer, &sql).await;
  task.cancel();
  assert!(response(&receiver).await.unwrap_err().is::<Cancelled>());
  task.shutdown().await.unwrap();
  assert_stopped(&observer, pid).await;
  assert_eq!(
    run_query(&observer, "SELECT name FROM items")
      .await
      .unwrap()
      .rows[0][0]
      .as_deref(),
    Some("before")
  );
  observer
    .client
    .batch_execute("DROP TRIGGER delay_update ON items")
    .await
    .unwrap();
  assert!(matches!(
    execute(&manager, request).await.0.unwrap(),
    Output::QuerySaved
  ));
  let current_pid = rows(
    execute(&manager, query(&database, "SELECT pg_backend_pid()"))
      .await
      .0,
  );
  assert_eq!(
    current_pid.rows[0][0].as_deref(),
    Some(pid.to_string().as_str())
  );
  manager.close_all();
}

// A lost COMMIT response retires the query snapshot and never retries through a fresh connection.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_custom_updates_lost_commit_requires_verification() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE items(id int PRIMARY KEY, name text); INSERT INTO items VALUES(1,'before'); CREATE FUNCTION slow_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(20); RETURN NULL; END $$; CREATE CONSTRAINT TRIGGER slow_commit AFTER UPDATE ON items DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION slow_commit()").await.unwrap();
  let manager = SessionManager::default();
  let result = rows(
    execute(&manager, query(&database, "SELECT * FROM items"))
      .await
      .0,
  );
  let request = update(&database, &result, 0, 1, "after");
  let (sender, receiver) = mpsc::channel();
  let task = spawn(
    &Handle::current(),
    sender,
    TunnelManager::default(),
    manager.clone(),
    1,
    request.clone(),
  );
  let pid = sleeping_backend(&observer, "COMMIT").await;
  observer
    .client
    .query_one("SELECT pg_terminate_backend($1)", &[&pid])
    .await
    .unwrap();
  assert!(response(&receiver).await.unwrap_err().is::<UnknownCommit>());
  assert!(task.shutdown().await.is_err());
  assert!(execute(&manager, request).await.0.is_err());
  assert_eq!(manager.states()[0].1, SessionState::Lost);
  manager.close_all();
}

// Recreated objects cannot inherit an old result's write authority; partitions retain valid keys.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_custom_updates_reject_recreated_objects_and_support_partitions() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE items(id int PRIMARY KEY, name text); INSERT INTO items VALUES(1,'before'); CREATE TABLE partitioned(id int PRIMARY KEY, name text) PARTITION BY RANGE(id); CREATE TABLE part_a PARTITION OF partitioned FOR VALUES FROM (0) TO (10); CREATE TABLE part_b PARTITION OF partitioned FOR VALUES FROM (10) TO (20); INSERT INTO partitioned VALUES(1,'before');").await.unwrap();
  let manager = SessionManager::default();
  let result = rows(
    execute(&manager, query(&database, "SELECT * FROM items"))
      .await
      .0,
  );
  observer.client.batch_execute("ALTER TABLE items DROP COLUMN name; ALTER TABLE items ADD COLUMN name text; UPDATE items SET name='before'").await.unwrap();
  assert!(
    execute(&manager, update(&database, &result, 0, 1, "bad"))
      .await
      .0
      .is_err()
  );
  let result = rows(
    execute(&manager, query(&database, "SELECT * FROM items"))
      .await
      .0,
  );
  observer.client.batch_execute("DROP TABLE items; CREATE TABLE items(id int PRIMARY KEY, name text); INSERT INTO items VALUES(1,'before')").await.unwrap();
  assert!(
    execute(&manager, update(&database, &result, 0, 1, "bad"))
      .await
      .0
      .is_err()
  );
  // A newly attached inheritance child must not match an old parent result's key.
  let result = rows(
    execute(&manager, query(&database, "SELECT * FROM items"))
      .await
      .0,
  );
  observer
    .client
    .batch_execute("CREATE TABLE child() INHERITS(items); INSERT INTO child VALUES(1,'before')")
    .await
    .unwrap();
  assert!(matches!(
    execute(&manager, update(&database, &result, 0, 1, "parent"))
      .await
      .0
      .unwrap(),
    Output::QuerySaved
  ));
  assert_eq!(
    run_query(&observer, "SELECT name FROM child")
      .await
      .unwrap()
      .rows[0][0]
      .as_deref(),
    Some("before")
  );
  assert!(
    rows(
      execute(&manager, query(&database, "SELECT * FROM items"))
        .await
        .0
    )
    .source
    .is_none()
  );
  let result = rows(
    execute(
      &manager,
      query(&database, "SELECT name,id FROM partitioned"),
    )
    .await
    .0,
  );
  assert!(matches!(
    execute(&manager, update(&database, &result, 0, 1, "11"))
      .await
      .0
      .unwrap(),
    Output::QuerySaved
  ));
  assert_eq!(
    run_query(&observer, "SELECT id FROM part_b")
      .await
      .unwrap()
      .rows[0][0]
      .as_deref(),
    Some("11")
  );
  manager.close_all();
}

// Updates preserve the session role and typed NULL/timestamp values while honoring database policies.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_custom_updates_respect_rls_and_typed_values() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE items(id int PRIMARY KEY, name text, run_on timestamptz, generated int GENERATED ALWAYS AS (id+1) STORED); INSERT INTO items(id,name,run_on) VALUES(1,NULL,'2026-09-21 12:00:00+00'); CREATE ROLE editor; GRANT SELECT,UPDATE ON items TO editor; ALTER TABLE items ENABLE ROW LEVEL SECURITY; CREATE POLICY visible ON items FOR SELECT USING(true); CREATE POLICY allowed ON items FOR UPDATE USING(true) WITH CHECK(name IS DISTINCT FROM 'denied');").await.unwrap();
  let manager = SessionManager::default();
  execute(
    &manager,
    query(
      &database,
      "SET ROLE editor; SET TIME ZONE 'America/New_York'",
    ),
  )
  .await
  .0
  .unwrap();
  let result = rows(
    execute(
      &manager,
      query(&database, "SELECT name, run_on, generated, id FROM items"),
    )
    .await
    .0,
  );
  assert!(!result.source.as_ref().unwrap().columns[2].editable);
  assert!(
    execute(&manager, update(&database, &result, 0, 0, "denied"))
      .await
      .0
      .is_err()
  );
  assert!(matches!(
    execute(&manager, update(&database, &result, 0, 0, "allowed"))
      .await
      .0
      .unwrap(),
    Output::QuerySaved
  ));
  let result = rows(
    execute(&manager, query(&database, "SELECT id,run_on FROM items"))
      .await
      .0,
  );
  assert!(matches!(
    execute(
      &manager,
      update(&database, &result, 0, 1, "2026-09-22 08:00:00-04")
    )
    .await
    .0
    .unwrap(),
    Output::QuerySaved
  ));
  let row = observer
    .client
    .query_one(
      "SELECT name, run_on = '2026-09-22 12:00:00+00'::timestamptz FROM items",
      &[],
    )
    .await
    .unwrap();
  assert_eq!(row.get::<_, String>(0), "allowed");
  assert!(row.get::<_, bool>(1));
  manager.close_all();
}
