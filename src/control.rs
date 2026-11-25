//! The per-user control directory: what is installed, whether it is paused, and
//! what the last runs did.
//!
//! Everything the background machinery needs outside the user's configuration
//! lives in one directory, so a single path describes the whole installation:
//!
//! | file | contents |
//! |---|---|
//! | `installed.toml` | [`Registration`]: the task's executable, its configuration layers, the requested and effective cadence, the owner |
//! | `control.toml` | [`Control`]: `paused` (an explicit stop) and `installing` (a commit in progress) |
//! | `health.json` | [`Health`]: the last completed attempt and the last success |
//! | `events.json` | up to [`MAX_EVENTS`] [`Event`]s, oldest first |
//! | `credentials.toml` | the fallback credential store (see [`crate::credentials`]) |
//! | `operation.lock` | held by any run that may talk to the portal |
//!
//! The directory is `service/` beside the system configuration file, so it does
//! not move when a portable configuration is used from another directory; a
//! second install by the same user replaces that user's task set rather than
//! adding one. `SRUN_PORTAL_STATE_DIR` overrides it for an isolated deployment or
//! a test, and the hidden `--control-dir` of the background entry point wins over
//! both, because the installed task must act on the directory it was installed
//! for.
//!
//! Every file is written through [`crate::persistence::atomic_write`], and the
//! three metadata files share one lock so a reader never sees a half-applied
//! install. A missing file means "the default": not installed, not paused, no
//! history. A file that exists but cannot be parsed is an error — silently
//! starting over would drop an operator's pause or invent an installation.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::ConfigError;
use crate::persistence::{self, FileLock};
use crate::settings::{self, ConfigSource};

/// The directory name inside the platform configuration directory.
pub const CONTROL_DIR_NAME: &str = "service";

/// The environment variable that relocates the whole control directory.
pub const ENV_STATE_DIR: &str = "SRUN_PORTAL_STATE_DIR";

/// How many events are kept; older ones are dropped first.
pub const MAX_EVENTS: usize = 200;

/// The record of an installed task set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub version: u32,
    /// The executable the platform's task runs.
    pub exe: PathBuf,
    /// The configuration layers the install read, so the task reads the same.
    pub source: ConfigSource,
    /// The cadence the user asked for.
    pub interval_secs: Option<u64>,
    /// What the platform can actually express (a minute is the finest Task
    /// Scheduler interval), reported next to the request.
    pub effective_interval_secs: Option<u64>,
    pub boot_delay_secs: Option<u64>,
    /// Who owns the task: the uid on Unix, the token SID on Windows.
    pub owner_id: String,
}

/// Whether background work is allowed to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Control {
    pub version: u32,
    /// Set by `disconnect`/`service stop`, cleared by `resume`/`service start`.
    pub paused: bool,
    /// Set while `service install` is committing platform changes.
    pub installing: bool,
}

impl Default for Control {
    fn default() -> Self {
        Control {
            version: 1,
            paused: false,
            installing: false,
        }
    }
}

/// The conclusion of one attempt. Only `reconnected`/`disconnected`/`online`
/// are network health; `busy` and `paused` are about scheduling, not reachability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Online,
    Reconnected,
    Disconnected,
    Paused,
    Busy,
    Failed,
}

impl Outcome {
    /// Whether this result says something about the network.
    pub fn is_network_health(self) -> bool {
        matches!(
            self,
            Outcome::Online | Outcome::Reconnected | Outcome::Disconnected
        )
    }
}

/// The class of a failure, so a script can branch without reading the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Config,
    Credentials,
    Network,
    Authentication,
    Protocol,
    Timeout,
    Platform,
}

/// One completed attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub schema_version: u32,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub result: Outcome,
    pub error_kind: Option<ErrorKind>,
    pub message: String,
    /// The `finished_at_ms` of the last attempt that ended with the device
    /// online, whatever happened since.
    pub last_success_at_ms: Option<u64>,
}

/// One recorded event: an attempt, or a control action such as an install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    /// An [`Outcome`] value, or one of the control values `installed`,
    /// `uninstalled`, `resumed`, `stopped`, `install_failed`.
    pub result: String,
    pub error_kind: Option<ErrorKind>,
    pub message: String,
}

