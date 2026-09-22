mod app;
mod db;
mod sql_editor;
mod storage;
mod theme;
mod ui;

use std::{io, sync::mpsc, time::Duration};

use anyhow::Context;
use app::App;
use ratatui::crossterm::{
  event::{
    self, Event, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
  },
  execute,
};
use storage::Storage;

// Starts the async database runtime and gives terminal ownership to Ratatui.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
  let storage = Storage::discover().context("could not locate the Textgres data directory")?;
  let profiles = storage
    .load_connections()
    .context("could not load saved connections")?;
  let scripts = storage
    .list_scripts()
    .context("could not list saved SQL scripts")?;
  // Read preferences before terminal setup so invalid configuration has a visible error.
  let settings = storage.load_settings().context("could not load settings")?;
  let (database_tx, database_rx) = mpsc::channel();
  let runtime = tokio::runtime::Handle::current();
  let mut app = App::new(storage, profiles, scripts, runtime, database_tx);
  app.settings = settings;

  let _keyboard_enhancement = KeyboardEnhancementGuard::enable();
  let result = ratatui::run(|terminal| run(terminal, &mut app, database_rx));
  // Restore the terminal first, then let pending SQL reach a known cancellation outcome.
  let shutdown_result = app.shutdown().await;
  result?;
  shutdown_result?;
  Ok(())
}

/// Enables modified Enter reporting and restores the terminal mode on every normal exit.
struct KeyboardEnhancementGuard;

impl KeyboardEnhancementGuard {
  fn enable() -> Self {
    // Unsupported terminals ignore this protocol; F5 remains the portable run key.
    let _ = execute!(
      io::stdout(),
      PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    Self
  }
}

impl Drop for KeyboardEnhancementGuard {
  fn drop(&mut self) {
    let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
  }
}

// Keeps terminal input synchronous while PostgreSQL work runs on Tokio workers.
fn run(
  terminal: &mut ratatui::DefaultTerminal,
  app: &mut App,
  database_rx: mpsc::Receiver<db::Response>,
) -> io::Result<()> {
  let mut dirty = true;
  while !app.should_quit {
    while let Ok(response) = database_rx.try_recv() {
      app.handle_database_response(response);
      dirty = true;
    }
    // Surface idle connection loss without requiring another query or keypress.
    dirty |= app.poll_sessions();

    // Avoid redrawing an unchanged terminal while still polling worker responses.
    if dirty {
      terminal.draw(|frame| ui::draw(frame, app))?;
      dirty = false;
    }

    if event::poll(Duration::from_millis(50))? {
      match event::read()? {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
          app.handle_key(key);
          dirty = true;
        }
        // Ratatui updates its size on draw, even when no key follows the resize.
        Event::Resize(_, _) => dirty = true,
        _ => {}
      }
    }
  }

  Ok(())
}
