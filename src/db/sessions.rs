// Persistent SQL connections are isolated by profile and database, never pooled across sessions.
use super::*;
use tokio::sync::Mutex as AsyncMutex;

// Server-observed state also covers arbitrary SQL, errors, and cancelled transactions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionState {
  Idle,
  // A read-only cursor transaction is owned by tg and closes before the next SQL command.
  Paging,
  Open,
  Failed,
  Unknown,
}

// A disconnected or lost session requires an explicit user action to open again.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SessionState {
  #[default]
  Disconnected,
  Connected(TransactionState),
  Lost,
}

impl SessionState {
  // Present server state without implying that a failed transaction has been rolled back.
  pub fn label(self) -> &'static str {
    match self {
      Self::Disconnected => "disconnected",
      Self::Connected(TransactionState::Idle) => "autocommit",
      Self::Connected(TransactionState::Paging) => "result cursor open",
      Self::Connected(TransactionState::Open) => "transaction open",
      Self::Connected(TransactionState::Failed) => "transaction failed · ROLLBACK required",
      Self::Connected(TransactionState::Unknown) => "transaction state unknown",
      Self::Lost => "session lost · reconnect required",
    }
  }

  // Unknown state must never allow an implicit rollback through disconnect or exit.
  pub fn needs_confirmation(self) -> bool {
    matches!(
      self,
      Self::Connected(
        TransactionState::Open | TransactionState::Failed | TransactionState::Unknown
      )
    )
  }
}

// The map lock covers lookup only; each session has its own operation lock.
#[derive(Clone, Default)]
pub struct SessionManager {
  entries: Arc<Mutex<SessionEntries>>,
  previews: paging::Previews,
}

// Each entry serializes only its own connection, not work on other databases.
type SessionEntries = HashMap<(String, String), Arc<AsyncMutex<Session>>>;

// The immutable profile prevents an existing socket from being relabeled as a new endpoint.
struct Session {
  profile: ConnectionProfile,
  connection: Option<DatabaseConnection>,
  pid: i32,
  state: SessionState,
  cursor: Option<paging::Cursor>,
  // Only the current result may write through this exact SQL session.
  edit_source: Option<TableResultSource>,
}

impl Session {
  // Keep custom updates on the originating connection and retire committed snapshots.
  async fn save_query(
    &mut self,
    profile: &ConnectionProfile,
    source: &TableResultSource,
    changes: &[RowChange],
    cancellation: &Arc<Cancellation>,
  ) -> anyhow::Result<Output> {
    anyhow::ensure!(
      &self.profile == profile && self.edit_source.as_ref() == Some(source),
      "Query result is no longer current; run the query again"
    );
    anyhow::ensure!(
      matches!(
        self.state,
        SessionState::Connected(TransactionState::Idle | TransactionState::Paging)
      ),
      "End the SQL transaction and run the query again before editing"
    );
    if let Some(cursor) = self.cursor.take() {
      anyhow::ensure!(
        cursor.owns_transaction,
        "Cannot save inside a user transaction"
      );
      cursor
        .close(self.connection.as_ref().expect("connected session"))
        .await?;
    }
    let connection = self
      .connection
      .as_mut()
      .ok_or_else(|| anyhow::anyhow!("SQL session lost; reconnect and run the query again"))?;
    connection.cancellation = cancellation.clone();
    let saved = changes::save(connection, source, changes).await;
    // Never retry a committed or uncertain write with the old row snapshot.
    if saved.is_ok()
      || saved
        .as_ref()
        .is_err_and(|error| error.is::<UnknownCommit>())
    {
      self.edit_source = None;
    }
    saved?;
    Ok(Output::QuerySaved)
  }

  // Idle network loss must invalidate the session even without another SQL execution.
  fn check_health(&mut self) {
    if self.connection.as_ref().is_some_and(|connection| {
      connection.client.is_closed() || connection.broken.load(Ordering::SeqCst)
    }) {
      self.connection = None;
      self.cursor = None;
      self.edit_source = None;
      self.state = SessionState::Lost;
    }
  }
}

impl SessionManager {
  // Return state without blocking the terminal on a running database operation.
  pub fn states(&self) -> Vec<((String, String), SessionState)> {
    let entries = self
      .entries
      .lock()
      .unwrap_or_else(|error| error.into_inner());
    entries
      .iter()
      .filter_map(|(key, entry)| {
        let mut session = entry.try_lock().ok()?;
        session.check_health();
        Some((key.clone(), session.state))
      })
      .collect()
  }

