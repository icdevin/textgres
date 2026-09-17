use std::{
  collections::HashMap,
  fmt,
  future::Future,
  io::{self, Read},
  net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
  process::{Child, Command, Stdio},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
  },
  thread,
  time::{Duration, Instant},
};

use anyhow::Context;
use futures_util::StreamExt;
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use tokio::{runtime::Handle, sync::Notify, task::JoinHandle};
use tokio_postgres::{
  Config, SimpleQueryMessage,
  config::SslMode,
  error::{DbError, ErrorPosition},
  types::ToSql,
};

use crate::storage::{ConnectionProfile, SshConfig};

// SQL sessions own connections independently of per-operation workers.
mod sessions;
// Table changes are validated and committed as a single atomic batch.
mod changes;
// Server cursors keep result transfer bounded without rerunning SQL for each page.
mod paging;
pub use changes::{RowChange, UnknownCommit};
pub use paging::PageRef;
pub(crate) use paging::page_status;
pub use sessions::{SessionManager, SessionState, TransactionState};

const MAX_RESULT_ROWS: usize = 500;
// A lost cancellation connection must not leave the terminal busy indefinitely.
const CANCELLATION_TIMEOUT: Duration = Duration::from_secs(5);

/// A table node carries enough context to reconnect and preview it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableRef {
  pub profile_id: String,
  pub database: String,
  pub schema: String,
  pub name: String,
  pub kind: String,
}

/// Database metadata required to validate and encode one editable column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultColumn {
  pub name: String,
  pub type_name: String,
  pub editable: bool,
  // Identity ALWAYS and generated columns must use their server-generated insert values.
  pub insertable: bool,
  pub primary_key: bool,
}

/// Direct table previews retain the source needed for safe row updates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableResultSource {
  pub table: TableRef,
  pub columns: Vec<ResultColumn>,
}

/// The bounded table model rendered by the results pane.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueryResult {
  pub columns: Vec<String>,
  pub rows: Vec<Vec<Option<String>>>,
  pub status: String,
  pub truncated: bool,
  pub source: Option<TableResultSource>,
  // A continuation identifies one live cursor, never SQL to execute again.
  pub page: Option<PageRef>,
}

// Requests carry a fixed target; SQL connections are owned by the session manager.
#[derive(Clone, Debug)]
pub enum Request {
  // Lifecycle actions never silently replace a lost SQL session.
  Connect {
    profile: ConnectionProfile,
    database: String,
    reconnect: bool,
  },
  Disconnect {
    profile: ConnectionProfile,
    database: String,
  },
  Databases {
    profile: ConnectionProfile,
    password: Option<String>,
  },
  Schemas {
    profile: ConnectionProfile,
    password: Option<String>,
    database: String,
  },
  Tables {
    profile: ConnectionProfile,
    password: Option<String>,
    database: String,
    schema: String,
  },
  Preview {
    profile: ConnectionProfile,
    password: Option<String>,
    table: TableRef,
  },
  Query {
    profile: ConnectionProfile,
    database: String,
    sql: String,
  },
  SaveChanges {
    profile: ConnectionProfile,
    password: Option<String>,
    source: TableResultSource,
    changes: Vec<RowChange>,
  },
  FetchPage {
    page: PageRef,
  },
}

/// Typed output prevents the UI from accepting a response for the wrong node.
#[derive(Debug)]
pub enum Output {
  Session,
  Databases {
    profile_id: String,
    names: Vec<String>,
  },
  Schemas {
    profile_id: String,
    database: String,
    names: Vec<String>,
  },
  Tables {
    profile_id: String,
    database: String,
    schema: String,
    tables: Vec<TableRef>,
  },
  Result(QueryResult),
  // The write is committed even when its separate preview refresh fails.
  Saved(anyhow::Result<QueryResult>),
  Page {
    requested: PageRef,
    result: QueryResult,
  },
}

/// An operation ID keeps responses associated with the worker that owns them.
#[derive(Debug)]
pub struct Response {
  pub session_state: Option<SessionState>,
  pub operation_id: u64,
  pub result: anyhow::Result<Output>,
}

// Distinguish confirmed cancellation from an unknown write outcome.
#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("Database operation cancelled")
  }
}

impl std::error::Error for Cancelled {}

// The flag also reaches blocking SSH setup; Notify wakes the async worker promptly.
#[derive(Default)]
struct Cancellation {
  requested: Arc<AtomicBool>,
  notification: Notify,
}

