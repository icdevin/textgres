// Isolated PostgreSQL clusters exercise cancellation without using saved credentials or data.
use std::sync::mpsc::{self, Receiver};

use super::*;

// Each test owns a disposable cluster, including its server lifetime and random port.
struct TestDatabase {
  directory: tempfile::TempDir,
  profile: ConnectionProfile,
}

impl TestDatabase {
  fn start() -> Self {
    let directory = tempfile::tempdir().unwrap();
    let initialized = Command::new("initdb")
      .args([
        "-A",
        "trust",
        "-U",
        "textgres_test",
        "--no-locale",
        "-E",
        "UTF8",
        "-D",
      ])
      .arg(directory.path())
      .output()
      .expect("install PostgreSQL binaries to run database tests");
    assert!(
      initialized.status.success(),
      "{}",
      String::from_utf8_lossy(&initialized.stderr)
    );
    let database = Self {
      directory,
      profile: ConnectionProfile {
        id: "test".into(),
        name: "Test".into(),
        host: "127.0.0.1".into(),
        port: reserve_local_port().unwrap(),
        database: "postgres".into(),
        user: "textgres_test".into(),
        password: None,
        require_tls: false,
        ssh: None,
      },
    };
    let started = Command::new("pg_ctl")
      .arg("-D")
      .arg(database.directory.path())
      .arg("-l")
      .arg(database.directory.path().join("server.log"))
      // Unix sockets are unused; disabling them avoids platform socket-directory defaults.
      .arg("-o")
      .arg(format!(
        "-h 127.0.0.1 -p {} -k '' -F",
        database.profile.port
      ))
      .args(["-w", "-t", "5", "start"])
      .output()
      .unwrap();
    assert!(
      started.status.success(),
      "{}",
      String::from_utf8_lossy(&started.stderr)
    );
    database
  }

  // Use the production connection path for both operations and independent observations.
  async fn connect(&self) -> DatabaseConnection {
    connect(
      &self.profile,
      None,
      "postgres",
      &TunnelManager::default(),
      &Arc::new(Cancellation::default()),
    )
    .await
    .unwrap()
  }

  // Dispatch through the public worker API so tests cover cancellation ownership.
  fn query(&self, sql: &str) -> (Task, Receiver<Response>) {
    self.dispatch(Request::Query {
      profile: self.profile.clone(),
      database: "postgres".into(),
      sql: sql.into(),
    })
  }

  fn dispatch(&self, request: Request) -> (Task, Receiver<Response>) {
    let (sender, receiver) = mpsc::channel();
    let task = spawn(
      &Handle::current(),
      sender,
      TunnelManager::default(),
      SessionManager::default(),
      1,
      request,
    );
    (task, receiver)
  }
}

impl Drop for TestDatabase {
  fn drop(&mut self) {
    // Stop all backends before the temporary directory is removed, including on panic.
    let _ = Command::new("pg_ctl")
      .arg("-D")
      .arg(self.directory.path())
      .args(["-m", "immediate", "-w", "-t", "5", "stop"])
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status();
  }
}

// Wait for server-side execution, not a timer that could race connection startup.
async fn sleeping_backend(observer: &DatabaseConnection, sql: &str) -> i32 {
  tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      let rows = observer.client.query(
        "SELECT pid FROM pg_stat_activity WHERE pid <> pg_backend_pid() AND (query = $1 OR query LIKE 'FETCH FORWARD 200 FROM \"textgres_result_%') AND state = 'active' AND wait_event = 'PgSleep'",
        &[&sql],
      ).await.unwrap();
      if let Some(row) = rows.first() {
        return row.get(0);
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  }).await.expect("query did not reach pg_sleep")
}

// Receive without blocking the async runtime that executes the worker.
async fn response(receiver: &Receiver<Response>) -> anyhow::Result<Output> {
  tokio::time::timeout(Duration::from_secs(7), async {
    loop {
      match receiver.try_recv() {
        Ok(response) => return response.result,
        Err(mpsc::TryRecvError::Empty) => tokio::time::sleep(Duration::from_millis(10)).await,
        Err(error) => panic!("worker exited without a response: {error}"),
      }
    }
  })
  .await
  .expect("worker did not finish within its cancellation deadline")
}

