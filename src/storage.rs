use std::{
  collections::HashSet,
  env, fmt,
  fs::{self, OpenOptions},
  io::Write,
  path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Context, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const CONNECTIONS_FILE: &str = "connections.toml";
const SCRIPTS_DIRECTORY: &str = "scripts";

/// A reusable connection, including its optional saved password.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConnectionProfile {
  pub id: String,
  pub name: String,
  pub host: String,
  pub port: u16,
  pub database: String,
  pub user: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub password: Option<String>,
  #[serde(default)]
  pub require_tls: bool,
}

// Prevent diagnostics from copying a saved password into logs or panic output.
impl fmt::Debug for ConnectionProfile {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("ConnectionProfile")
      .field("id", &self.id)
      .field("name", &self.name)
      .field("host", &self.host)
      .field("port", &self.port)
      .field("database", &self.database)
      .field("user", &self.user)
      .field("password", &self.password.as_ref().map(|_| "<redacted>"))
      .field("require_tls", &self.require_tls)
      .finish()
  }
}

/// A saved SQL script and its editable contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Script {
  pub name: String,
  pub sql: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ConnectionFile {
  #[serde(default = "format_version")]
  version: u8,
  #[serde(default)]
  connections: Vec<ConnectionProfile>,
}

const fn format_version() -> u8 {
  2
}

/// Owns all durable paths so storage behavior stays separate from UI state.
#[derive(Clone, Debug)]
pub struct Storage {
  root: PathBuf,
}

impl Storage {
  /// Uses an override for tests and automation, then the platform data directory.
  pub fn discover() -> anyhow::Result<Self> {
    if let Some(root) = env::var_os("TEXTGRES_DATA_DIR") {
      return Self::new(root.into());
    }

    let directories = ProjectDirs::from("io", "doolin", "textgres")
      .context("the operating system did not provide a user data directory")?;
    Self::new(directories.data_local_dir().to_owned())
  }

  /// Creates the root early so later write errors occur at startup or save time.
  pub fn new(root: PathBuf) -> anyhow::Result<Self> {
    fs::create_dir_all(root.join(SCRIPTS_DIRECTORY))
      .with_context(|| format!("could not create {}", root.display()))?;
    Ok(Self { root })
  }

  /// Loads and validates the versioned connection file.
  pub fn load_connections(&self) -> anyhow::Result<Vec<ConnectionProfile>> {
    let path = self.root.join(CONNECTIONS_FILE);
    if !path.exists() {
      return Ok(Vec::new());
    }

    let source =
      fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))?;
    let file: ConnectionFile =
      toml::from_str(&source).with_context(|| format!("could not parse {}", path.display()))?;
    // Version 1 profiles had no password and deserialize with `None`.
    if !matches!(file.version, 1 | 2) {
      bail!(
        "unsupported connection format version {} in {}",
        file.version,
        path.display()
      );
    }

    let mut ids = HashSet::new();
    for profile in &file.connections {
      validate_profile(profile)?;
      if !ids.insert(&profile.id) {
        bail!("duplicate connection id {:?}", profile.id);
      }
    }
    Ok(file.connections)
  }

  /// Replaces the connection configuration in one owner-readable operation.
  pub fn save_connections(&self, profiles: &[ConnectionProfile]) -> anyhow::Result<()> {
    let mut ids = HashSet::new();
    for profile in profiles {
      validate_profile(profile)?;
      if !ids.insert(&profile.id) {
        bail!("duplicate connection id {:?}", profile.id);
      }
    }

    let file = ConnectionFile {
      version: format_version(),
      connections: profiles.to_vec(),
    };
    let encoded = toml::to_string_pretty(&file).context("could not encode connections")?;
    let path = self.root.join(CONNECTIONS_FILE);
    let temporary = self.root.join("connections.toml.tmp");
    write_private_file(&temporary, encoded.as_bytes())?;
    fs::rename(&temporary, &path)
      .with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
  }

  /// Returns stable, extension-free script names for the explorer.
  pub fn list_scripts(&self) -> anyhow::Result<Vec<String>> {
    let directory = self.root.join(SCRIPTS_DIRECTORY);
    let mut names = Vec::new();
    for entry in
      fs::read_dir(&directory).with_context(|| format!("could not read {}", directory.display()))?
    {
      let entry = entry?;
      let path = entry.path();
      if path.extension().and_then(|value| value.to_str()) == Some("sql")
        && let Some(stem) = path.file_stem().and_then(|value| value.to_str())
      {
        names.push(stem.to_owned());
      }
    }
    names.sort_unstable_by_key(|name| name.to_lowercase());
    Ok(names)
  }

  /// Loads one script after validating that its name cannot escape the script directory.
  pub fn load_script(&self, name: &str) -> anyhow::Result<Script> {
    validate_script_name(name)?;
    let path = self.script_path(name);
    let sql =
      fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))?;
    Ok(Script {
      name: name.to_owned(),
      sql,
    })
  }

  /// Saves UTF-8 SQL without adding or removing a final newline.
  pub fn save_script(&self, name: &str, sql: &str) -> anyhow::Result<()> {
    validate_script_name(name)?;
    let path = self.script_path(name);
    fs::write(&path, sql).with_context(|| format!("could not write {}", path.display()))
  }

  fn script_path(&self, name: &str) -> PathBuf {
    self
      .root
      .join(SCRIPTS_DIRECTORY)
      .join(format!("{name}.sql"))
  }

  #[cfg(test)]
  pub fn root(&self) -> &std::path::Path {
    &self.root
  }
}

