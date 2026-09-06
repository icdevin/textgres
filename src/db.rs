use std::{sync::mpsc::Sender, time::Duration};

use futures_util::StreamExt;
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use tokio::runtime::Handle;
use tokio_postgres::{Config, SimpleQueryMessage, config::SslMode};

use crate::storage::ConnectionProfile;

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

/// The bounded table model rendered by the results pane.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub status: String,
    pub truncated: bool,
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
}

/// A worker response always resolves the UI's one active operation.
#[derive(Debug)]
pub struct Response {
    pub result: anyhow::Result<Output>,
}

/// Runs database I/O outside the terminal event loop.
pub fn spawn(runtime: &Handle, sender: Sender<Response>, request: Request) {
    runtime.spawn(async move {
        let result = execute(request).await;
        // The receiver can disappear during normal application shutdown.
        let _ = sender.send(Response { result });
    });
}

async fn execute(request: Request) -> anyhow::Result<Output> {
    match request {
        Request::Databases { profile, password } => {
            let profile_id = profile.id.clone();
            let client = connect(&profile, password.as_deref(), &profile.database).await?;
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
            let client = connect(&profile, password.as_deref(), &database).await?;
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
            let client = connect(&profile, password.as_deref(), &database).await?;
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
            let client = connect(&profile, password.as_deref(), &table.database).await?;
            let sql = format!(
                "SELECT * FROM {}.{} LIMIT 200",
                quote_identifier(&table.schema),
                quote_identifier(&table.name)
            );
            Ok(Output::Result(run_query(&client, &sql).await?))
        }
        Request::Query {
            profile,
            password,
            database,
            sql,
        } => {
            let client = connect(&profile, password.as_deref(), &database).await?;
            Ok(Output::Result(run_query(&client, &sql).await?))
        }
    }
}

async fn connect(
    profile: &ConnectionProfile,
    password: Option<&str>,
    database: &str,
) -> anyhow::Result<tokio_postgres::Client> {
    let mut config = Config::new();
    config
        .host(&profile.host)
        .port(profile.port)
        .dbname(database)
        .user(&profile.user)
        .application_name("textgres")
        .connect_timeout(Duration::from_secs(10))
        .ssl_mode(if profile.require_tls {
            SslMode::Require
        } else {
            SslMode::Disable
        });

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
                        .map(|index| row.get(index).unwrap_or("NULL").to_owned())
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
}