impl Cancellation {
  fn request(&self) {
    self.requested.store(true, Ordering::SeqCst);
    self.notification.notify_one();
  }

  // The stored notification permit closes the race between checking and waiting.
  async fn wait(&self) {
    if !self.requested.load(Ordering::SeqCst) {
      self.notification.notified().await;
    }
  }
}

/// The worker retains ownership until PostgreSQL reports the operation's outcome.
pub struct Task {
  operation_id: u64,
  cancellation: Arc<Cancellation>,
  worker: JoinHandle<anyhow::Result<()>>,
}

impl Task {
  pub const fn operation_id(&self) -> u64 {
    self.operation_id
  }

  pub fn cancel(&self) {
    // Aborting the request future would leave PostgreSQL free to commit its work.
    self.cancellation.request();
  }

  pub async fn shutdown(mut self) -> anyhow::Result<()> {
    // Keep the runtime alive long enough to deliver cancellation on normal exit.
    self.cancel();
    (&mut self.worker)
      .await
      .context("database worker stopped unexpectedly")?
  }
}

impl Drop for Task {
  fn drop(&mut self) {
    // Dropping the UI handle still gives the bounded worker a chance to cancel.
    self.cancel();
  }
}

// Own the protocol driver so no database connection outlives its operation.
struct DatabaseConnection {
  client: tokio_postgres::Client,
  driver: JoinHandle<()>,
  tls: MakeTlsConnector,
  cancellation: Arc<Cancellation>,
  broken: AtomicBool,
}

impl Drop for DatabaseConnection {
  fn drop(&mut self) {
    self.driver.abort();
  }
}

impl DatabaseConnection {
  // Supervise each statement separately so cancellation cannot start a later phase.
  async fn run<T>(
    &self,
    work: impl Future<Output = Result<T, tokio_postgres::Error>>,
  ) -> anyhow::Result<T> {
    ensure_not_cancelled(&self.cancellation.requested)?;
    tokio::pin!(work);
    let result = tokio::select! {
      biased;
      result = &mut work => result,
      () = self.cancellation.wait() => {
        let token = self.client.cancel_token();
        let mut cancel_error = None;
        // The original response, not successful delivery of CancelRequest, is authoritative.
        let completion = async {
          tokio::select! {
            biased;
            result = &mut work => result,
            sent = token.cancel_query(self.tls.clone()) => {
              cancel_error = sent.err();
              work.await
            }
          }
        };
        match tokio::time::timeout(CANCELLATION_TIMEOUT, completion).await {
          Ok(result) => result,
          Err(_) => {
            let detail = cancel_error.map_or_else(String::new, |error| format!("; cancel request failed: {error}"));
            self.broken.store(true, Ordering::SeqCst);
            anyhow::bail!("Cancellation was not confirmed within 5 seconds; write outcome is unknown{detail}");
          }
        }
      }
    };
    match result {
      Err(error)
        if self.cancellation.requested.load(Ordering::SeqCst)
          && error.code() == Some(&tokio_postgres::error::SqlState::QUERY_CANCELED) =>
      {
        Err(Cancelled.into())
      }
      Err(error) => {
        let Some(database_error) = error.as_db_error() else {
          self.broken.store(true, Ordering::SeqCst);
          return Err(error).context("Database connection failed; write outcome is unknown");
        };
        if matches!(database_error.severity(), "FATAL" | "PANIC") {
          self.broken.store(true, Ordering::SeqCst);
          // A server disconnect can arrive after a commit but before its acknowledgement.
          anyhow::bail!(
            "Database connection failed; write outcome is unknown: {}",
            format_database_error(database_error)
          );
        }
        Err(error.into())
      }
      Ok(value) => Ok(value),
    }
  }
}

/// Shared tunnel state avoids a new SSH login for each explorer request.
#[derive(Clone, Default)]
pub struct TunnelManager {
  tunnels: Arc<Mutex<HashMap<String, ActiveTunnel>>>,
}

#[derive(Eq, PartialEq)]
struct TunnelSettings {
  database_host: String,
  database_port: u16,
  ssh: SshConfig,
}

struct ActiveTunnel {
  settings: TunnelSettings,
  process: SshTunnelProcess,
}