// PostgreSQL must stop the backend as well as reject the cancelled INSERT.
async fn assert_stopped(observer: &DatabaseConnection, pid: i32) {
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      let active: bool = observer
        .client
        .query_one(
          "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND state = 'active')",
          &[&pid],
        )
        .await
        .unwrap()
        .get(0);
      if !active {
        return;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("cancelled backend is still active");
}

// Cancelling before setup begins must not contact even an invalid database host.
#[tokio::test]
async fn cancellation_before_connect_does_not_start_io() {
  let cancellation = Arc::new(Cancellation::default());
  cancellation.request();
  let profile = ConnectionProfile {
    id: "test".into(),
    name: "Test".into(),
    host: "invalid.invalid".into(),
    port: 1,
    database: "postgres".into(),
    user: "test".into(),
    password: None,
    require_tls: false,
    ssh: None,
  };
  let result = connect(
    &profile,
    None,
    "postgres",
    &TunnelManager::default(),
    &cancellation,
  )
  .await;
  assert!(result.err().unwrap().is::<Cancelled>());
}

// A delayed write must not commit after the user cancels it.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_cancelled_insert_does_not_commit() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE TABLE cancellation_probe (id int)")
    .await
    .unwrap();
  let sql = "INSERT INTO cancellation_probe SELECT 1 FROM pg_sleep(20)";
  let (task, receiver) = database.query(sql);
  let pid = sleeping_backend(&observer, sql).await;
  task.cancel();
  // Repeated Escape must not reset the deadline or discard the result.
  task.cancel();
  assert!(response(&receiver).await.unwrap_err().is::<Cancelled>());
  assert_stopped(&observer, pid).await;
  let count: i64 = observer
    .client
    .query_one("SELECT count(*) FROM cancellation_probe", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(count, 0);
}

// Normal exit must deliver cancellation before the runtime and its sockets disappear.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_shutdown_cancels_an_active_write() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE TABLE cancellation_probe (id int)")
    .await
    .unwrap();
  let sql = "INSERT INTO cancellation_probe SELECT 1 FROM pg_sleep(20)";
  let (task, receiver) = database.query(sql);
  let pid = sleeping_backend(&observer, sql).await;
  task.shutdown().await.unwrap();
  assert!(response(&receiver).await.unwrap_err().is::<Cancelled>());
  assert_stopped(&observer, pid).await;
  let count: i64 = observer
    .client
    .query_one("SELECT count(*) FROM cancellation_probe", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(count, 0);
}

// A response queued before Escape remains successful even when cancellation is requested later.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_late_cancellation_preserves_success() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer
    .client
    .batch_execute("CREATE TABLE cancellation_probe (id int)")
    .await
    .unwrap();
  let (task, receiver) = database.query("INSERT INTO cancellation_probe VALUES (1)");
  tokio::time::timeout(Duration::from_secs(5), async {
    while !task.worker.is_finished() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap();
  task.cancel();
  assert!(matches!(
    response(&receiver).await.unwrap(),
    Output::Result(_)
  ));
  let count: i64 = observer
    .client
    .query_one("SELECT count(*) FROM cancellation_probe", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(count, 1);
}

// The display row cap must not hide a cancellation response from a later statement.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_truncated_results_still_receive_cancellation() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  let sql = "SELECT generate_series(1, 501); SELECT pg_sleep(20)";
  let (task, receiver) = database.query(sql);
  let pid = sleeping_backend(&observer, sql).await;
  task.cancel();
  assert!(response(&receiver).await.unwrap_err().is::<Cancelled>());
  assert_stopped(&observer, pid).await;
}

// Draining capped results must also retain late errors and the final result set.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_truncated_results_are_drained_to_completion() {
  let database = TestDatabase::start();
  let client = database.connect().await;
  let result = run_query(&client, "SELECT generate_series(1, 501)")
    .await
    .unwrap();
  assert_eq!(result.rows.len(), MAX_RESULT_ROWS);
  assert!(result.truncated);
  let error = run_query(&client, "SELECT generate_series(1, 501); SELECT 1/0")
    .await
    .unwrap_err();
  assert!(format_error(&error).contains("22012"));
  let result = run_query(
    &client,
    "SELECT generate_series(1, 501); SELECT 42 AS final_result",
  )
  .await
  .unwrap();
  assert_eq!(result.columns, vec!["final_result"]);
  assert_eq!(result.rows, vec![vec![Some("42".into())]]);
  assert!(!result.truncated);
}

// A disconnected backend provides no reliable acknowledgement of the write outcome.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_connection_loss_reports_an_unknown_outcome() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  let sql = "SELECT pg_sleep(20)";
  let (task, receiver) = database.query(sql);
  let pid = sleeping_backend(&observer, sql).await;
  observer
    .client
    .query_one("SELECT pg_terminate_backend($1)", &[&pid])
    .await
    .unwrap();
  let error = response(&receiver).await.unwrap_err();
  assert!(!error.is::<Cancelled>());
  assert!(format_error(&error).contains("write outcome is unknown"));
  // Exit must also surface this error after the terminal receiver has closed.
  assert!(
    task
      .shutdown()
      .await
      .unwrap_err()
      .to_string()
      .contains("write outcome is unknown")
  );
}