/// A UTC timestamp in Unix milliseconds, the unit every record uses.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The control directory in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlDir {
    path: PathBuf,
}

impl ControlDir {
    /// Resolve the directory: an explicit `--control-dir`, then
    /// `SRUN_PORTAL_STATE_DIR`, then `service/` beside the system configuration.
    ///
    /// Nothing is created here; [`ControlDir::ensure`] does that, and only the
    /// commands that write call it.
    pub fn resolve(explicit: Option<&Path>) -> Result<ControlDir, ConfigError> {
        if let Some(path) = explicit {
            return Ok(ControlDir {
                path: absolute(path)?,
            });
        }
        if let Some(value) = std::env::var_os(ENV_STATE_DIR) {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(ConfigError::Invalid(format!(
                    "{ENV_STATE_DIR} must be an absolute path"
                )));
            }
            return Ok(ControlDir { path });
        }
        let config = settings::system_config_path()?;
        let parent = config
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(ControlDir {
            path: parent.join(CONTROL_DIR_NAME),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create the directory private to this user.
    pub fn ensure(&self) -> Result<(), ConfigError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(&self.path)
                .map_err(|err| ConfigError::Io(format!("{}: {err}", self.path.display())))?;
            let mode = std::fs::metadata(&self.path)
                .map_err(|err| ConfigError::Io(format!("{}: {err}", self.path.display())))?
                .permissions()
                .mode()
                & 0o777;
            if mode & 0o077 != 0 {
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o700))
                    .map_err(|err| ConfigError::Io(format!("{}: {err}", self.path.display())))?;
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(&self.path)
                .map_err(|err| ConfigError::Io(format!("{}: {err}", self.path.display())))
        }
    }

    pub fn installed_path(&self) -> PathBuf {
        self.path.join("installed.toml")
    }

    pub fn control_path(&self) -> PathBuf {
        self.path.join("control.toml")
    }

    pub fn health_path(&self) -> PathBuf {
        self.path.join("health.json")
    }

    pub fn events_path(&self) -> PathBuf {
        self.path.join("events.json")
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.path.join(crate::credentials::CREDENTIALS_FILE_NAME)
    }

    pub fn operation_lock_path(&self) -> PathBuf {
        self.path.join("operation")
    }

    /// Hold the lock every portal-talking run must have.
    pub fn lock_operation(&self) -> Result<FileLock, ConfigError> {
        self.ensure()?;
        FileLock::acquire(&self.operation_lock_path()).map_err(|err| {
            ConfigError::Io(format!("{}: {err}", self.operation_lock_path().display()))
        })
    }

    /// Take the operation lock only if no other process holds it. `Ok(None)`
    /// means a run is in progress.
    pub fn try_lock_operation(&self) -> Result<Option<FileLock>, ConfigError> {
        FileLock::try_acquire(&self.operation_lock_path()).map_err(|err| {
            ConfigError::Io(format!("{}: {err}", self.operation_lock_path().display()))
        })
    }

    /// Whether a run holds the operation lock, without creating anything.
    pub fn is_operation_held(&self) -> Result<bool, ConfigError> {
        persistence::is_locked(&self.operation_lock_path()).map_err(|err| {
            ConfigError::Io(format!("{}: {err}", self.operation_lock_path().display()))
        })
    }

    /// The lock that serialises changes to the metadata files. Never held while
    /// waiting for the operation lock.
    pub fn lock_metadata(&self) -> Result<FileLock, ConfigError> {
        self.ensure()?;
        FileLock::acquire(&self.control_path())
            .map_err(|err| ConfigError::Io(format!("{}: {err}", self.control_path().display())))
    }

    pub fn read_registration(&self) -> Result<Option<Registration>, ConfigError> {
        read_toml(&self.installed_path())
    }

    pub fn write_registration(&self, registration: &Registration) -> Result<(), ConfigError> {
        write_toml(&self.installed_path(), registration)
    }

    pub fn remove_registration(&self) -> Result<(), ConfigError> {
        match std::fs::remove_file(self.installed_path()) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(ConfigError::Io(format!(
                "{}: {err}",
                self.installed_path().display()
            ))),
        }
    }

    /// The pause/in-flight record; a missing file is the default (running, not
    /// installing).
    pub fn read_control(&self) -> Result<Control, ConfigError> {
        Ok(read_toml(&self.control_path())?.unwrap_or_default())
    }

    pub fn write_control(&self, control: &Control) -> Result<(), ConfigError> {
        write_toml(&self.control_path(), control)
    }

    /// Change the control record under the metadata lock, so two commands
    /// cannot clobber each other's field.
    pub fn update_control(
        &self,
        change: impl FnOnce(&mut Control),
    ) -> Result<Control, ConfigError> {
        let _lock = self.lock_metadata()?;
        let mut control = self.read_control()?;
        change(&mut control);
        self.write_control(&control)?;
        Ok(control)
    }

    pub fn read_health(&self) -> Result<Option<Health>, ConfigError> {
        read_json(&self.health_path())
    }

    /// Record the conclusion of one attempt, keeping the previous success time
    /// when this attempt was not itself a success.
    pub fn record_health(&self, mut health: Health) -> Result<Health, ConfigError> {
        let _lock = self.lock_metadata()?;
        let previous = self.read_health()?;
        if health.result.is_network_health() {
            health.last_success_at_ms = Some(health.finished_at_ms);
        } else if health.last_success_at_ms.is_none() {
            health.last_success_at_ms = previous.and_then(|h| h.last_success_at_ms);
        }
        write_json(&self.health_path(), &health)?;
        Ok(health)
    }

    pub fn read_events(&self) -> Result<Vec<Event>, ConfigError> {
        Ok(read_json::<Vec<Event>>(&self.events_path())?.unwrap_or_default())
    }

    /// Append one event, dropping the oldest beyond [`MAX_EVENTS`].
    pub fn append_event(&self, event: Event) -> Result<(), ConfigError> {
        let _lock = self.lock_metadata()?;
        let mut events = self.read_events()?;
        events.push(event);
        if events.len() > MAX_EVENTS {
            let excess = events.len() - MAX_EVENTS;
            events.drain(..excess);
        }
        write_json(&self.events_path(), &events)
    }
}