  // Profile edits cannot close a live transaction or retarget an existing session.
  pub fn invalidate_profile(&self, profile_id: &str) -> anyhow::Result<()> {
    let mut entries = self
      .entries
      .lock()
      .map_err(|_| anyhow::anyhow!("SQL session state is unavailable"))?;
    for ((id, _), entry) in entries.iter().filter(|((id, _), _)| id == profile_id) {
      let mut session = entry
        .try_lock()
        .map_err(|_| anyhow::anyhow!("Wait for the running operation on {id}"))?;
      session.check_health();
      anyhow::ensure!(
        session.connection.is_none(),
        "Disconnect all SQL sessions for this profile before changing or deleting it"
      );
    }
    entries.retain(|(id, _), _| id != profile_id);
    self.previews.clear(Some(profile_id));
    Ok(())
  }

  // Shutdown occurs only after the UI resolves open-transaction confirmation.
  pub fn close_all(&self) {
    self
      .entries
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .clear();
    self.previews.clear(None);
  }

  // Expansion establishes the SQL session; metadata still uses a separate connection.
  pub(super) async fn execute(
    &self,
    request: Request,
    tunnels: &TunnelManager,
    cancellation: &Arc<Cancellation>,
  ) -> (anyhow::Result<Output>, Option<SessionState>) {
    // Table cursors have their own connections and never borrow the SQL session transaction.
    match &request {
      // Disconnect releases idle preview cursors too; already loaded rows stay in the UI.
      Request::Disconnect { profile, database } => self.previews.remove(&profile.id, database),
      Request::Preview {
        profile,
        password,
        table,
      } => {
        if let Err(error) = self
          .close_result_cursor(&table.profile_id, &table.database)
          .await
        {
          return (Err(error), None);
        }
        return (
          self
            .previews
            .start(
              profile,
              password.as_deref(),
              table.clone(),
              tunnels,
              cancellation,
            )
            .await
            .map(Output::Result),
          None,
        );
      }
      Request::SaveChanges {
        profile,
        password,
        source,
        changes,
      } if source.query.is_none() => {
        self
          .previews
          .remove(&source.table.profile_id, &source.table.database);
        // Preserve the committed outcome even if opening its fresh cursor fails.
        let result = async {
          self
            .close_result_cursor(&source.table.profile_id, &source.table.database)
            .await?;
          let connection = connect(
            profile,
            password.as_deref(),
            &source.table.database,
            tunnels,
            cancellation,
          )
          .await?;
          changes::save(&connection, source, changes).await?;
          Ok(Output::Saved(
            self
              .previews
              .start_connected(connection, source.table.clone())
              .await,
          ))
        }
        .await;
        return (result, None);
      }
      Request::FetchPage { page } if page.preview => {
        return (
          self
            .previews
            .fetch(page, cancellation)
            .await
            .map(|result| Output::Page {
              requested: page.clone(),
              result,
            }),
          None,
        );
      }
      Request::Query {
        profile, database, ..
      } => self.previews.remove(&profile.id, database),
      _ => {}
    }
    let (profile_id, database) = match &request {
      Request::FetchPage { page } => (page.profile_id.clone(), page.database.clone()),
      Request::SaveChanges { source, .. } => (
        source.table.profile_id.clone(),
        source.table.database.clone(),
      ),
      Request::Query {
        profile, database, ..
      }
      | Request::Connect {
        profile, database, ..
      } => (profile.id.clone(), database.clone()),
      Request::Disconnect { profile, database }
      | Request::Schemas {
        profile, database, ..
      } => (profile.id.clone(), database.clone()),
      Request::Databases { profile, .. } => (profile.id.clone(), profile.database.clone()),
      _ => return (super::execute(request, tunnels, cancellation).await, None),
    };
    let (entry, first) = {
      let Ok(mut entries) = self.entries.lock() else {
        return (
          Err(anyhow::anyhow!("SQL session state is unavailable")),
          None,
        );
      };
      let key = (profile_id, database.clone());
      if let Some(entry) = entries.get(&key) {
        (entry.clone(), false)
      } else {
        if matches!(
          request,
          Request::FetchPage { .. } | Request::SaveChanges { .. }
        ) {
          return (
            Err(anyhow::anyhow!(
              "SQL result cursor is closed; run the query again"
            )),
            None,
          );
        }
        let profile = match &request {
          Request::Query { profile, .. }
          | Request::Connect { profile, .. }
          | Request::Disconnect { profile, .. }
          | Request::Databases { profile, .. }
          | Request::Schemas { profile, .. } => profile.clone(),
          _ => unreachable!("only session requests reach this map"),
        };
        let entry = Arc::new(AsyncMutex::new(Session {
          profile,
          connection: None,
          pid: 0,
          state: SessionState::Disconnected,
          cursor: None,
          edit_source: None,
        }));
        entries.insert(key, entry.clone());
        // A first query may connect lazily, but subsequent failures require explicit reconnect.
        (entry, true)
      }
    };
    Self::run(entry, request, &database, tunnels, cancellation, first).await
  }

