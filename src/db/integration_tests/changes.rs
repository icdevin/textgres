// Real PostgreSQL tests verify transaction boundaries, defaults, conflicts, and cancellation.
use super::*;

// Load authoritative metadata and row values through the same preview used by the UI.
async fn preview(observer: &DatabaseConnection, name: &str) -> QueryResult {
  preview_table(
    observer,
    TableRef {
      profile_id: "test".into(),
      database: "postgres".into(),
      schema: "public".into(),
      name: name.into(),
      kind: "table".into(),
    },
  )
  .await
  .unwrap()
}

// A successful save includes its independent refresh, which must not affect commit semantics.
async fn save(
  database: &TestDatabase,
  source: TableResultSource,
  changes: Vec<RowChange>,
) -> anyhow::Result<QueryResult> {
  let (_task, receiver) = database.dispatch(Request::SaveChanges {
    profile: database.profile.clone(),
    password: None,
    source,
    changes,
  });
  match response(&receiver).await? {
    Output::Saved(refresh) => refresh,
    other => panic!("unexpected batch output {other:?}"),
  }
}

// Mixed deletion, modification, and insertion commit together and preserve generated defaults.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_batch_commits_mixed_changes_and_defaults() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE batch (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name text DEFAULT 'default name', note text, computed text GENERATED ALWAYS AS (upper(name)) STORED); INSERT INTO batch(name,note) VALUES ('first','keep'),('second','remove')").await.unwrap();
  let result = preview(&observer, "batch").await;
  let first = result
    .rows
    .iter()
    .find(|row| row[1].as_deref() == Some("first"))
    .unwrap()
    .clone();
  let second = result
    .rows
    .iter()
    .find(|row| row[1].as_deref() == Some("second"))
    .unwrap()
    .clone();
  let source = result.source.unwrap();
  assert!(!source.columns[0].insertable);
  assert!(!source.columns[3].insertable);
  let mut edited = first.clone();
  edited[1] = Some("changed".into());
  let refreshed = save(
    &database,
    source,
    vec![
      RowChange::Delete { original: second },
      RowChange::Update {
        original: first,
        values: edited,
      },
      RowChange::Insert {
        values: vec![None, None, None, None],
        defaults: vec![true, true, false, true],
      },
    ],
  )
  .await
  .unwrap();
  assert_eq!(refreshed.rows.len(), 2);
  assert!(
    refreshed
      .rows
      .iter()
      .any(|row| row[1].as_deref() == Some("changed") && row[3].as_deref() == Some("CHANGED"))
  );
  assert!(
    refreshed
      .rows
      .iter()
      .any(|row| row[1].as_deref() == Some("default name") && row[2].is_none())
  );
}

// A zero-row conflict after an earlier successful statement must roll back that earlier write.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_batch_conflict_rolls_back_every_row() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE batch(id int PRIMARY KEY, value text); INSERT INTO batch VALUES(1,'first'),(2,'second')").await.unwrap();
  let result = preview(&observer, "batch").await;
  let first = result
    .rows
    .iter()
    .find(|row| row[0].as_deref() == Some("1"))
    .unwrap()
    .clone();
  let second = result
    .rows
    .iter()
    .find(|row| row[0].as_deref() == Some("2"))
    .unwrap()
    .clone();
  observer
    .client
    .batch_execute("UPDATE batch SET value='concurrent' WHERE id=2")
    .await
    .unwrap();
  let mut edited = first.clone();
  edited[1] = Some("must roll back".into());
  let error = save(
    &database,
    result.source.unwrap(),
    vec![
      RowChange::Update {
        original: first,
        values: edited,
      },
      RowChange::Delete { original: second },
    ],
  )
  .await
  .unwrap_err();
  assert!(format!("{error:#}").contains("expected one matching row, found 0"));
  let rows = observer
    .client
    .query("SELECT value FROM batch ORDER BY id", &[])
    .await
    .unwrap();
  assert_eq!(rows[0].get::<_, String>(0), "first");
  assert_eq!(rows[1].get::<_, String>(0), "concurrent");
}

// Constraint failures reported at COMMIT still roll back the whole batch and allow a safe retry.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_batch_deferred_constraint_failure_is_atomic() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer
    .client
    .batch_execute(
      "CREATE TABLE batch(id int PRIMARY KEY, value text UNIQUE DEFERRABLE INITIALLY DEFERRED)",
    )
    .await
    .unwrap();
  let result = preview(&observer, "batch").await;
  let error = save(
    &database,
    result.source.unwrap(),
    vec![
      RowChange::Insert {
        values: vec![Some("1".into()), Some("duplicate".into())],
        defaults: vec![false; 2],
      },
      RowChange::Insert {
        values: vec![Some("2".into()), Some("duplicate".into())],
        defaults: vec![false; 2],
      },
    ],
  )
  .await
  .unwrap_err();
  assert!(!error.is::<UnknownCommit>());
  assert!(format_error(&error).contains("23505"));
  let count: i64 = observer
    .client
    .query_one("SELECT count(*) FROM batch", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(count, 0);
}