fn absolute(path: &Path) -> Result<PathBuf, ConfigError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|dir| dir.join(path))
        .map_err(|err| ConfigError::Io(format!("cannot resolve {}: {err}", path.display())))
}

fn read_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(ConfigError::Io(format!("{}: {err}", path.display()))),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|err| ConfigError::Format {
            path: path.to_path_buf(),
            detail: err.to_string(),
        })
}

fn write_toml<T: Serialize>(path: &Path, value: &T) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(value).map_err(|err| ConfigError::Format {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })?;
    persistence::atomic_write(path, text.as_bytes(), false)
        .map_err(|err| ConfigError::Io(format!("{}: {err}", path.display())))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(ConfigError::Io(format!("{}: {err}", path.display()))),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| ConfigError::Format {
            path: path.to_path_buf(),
            detail: err.to_string(),
        })
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), ConfigError> {
    let mut text = serde_json::to_string_pretty(value).map_err(|err| ConfigError::Format {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })?;
    text.push('\n');
    persistence::atomic_write(path, text.as_bytes(), false)
        .map_err(|err| ConfigError::Io(format!("{}: {err}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "srun-portal-control-{tag}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn control(dir: &TempDir) -> ControlDir {
        ControlDir {
            path: dir.path.join("state"),
        }
    }

    fn health(result: Outcome, finished: u64) -> Health {
        Health {
            schema_version: 1,
            started_at_ms: finished.saturating_sub(100),
            finished_at_ms: finished,
            result,
            error_kind: None,
            message: String::new(),
            last_success_at_ms: None,
        }
    }

    #[test]
    fn missing_files_mean_defaults() {
        let dir = TempDir::new("defaults");
        let control = control(&dir);
        assert_eq!(control.read_control().unwrap(), Control::default());
        assert_eq!(control.read_registration().unwrap(), None);
        assert_eq!(control.read_health().unwrap(), None);
        assert!(control.read_events().unwrap().is_empty());
        assert!(!control.path().exists(), "reading creates nothing");
    }

    #[test]
    fn registration_round_trips() {
        let dir = TempDir::new("registration");
        let control = control(&dir);
        let registration = Registration {
            version: 1,
            exe: PathBuf::from("/opt/srun-portal"),
            source: ConfigSource::single("/home/u/.config/srun-portal/srun-portal.toml"),
            interval_secs: Some(300),
            effective_interval_secs: Some(300),
            boot_delay_secs: Some(30),
            owner_id: "1000".to_string(),
        };
        control.write_registration(&registration).unwrap();
        assert_eq!(control.read_registration().unwrap(), Some(registration));

        control.remove_registration().unwrap();
        assert_eq!(control.read_registration().unwrap(), None);
        control.remove_registration().unwrap();
    }

    #[test]
    fn control_and_health_track_the_last_attempt_and_last_success() {
        let dir = TempDir::new("health");
        let control = control(&dir);

        let first = control
            .record_health(health(Outcome::Online, 1_000))
            .unwrap();
        assert_eq!(first.last_success_at_ms, Some(1_000));

        // A later failure keeps the earlier success.
        let mut failed = health(Outcome::Failed, 2_000);
        failed.error_kind = Some(ErrorKind::Network);
        let failed = control.record_health(failed).unwrap();
        assert_eq!(failed.last_success_at_ms, Some(1_000));
        assert_eq!(control.read_health().unwrap().unwrap(), failed);

        // A new success moves it.
        let again = control
            .record_health(health(Outcome::Reconnected, 3_000))
            .unwrap();
        assert_eq!(again.last_success_at_ms, Some(3_000));
    }

    #[test]
    fn events_are_bounded_and_ordered() {
        let dir = TempDir::new("events");
        let control = control(&dir);
        for i in 0..(MAX_EVENTS + 5) {
            control
                .append_event(Event {
                    schema_version: 1,
                    started_at_ms: i as u64,
                    finished_at_ms: i as u64,
                    result: "installed".to_string(),
                    error_kind: None,
                    message: String::new(),
                })
                .unwrap();
        }
        let events = control.read_events().unwrap();
        assert_eq!(events.len(), MAX_EVENTS);
        assert_eq!(events.first().unwrap().started_at_ms, 5);
        assert_eq!(events.last().unwrap().started_at_ms, MAX_EVENTS as u64 + 4);
    }

    #[test]
    fn a_corrupt_record_is_an_error_not_a_reset() {
        let dir = TempDir::new("corrupt");
        let control = control(&dir);
        control.ensure().unwrap();
        std::fs::write(control.control_path(), "paused = \"yes\"\n").unwrap();
        assert!(control.read_control().is_err());
        std::fs::write(control.health_path(), "{").unwrap();
        assert!(control.read_health().is_err());
        std::fs::write(control.events_path(), "not json").unwrap();
        assert!(control.read_events().is_err());
    }

    #[test]
    fn the_directory_is_private_and_the_operation_lock_excludes_others() {
        let dir = TempDir::new("lock");
        let control = control(&dir);
        let held = control.lock_operation().unwrap();
        assert!(control.path().exists());
        assert!(
            control.try_lock_operation().unwrap().is_none(),
            "a second acquire must not succeed while one is held"
        );
        drop(held);
        assert!(control.try_lock_operation().unwrap().is_some());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(control.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn asking_whether_a_run_is_in_progress_creates_nothing() {
        let dir = TempDir::new("busy-check");
        let control = control(&dir);
        assert!(!control.is_operation_held().unwrap());
        assert!(!control.path().exists(), "no directory was created");

        let held = control.lock_operation().unwrap();
        assert!(control.is_operation_held().unwrap());
        drop(held);
        assert!(!control.is_operation_held().unwrap());
    }

    #[test]
    fn explicit_and_environment_directories_win() {
        let dir = TempDir::new("dirs-win");
        let explicit = dir.path.join("explicit-state");
        assert_eq!(
            ControlDir::resolve(Some(&explicit)).unwrap().path(),
            explicit.as_path()
        );

        let env = dir.path.join("env-state");
        // Set and cleared inside the test: no other test reads this variable.
        std::env::set_var(ENV_STATE_DIR, &env);
        let resolved = ControlDir::resolve(None).unwrap();
        std::env::remove_var(ENV_STATE_DIR);
        assert_eq!(resolved.path(), env.as_path());

        let default = ControlDir::resolve(None).unwrap();
        assert!(
            default.path().ends_with(CONTROL_DIR_NAME),
            "{:?}",
            default.path()
        );
    }
}
