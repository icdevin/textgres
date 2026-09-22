// Each cursor fetches a bounded page from the same execution and database snapshot.
use super::*;
use std::sync::atomic::AtomicU64;
use tokio::sync::Mutex as AsyncMutex;

pub(super) const PAGE_SIZE: usize = 200;
static NEXT_CURSOR: AtomicU64 = AtomicU64::new(1);

// Continuations cannot accidentally fetch a newer result or another database's cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageRef {
  pub(crate) id: u64,
  pub(crate) profile_id: String,
  pub(crate) database: String,
  pub(crate) preview: bool,
}

// SQL sessions own their transaction; preview connections always own a read-only transaction.
pub(super) struct Cursor {
  pub page: PageRef,
  pub owns_transaction: bool,
}

impl Cursor {
  // A process-unique name also prevents a stale UI continuation from addressing a replacement.
  pub fn new(profile_id: &str, database: &str, preview: bool, owns_transaction: bool) -> Self {
    Self {
      page: PageRef {
        id: NEXT_CURSOR.fetch_add(1, Ordering::Relaxed),
        profile_id: profile_id.into(),
        database: database.into(),
        preview,
      },
      owns_transaction,
    }
  }

  // Only internally generated identifiers enter cursor commands.
  fn name(&self) -> String {
    format!("\"textgres_result_{}\"", self.page.id)
  }

  // NO SCROLL avoids storing already-consumed rows on the server; the UI retains fetched pages.
  pub async fn open(&self, connection: &DatabaseConnection, sql: &str) -> anyhow::Result<()> {
    if self.owns_transaction {
      connection
        .run(connection.client.batch_execute("BEGIN READ ONLY"))
        .await?;
    }
    connection
      .run(connection.client.batch_execute(&format!(
        "DECLARE {} NO SCROLL CURSOR FOR\n{sql}\n",
        self.name()
      )))
      .await?;
    Ok(())
  }

  // Exactly 200 rows keeps even expensive row expressions from running ahead of the UI.
  pub async fn fetch(&self, connection: &DatabaseConnection) -> anyhow::Result<QueryResult> {
    let mut result = run_query(
      connection,
      &format!("FETCH FORWARD {PAGE_SIZE} FROM {}", self.name()),
    )
    .await?;
    if result.rows.len() == PAGE_SIZE {
      result.page = Some(self.page.clone());
    }
    result.status = page_status(result.rows.len(), result.page.is_some());
    Ok(result)
  }

  // Never commit or roll back a transaction started by the user.
  pub async fn close(&self, connection: &DatabaseConnection) -> anyhow::Result<()> {
    let sql = if self.owns_transaction {
      "ROLLBACK".into()
    } else {
      format!("CLOSE {}", self.name())
    };
    // Cleanup must still run after cancellation; a timeout forces the connection to be dropped.
    match tokio::time::timeout(CANCELLATION_TIMEOUT, connection.client.batch_execute(&sql)).await {
      Ok(Ok(())) => Ok(()),
      Ok(Err(error)) => Err(error.into()),
      Err(_) => {
        connection.broken.store(true, Ordering::SeqCst);
        anyhow::bail!("Timed out closing result cursor; reconnect the SQL session")
      }
    }
  }

  // Surface cleanup failures after successful reads, while retaining an earlier SQL/cancellation error.
  pub async fn finish(
    &self,
    connection: &DatabaseConnection,
    result: anyhow::Result<QueryResult>,
  ) -> anyhow::Result<QueryResult> {
    if result.as_ref().is_ok_and(|result| result.page.is_some()) {
      return result;
    }
    let cleanup = self.close(connection).await;
    match result {
      Ok(result) => {
        cleanup?;
        Ok(result)
      }
      Err(error) => Err(error),
    }
  }
}

// The total is unknown until FETCH returns fewer than a full page, including a final empty page.
pub(crate) fn page_status(rows: usize, more: bool) -> String {
  if more {
    format!("{rows} rows loaded · scroll down for more")
  } else {
    format!("{rows} row(s) · complete")
  }
}

// Parse statement boundaries rather than guessing around comments, strings, CTEs, or semicolons.
// Unsupported grammar and writes retain the normal execute-to-completion path.
pub(super) fn select_statement(sql: &str) -> Option<String> {
  let mut parser = tree_sitter::Parser::new();
  parser
    .set_language(&tree_sitter_sequel::LANGUAGE.into())
    .ok()?;
  let tree = parser.parse(sql, None)?;
  let root = tree.root_node();
  if root.has_error() {
    return None;
  }
  let mut walk = root.walk();
  let statements: Vec<_> = root
    .named_children(&mut walk)
    .filter(|node| !matches!(node.kind(), "comment" | "marginalia"))
    .collect();
  if statements.len() != 1 || statements[0].kind() != "statement" {
    return None;
  }
  let statement = statements[0];
  let mut walk = statement.walk();
  let children: Vec<_> = statement
    .named_children(&mut walk)
    .filter(|node| !matches!(node.kind(), "comment" | "marginalia"))
    .collect();
  if !children
    .iter()
    .any(|node| matches!(node.kind(), "select" | "set_operation"))
    || children.iter().any(|node| {
      !matches!(
        node.kind(),
        "select" | "from" | "set_operation" | "cte" | "keyword_with" | "keyword_recursive"
      )
    })
  {
    return None;
  }
  let mut pending = vec![statement];
  while let Some(node) = pending.pop() {
    // SELECT INTO, locking reads, and data-modifying CTEs must execute fully as written.
    if matches!(
      node.kind(),
      "keyword_into"
        | "keyword_insert"
        | "keyword_update"
        | "keyword_delete"
        | "keyword_merge"
        | "keyword_for"
    ) {
      return None;
    }
    let mut walk = node.walk();
    pending.extend(node.named_children(&mut walk));
  }
  Some(sql[statement.byte_range()].to_owned())
}

