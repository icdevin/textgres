// Resolve custom result identity from PostgreSQL metadata, never from display labels.
use super::*;

// A source belongs to one execution in one retained SQL session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuerySource {
  pub sql: String,
  pub execution_id: u64,
  pub table_oid: u32,
  pub table_columns: Vec<ResultColumn>,
  // Attribute numbers detect dropped/recreated columns with otherwise identical definitions.
  pub attribute_ids: Vec<i16>,
  // None marks an expression; mapped indexes address the complete catalog snapshot.
  pub mapping: Vec<Option<usize>>,
}

// A conservative syntax gate excludes shapes whose rows do not directly represent one table.
fn supported(sql: &str) -> bool {
  let mut parser = tree_sitter::Parser::new();
  if parser
    .set_language(&tree_sitter_sequel::LANGUAGE.into())
    .is_err()
  {
    return false;
  }
  let Some(tree) = parser.parse(sql, None) else {
    return false;
  };
  if tree.root_node().has_error() {
    return false;
  }
  let mut pending = vec![tree.root_node()];
  let mut selects = 0;
  let mut relations = 0;
  while let Some(node) = pending.pop() {
    match node.kind() {
      "select" => selects += 1,
      "relation" => {
        relations += 1;
        let mut walk = node.walk();
        if !node
          .named_children(&mut walk)
          .any(|child| child.kind() == "object_reference")
        {
          return false;
        }
      }
      // Function calls can be aggregates or set-returning, even under an ordinary SELECT.
      "cte" | "subquery" | "set_operation" | "group_by" | "keyword_distinct" | "join"
      | "cross_join" | "lateral_join" | "lateral_cross_join" | "invocation" | "window_function"
      | "window_clause" | "keyword_into" | "keyword_for" => return false,
      _ => {}
    }
    let mut walk = node.walk();
    pending.extend(node.named_children(&mut walk));
  }
  selects == 1 && relations == 1
}

// Analyze only the statement already accepted for a cursor; prepare never executes it again.
pub(super) async fn attach(
  connection: &DatabaseConnection,
  sql: &str,
  page: &PageRef,
  autocommit: bool,
  result: &mut QueryResult,
) -> anyhow::Result<()> {
  result.read_only_reason = Some("Custom SQL results require a single-table SELECT without joins, grouping, DISTINCT, subqueries, or function calls".into());
  if !supported(sql) {
    return Ok(());
  }
  if !autocommit {
    result.read_only_reason = Some("Query results are read-only inside an explicit transaction; end the transaction and run the query again".into());
    return Ok(());
  }
  let statement = connection.run(connection.client.prepare(sql)).await?;
  let fields = statement.columns();
  anyhow::ensure!(
    fields
      .iter()
      .map(|field| field.name())
      .eq(result.columns.iter().map(String::as_str)),
    "Query metadata changed; run the query again"
  );
  let Some(oid) = fields.iter().find_map(|field| field.table_oid()) else {
    return Ok(());
  };
  if fields
    .iter()
    .any(|field| field.table_oid().is_some_and(|other| other != oid))
  {
    return Ok(());
  }
  let row = connection.run(connection.client.query_one(
    "SELECT n.nspname, c.relname, c.relkind::text, EXISTS (SELECT 1 FROM pg_catalog.pg_inherits WHERE inhparent = c.oid) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = $1", &[&oid]
  )).await?;
  let kind: String = row.get(2);
  // Ordinary inheritance does not enforce primary-key uniqueness across child tables.
  if !matches!(kind.as_str(), "r" | "p") || (kind == "r" && row.get::<_, bool>(3)) {
    result.read_only_reason = Some("Custom updates require a physical table; views and ordinary inheritance parents are read-only".into());
    return Ok(());
  }
  let table = TableRef {
    profile_id: page.profile_id.clone(),
    database: page.database.clone(),
    schema: row.get(0),
    name: row.get(1),
    kind: if kind == "p" {
      "partitioned table"
    } else {
      "table"
    }
    .into(),
  };
  let catalog = table_columns(connection, &table).await?;
  let attributes = connection.run(connection.client.query(
    "SELECT attnum FROM pg_catalog.pg_attribute WHERE attrelid = $1 AND attnum > 0 AND NOT attisdropped ORDER BY attnum", &[&oid]
  )).await?;
  anyhow::ensure!(
    catalog.len() == attributes.len(),
    "Table definition changed; run the query again"
  );
  let attribute_ids: Vec<i16> = attributes.iter().map(|row| row.get(0)).collect();
  let mut mapping = Vec::new();
  let mut columns = Vec::new();
  for field in fields {
    let index = field
      .column_id()
      .filter(|_| field.table_oid() == Some(oid))
      .and_then(|id| attribute_ids.iter().position(|attribute| *attribute == id));
    // Two aliases for the same column would allow contradictory edits to one value.
    if index.is_some() && mapping.contains(&index) {
      result.read_only_reason = Some(
        "The same source column appears more than once; select each column once to edit rows"
          .into(),
      );
      return Ok(());
    }
    columns.push(index.map_or_else(
      || ResultColumn {
        name: field.name().into(),
        type_name: String::new(),
        editable: false,
        insertable: false,
        primary_key: false,
      },
      |index| catalog[index].clone(),
    ));
    mapping.push(index);
  }
  let keys: Vec<_> = catalog
    .iter()
    .enumerate()
    .filter(|(_, column)| column.primary_key)
    .collect();
  if keys.is_empty() {
    result.read_only_reason =
      Some("This table has no primary key; query results are read-only".into());
    return Ok(());
  }
  let missing: Vec<_> = keys
    .iter()
    .filter(|(index, _)| !mapping.contains(&Some(*index)))
    .map(|(_, column)| column.name.as_str())
    .collect();
  if !missing.is_empty() {
    result.read_only_reason = Some(format!(
      "Include primary key column(s) {} to edit rows",
      missing.join(", ")
    ));
    return Ok(());
  }
  result.read_only_reason = None;
  result.source = Some(TableResultSource {
    table,
    columns,
    query: Some(Box::new(QuerySource {
      sql: sql.into(),
      execution_id: page.id,
      table_oid: oid,
      table_columns: catalog,
      attribute_ids,
      mapping,
    })),
  });
  Ok(())
}