// A missing original response must hit the deadline even if CancelRequest delivery succeeds.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_unconfirmed_cancellation_has_a_deadline() {
  let database = TestDatabase::start();
  let client = database.connect().await;
  let cancellation = client.cancellation.clone();
  let signal = tokio::spawn(async move {
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancellation.request();
  });
  let started = Instant::now();
  // Model an unresponsive query future while retaining a real cancellation connection.
  let error = client
    .run(std::future::pending::<Result<(), tokio_postgres::Error>>())
    .await
    .unwrap_err();
  signal.await.unwrap();
  assert!(started.elapsed() < Duration::from_secs(7));
  assert!(format_error(&error).contains("write outcome is unknown"));
  let driver = client.driver.abort_handle();
  drop(client);
  tokio::task::yield_now().await;
  assert!(driver.is_finished());
}

// Cancellation during refresh must preserve the already committed row update.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_cancelled_refresh_reports_the_saved_row() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  // The SELECT policy becomes slow only after the update's AFTER trigger has run.
  observer
    .client
    .batch_execute(
      r#"
    CREATE TABLE cancellation_probe (id int PRIMARY KEY, value text);
    INSERT INTO cancellation_probe VALUES (1, 'before');
    CREATE ROLE preview_user LOGIN;
    GRANT SELECT, UPDATE ON cancellation_probe TO preview_user;
    ALTER TABLE cancellation_probe ENABLE ROW LEVEL SECURITY;
    CREATE FUNCTION slow_preview() RETURNS boolean LANGUAGE plpgsql AS $$
      BEGIN
        IF current_setting('textgres.slow_preview', true) = 'yes' THEN
          PERFORM pg_sleep(20);
        END IF;
        RETURN true;
      END
    $$;
    CREATE POLICY preview_policy ON cancellation_probe USING (slow_preview()) WITH CHECK (true);
    CREATE FUNCTION enable_slow_preview() RETURNS trigger LANGUAGE plpgsql AS $$
      BEGIN
        PERFORM set_config('textgres.slow_preview', 'yes', false);
        RETURN NULL;
      END
    $$;
    CREATE TRIGGER enable_slow_preview AFTER UPDATE ON cancellation_probe
      FOR EACH STATEMENT EXECUTE FUNCTION enable_slow_preview();
  "#,
    )
    .await
    .unwrap();
  let table = TableRef {
    profile_id: "test".into(),
    database: "postgres".into(),
    schema: "public".into(),
    name: "cancellation_probe".into(),
    kind: "table".into(),
  };
  let preview = preview_table(&observer, table).await.unwrap();
  let mut profile = database.profile.clone();
  profile.user = "preview_user".into();
  let (task, receiver) = database.dispatch(Request::SaveChanges {
    profile,
    password: None,
    source: preview.source.unwrap(),
    changes: vec![RowChange::Update {
      original: preview.rows[0].clone(),
      values: vec![Some("1".into()), Some("after".into())],
    }],
  });
  let pid = sleeping_backend(
    &observer,
    "SELECT * FROM \"public\".\"cancellation_probe\" LIMIT 200",
  )
  .await;
  task.cancel();
  let Output::Saved(refresh) = response(&receiver).await.unwrap() else {
    panic!("committed update must remain successful");
  };
  assert!(refresh.unwrap_err().is::<Cancelled>());
  assert_stopped(&observer, pid).await;
  let value: String = observer
    .client
    .query_one("SELECT value FROM cancellation_probe", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(value, "after");
}