  // Replacing SQL results with a table preview releases only tg's cursor, never user work.
  async fn close_result_cursor(&self, profile_id: &str, database: &str) -> anyhow::Result<()> {
    let entry = self
      .entries
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .get(&(profile_id.into(), database.into()))
      .cloned();
    if let Some(entry) = entry {
      let mut session = entry
        .try_lock()
        .map_err(|_| anyhow::anyhow!("Wait for the SQL session's operation to finish"))?;
      session.check_health();
      session.edit_source = None;
      if let Some(cursor) = session.cursor.take()
        && let Some(connection) = &session.connection
      {
        let result = cursor.close(connection).await;
        session.check_health();
        result?;
        if cursor.owns_transaction {
          session.state = SessionState::Connected(TransactionState::Idle);
        }
      }
    }
    Ok(())
  }

  // Holding only this session's lock permits other databases to execute concurrently.
  async fn run(
    entry: Arc<AsyncMutex<Session>>,
    request: Request,
    database: &str,
    tunnels: &TunnelManager,
    cancellation: &Arc<Cancellation>,
    first: bool,
  ) -> (anyhow::Result<Output>, Option<SessionState>) {
    let Ok(mut session) = entry.try_lock() else {
      return (
        Err(anyhow::anyhow!(
          "This SQL session already has an active operation"
        )),
        None,
      );
    };
    session.check_health();
    let result = async {
      ensure_not_cancelled(&cancellation.requested)?;
      if let Request::Disconnect { .. } = request {
        session.cursor = None;
        session.edit_source = None;
        session.connection = None;
        session.state = SessionState::Disconnected;
        return Ok(Output::Session);
      }
      if let Request::FetchPage { page } = &request {
        let Some(cursor) = session.cursor.take() else {
          anyhow::bail!("SQL result cursor is closed; run the query again");
        };
        if cursor.page != *page {
          session.cursor = Some(cursor);
          anyhow::bail!("SQL result cursor was replaced; run the query again");
        }
        let connection = session
          .connection
          .as_mut()
          .ok_or_else(|| anyhow::anyhow!("SQL session lost; reconnect and run the query again"))?;
        connection.cancellation = cancellation.clone();
        let fetched = cursor.fetch(connection).await;
        let result = cursor.finish(connection, fetched).await;
        if result.as_ref().is_ok_and(|result| result.page.is_some()) {
          session.cursor = Some(cursor);
        }
        return result.map(|result| Output::Page {
          requested: page.clone(),
          result,
        });
      }
      if let Request::SaveChanges {
        profile,
        source,
        changes,
        ..
      } = &request
      {
        return session
          .save_query(profile, source, changes, cancellation)
          .await;
      }
      let (profile, explicit, reconnect) = match &request {
        Request::Query { profile, .. }
        | Request::Databases { profile, .. }
        | Request::Schemas { profile, .. } => (profile, false, false),
        Request::Connect {
          profile, reconnect, ..
        } => (profile, true, *reconnect),
        _ => unreachable!("only SQL and lifecycle requests reach a session"),
      };
      anyhow::ensure!(
        &session.profile == profile,
        "Connection settings changed; disconnect and reopen the session"
      );
      if reconnect {
        session.cursor = None;
        session.edit_source = None;
        session.connection = None;
        session.state = SessionState::Disconnected;
      }
      if session.connection.is_none() {
        anyhow::ensure!(
          first || explicit,
          "SQL session is disconnected or lost; use Connect or Reconnect explicitly"
        );
        let connection = connect(
          profile,
          profile.password.as_deref(),
          database,
          tunnels,
          cancellation,
        )
        .await?;
        session.pid = connection
          .run(connection.client.query_one("SELECT pg_backend_pid()", &[]))
          .await?
          .get(0);
        session.connection = Some(connection);
        session.state = SessionState::Connected(TransactionState::Idle);
      }
      // Even failed replacement SQL retires the old result's write authority.
      if matches!(request, Request::Query { .. }) {
        session.edit_source = None;
      }
      // Close our earlier cursor before user SQL, including BEGIN/COMMIT/ROLLBACK.
      if matches!(request, Request::Query { .. })
        && let Some(cursor) = session.cursor.take()
      {
        let connection = session
          .connection
          .as_mut()
          .expect("connection was just established");
        connection.cancellation = cancellation.clone();
        cursor.close(connection).await?;
        if cursor.owns_transaction {
          session.state = SessionState::Connected(TransactionState::Idle);
        }
      }
      let state = session.state;
      let connection = session
        .connection
        .as_mut()
        .expect("connection was just established");
      // Cancellation applies to one operation, not all later work in the same session.
      connection.cancellation = cancellation.clone();
      match request {
        Request::Query { sql, profile, .. } => {
          // Unknown/failed transactions retain the original execution path; never guess ownership.
          let statement = if matches!(
            state,
            SessionState::Connected(TransactionState::Idle | TransactionState::Open)
          ) {
            paging::select_statement(&sql)
          } else {
            None
          };
          if let Some(statement) = statement {
            let cursor = paging::Cursor::new(
              &profile.id,
              database,
              false,
              state == SessionState::Connected(TransactionState::Idle),
            );
            let result = async {
              cursor.open(connection, &statement).await?;
              let mut result = cursor.fetch(connection).await?;
              query_edits::attach(
                connection,
                &statement,
                &cursor.page,
                cursor.owns_transaction,
                &mut result,
              )
              .await?;
              Ok(result)
            }
            .await;
            let result = cursor.finish(connection, result).await?;
            session.edit_source = result.source.clone();
            if result.page.is_some() {
              session.cursor = Some(cursor);
            }
            Ok(Output::Result(result))
          } else {
            Ok(Output::Result(run_query(connection, &sql).await?))
          }
        }
        // Browsing must work even when the retained SQL connection has a failed transaction.
        Request::Databases { .. } | Request::Schemas { .. } => {
          super::execute(request, tunnels, cancellation).await
        }
        _ => Ok(Output::Session),
      }
    }
    .await;
    session.check_health();
    if session.connection.is_some() {
      // Do not issue probes in the SQL connection: an aborted transaction must remain untouched.
      let state = observe_transaction(&session.profile, database, session.pid, tunnels).await;
      session.state = SessionState::Connected(
        if state == TransactionState::Open
          && session
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.owns_transaction)
        {
          TransactionState::Paging
        } else {
          state
        },
      );
      session.check_health();
    }
    (result, Some(session.state))
  }
}

// PostgreSQL reports the real state, including read-only transactions and multi-statement scripts.
async fn observe_transaction(
  profile: &ConnectionProfile,
  database: &str,
  pid: i32,
  tunnels: &TunnelManager,
) -> TransactionState {
  let observe = async {
    let observer = connect(
      profile,
      profile.password.as_deref(),
      database,
      tunnels,
      &Arc::new(Cancellation::default()),
    )
    .await?;
    loop {
      let row = observer
        .client
        .query_opt(
          "SELECT state FROM pg_catalog.pg_stat_activity WHERE pid = $1",
          &[&pid],
        )
        .await?;
      let state = row.and_then(|row| row.get::<_, Option<String>>(0));
      match state.as_deref() {
        Some("idle") => return Ok::<_, anyhow::Error>(TransactionState::Idle),
        Some("idle in transaction") => return Ok(TransactionState::Open),
        Some("idle in transaction (aborted)") => return Ok(TransactionState::Failed),
        Some("active") => tokio::time::sleep(Duration::from_millis(10)).await,
        _ => return Ok(TransactionState::Unknown),
      }
    }
  };
  tokio::time::timeout(Duration::from_secs(2), observe)
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or(TransactionState::Unknown)
}