// Saved passwords require stricter permissions than ordinary application data.
fn write_private_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
  let mut options = OpenOptions::new();
  options.write(true).create(true).truncate(true);
  #[cfg(unix)]
  options.mode(0o600);

  let mut file = options
    .open(path)
    .with_context(|| format!("could not write {}", path.display()))?;
  #[cfg(unix)]
  file
    .set_permissions(fs::Permissions::from_mode(0o600))
    .with_context(|| format!("could not secure {}", path.display()))?;
  file
    .write_all(contents)
    .with_context(|| format!("could not write {}", path.display()))?;
  Ok(())
}

fn validate_profile(profile: &ConnectionProfile) -> anyhow::Result<()> {
  if profile.id.trim().is_empty()
    || profile.name.trim().is_empty()
    || profile.host.trim().is_empty()
    || profile.database.trim().is_empty()
    || profile.user.trim().is_empty()
  {
    bail!("connection id, name, host, database, and user must not be empty");
  }
  if profile.port == 0 {
    bail!("connection port must be greater than zero");
  }
  Ok(())
}

fn validate_script_name(name: &str) -> anyhow::Result<()> {
  let valid = !name.is_empty()
    && name.len() <= 100
    && name
      .chars()
      .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
  if !valid {
    bail!("script names may contain only letters, numbers, '-' and '_'");
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn profile() -> ConnectionProfile {
    ConnectionProfile {
      id: "local".into(),
      name: "Local".into(),
      host: "localhost".into(),
      port: 5432,
      database: "postgres".into(),
      user: "postgres".into(),
      password: Some("secret".into()),
      require_tls: false,
    }
  }

  #[test]
  fn round_trips_profiles_with_passwords() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::new(directory.path().to_owned()).unwrap();

    storage.save_connections(&[profile()]).unwrap();

    assert_eq!(storage.load_connections().unwrap(), vec![profile()]);
    let source = fs::read_to_string(storage.root().join(CONNECTIONS_FILE)).unwrap();
    assert!(source.contains("password = \"secret\""));
    assert!(source.contains("version = 2"));
  }

  #[test]
  fn redacts_passwords_from_debug_output() {
    let output = format!("{:?}", profile());

    assert!(output.contains("<redacted>"));
    assert!(!output.contains("secret"));
  }

  #[cfg(unix)]
  #[test]
  fn restricts_connection_file_to_its_owner() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::new(directory.path().to_owned()).unwrap();

    storage.save_connections(&[profile()]).unwrap();

    let mode = fs::metadata(storage.root().join(CONNECTIONS_FILE))
      .unwrap()
      .permissions()
      .mode();
    assert_eq!(mode & 0o777, 0o600);
  }

  #[test]
  fn loads_version_one_profiles_without_passwords() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::new(directory.path().to_owned()).unwrap();
    fs::write(
      storage.root().join(CONNECTIONS_FILE),
      r#"version = 1

[[connections]]
id = "local"
name = "Local"
host = "localhost"
port = 5432
database = "postgres"
user = "postgres"
require_tls = false
"#,
    )
    .unwrap();

    let profiles = storage.load_connections().unwrap();

    assert_eq!(profiles[0].password, None);
  }

  #[test]
  fn rejects_script_path_traversal() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::new(directory.path().to_owned()).unwrap();

    assert!(storage.save_script("../secret", "select 1").is_err());
  }

  #[test]
  fn saves_and_lists_scripts() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::new(directory.path().to_owned()).unwrap();

    storage.save_script("inspect_users", "select 1;").unwrap();

    assert_eq!(storage.list_scripts().unwrap(), vec!["inspect_users"]);
    assert_eq!(
      storage.load_script("inspect_users").unwrap().sql,
      "select 1;"
    );
  }
}
