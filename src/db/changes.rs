// Batch writes use one dedicated transaction, separate from the SQL editor's session.
use super::*;

// A false default flag with a None value means SQL NULL; true lets PostgreSQL supply DEFAULT.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RowChange {
  Insert {
    values: Vec<Option<String>>,
    defaults: Vec<bool>,
  },
  Update {
    original: Vec<Option<String>>,
    values: Vec<Option<String>>,
  },
  Delete {
    original: Vec<Option<String>>,
  },
}

// Retrying inserts after a lost COMMIT acknowledgement can duplicate data.
#[derive(Debug)]
pub struct UnknownCommit(pub anyhow::Error);

impl fmt::Display for UnknownCommit {
  // Keep the recovery instruction visible even when callers show only the outer error.
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "Save outcome is unknown; refresh and verify the table before making more changes: {}",
      self.0
    )
  }
}
impl std::error::Error for UnknownCommit {}

// Primary keys keep native equality; other values also support json, xml, and geometric types.
pub(super) fn predicate(column: &ResultColumn, parameter: usize) -> String {
  let name = quote_identifier(&column.name);
  let value = format!("${parameter}::text::{}", column.type_name);
  if column.primary_key {
    format!("{name} IS NOT DISTINCT FROM {value}")
  } else {
    format!("to_jsonb({name}) IS NOT DISTINCT FROM to_jsonb({value})")
  }
}

// Validate identity, read-only columns, and affected counts before committing any row.
pub(super) async fn save(
  client: &DatabaseConnection,
  source: &TableResultSource,
  changes: &[RowChange],
) -> anyhow::Result<()> {
  anyhow::ensure!(!changes.is_empty(), "No table changes to save");
  anyhow::ensure!(
    matches!(source.table.kind.as_str(), "table" | "partitioned table"),
    "This object is read-only"
  );
  client.run(client.client.batch_execute("BEGIN")).await?;
  let write = async {
    // Prevent DDL from changing column types or keys between validation and the writes.
    client
      .run(client.client.batch_execute(&format!(
        "LOCK TABLE {}.{} IN ROW EXCLUSIVE MODE",
        quote_identifier(&source.table.schema),
        quote_identifier(&source.table.name)
      )))
      .await?;
    anyhow::ensure!(
      table_columns(client, &source.table).await? == source.columns,
      "Table definition changed; refresh before editing"
    );
    for change in changes {
      let (sql, parameters) = statement(source, change)?;
      let refs: Vec<_> = parameters
        .iter()
        .map(|value| value as &(dyn ToSql + Sync))
        .collect();
      let affected = client.run(client.client.execute(&sql, &refs)).await?;
      anyhow::ensure!(
        affected == 1,
        "Row conflict: expected one matching row, found {affected}; no changes saved"
      );
    }
    Ok::<_, anyhow::Error>(())
  }
  .await;
  if let Err(error) = write {
    // Cancellation blocks supervised work, so rollback uses its own bounded cleanup attempt.
    // The owning worker drops the connection on return even if this rollback cannot complete.
    let _ = tokio::time::timeout(
      CANCELLATION_TIMEOUT,
      client.client.batch_execute("ROLLBACK"),
    )
    .await;
    return Err(error).context("Batch not committed");
  }
  // Only losing the COMMIT response leaves the batch's outcome uncertain.
  if let Err(error) = client.run(client.client.batch_execute("COMMIT")).await {
    if client.broken.load(Ordering::SeqCst) {
      return Err(UnknownCommit(error).into());
    }
    let _ = tokio::time::timeout(
      CANCELLATION_TIMEOUT,
      client.client.batch_execute("ROLLBACK"),
    )
    .await;
    return Err(error).context("Batch not committed");
  }
  Ok(())
}

// Values always use parameters; DEFAULT is represented explicitly instead of overloading NULL.
fn statement(
  source: &TableResultSource,
  change: &RowChange,
) -> anyhow::Result<(String, Vec<Option<String>>)> {
  let table = format!(
    "{}.{}",
    quote_identifier(&source.table.schema),
    quote_identifier(&source.table.name)
  );
  match change {
    RowChange::Update { original, values } => build_update(source, original, values),
    RowChange::Delete { original } => {
      anyhow::ensure!(
        original.len() == source.columns.len(),
        "Row does not match table metadata"
      );
      anyhow::ensure!(
        source.columns.iter().any(|column| column.primary_key),
        "Deletion requires a primary key"
      );
      // Compare the full original row so deletion cannot silently erase someone else's edits.
      let predicates: Vec<_> = source
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| predicate(column, index + 1))
        .collect();
      Ok((
        format!("DELETE FROM {table} WHERE {}", predicates.join(" AND ")),
        original.clone(),
      ))
    }
    RowChange::Insert { values, defaults } => {
      anyhow::ensure!(
        values.len() == source.columns.len() && defaults.len() == values.len(),
        "Row does not match table metadata"
      );
      let mut columns = Vec::new();
      let mut expressions = Vec::new();
      let mut parameters = Vec::new();
      for (index, column) in source.columns.iter().enumerate() {
        if defaults[index] {
          continue;
        }
        anyhow::ensure!(
          column.insertable,
          "Column {} requires its generated default",
          column.name
        );
        columns.push(quote_identifier(&column.name));
        parameters.push(values[index].clone());
        expressions.push(format!("${}::text::{}", parameters.len(), column.type_name));
      }
      let sql = if columns.is_empty() {
        format!("INSERT INTO {table} DEFAULT VALUES")
      } else {
        format!(
          "INSERT INTO {table} ({}) VALUES ({})",
          columns.join(", "),
          expressions.join(", ")
        )
      };
      Ok((sql, parameters))
    }
  }
}
