//! Where cued keeps things (DESIGN.md §5, §7.4).
//!
//! - store + logs: `$XDG_DATA_HOME/cued` (default `~/.local/share/cued`), 0700
//! - daemon log: `daemon.log` in the data dir — where a backgrounded daemon's
//!   own diagnostics go, since nothing else is listening (§5.2)
//! - socket: `/run/user/<uid>/cued.sock` when that private runtime directory
//!   belongs to the user; otherwise `~/.local/share/cued/run/cued.sock`.
//!   `CUED_SOCKET_DIR` is an explicit override for isolated deployments/tests.
//! - config: `$XDG_CONFIG_HOME/cued/config.toml` (default `~/.config/…`)

use std::fs::DirBuilder;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub db_file: PathBuf,
    /// flock target — the authoritative single-instance guard (§5.2).
    pub lock_file: PathBuf,
    pub logs_dir: PathBuf,
    /// The daemon's own diagnostics (§5.2). Step output goes to per-attempt
    /// files under `logs_dir`; this is the daemon talking about itself.
    pub daemon_log: PathBuf,
    pub socket_file: PathBuf,
    pub config_file: PathBuf,
}

impl Paths {
    /// Resolve XDG data/config locations and the deterministic per-user
    /// socket location. Creates private directories; the daemon binds the
    /// socket and the store creates the DB.
    pub fn resolve() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;

        let data_dir = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("cued");

        let config_file = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("cued/config.toml");

        // Do not let a launcher's optional XDG_RUNTIME_DIR redirect clients
        // away from the installed daemon. Derive the standard runtime path
        // from uid, then use one stable home fallback when it is unavailable.
        // SAFETY: getuid is a read-only query of the current process identity.
        let uid = unsafe { libc::getuid() };
        let socket_dir = match std::env::var_os("CUED_SOCKET_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => {
                let runtime = PathBuf::from(format!("/run/user/{uid}"));
                match std::fs::symlink_metadata(&runtime) {
                    Ok(metadata)
                        if metadata.file_type().is_dir()
                            && metadata.uid() == uid
                            && metadata.permissions().mode() & 0o777 == 0o700 =>
                    {
                        runtime
                    }
                    Ok(_) => stable_socket_dir(&home),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        stable_socket_dir(&home)
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("checking runtime directory {}", runtime.display())
                        });
                    }
                }
            }
        };
        anyhow::ensure!(
            socket_dir.is_absolute(),
            "socket directory must be an absolute path"
        );
        let socket_file = socket_dir.join("cued.sock");

        let paths = Self {
            db_file: data_dir.join("cued.db"),
            lock_file: data_dir.join("cued.lock"),
            logs_dir: data_dir.join("logs"),
            daemon_log: data_dir.join("daemon.log"),
            data_dir,
            socket_file,
            config_file,
        };

        create_private_dir(&paths.data_dir)?;
        create_private_dir(&paths.logs_dir)?;
        if let Some(socket_dir) = paths.socket_file.parent() {
            create_private_dir(socket_dir)?;
            let metadata = std::fs::symlink_metadata(socket_dir)
                .with_context(|| format!("checking socket directory {}", socket_dir.display()))?;
            anyhow::ensure!(
                metadata.file_type().is_dir()
                    && metadata.uid() == uid
                    && metadata.permissions().mode() & 0o777 == 0o700,
                "socket directory {} must be an owned directory with mode 0700",
                socket_dir.display()
            );
        }

        Ok(paths)
    }

    /// Everything one run captured — what §10.2 removes when the run is
    /// pruned.
    pub fn run_log_dir(&self, job: crate::model::JobId, run: crate::model::RunId) -> PathBuf {
        self.job_log_dir(job).join(format!("r{}", run.0))
    }

    /// Everything one job captured, across all its runs.
    pub fn job_log_dir(&self, job: crate::model::JobId) -> PathBuf {
        self.logs_dir.join(job.to_string())
    }

    /// Per-attempt log file: logs/<job>/<run>/<step>.<attempt>.log (§2.1).
    ///
    /// The step id is reduced to a single safe path component on the way in.
    /// `validate` (§6.3) already rejects ids that would need it, but this is
    /// the mechanism rather than the policy: a graph stored before that
    /// check existed is still loaded and run, and a future front-end that
    /// skipped the gate shouldn't be able to write outside the logs
    /// directory. Both the writing and reading sides go through here, so
    /// they agree on the name whatever it started as.
    pub fn step_log(
        &self,
        job: crate::model::JobId,
        run: crate::model::RunId,
        step: &str,
        attempt: u32,
    ) -> PathBuf {
        self.run_log_dir(job, run)
            .join(format!("{}.{attempt}.log", safe_component(step)))
    }
}

/// One path component, guaranteed: anything outside `[A-Za-z0-9_-]` becomes
/// `_`, so the result can hold no separator and can't be `.` or `..`.
/// Collisions are possible in principle for ids §6.3 would have refused
/// anyway, and a collision is a far better outcome than an escape.
fn safe_component(raw: &str) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if mapped.is_empty() {
        "_".to_string()
    } else {
        mapped
    }
}

fn stable_socket_dir(home: &Path) -> PathBuf {
    home.join(".local/share/cued/run")
}

fn create_private_dir(dir: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{JobId, RunId};

    fn paths() -> Paths {
        Paths {
            data_dir: "/data".into(),
            db_file: "/data/cued.db".into(),
            lock_file: "/data/cued.lock".into(),
            logs_dir: "/data/logs".into(),
            daemon_log: "/data/daemon.log".into(),
            socket_file: "/run/cued.sock".into(),
            config_file: "/cfg.toml".into(),
        }
    }

    /// §2.1's log path is built from a user-given step id (§3.1). Before
    /// this was reduced to one component, an id of `../../..` walked the
    /// log file out of the data directory entirely — and a lexical
    /// `starts_with` check wouldn't have noticed, since it doesn't resolve
    /// `..`.
    #[test]
    fn a_step_id_cannot_walk_out_of_its_run_directory() {
        let paths = paths();
        let run_dir = paths.run_log_dir(JobId(1), RunId(1));

        for nasty in ["../../../../tmp/pwned", "..", ".", "/etc/passwd", "a/b", ""] {
            let log = paths.step_log(JobId(1), RunId(1), nasty, 1);
            assert_eq!(
                log.parent(),
                Some(run_dir.as_path()),
                "step id {nasty:?} escaped to {}",
                log.display()
            );
            // No component of the name can be a traversal or a separator.
            let name = log.file_name().expect("a file name").to_string_lossy();
            assert!(!name.contains('/'), "{name:?}");
            assert!(!name.contains(".."), "{name:?}");
        }
    }

    /// Ordinary ids — the ones every front-end generates — must come
    /// through untouched, or `cued logs` would be reading a different file
    /// than the daemon wrote.
    #[test]
    fn ordinary_ids_are_left_alone() {
        let paths = paths();
        for id in ["run", "remind", "step1", "deploy-prod", "build_all"] {
            let log = paths.step_log(JobId(7), RunId(3), id, 2);
            assert_eq!(
                log,
                Path::new(&format!("/data/logs/j7/r3/{id}.2.log")),
                "{id} was rewritten"
            );
        }
    }
}