// Cancel during the second write must undo the first write and terminate the active statement.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_batch_cancellation_rolls_back_prior_changes() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE batch(id int PRIMARY KEY, value text); INSERT INTO batch VALUES(1,'first'),(2,'second'); CREATE FUNCTION slow_batch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id=2 THEN PERFORM pg_sleep(20); END IF; RETURN NEW; END $$; CREATE TRIGGER slow_batch BEFORE UPDATE ON batch FOR EACH ROW EXECUTE FUNCTION slow_batch()").await.unwrap();
  let result = preview(&observer, "batch").await;
  let source = result.source.unwrap();
  let first = result
    .rows
    .iter()
    .find(|row| row[0].as_deref() == Some("1"))
    .unwrap()
    .clone();
  let second = result
    .rows
    .iter()
    .find(|row| row[0].as_deref() == Some("2"))
    .unwrap()
    .clone();
  let values = vec![Some("2".into()), Some("changed".into())];
  let sql = build_update(&source, &second, &values).unwrap().0;
  let (task, receiver) = database.dispatch(Request::SaveChanges {
    profile: database.profile.clone(),
    password: None,
    source,
    changes: vec![
      RowChange::Update {
        original: first,
        values: vec![Some("1".into()), Some("changed".into())],
      },
      RowChange::Update {
        original: second,
        values,
      },
    ],
  });
  let pid = sleeping_backend(&observer, &sql).await;
  task.cancel();
  assert!(response(&receiver).await.unwrap_err().is::<Cancelled>());
  task.shutdown().await.unwrap();
  assert_stopped(&observer, pid).await;
  let rows = observer
    .client
    .query("SELECT value FROM batch ORDER BY id", &[])
    .await
    .unwrap();
  assert_eq!(rows[0].get::<_, String>(0), "first");
  assert_eq!(rows[1].get::<_, String>(0), "second");
}

// Losing the backend during COMMIT requires verification rather than blindly retrying inserted rows.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_batch_lost_commit_response_is_unknown() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE batch(id int PRIMARY KEY); CREATE FUNCTION slow_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(20); RETURN NULL; END $$; CREATE CONSTRAINT TRIGGER slow_commit AFTER INSERT ON batch DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION slow_commit()").await.unwrap();
  let result = preview(&observer, "batch").await;
  let (task, receiver) = database.dispatch(Request::SaveChanges {
    profile: database.profile.clone(),
    password: None,
    source: result.source.unwrap(),
    changes: vec![RowChange::Insert {
      values: vec![Some("1".into())],
      defaults: vec![false],
    }],
  });
  let pid = sleeping_backend(&observer, "COMMIT").await;
  observer
    .client
    .query_one("SELECT pg_terminate_backend($1)", &[&pid])
    .await
    .unwrap();
  assert!(response(&receiver).await.unwrap_err().is::<UnknownCommit>());
  assert!(task.shutdown().await.is_err());
}

// JSON conflict checks work without PostgreSQL's missing json equality operator.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_batch_json_conflicts_and_schema_changes_are_safe() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer.client.batch_execute("CREATE TABLE batch(id int PRIMARY KEY, value json); INSERT INTO batch VALUES(1,'{\"a\":1}'),(2,'{\"b\":2}')").await.unwrap();
  let result = preview(&observer, "batch").await;
  let source = result.source.unwrap();
  let first = result
    .rows
    .iter()
    .find(|row| row[0].as_deref() == Some("1"))
    .unwrap()
    .clone();
  let second = result
    .rows
    .iter()
    .find(|row| row[0].as_deref() == Some("2"))
    .unwrap()
    .clone();
  save(
    &database,
    source.clone(),
    vec![
      RowChange::Update {
        original: first,
        values: vec![Some("1".into()), Some("{\"a\":3}".into())],
      },
      RowChange::Delete { original: second },
    ],
  )
  .await
  .unwrap();
  observer
    .client
    .batch_execute("ALTER TABLE batch ADD COLUMN added text")
    .await
    .unwrap();
  let error = save(
    &database,
    source,
    vec![RowChange::Insert {
      values: vec![Some("3".into()), None],
      defaults: vec![false; 2],
    }],
  )
  .await
  .unwrap_err();
  assert!(format!("{error:#}").contains("Table definition changed"));
  let count: i64 = observer
    .client
    .query_one("SELECT count(*) FROM batch", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(count, 1);
}

// Invalid explicit generated values must fail before any earlier valid insertion can commit.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_batch_rejects_generated_values_and_supports_default_only_rows() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE TABLE batch(id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY)")
    .await
    .unwrap();
  let source = preview(&observer, "batch").await.source.unwrap();
  let error = save(
    &database,
    source.clone(),
    vec![
      RowChange::Insert {
        values: vec![None],
        defaults: vec![true],
      },
      RowChange::Insert {
        values: vec![Some("10".into())],
        defaults: vec![false],
      },
    ],
  )
  .await
  .unwrap_err();
  assert!(format!("{error:#}").contains("generated default"));
  let count: i64 = observer
    .client
    .query_one("SELECT count(*) FROM batch", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(count, 0);
  let result = save(
    &database,
    source,
    vec![RowChange::Insert {
      values: vec![None],
      defaults: vec![true],
    }],
  )
  .await
  .unwrap();
  assert_eq!(result.rows.len(), 1);
  assert!(result.rows[0][0].is_some());
}
