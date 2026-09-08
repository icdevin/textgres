use std::{
  collections::HashMap,
  io::{self, Read},
  net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
  process::{Child, Command, Stdio},
  sync::{Arc, Mutex, mpsc::Sender},
  thread,
  time::{Duration, Instant},
};

use anyhow::Context;
use futures_util::StreamExt;
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use tokio::runtime::Handle;
use tokio_postgres::{
  Config, SimpleQueryMessage,
  config::SslMode,
  error::{DbError, ErrorPosition},
  types::ToSql,
};

use crate::storage::{ConnectionProfile, SshConfig};

const MAX_RESULT_ROWS: usize = 500;

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
}

/// Each request is self-contained so no connection can outlive its owning task.
#[derive(Clone, Debug)]
pub enum Request {
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
    password: Option<String>,
    database: String,
    sql: String,
  },
  UpdateRow {
    profile: ConnectionProfile,
    password: Option<String>,
    source: TableResultSource,
    original: Vec<Option<String>>,
    values: Vec<Option<String>>,
  },
}

/// Typed output prevents the UI from accepting a response for the wrong node.
#[derive(Debug)]
pub enum Output {
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
  Updated(QueryResult),
}

/// A worker response always resolves the UI's one active operation.
#[derive(Debug)]
pub struct Response {
  pub result: anyhow::Result<Output>,
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

  async fn endpoint(&self, profile: &ConnectionProfile) -> anyhow::Result<Option<u16>> {
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
    tokio::task::spawn_blocking(move || manager.endpoint_blocking(profile_id, settings))
      .await
      .context("SSH tunnel worker stopped unexpectedly")?
      .map(Some)
  }

  fn endpoint_blocking(&self, profile_id: String, settings: TunnelSettings) -> anyhow::Result<u16> {
    let mut tunnels = self
      .tunnels
      .lock()
      .map_err(|_| anyhow::anyhow!("SSH tunnel state is unavailable"))?;
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
    let process = SshTunnelProcess::start(&settings)?;
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
  fn start(settings: &TunnelSettings) -> anyhow::Result<Self> {
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

/// Runs database I/O outside the terminal event loop.
pub fn spawn(runtime: &Handle, sender: Sender<Response>, tunnels: TunnelManager, request: Request) {
  runtime.spawn(async move {
    let result = execute(request, &tunnels).await;
    // The receiver can disappear during normal application shutdown.
    let _ = sender.send(Response { result });
  });
}

async fn execute(request: Request, tunnels: &TunnelManager) -> anyhow::Result<Output> {
  match request {
    Request::Databases { profile, password } => {
      let profile_id = profile.id.clone();
      let client = connect(&profile, password.as_deref(), &profile.database, tunnels).await?;
      let rows = client
        .query(
          "SELECT datname FROM pg_database \
                     WHERE datallowconn AND NOT datistemplate ORDER BY datname",
          &[],
        )
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
      let client = connect(&profile, password.as_deref(), &database, tunnels).await?;
      let rows = client
        .query(
          "SELECT nspname FROM pg_namespace \
                     WHERE nspname NOT IN ('pg_catalog', 'information_schema') \
                     AND nspname NOT LIKE 'pg_toast%' ORDER BY nspname",
          &[],
        )
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
      let client = connect(&profile, password.as_deref(), &database, tunnels).await?;
      let rows = client
        .query(
          "SELECT c.relname, CASE c.relkind \
                         WHEN 'r' THEN 'table' WHEN 'p' THEN 'partitioned table' \
                         WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' \
                         WHEN 'f' THEN 'foreign table' ELSE c.relkind::text END \
                     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
                     ORDER BY c.relname",
          &[&schema],
        )
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
    Request::Preview {
      profile,
      password,
      table,
    } => {
      let client = connect(&profile, password.as_deref(), &table.database, tunnels).await?;
      Ok(Output::Result(preview_table(&client, table).await?))
    }
    Request::Query {
      profile,
      password,
      database,
      sql,
    } => {
      let client = connect(&profile, password.as_deref(), &database, tunnels).await?;
      Ok(Output::Result(run_query(&client, &sql).await?))
    }
    Request::UpdateRow {
      profile,
      password,
      source,
      original,
      values,
    } => {
      let client = connect(
        &profile,
        password.as_deref(),
        &source.table.database,
        tunnels,
      )
      .await?;
      update_row(&client, &source, &original, &values).await?;
      Ok(Output::Updated(preview_table(&client, source.table).await?))
    }
  }
}

async fn preview_table(
  client: &tokio_postgres::Client,
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
  client: &tokio_postgres::Client,
  table: &TableRef,
) -> anyhow::Result<Vec<ResultColumn>> {
  let rows = client
    .query(
      "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), \
              a.attgenerated = '', \
              EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = c.oid \
                AND i.indisprimary AND a.attnum = ANY(i.indkey::smallint[])) \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attribute a ON a.attrelid = c.oid \
        WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
        ORDER BY a.attnum",
      &[&table.schema, &table.name],
    )
    .await?;
  let table_is_editable = matches!(table.kind.as_str(), "table" | "partitioned table");
  Ok(
    rows
      .into_iter()
      .map(|row| ResultColumn {
        name: row.get(0),
        type_name: row.get(1),
        editable: table_is_editable && row.get(2),
        primary_key: row.get(3),
      })
      .collect(),
  )
}

async fn update_row(
  client: &tokio_postgres::Client,
  source: &TableResultSource,
  original: &[Option<String>],
  values: &[Option<String>],
) -> anyhow::Result<()> {
  let (sql, parameters) = build_update(source, original, values)?;
  let parameter_refs = parameters
    .iter()
    .map(|value| value as &(dyn ToSql + Sync))
    .collect::<Vec<_>>();
  client
    .batch_execute("SET statement_timeout = '30s'")
    .await?;
  let affected = client.execute(&sql, &parameter_refs).await?;
  anyhow::ensure!(
    affected == 1,
    "row update conflict: expected one matching row, found {affected}"
  );
  Ok(())
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
      format!(
        "{} IS NOT DISTINCT FROM ${}::text::{}",
        quote_identifier(&column.name),
        parameters.len(),
        column.type_name
      )
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
) -> anyhow::Result<tokio_postgres::Client> {
  let tunnel_port = tunnels.endpoint(profile).await?;
  let mut config = Config::new();
  config
    .host(&profile.host)
    .dbname(database)
    .user(&profile.user)
    .application_name("textgres")
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
  let (client, connection) = config.connect(tls).await?;
  tokio::spawn(async move {
    // Query futures receive connection failures; no terminal output is safe here.
    let _ = connection.await;
  });
  Ok(client)
}

async fn run_query(client: &tokio_postgres::Client, sql: &str) -> anyhow::Result<QueryResult> {
  client
    .batch_execute("SET statement_timeout = '30s'")
    .await?;
  let stream = client.simple_query_raw(sql).await?;
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
          break;
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
          primary_key: true,
        },
        ResultColumn {
          name: "display name".into(),
          type_name: "text".into(),
          editable: true,
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
       \"display name\" IS NOT DISTINCT FROM $3::text::text"
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
