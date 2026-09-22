// Pending table edits stay local to their source workspace until an explicit batch save.
use super::*;
use std::collections::BTreeSet;

// Original rows support exact discard and optimistic concurrency checks; new rows follow them.
#[derive(Default)]
pub struct TableEdits {
  original: Option<Vec<Vec<Option<String>>>>,
  pub new_defaults: Vec<Vec<bool>>,
  pub deleted: BTreeSet<usize>,
  pub uncertain: bool,
}

impl TableEdits {
  // Keep fetched originals before pending inserts so row indexes and discard remain correct.
  pub(super) fn append_page(
    &mut self,
    result: &mut QueryResult,
    rows: Vec<Vec<Option<String>>>,
    selected: &mut usize,
  ) {
    if let Some(original) = &mut self.original {
      let boundary = original.len();
      if *selected >= boundary && !self.new_defaults.is_empty() {
        *selected += rows.len();
      }
      original.extend(rows.iter().cloned());
      result.rows.splice(boundary..boundary, rows);
    } else {
      result.rows.extend(rows);
    }
  }
  // Capture before the first local change, not after the edited values reach the result model.
  fn begin(&mut self, result: &QueryResult) {
    self.original.get_or_insert_with(|| result.rows.clone());
  }

  // New row positions are stable because only other appended rows can be removed locally.
  pub fn defaults(&self, row: usize) -> Option<&Vec<bool>> {
    row
      .checked_sub(self.original.as_ref()?.len())
      .and_then(|index| self.new_defaults.get(index))
  }

  // Count rows rather than cells so the title matches the unit of batch validation.
  pub fn count(&self, result: &QueryResult) -> usize {
    self.original.as_ref().map_or(0, |original| {
      original
        .iter()
        .enumerate()
        .filter(|(index, values)| {
          self.deleted.contains(index) || result.rows.get(*index) != Some(*values)
        })
        .count()
    }) + self.new_defaults.len()
  }

  // Only changed existing cells are yellow; insertion/deletion colors take precedence in the UI.
  pub fn cell_changed(&self, result: &QueryResult, row: usize, column: usize) -> bool {
    self
      .original
      .as_ref()
      .and_then(|rows| rows.get(row))
      .is_some_and(|original| {
        original.get(column) != result.rows.get(row).and_then(|values| values.get(column))
      })
  }

  // Delete before insert permits replacing a key; any ordering constraint failure rolls back all rows.
  fn changes(&self, result: &QueryResult) -> Vec<db::RowChange> {
    let Some(original) = &self.original else {
      return Vec::new();
    };
    let mut changes: Vec<_> = self
      .deleted
      .iter()
      .map(|index| db::RowChange::Delete {
        original: original[*index].clone(),
      })
      .collect();
    for (index, values) in original.iter().enumerate() {
      if !self.deleted.contains(&index) && result.rows[index] != *values {
        changes.push(db::RowChange::Update {
          original: values.clone(),
          values: result.rows[index].clone(),
        });
      }
    }
    for (index, defaults) in self.new_defaults.iter().enumerate() {
      changes.push(db::RowChange::Insert {
        values: result.rows[original.len() + index].clone(),
        defaults: defaults.clone(),
      });
    }
    changes
  }

  // An uncertain COMMIT cannot be resolved by discarding the local diff; refresh is still required.
  fn discard(&mut self, result: &mut QueryResult) {
    if let Some(original) = self.original.take() {
      result.rows = original;
    }
    self.new_defaults.clear();
    self.deleted.clear();
  }
}

impl App {
  // Edits cannot race an operation or be retried after an unknown COMMIT outcome.
  fn editable_source(&self) -> Result<db::TableResultSource, String> {
    if self.workspace.database_task.is_some() {
      return Err("Wait for this workspace's operation to finish".into());
    }
    if self.workspace.edits.uncertain {
      return Err("Save outcome is unknown; refresh and verify the table first".into());
    }
    if let Some(reason) = &self.workspace.result.read_only_reason {
      return Err(reason.clone());
    }
    let source = self
      .workspace
      .result
      .source
      .clone()
      .ok_or("Select a table preview or a query with its full primary key to edit rows")?;
    if !matches!(source.table.kind.as_str(), "table" | "partitioned table") {
      return Err("This object is read-only".into());
    }
    // A retained query result cannot write after disconnect or inside a user transaction.
    if source.query.is_some()
      && !matches!(
        self.workspace.session_state,
        db::SessionState::Connected(db::TransactionState::Idle | db::TransactionState::Paging)
      )
    {
      return Err(
        "Reconnect or end the SQL transaction, then run the query again before editing".into(),
      );
    }
    Ok(source)
  }