impl TunnelManager {
  /// Stops a tunnel when its saved profile changes or is removed.
  pub fn invalidate(&self, profile_id: &str) -> anyhow::Result<()> {
    self
      .tunnels
      .lock()
      .map_err(|_| anyhow::anyhow!("SSH tunnel state is unavailable"))?
      .remove(profile_id);
    Ok(())
  }

  async fn endpoint(
    &self,
    profile: &ConnectionProfile,
    cancelled: Arc<AtomicBool>,
  ) -> anyhow::Result<Option<u16>> {
    let Some(ssh) = &profile.ssh else {
      return Ok(None);
    };
    let profile_id = profile.id.clone();
    let settings = TunnelSettings {
      database_host: profile.host.clone(),
      database_port: profile.port,
      ssh: ssh.clone(),
    };
    let manager = self.clone();
    tokio::task::spawn_blocking(move || manager.endpoint_blocking(profile_id, settings, &cancelled))
      .await
      .context("SSH tunnel worker stopped unexpectedly")?
      .map(Some)
  }

  fn endpoint_blocking(
    &self,
    profile_id: String,
    settings: TunnelSettings,
    cancelled: &AtomicBool,
  ) -> anyhow::Result<u16> {
    ensure_not_cancelled(cancelled)?;
    let mut tunnels = self
      .tunnels
      .lock()
      .map_err(|_| anyhow::anyhow!("SSH tunnel state is unavailable"))?;
    ensure_not_cancelled(cancelled)?;
    let reusable = if let Some(tunnel) = tunnels.get_mut(&profile_id) {
      tunnel.settings == settings && tunnel.process.is_running()?
    } else {
      false
    };
    if reusable {
      return Ok(tunnels[&profile_id].process.local_port);
    }

    // Replacing the entry drops any stale process before binding a new port.
    tunnels.remove(&profile_id);
    let process = SshTunnelProcess::start(&settings, cancelled)?;
    ensure_not_cancelled(cancelled)?;
    let local_port = process.local_port;
    tunnels.insert(profile_id, ActiveTunnel { settings, process });
    Ok(local_port)
  }
}

struct SshTunnelProcess {
  child: Child,
  local_port: u16,
}

impl SshTunnelProcess {
  fn start(settings: &TunnelSettings, cancelled: &AtomicBool) -> anyhow::Result<Self> {
    ensure_not_cancelled(cancelled)?;
    let local_port = reserve_local_port()?;
    let mut child = Command::new("ssh")
      .args(ssh_arguments(settings, local_port))
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::piped())
      .spawn()
      .context("could not start OpenSSH; install the ssh client")?;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, local_port));
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
      if cancelled.load(Ordering::SeqCst) {
        // Stop the external process because aborting its Tokio parent cannot do so.
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("SSH connection was cancelled");
      }
      if let Some(status) = child
        .try_wait()
        .context("could not inspect the SSH process")?
      {
        let detail = read_ssh_stderr(&mut child);
        anyhow::bail!("SSH tunnel failed with {status}: {detail}");
      }
      if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
        // Drain later SSH diagnostics so the child cannot block on a full pipe.
        if let Some(mut stderr) = child.stderr.take() {
          thread::spawn(move || {
            let _ = io::copy(&mut stderr, &mut io::sink());
          });
        }
        return Ok(Self { child, local_port });
      }
      if Instant::now() >= deadline {
        let _ = child.kill();
        let _ = child.wait();
        let detail = read_ssh_stderr(&mut child);
        anyhow::bail!("SSH tunnel did not become ready within 10 seconds: {detail}");
      }
      thread::sleep(Duration::from_millis(50));
    }
  }

  fn is_running(&mut self) -> anyhow::Result<bool> {
    Ok(
      self
        .child
        .try_wait()
        .context("could not inspect the SSH process")?
        .is_none(),
    )
  }
}

fn ensure_not_cancelled(cancelled: &AtomicBool) -> anyhow::Result<()> {
  if cancelled.load(Ordering::SeqCst) {
    return Err(Cancelled.into());
  }
  Ok(())
}