// Revalidate the physical table and projection while the save transaction holds its table lock.
pub(super) async fn validate(
  connection: &DatabaseConnection,
  source: &TableResultSource,
) -> anyhow::Result<()> {
  let current = table_columns(connection, &source.table).await?;
  let Some(query) = &source.query else {
    anyhow::ensure!(
      current == source.columns,
      "Table definition changed; refresh before editing"
    );
    return Ok(());
  };
  let name = format!(
    "{}.{}",
    quote_identifier(&source.table.schema),
    quote_identifier(&source.table.name)
  );
  let row = connection
    .run(
      connection
        .client
        .query_one("SELECT $1::text::regclass::oid", &[&name]),
    )
    .await?;
  anyhow::ensure!(
    row.get::<_, u32>(0) == query.table_oid && current == query.table_columns,
    "Table definition changed; run the query again"
  );
  let attributes = connection.run(connection.client.query(
    "SELECT attnum FROM pg_catalog.pg_attribute WHERE attrelid = $1 AND attnum > 0 AND NOT attisdropped ORDER BY attnum", &[&query.table_oid]
  )).await?;
  anyhow::ensure!(
    attributes
      .iter()
      .map(|row| row.get::<_, i16>(0))
      .eq(query.attribute_ids.iter().copied()),
    "Table definition changed; run the query again"
  );
  anyhow::ensure!(
    query.mapping.len() == source.columns.len(),
    "Invalid query column mapping"
  );
  for (column, index) in source.columns.iter().zip(&query.mapping) {
    anyhow::ensure!(
      match index {
        Some(index) => current.get(*index) == Some(column),
        None => !column.editable && !column.primary_key && !column.insertable,
      },
      "Query columns changed; run the query again"
    );
  }
  anyhow::ensure!(
    current.iter().any(|column| column.primary_key)
      && current
        .iter()
        .enumerate()
        .filter(|(_, column)| column.primary_key)
        .all(|(index, _)| query.mapping.contains(&Some(index))),
    "Include the complete primary key to edit rows"
  );
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  // Unknown or row-combining shapes must fail closed, including disguised function sources.
  #[test]
  fn restricts_custom_update_query_shapes() {
    for sql in [
      "SELECT id, name AS label FROM public.items WHERE id > 2 ORDER BY name LIMIT 5",
      "SELECT i.*, id + 1 AS next FROM items i",
      "SELECT id, CASE WHEN name IS NULL THEN 'empty' ELSE name END FROM items",
    ] {
      assert!(supported(sql), "{sql}");
    }
    for sql in [
      "SELECT 1",
      "SELECT * FROM a, b",
      "SELECT * FROM a JOIN b USING(id)",
      "SELECT DISTINCT id FROM a",
      "SELECT id FROM a GROUP BY id",
      "SELECT id FROM a UNION SELECT id FROM b",
      "SELECT * FROM (SELECT * FROM a) q",
      "WITH q AS (SELECT * FROM a) SELECT * FROM q",
      "SELECT id, generate_series(1,2) FROM a",
      "SELECT id, count(*) OVER () FROM a",
      "SELECT * FROM generate_series(1,2) a",
      "SELECT id FROM a WHERE id IN (SELECT id FROM b)",
    ] {
      assert!(!supported(sql), "{sql}");
    }
  }
}