// TLS cancellation must keep the original hostname while connecting to its routed address.
#[tokio::test]
#[ignore = "requires local initdb, pg_ctl, and openssl binaries"]
async fn postgres_cancellation_preserves_tls_and_routed_endpoint() {
  let database = TestDatabase::start();
  let root = database.directory.path();
  let generated = Command::new("openssl")
    .args([
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-days",
      "1",
      "-subj",
      "/CN=db.test.invalid",
      "-addext",
      "subjectAltName=DNS:db.test.invalid",
    ])
    .arg("-keyout")
    .arg(root.join("server.key"))
    .arg("-out")
    .arg(root.join("server.crt"))
    .output()
    .unwrap();
  assert!(
    generated.status.success(),
    "{}",
    String::from_utf8_lossy(&generated.stderr)
  );
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
      root.join("server.key"),
      std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
  }
  // Trust only this test certificate; never alter the system trust store.
  let certificate =
    native_tls::Certificate::from_pem(&std::fs::read(root.join("server.crt")).unwrap()).unwrap();
  let tls = MakeTlsConnector::new(
    TlsConnector::builder()
      .add_root_certificate(certificate)
      .build()
      .unwrap(),
  );
  use std::io::Write;
  std::fs::OpenOptions::new()
    .append(true)
    .open(root.join("postgresql.conf"))
    .unwrap()
    .write_all(b"\nssl = on\nssl_cert_file = 'server.crt'\nssl_key_file = 'server.key'\n")
    .unwrap();
  let restarted = Command::new("pg_ctl")
    .arg("-D")
    .arg(root)
    .arg("-l")
    .arg(root.join("server.log"))
    .args(["-m", "fast", "-w", "-t", "5", "restart"])
    .output()
    .unwrap();
  assert!(
    restarted.status.success(),
    "{}",
    String::from_utf8_lossy(&restarted.stderr)
  );

  // This is the same host/hostaddr separation used by an SSH-forwarded connection.
  let mut config = Config::new();
  config
    .host("db.test.invalid")
    .hostaddr(IpAddr::V4(Ipv4Addr::LOCALHOST))
    .port(database.profile.port)
    .dbname("postgres")
    .user("textgres_test")
    .ssl_mode(SslMode::Require);
  let (client, connection) = config.connect(tls.clone()).await.unwrap();
  let driver = tokio::spawn(async move {
    let _ = connection.await;
  });
  let cancellation = Arc::new(Cancellation::default());
  let client = DatabaseConnection {
    client,
    driver,
    tls,
    cancellation: cancellation.clone(),
    broken: AtomicBool::new(false),
  };
  let encrypted: bool = client
    .client
    .query_one(
      "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
      &[],
    )
    .await
    .unwrap()
    .get(0);
  assert!(encrypted);
  let sql = "SELECT pg_sleep(20)";
  let work = tokio::spawn(async move { run_query(&client, sql).await });
  let observer = database.connect().await;
  let pid = sleeping_backend(&observer, sql).await;
  cancellation.request();
  let error = tokio::time::timeout(Duration::from_secs(7), work)
    .await
    .unwrap()
    .unwrap()
    .unwrap_err();
  assert!(error.is::<Cancelled>(), "{error:#}");
  assert_stopped(&observer, pid).await;
}

// Generated row updates use the extended protocol and must also roll back on cancellation.
#[tokio::test]
#[ignore = "requires local initdb and pg_ctl binaries"]
async fn postgres_cancelled_row_update_preserves_values() {
  let database = TestDatabase::start();
  let observer = database.connect().await;
  observer
    .client
    .batch_execute(
      r#"
    CREATE TABLE cancellation_probe (id int PRIMARY KEY, value text);
    INSERT INTO cancellation_probe VALUES (1, 'before');
    CREATE FUNCTION delay_update() RETURNS trigger LANGUAGE plpgsql AS $$
      BEGIN
        PERFORM pg_sleep(20);
        RETURN NEW;
      END
    $$;
    CREATE TRIGGER delay_update BEFORE UPDATE ON cancellation_probe
      FOR EACH ROW EXECUTE FUNCTION delay_update();
  "#,
    )
    .await
    .unwrap();
  let preview = preview_table(
    &observer,
    TableRef {
      profile_id: "test".into(),
      database: "postgres".into(),
      schema: "public".into(),
      name: "cancellation_probe".into(),
      kind: "table".into(),
    },
  )
  .await
  .unwrap();
  let source = preview.source.unwrap();
  let original = preview.rows[0].clone();
  let values = vec![Some("1".into()), Some("after".into())];
  let (sql, _) = build_update(&source, &original, &values).unwrap();
  let (task, receiver) = database.dispatch(Request::SaveChanges {
    profile: database.profile.clone(),
    password: None,
    source,
    changes: vec![RowChange::Update { original, values }],
  });
  let pid = sleeping_backend(&observer, &sql).await;
  task.cancel();
  assert!(response(&receiver).await.unwrap_err().is::<Cancelled>());
  assert_stopped(&observer, pid).await;
  let value: String = observer
    .client
    .query_one("SELECT value FROM cancellation_probe", &[])
    .await
    .unwrap()
    .get(0);
  assert_eq!(value, "before");
}

// Persistent-session regressions share the isolated cluster fixture.
mod sessions;

// Batch-write regressions share the disposable PostgreSQL fixture.
mod changes;

// Cursor tests verify bounded fetching and retained transaction ownership.
mod paging;

// Explorer disconnect tests drive actual key handling and PostgreSQL connection lifetimes.
mod explorer_sessions;