impl Drop for SshTunnelProcess {
  fn drop(&mut self) {
    // OpenSSH has no useful shutdown exchange for this dedicated child.
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

fn reserve_local_port() -> anyhow::Result<u16> {
  // OpenSSH cannot request an ephemeral local-forward port directly.
  let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
    .context("could not reserve a local SSH tunnel port")?;
  Ok(listener.local_addr()?.port())
}

fn ssh_arguments(settings: &TunnelSettings, local_port: u16) -> Vec<String> {
  let database_host = if settings.database_host.contains(':') {
    format!("[{}]", settings.database_host)
  } else {
    settings.database_host.clone()
  };
  let forward = format!(
    "127.0.0.1:{local_port}:{database_host}:{}",
    settings.database_port
  );
  let mut arguments = vec![
    "-N".into(),
    "-T".into(),
    "-o".into(),
    "BatchMode=yes".into(),
    "-o".into(),
    "StrictHostKeyChecking=yes".into(),
    "-o".into(),
    "ExitOnForwardFailure=yes".into(),
    "-o".into(),
    "ConnectTimeout=10".into(),
    "-o".into(),
    "ServerAliveInterval=30".into(),
    "-o".into(),
    "ServerAliveCountMax=3".into(),
    "-L".into(),
    forward,
    "-p".into(),
    settings.ssh.port.to_string(),
    "-l".into(),
    settings.ssh.user.clone(),
  ];
  if let Some(identity_file) = settings
    .ssh
    .identity_file
    .as_deref()
    .filter(|value| !value.is_empty())
  {
    arguments.extend(["-i".into(), identity_file.into()]);
  }
  arguments.push(settings.ssh.host.clone());
  arguments
}

fn read_ssh_stderr(child: &mut Child) -> String {
  let mut detail = String::new();
  if let Some(mut stderr) = child.stderr.take() {
    let _ = stderr.read_to_string(&mut detail);
  }
  let detail = detail.trim();
  if detail.is_empty() {
    "no diagnostic output".into()
  } else {
    detail.into()
  }
}

/// Preserves structured PostgreSQL diagnostics and falls back for transport errors.
pub fn format_error(error: &anyhow::Error) -> String {
  let database_error = error
    .chain()
    .find_map(|cause| cause.downcast_ref::<tokio_postgres::Error>())
    .and_then(tokio_postgres::Error::as_db_error);
  let Some(database_error) = database_error else {
    return format!("Database error: {error:#}");
  };
  format_database_error(database_error)
}

fn format_database_error(error: &DbError) -> String {
  let mut lines = vec![sql_error_header(
    error.code().code(),
    error.severity(),
    error.message(),
  )];
  if let Some(detail) = error.detail() {
    lines.push(format!("Detail: {detail}"));
  }
  if let Some(hint) = error.hint() {
    lines.push(format!("Hint: {hint}"));
  }
  if let Some(position) = error.position() {
    lines.extend(format_error_position(position));
  }
  lines.join("\n")
}

fn sql_error_header(code: &str, severity: &str, message: &str) -> String {
  format!("SQL Error [{code}]: {severity}: {message}")
}

fn format_error_position(position: &ErrorPosition) -> Vec<String> {
  match position {
    ErrorPosition::Original(position) => vec![format!("Position: {position}")],
    ErrorPosition::Internal { position, query } => vec![
      format!("Internal position: {position}"),
      format!("Internal query: {query}"),
    ],
  }
}

/// Runs cancellable database I/O outside the terminal event loop.
pub fn spawn(
  runtime: &Handle,
  sender: Sender<Response>,
  tunnels: TunnelManager,
  sessions: SessionManager,
  operation_id: u64,
  request: Request,
) -> Task {
  let cancellation = Arc::new(Cancellation::default());
  let worker_cancellation = cancellation.clone();
  let worker = runtime.spawn(async move {
    let (result, session_state) = sessions
      .execute(request, &tunnels, &worker_cancellation)
      .await;
    // The terminal receiver closes on exit; still report an uncertain write to the caller.
    let shutdown_result = match &result {
      Err(error) if !error.is::<Cancelled>() => Err(anyhow::anyhow!(format_error(error))),
      _ => Ok(()),
    };
    // The receiver can disappear during normal application shutdown.
    let _ = sender.send(Response {
      session_state,
      operation_id,
      result,
    });
    shutdown_result
  });
  Task {
    operation_id,
    cancellation,
    worker,
  }
}

async fn execute(
  request: Request,
  tunnels: &TunnelManager,
  cancelled: &Arc<Cancellation>,
) -> anyhow::Result<Output> {
  match request {
    Request::Connect { .. }
    | Request::Disconnect { .. }
    | Request::Query { .. }
    | Request::FetchPage { .. } => {
      anyhow::bail!("session lifecycle requests require a session manager")
    }
    Request::Databases { profile, password } => {
      let profile_id = profile.id.clone();
      let client = connect(
        &profile,
        password.as_deref(),
        &profile.database,
        tunnels,
        cancelled,
      )
      .await?;
      let rows = client
        .run(client.client.query(
          "SELECT datname FROM pg_database \
                     WHERE datallowconn AND NOT datistemplate ORDER BY datname",
          &[],
        ))
        .await?;
      Ok(Output::Databases {
        profile_id,
        names: rows.into_iter().map(|row| row.get(0)).collect(),
      })
    }
    Request::Schemas {
      profile,
      password,
      database,
    } => {
      let profile_id = profile.id.clone();
      let client = connect(&profile, password.as_deref(), &database, tunnels, cancelled).await?;
      let rows = client
        .run(client.client.query(
          "SELECT nspname FROM pg_namespace \
                     WHERE nspname NOT IN ('pg_catalog', 'information_schema') \
                     AND nspname NOT LIKE 'pg_toast%' ORDER BY nspname",
          &[],
        ))
        .await?;
      Ok(Output::Schemas {
        profile_id,
        database,
        names: rows.into_iter().map(|row| row.get(0)).collect(),
      })
    }
    Request::Tables {
      profile,
      password,
      database,
      schema,
    } => {
      let profile_id = profile.id.clone();
      let client = connect(&profile, password.as_deref(), &database, tunnels, cancelled).await?;
      let rows = client
        .run(client.client.query(
          "SELECT c.relname, CASE c.relkind \
                         WHEN 'r' THEN 'table' WHEN 'p' THEN 'partitioned table' \
                         WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' \
                         WHEN 'f' THEN 'foreign table' ELSE c.relkind::text END \
                     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
                     ORDER BY c.relname",
          &[&schema],
        ))
        .await?;
      let tables = rows
        .into_iter()
        .map(|row| TableRef {
          profile_id: profile_id.clone(),
          database: database.clone(),
          schema: schema.clone(),
          name: row.get(0),
          kind: row.get(1),
        })
        .collect();
      Ok(Output::Tables {
        profile_id,
        database,
        schema,
        tables,
      })
    }
    Request::Preview { .. } | Request::SaveChanges { .. } => {
      anyhow::bail!("table results require a cursor manager")
    }
  }
}

#[cfg(test)]
async fn preview_table(
  client: &DatabaseConnection,
  table: TableRef,
) -> anyhow::Result<QueryResult> {
  let columns = table_columns(client, &table).await?;
  let sql = format!(
    "SELECT * FROM {}.{} LIMIT 200",
    quote_identifier(&table.schema),
    quote_identifier(&table.name)
  );
  let mut result = run_query(client, &sql).await?;
  // Disable updates when metadata and result columns do not align exactly.
  if columns
    .iter()
    .map(|column| &column.name)
    .eq(&result.columns)
  {
    result.source = Some(TableResultSource { table, columns });
  }
  Ok(result)
}

async fn table_columns(
  client: &DatabaseConnection,
  table: &TableRef,
) -> anyhow::Result<Vec<ResultColumn>> {
  let rows = client
    .run(client.client.query(
      "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), \
              a.attgenerated = '', \
              EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = c.oid \
                AND i.indisprimary AND a.attnum = ANY(i.indkey::smallint[])), \
              a.attgenerated = '' AND a.attidentity <> 'a' \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attribute a ON a.attrelid = c.oid \
        WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
        ORDER BY a.attnum",
      &[&table.schema, &table.name],
    ))
    .await?;
  let table_is_editable = matches!(table.kind.as_str(), "table" | "partitioned table");
  Ok(
    rows
      .into_iter()
      .map(|row| ResultColumn {
        name: row.get(0),
        type_name: row.get(1),
        editable: table_is_editable && row.get(2),
        insertable: table_is_editable && row.get(4),
        primary_key: row.get(3),
      })
      .collect(),
  )
}

// Build typed parameters from catalog metadata; never interpolate edited values.
fn build_update(
  source: &TableResultSource,
  original: &[Option<String>],
  values: &[Option<String>],
) -> anyhow::Result<(String, Vec<Option<String>>)> {
  anyhow::ensure!(
    source.columns.len() == original.len() && original.len() == values.len(),
    "row data does not match the table metadata"
  );
  anyhow::ensure!(
    source
      .columns
      .iter()
      .enumerate()
      .all(|(i, column)| column.editable || original[i] == values[i]),
    "generated or read-only columns cannot be changed"
  );
  let changed = source
    .columns
    .iter()
    .enumerate()
    .filter(|(index, column)| column.editable && original[*index] != values[*index])
    .collect::<Vec<_>>();
  anyhow::ensure!(!changed.is_empty(), "the row has no editable changes");
  let keys = source
    .columns
    .iter()
    .enumerate()
    .filter(|(_, column)| column.primary_key)
    .collect::<Vec<_>>();
  anyhow::ensure!(!keys.is_empty(), "the table has no primary key");

  let mut parameters = Vec::new();
  let assignments = changed
    .iter()
    .map(|(index, column)| {
      parameters.push(values[*index].clone());
      format!(
        "{} = ${}::text::{}",
        quote_identifier(&column.name),
        parameters.len(),
        column.type_name
      )
    })
    .collect::<Vec<_>>();
  let mut predicate_columns = keys;
  // Match original edited values too, so a concurrent edit is not overwritten.
  predicate_columns.extend(
    changed
      .iter()
      .copied()
      .filter(|(_, column)| !column.primary_key),
  );
  let predicates = predicate_columns
    .iter()
    .map(|(index, column)| {
      parameters.push(original[*index].clone());
      changes::predicate(column, parameters.len())
    })
    .collect::<Vec<_>>();
  let sql = format!(
    "UPDATE {}.{} SET {} WHERE {}",
    quote_identifier(&source.table.schema),
    quote_identifier(&source.table.name),
    assignments.join(", "),
    predicates.join(" AND ")
  );
  Ok((sql, parameters))
}

async fn connect(
  profile: &ConnectionProfile,
  password: Option<&str>,
  database: &str,
  tunnels: &TunnelManager,
  cancelled: &Arc<Cancellation>,
) -> anyhow::Result<DatabaseConnection> {
  // No SQL has started during setup, so dropping this future is safe.
  tokio::select! {
    biased;
    () = cancelled.wait() => Err(Cancelled.into()),
    result = connect_inner(profile, password, database, tunnels, cancelled) => result,
  }
}

// Keep connection setup separate from the lifetime of an executing statement.
async fn connect_inner(
  profile: &ConnectionProfile,
  password: Option<&str>,
  database: &str,
  tunnels: &TunnelManager,
  cancelled: &Arc<Cancellation>,
) -> anyhow::Result<DatabaseConnection> {
  let tunnel_port = tunnels
    .endpoint(profile, cancelled.requested.clone())
    .await?;
  let mut config = Config::new();
  config
    .host(&profile.host)
    .dbname(database)
    .user(&profile.user)
    .application_name("textgres")
    // Set the default at startup so ROLLBACK still works in an aborted transaction.
    .options("-c statement_timeout=30000")
    .connect_timeout(Duration::from_secs(10))
    .ssl_mode(if profile.require_tls {
      SslMode::Require
    } else {
      SslMode::Disable
    });
  if let Some(tunnel_port) = tunnel_port {
    // Preserve the database host for TLS while routing TCP through localhost.
    config
      .hostaddr(IpAddr::V4(Ipv4Addr::LOCALHOST))
      .port(tunnel_port);
  } else {
    config.port(profile.port);
  }

  if let Some(password) = password.filter(|value| !value.is_empty()) {
    config.password(password);
  } else if let Ok(password) = std::env::var("PGPASSWORD") {
    config.password(password);
  }

  // Native roots verify remote PostgreSQL certificates when TLS is required.
  let tls = MakeTlsConnector::new(TlsConnector::builder().build()?);
  let (client, connection) = config.connect(tls.clone()).await?;
  let driver = tokio::spawn(async move {
    // Query futures receive connection failures; no terminal output is safe here.
    let _ = connection.await;
  });
  Ok(DatabaseConnection {
    client,
    driver,
    tls,
    cancellation: cancelled.clone(),
    broken: AtomicBool::new(false),
  })
}

async fn run_query(client: &DatabaseConnection, sql: &str) -> anyhow::Result<QueryResult> {
  // Keep reading through ReadyForQuery so late errors and cancellation are observed.
  client
    .run(async {
      let stream = client.client.simple_query_raw(sql).await?;
      futures_util::pin_mut!(stream);

      let mut result = QueryResult::default();
      let mut affected = Vec::new();
      while let Some(message) = stream.next().await {
        match message? {
          SimpleQueryMessage::RowDescription(columns) => {
            // The last result set is the least surprising view for multi-statement SQL.
            result.columns = columns
              .iter()
              .map(|column| column.name().to_owned())
              .collect();
            result.rows.clear();
            result.truncated = false;
          }
          SimpleQueryMessage::Row(row) => {
            if result.rows.len() == MAX_RESULT_ROWS {
              result.truncated = true;
              continue;
            }
            result.rows.push(
              (0..row.len())
                // Preserve nullability so the renderer never confuses NULL with text.
                .map(|index| row.get(index).map(str::to_owned))
                .collect(),
            );
          }
          SimpleQueryMessage::CommandComplete(count) => affected.push(count),
          _ => {}
        }
      }

      result.status = if result.truncated {
        format!("Showing first {MAX_RESULT_ROWS} rows; result truncated")
      } else if !result.columns.is_empty() {
        format!("{} row(s)", result.rows.len())
      } else if affected.is_empty() {
        "Query completed".to_owned()
      } else {
        format!(
          "Query completed; {} row(s) affected",
          affected.iter().sum::<u64>()
        )
      };
      Ok(result)
    })
    .await
}

// PostgreSQL identifiers cannot use value parameters, so quote them as identifiers.
fn quote_identifier(identifier: &str) -> String {
  format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn quotes_postgres_identifiers() {
    assert_eq!(quote_identifier("odd\"name"), "\"odd\"\"name\"");
  }

  #[test]
  fn formats_postgres_error_identity_and_position() {
    let mut lines = vec![sql_error_header(
      "42P01",
      "ERROR",
      "relation \"derp\" does not exist",
    )];
    lines.extend(format_error_position(&ErrorPosition::Original(52)));

    assert_eq!(
      lines.join("\n"),
      "SQL Error [42P01]: ERROR: relation \"derp\" does not exist\nPosition: 52"
    );
  }

  #[test]
  fn builds_primary_key_update_with_typed_parameters() {
    let source = TableResultSource {
      table: TableRef {
        profile_id: "local".into(),
        database: "postgres".into(),
        schema: "odd schema".into(),
        name: "user".into(),
        kind: "table".into(),
      },
      columns: vec![
        ResultColumn {
          name: "id".into(),
          type_name: "integer".into(),
          editable: true,
          insertable: true,
          primary_key: true,
        },
        ResultColumn {
          name: "display name".into(),
          type_name: "text".into(),
          editable: true,
          insertable: true,
          primary_key: false,
        },
      ],
    };

    let (sql, parameters) = build_update(
      &source,
      &[Some("7".into()), Some("before".into())],
      &[Some("7".into()), Some("after'; DROP TABLE x".into())],
    )
    .unwrap();

    assert_eq!(
      sql,
      "UPDATE \"odd schema\".\"user\" SET \"display name\" = $1::text::text \
       WHERE \"id\" IS NOT DISTINCT FROM $2::text::integer AND \
       to_jsonb(\"display name\") IS NOT DISTINCT FROM to_jsonb($3::text::text)"
    );
    assert_eq!(
      parameters,
      vec![
        Some("after'; DROP TABLE x".into()),
        Some("7".into()),
        Some("before".into())
      ]
    );
    assert!(!sql.contains("DROP"));
  }

  #[test]
  fn builds_noninteractive_local_forward_arguments() {
    // The remote database endpoint must never become the local TCP target.
    let settings = TunnelSettings {
      database_host: "db.internal".into(),
      database_port: 5432,
      ssh: SshConfig {
        host: "gateway.example.com".into(),
        port: 2222,
        user: "devin".into(),
        identity_file: Some("~/.ssh/work".into()),
      },
    };

    let arguments = ssh_arguments(&settings, 32123);

    assert!(
      arguments
        .windows(2)
        .any(|pair| pair == ["-L", "127.0.0.1:32123:db.internal:5432"])
    );
    assert!(
      arguments
        .windows(2)
        .any(|pair| pair == ["-i", "~/.ssh/work"])
    );
    assert!(arguments.windows(2).any(|pair| pair == ["-p", "2222"]));
    assert!(arguments.contains(&"BatchMode=yes".into()));
    assert!(!arguments.contains(&"ClearAllForwardings=yes".into()));
    assert_eq!(arguments.last().unwrap(), "gateway.example.com");
  }
}

// Real-server regression tests remain separate from SQL-construction unit tests.
#[cfg(test)]
mod integration_tests;