  // New columns start at DEFAULT, which preserves identity, generated values, and server defaults.
  pub(super) fn add_row(&mut self) -> Result<(), String> {
    let source = self.editable_source()?;
    // Custom projections support updates only; use a table preview for inserts and deletes.
    if source.query.is_some() {
      return Err("Custom query results support updates only".into());
    }

    self.workspace.edits.begin(&self.workspace.result);
    self
      .workspace
      .result
      .rows
      .push(vec![None; source.columns.len()]);
    self
      .workspace
      .edits
      .new_defaults
      .push(vec![true; source.columns.len()]);
    self.workspace.result_row = self.workspace.result.rows.len() - 1;
    self.open_row_detail();
    self.set_status(
      "New row staged; Ctrl+S in the row editor stages values, Ctrl+S in Results saves all changes"
        .into(),
      false,
    );
    Ok(())
  }

  // Existing rows stay visible in red; deleting a new row simply removes the pending insert.
  pub(super) fn delete_row(&mut self) -> Result<(), String> {
    let source = self.editable_source()?;
    // Custom projections support updates only; use a table preview for inserts and deletes.
    if source.query.is_some() {
      return Err("Custom query results support updates only".into());
    }

    let row = self.workspace.result_row;
    if self.workspace.result.rows.get(row).is_none() {
      return Err("Select a row to delete".into());
    }
    if self.workspace.edits.defaults(row).is_some() {
      let original_len = self.workspace.edits.original.as_ref().unwrap().len();
      self.workspace.edits.new_defaults.remove(row - original_len);
      self.workspace.result.rows.remove(row);
      self.workspace.result_row = row.min(self.workspace.result.rows.len().saturating_sub(1));
    } else {
      if !source.columns.iter().any(|column| column.primary_key) {
        return Err("Deletion requires a primary key".into());
      }
      self.workspace.edits.begin(&self.workspace.result);
      if !self.workspace.edits.deleted.remove(&row) {
        self.workspace.edits.deleted.insert(row);
      }
    }
    self.set_status(
      "Deletion staged; Delete toggles an existing row's deletion".into(),
      false,
    );
    Ok(())
  }

  // Row forms stage only local values; the Results save command owns the transaction boundary.
  pub(super) fn stage_row(&mut self, form: &RowDetail) -> Result<(), String> {
    if form.source != self.workspace.result.source {
      return Err("The table preview is no longer current; reload the table before editing".into());
    }
    // A viewer opened during paging can retain an old new-row index after originals are appended.
    if form.read_only {
      return Err("Close and reopen this row before editing".into());
    }
    let source = self.editable_source()?;
    if form.source.as_ref() != Some(&source)
      || self.workspace.result.rows.get(form.row_index) != Some(&form.original)
    {
      return Err("The table preview is no longer current; reload the table before editing".into());
    }
    if self.workspace.edits.deleted.contains(&form.row_index) {
      return Err("Undo this row's deletion before modifying it".into());
    }
    let is_new = self.workspace.edits.defaults(form.row_index).is_some();
    if !is_new && !source.columns.iter().any(|column| column.primary_key) {
      return Err("Modifying rows requires a primary key".into());
    }
    if form.values.len() != source.columns.len() || form.defaults.len() != source.columns.len() {
      return Err("Row does not match table metadata".into());
    }
    for (index, column) in source.columns.iter().enumerate() {
      if is_new {
        if !column.insertable && !form.defaults[index] {
          return Err(format!(
            "Column {} requires its generated default",
            column.name
          ));
        }
      } else if !column.editable && form.values[index] != form.original[index] {
        return Err(format!("Column {} is read-only", column.name));
      }
    }
    self.workspace.edits.begin(&self.workspace.result);
    self.workspace.result.rows[form.row_index] = form.values.clone();
    if is_new {
      let original_len = self.workspace.edits.original.as_ref().unwrap().len();
      self.workspace.edits.new_defaults[form.row_index - original_len] = form.defaults.clone();
    }
    self.set_status(
      "Row staged; Ctrl+S in Results saves all changes".into(),
      false,
    );
    Ok(())
  }

