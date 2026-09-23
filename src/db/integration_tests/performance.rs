// Separate server execution from connection and session bookkeeping on a local database.
use super::*;

// Measure production SQL paths without UI polling or the test response helper's sleep.
#[tokio::test]
#[ignore = "manual benchmark; requires local initdb and pg_ctl binaries"]
async fn postgres_performance() {
  let database = TestDatabase::start();
  let connection = database.connect().await;
  connection.client.batch_execute("CREATE TABLE perf_rows (id integer PRIMARY KEY, value text); INSERT INTO perf_rows SELECT i, 'value ' || i FROM generate_series(1, 10000) i").await.unwrap();
  let manager = SessionManager::default();
  let tunnels = TunnelManager::default();
  // Root-store construction can dominate even when this local profile disables TLS.
  let mut tls_samples = Vec::new();
  for _ in 0..21 {
    let start = Instant::now();
    std::hint::black_box(TlsConnector::builder().build().unwrap());
    tls_samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
  }
  tls_samples.sort_by(f64::total_cmp);
  eprintln!(
    "tls/build-root-store: {:.1} us/op (median of 21)",
    tls_samples[10]
  );
  for (label, sql, mode) in [
    ("raw/select-1", "SELECT 1", 0),
    ("connect/select-1", "SELECT 1", 1),
    ("session/select-1", "SELECT 1", 2),
    ("raw/200-rows", "SELECT * FROM perf_rows LIMIT 200", 0),
    (
      "session/200-editable-rows",
      "SELECT * FROM perf_rows LIMIT 200",
      2,
    ),
  ] {
    let mut samples = Vec::new();
    for iteration in 0..22 {
      let start = Instant::now();
      match mode {
        0 => {
          run_query(&connection, sql).await.unwrap();
        }
        1 => {
          run_query(&database.connect().await, sql).await.unwrap();
        }
        _ => {
          let (result, _) = manager
            .execute(
              Request::Query {
                profile: database.profile.clone(),
                database: "postgres".into(),
                sql: sql.into(),
              },
              &tunnels,
              &Arc::new(Cancellation::default()),
            )
            .await;
          assert!(matches!(result.unwrap(), Output::Result(_)));
        }
      }
      // Exclude initial session setup and cold server metadata caches.
      if iteration > 0 {
        samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
      }
    }
    samples.sort_by(f64::total_cmp);
    eprintln!("{label}: {:.1} us/op (median of 21)", samples[10]);
  }
  manager.close_all();
}