// Previews use independent connections so fetching does not alter a user's SQL transaction.
#[derive(Clone, Default)]
pub(super) struct Previews {
  entries: Arc<Mutex<PreviewEntries>>,
}

// One lock per preview keeps slow page fetches independent across databases.
type PreviewEntries = HashMap<(String, String), Arc<AsyncMutex<Preview>>>;

// The dedicated connection owns the cursor's snapshot and is dropped on replacement or failure.
struct Preview {
  connection: DatabaseConnection,
  cursor: Cursor,
}

impl Previews {
  // Dropping the previous connection closes its cursor and read-only transaction together.
  pub fn remove(&self, profile_id: &str, database: &str) {
    self
      .entries
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .remove(&(profile_id.into(), database.into()));
  }

  // Profile invalidation and shutdown must release idle preview connections as well as SQL sessions.
  pub fn clear(&self, profile_id: Option<&str>) {
    self
      .entries
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .retain(|(id, _), _| profile_id.is_some_and(|profile| profile != id));
  }

  // Starting a preview also validates that its metadata matches its actual result columns.
  pub async fn start(
    &self,
    profile: &ConnectionProfile,
    password: Option<&str>,
    table: TableRef,
    tunnels: &TunnelManager,
    cancellation: &Arc<Cancellation>,
  ) -> anyhow::Result<QueryResult> {
    self.remove(&table.profile_id, &table.database);
    let connection = connect(profile, password, &table.database, tunnels, cancellation).await?;
    self.start_connected(connection, table).await
  }

  // Reuse a committed batch's connection so its follow-up preview retains session settings.
  pub async fn start_connected(
    &self,
    connection: DatabaseConnection,
    table: TableRef,
  ) -> anyhow::Result<QueryResult> {
    self.remove(&table.profile_id, &table.database);
    let cursor = Cursor::new(&table.profile_id, &table.database, true, true);
    let columns = table_columns(&connection, &table).await?;
    cursor
      .open(
        &connection,
        &format!(
          "SELECT * FROM {}.{}",
          quote_identifier(&table.schema),
          quote_identifier(&table.name)
        ),
      )
      .await?;
    let mut result = cursor.fetch(&connection).await?;
    if columns
      .iter()
      .map(|column| &column.name)
      .eq(result.columns.iter())
    {
      result.source = Some(TableResultSource {
        query: None,
        table: table.clone(),
        columns,
      });
    }
    if result.page.is_some() {
      self
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
          (table.profile_id, table.database),
          Arc::new(AsyncMutex::new(Preview { connection, cursor })),
        );
    }
    // With no continuation, dropping the dedicated connection releases its read-only transaction.
    Ok(result)
  }

  // A failed fetch invalidates the cursor: retrying a FETCH could skip rows already consumed.
  pub async fn fetch(
    &self,
    page: &PageRef,
    cancellation: &Arc<Cancellation>,
  ) -> anyhow::Result<QueryResult> {
    let entry = self
      .entries
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .get(&(page.profile_id.clone(), page.database.clone()))
      .cloned()
      .ok_or_else(|| anyhow::anyhow!("Result cursor is closed; refresh the table"))?;
    let mut preview = entry
      .try_lock()
      .map_err(|_| anyhow::anyhow!("Result cursor is busy"))?;
    anyhow::ensure!(
      &preview.cursor.page == page,
      "Result cursor was replaced; refresh the table"
    );
    preview.connection.cancellation = cancellation.clone();
    let result = preview.cursor.fetch(&preview.connection).await;
    if !result.as_ref().is_ok_and(|result| result.page.is_some()) {
      self.remove(&page.profile_id, &page.database);
    }
    result
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // Comments and quoted semicolons must not defeat classification or become extra statements.
  #[test]
  fn classifies_single_reads_without_rewriting_sql() {
    for sql in [
      "select * from race_data.apx_cp_p;",
      "-- hello\nSELECT ';' AS value; -- trailing",
      "/* leading */ SELECT $$a;b$$;",
      "WITH data AS (SELECT * FROM items) SELECT * FROM data",
      "SELECT * FROM a UNION ALL SELECT * FROM b",
      "SELECT * FROM items ORDER BY id LIMIT 400 OFFSET 2",
    ] {
      assert!(select_statement(sql).is_some(), "not paged: {sql}");
    }
    for sql in [
      "SELECT 1; SELECT 2",
      "SELECT * INTO new_table FROM items",
      "WITH changed AS (DELETE FROM items RETURNING *) SELECT * FROM changed",
      "UPDATE items SET id=1 RETURNING *",
      "CREATE VIEW v AS SELECT * FROM items",
      "EXPLAIN ANALYZE SELECT * FROM items",
      "SELECT * FROM items FOR UPDATE",
      "SELECT 'unfinished",
      "BEGIN; SELECT * FROM items; COMMIT",
    ] {
      assert!(select_statement(sql).is_none(), "unsafe paging: {sql}");
    }
  }
}