  // Snapshot the complete batch so later navigation cannot change its database or values.
  pub(super) fn save_changes(&mut self) -> Result<(), String> {
    let source = self.editable_source()?;
    let changes = self.workspace.edits.changes(&self.workspace.result);
    if changes.is_empty() {
      return Err("No table changes to save".into());
    }
    let profile = self
      .profile(&source.table.profile_id)
      .cloned()
      .ok_or("The source connection no longer exists")?;
    if let Some(query) = &source.query {
      self.workspace.refresh_query = Some((source.table.clone(), query.sql.clone()));
      self.workspace.refresh_table = None;
    } else {
      self.workspace.refresh_table = Some(source.table.clone());
    }
    self.dispatch(
      format!("Saving {} row change(s)", changes.len()),
      Request::SaveChanges {
        password: profile.password.clone(),
        profile,
        source,
        changes,
      },
    );
    Ok(())
  }

  // Discard restores the exact preview and never issues database writes.
  pub(super) fn discard_changes(&mut self) {
    if self.workspace.database_task.is_some() {
      self.set_status(
        "Wait for the running operation before discarding changes".into(),
        true,
      );
      return;
    }
    self.workspace.edits.discard(&mut self.workspace.result);
    self.workspace.result_row = self
      .workspace
      .result_row
      .min(self.workspace.result.rows.len().saturating_sub(1));
    if self.workspace.edits.uncertain {
      self.set_status(
        "Local changes discarded; save outcome is still unknown. Refresh and verify the table"
          .into(),
        true,
      );
    } else {
      self.set_status("Table changes discarded".into(), false);
    }
  }

  // Refresh explicitly reruns the original source after resolving pending or uncertain edits.
  pub(super) fn refresh_results(&mut self, confirmed: bool) -> Result<(), String> {
    if self.workspace.database_task.is_some() {
      return Err("Wait for this workspace's operation to finish".into());
    }
    // This is an explicit rerun; saving never executes custom SQL a second time.
    if let Some((table, sql)) = self.workspace.refresh_query.clone() {
      if !confirmed
        && (self.workspace.edits.count(&self.workspace.result) > 0
          || self.workspace.edits.uncertain)
      {
        self.workspace.overlay = Some(Overlay::ConfirmRefresh);
        return Ok(());
      }
      let profile = self
        .profile(&table.profile_id)
        .cloned()
        .ok_or("The source connection no longer exists")?;
      self.dispatch(
        "Rerunning original query".into(),
        Request::Query {
          profile,
          database: table.database,
          sql,
        },
      );
      return Ok(());
    }
    let table = self
      .workspace
      .result
      .source
      .as_ref()
      .map(|source| source.table.clone())
      .or_else(|| self.workspace.refresh_table.clone())
      .ok_or("Refresh is available for table previews; run custom SQL explicitly")?;
    if !confirmed && self.workspace.edits.count(&self.workspace.result) > 0 {
      self.workspace.overlay = Some(Overlay::ConfirmRefresh);
      return Ok(());
    }
    let profile = self
      .profile(&table.profile_id)
      .cloned()
      .ok_or("The source connection no longer exists")?;
    // The diff is replaced only on success, so a failed/cancelled refresh loses no pending values.
    self.dispatch(
      format!("Refreshing {}.{}", table.schema, table.name),
      Request::Preview {
        password: profile.password.clone(),
        profile,
        table,
      },
    );
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // Batch generation must retain loaded originals, skip updates for deleted rows, and preserve NULL/default flags.
  #[test]
  fn batch_uses_original_values_and_orders_deletes_before_inserts() {
    let mut result = QueryResult {
      rows: vec![vec![Some("first".into())], vec![Some("second".into())]],
      ..Default::default()
    };
    let mut edits = TableEdits::default();
    edits.begin(&result);
    result.rows[0][0] = Some("edited then deleted".into());
    edits.deleted.insert(0);
    result.rows[1][0] = Some("changed".into());
    result.rows.push(vec![None]);
    result.rows.push(vec![None]);
    edits.new_defaults = vec![vec![false], vec![true]];
    assert_eq!(edits.count(&result), 4);
    assert_eq!(
      edits.changes(&result),
      vec![
        db::RowChange::Delete {
          original: vec![Some("first".into())]
        },
        db::RowChange::Update {
          original: vec![Some("second".into())],
          values: vec![Some("changed".into())]
        },
        db::RowChange::Insert {
          values: vec![None],
          defaults: vec![false]
        },
        db::RowChange::Insert {
          values: vec![None],
          defaults: vec![true]
        },
      ]
    );
  }
}
