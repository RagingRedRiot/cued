//! Persistence backends (DESIGN.md §8): how the daemon starts and survives,
//! chosen by the operator at `cued setup`. Orthogonal to *what uid it runs
//! as* (always the invoking user — the §7.2 structural safety).
//!
//! Setup must probe before offering (§8.2) — linger state, `systemctl
//! --user` reachability, `crontab` presence — and teardown must be
//! symmetric with install.
//!
//! The §8.1 three-way is two backends: `SystemdUser` carries linger as a
//! property rather than being two types, because the unit it installs is
//! identical either way — linger is a grant on the *user*, not a different
//! way to start the daemon.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

/// Everything we write to a user's crontab lives between these, so teardown
/// can be surgical about a file we don't own (§8.2). Two markers rather than
/// one because guessing how many lines after a single marker are ours is how
/// you eat somebody's `@daily backup`.
const CRON_BEGIN: &str = "# >>> cued (managed by `cued setup`) >>>";
const CRON_END: &str = "# <<< cued <<<";

const UNIT_NAME: &str = "cued.service";

pub trait PersistenceBackend {
    fn name(&self) -> &'static str;
    /// The §8.1 tradeoff in one line — what `cued setup` offers the user.
    fn tradeoff(&self) -> &'static str;
    /// Is this backend usable on this host at all (§8.2 probing)?
    fn available(&self) -> Availability;
    fn status(&self) -> Result<BackendStatus>;
    /// Returns notes worth showing the user (a linger grant that needs an
    /// admin, say) — an install can succeed and still have something to say.
    fn install(&self, exe: &Path) -> Result<Vec<String>>;
    /// Symmetric with install (§8.2).
    fn uninstall(&self) -> Result<Vec<String>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendStatus {
    Installed,
    NotInstalled,
}

/// §8.2: an unusable backend must say *why*, so the user picks from reality
/// rather than discovering the problem halfway through an install.
///
/// `Caveat` is the state that matters for honesty: installable, but weaker
/// than the backend's headline claim. A scheduler that says "survives
/// reboot" when it might not is worse than one that says "I can't tell" —
/// the whole value of durability is being able to stop thinking about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available,
    Caveat(String),
    Unavailable(String),
}

impl Availability {
    /// Can this be installed at all? A caveat is a warning, not a refusal —
    /// the user may well know their own host better than our probe does.
    pub fn is_available(&self) -> bool {
        !matches!(self, Self::Unavailable(_))
    }
}

/// `systemd --user` (± linger): supervised — restart-on-crash, journal (§8.1).
#[derive(Debug, Clone, Copy)]
pub struct SystemdUser {
    /// Ask for `loginctl enable-linger`, which is what makes the unit
    /// survive logout. Polkit-gated, so it may need an admin once (§8.1).
    pub linger: bool,
}

/// user cron `@reboot`: zero-root background persistence, no supervision (§8.1).
#[derive(Debug, Clone, Copy)]
pub struct CronReboot;

// ---------------------------------------------------------------------------
// systemd --user
// ---------------------------------------------------------------------------

impl PersistenceBackend for SystemdUser {
    fn name(&self) -> &'static str {
        if self.linger {
            "systemd --user + linger"
        } else {
            "systemd --user"
        }
    }

    fn tradeoff(&self) -> &'static str {
        if self.linger {
            "survives logout and reboot; supervised (restart-on-crash, journal). \
             Linger may need an admin once."
        } else {
            "supervised, but dies at logout. No root, ever."
        }
    }

    fn available(&self) -> Availability {
        if !in_path("systemctl") {
            return Availability::Unavailable("systemctl is not on PATH".into());
        }
        // The binary existing proves nothing — what matters is whether this
        // session can reach a user bus to talk to.
        match Command::new("systemctl")
            .args(["--user", "show", "--property=Version"])
            .output()
        {
            Ok(out) if out.status.success() => Availability::Available,
            Ok(out) => Availability::Unavailable(format!(
                "systemctl --user is not reachable ({})",
                first_line(&String::from_utf8_lossy(&out.stderr))
            )),
            Err(error) => Availability::Unavailable(format!("running systemctl: {error}")),
        }
    }

    fn status(&self) -> Result<BackendStatus> {
        Ok(if unit_path()?.exists() {
            BackendStatus::Installed
        } else {
            BackendStatus::NotInstalled
        })
    }

    fn install(&self, exe: &Path) -> Result<Vec<String>> {
        let path = unit_path()?;
        let dir = path.parent().expect("unit path has a parent");
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&path, unit_text(exe))
            .with_context(|| format!("writing {}", path.display()))?;

        let mut notes = vec![format!("wrote {}", path.display())];
        systemctl(&["--user", "daemon-reload"])?;
        systemctl(&["--user", "enable", "--now", UNIT_NAME])?;
        notes.push(format!("enabled and started {UNIT_NAME}"));

        if self.linger {
            match enable_linger() {
                Ok(()) => notes.push("linger enabled — the daemon survives logout".into()),
                // §8.1: polkit may refuse. That's a provisioning grant the
                // user can't self-serve on some distros, not a broken
                // install — the unit is in place and works while logged in.
                Err(error) => notes.push(format!(
                    "could not enable linger ({error}). The unit is installed and \
                     runs while you're logged in; for reboot survival ask an admin \
                     to run: sudo loginctl enable-linger {}",
                    username()
                )),
            }
        }
        Ok(notes)
    }

    fn uninstall(&self) -> Result<Vec<String>> {
        let path = unit_path()?;
        let mut notes = Vec::new();
        // Disable before removing the file, or systemd has nothing to read.
        if path.exists() {
            let _ = systemctl(&["--user", "disable", "--now", UNIT_NAME]);
            std::fs::remove_file(&path)
                .with_context(|| format!("removing {}", path.display()))?;
            let _ = systemctl(&["--user", "daemon-reload"]);
            notes.push(format!("disabled {UNIT_NAME} and removed {}", path.display()));
        }
        // Linger is deliberately left alone: it is a grant on the user
        // account, not something cued owns, and other user services may be
        // relying on it. Revoking it here would be reaching outside our own
        // install (§8.2 says symmetric, not greedy).
        if linger_enabled() {
            notes.push(format!(
                "left linger enabled — it's an account-level setting other user \
                 services may need; disable with: loginctl disable-linger {}",
                username()
            ));
        }
        Ok(notes)
    }
}

/// A path as one `ExecStart` word.
///
/// systemd splits the command on whitespace, so an unquoted path containing
/// any would make `argv[0]` a prefix of itself and the unit fail to start.
/// Two layers apply, and in this order: specifier expansion (`%x`) runs over
/// the value first, then the result is split and unquoted. So `%` doubles to
/// survive the first layer, and `\` and `"` are escaped for the second.
fn systemd_quote(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{escaped}\"").replace('%', "%%")
}

/// A path as one word of a crontab command.
///
/// The command is handed to `/bin/sh`, so the path is single-quoted the
/// usual way. But cron reads the line first, and there `%` means *newline* —
/// everything past the first unescaped one becomes the command's stdin. So
/// `%` is escaped for cron *after* the shell quoting, because cron consumes
/// the backslash before `sh` ever sees the token.
fn cron_quote(path: &Path) -> String {
    let quoted = format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    quoted.replace('%', "\\%")
}

/// The unit (§8.1): `--foreground` because systemd is the thing capturing
/// stderr here — the daemon's own log file (§5.2) exists for the case where
/// nothing else is listening, and duplicating into both would be worse than
/// either.
fn unit_text(exe: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=cued — durable per-user scheduling\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} daemon --foreground\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        systemd_quote(exe)
    )
}

fn unit_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
            .join(".config"),
    };
    Ok(unit_path_in(&base))
}

/// Split from `unit_path` so the layout is testable without mutating the
/// process environment out from under other tests.
fn unit_path_in(config_home: &Path) -> PathBuf {
    config_home.join("systemd/user").join(UNIT_NAME)
}

fn systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("running systemctl {}", args.join(" ")))?;
    ensure!(
        output.status.success(),
        "systemctl {} failed: {}",
        args.join(" "),
        first_line(&String::from_utf8_lossy(&output.stderr))
    );
    Ok(())
}

fn enable_linger() -> Result<()> {
    ensure!(in_path("loginctl"), "loginctl is not on PATH");
    let output = Command::new("loginctl")
        .args(["enable-linger", &username()])
        .output()
        .context("running loginctl enable-linger")?;
    ensure!(
        output.status.success(),
        "{}",
        first_line(&String::from_utf8_lossy(&output.stderr))
    );
    Ok(())
}

/// §8.2's probe: is logout survival already granted on this host?
pub fn linger_enabled() -> bool {
    if !in_path("loginctl") {
        return false;
    }
    Command::new("loginctl")
        .args(["show-user", &username(), "--property=Linger"])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "Linger=yes")
}

// ---------------------------------------------------------------------------
// cron @reboot
// ---------------------------------------------------------------------------

impl PersistenceBackend for CronReboot {
    fn name(&self) -> &'static str {
        "cron @reboot"
    }

    fn tradeoff(&self) -> &'static str {
        "survives logout and reboot with zero root involvement; \
         no supervision (no restart-on-crash, no journal)."
    }

    /// `crontab` being on PATH proves only that *a* cron exists. `@reboot`
    /// is a Vixie extension — POSIX specifies the five-field format and
    /// nothing else — so the presence of the command says nothing about
    /// whether the line we install will ever fire. Identify the flavour
    /// before promising reboot survival.
    fn available(&self) -> Availability {
        if which("crontab").is_none() {
            return Availability::Unavailable("crontab is not on PATH".into());
        }
        availability_for(cron_flavor())
    }

    fn status(&self) -> Result<BackendStatus> {
        Ok(if read_crontab()?.contains(CRON_BEGIN) {
            BackendStatus::Installed
        } else {
            BackendStatus::NotInstalled
        })
    }

    fn install(&self, exe: &Path) -> Result<Vec<String>> {
        let current = read_crontab()?;
        write_crontab(&crontab_with_entry(&current, exe))?;
        let mut notes = vec![format!(
            "added an @reboot line to your crontab for {}",
            exe.display()
        )];
        // Nothing here can prove @reboot fires; only a reboot can. Say how
        // to check, rather than leaving the user to discover it the hard way
        // — via a job that didn't run.
        if !matches!(cron_flavor(), CronFlavor::RebootCapable(_)) {
            notes.push(
                "this cron's @reboot support could not be confirmed — after your next reboot, \
                 check the daemon came up: `cued list` should answer without \
                 printing the ad-hoc warning"
                    .into(),
            );
        }
        Ok(notes)
    }

    fn uninstall(&self) -> Result<Vec<String>> {
        let current = read_crontab()?;
        if !current.contains(CRON_BEGIN) {
            return Ok(Vec::new());
        }
        let stripped = crontab_without_entry(&current);
        if stripped.trim().is_empty() {
            // Our block was the whole file. Writing back an empty one would
            // leave the user with a crontab they didn't have before; remove
            // it instead, which is what "symmetric" means (§8.2).
            remove_crontab()?;
        } else {
            write_crontab(&stripped)?;
        }
        Ok(vec!["removed the @reboot line from your crontab".into()])
    }
}

/// Our block, appended to whatever was already there. Idempotent: an
/// existing block is replaced rather than duplicated, so re-running setup
/// after moving the binary does the right thing.
fn crontab_with_entry(existing: &str, exe: &Path) -> String {
    let mut out = crontab_without_entry(existing);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(CRON_BEGIN);
    out.push('\n');
    out.push_str(&format!("@reboot {} daemon\n", cron_quote(exe)));
    out.push_str(CRON_END);
    out.push('\n');
    out
}

/// Everything between our markers, markers included, and nothing else —
/// this edits a file full of the user's own jobs.
fn crontab_without_entry(existing: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in existing.lines() {
        if line.trim() == CRON_BEGIN {
            inside = true;
            continue;
        }
        if line.trim() == CRON_END {
            inside = false;
            continue;
        }
        if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// An empty crontab is not an error: `crontab -l` exits non-zero saying "no
/// crontab for <user>", which is a perfectly good answer to "what's in it".
fn read_crontab() -> Result<String> {
    let output = Command::new("crontab")
        .arg("-l")
        .output()
        .context("running crontab -l")?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("no crontab") {
        return Ok(String::new());
    }
    bail!("crontab -l failed: {}", first_line(&stderr))
}

fn write_crontab(text: &str) -> Result<()> {
    use std::io::Write;
    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running crontab -")?;
    child
        .stdin
        .take()
        .context("crontab stdin")?
        .write_all(text.as_bytes())
        .context("writing the new crontab")?;
    let output = child.wait_with_output().context("waiting for crontab")?;
    ensure!(
        output.status.success(),
        "crontab - failed: {}",
        first_line(&String::from_utf8_lossy(&output.stderr))
    );
    Ok(())
}

/// Which cron is on this host, to the extent it can be told from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CronFlavor {
    /// Documents `@reboot`: vixie-cron, cronie, dcron, fcron.
    RebootCapable(&'static str),
    /// BusyBox. Its `@` shortcut table varies by build and by the
    /// `FEATURE_CROND_SPECIAL_TIMES` config, so `@reboot` may be accepted
    /// by `crontab` and then simply never fire.
    BusyBox,
    Unknown,
}

fn cron_flavor() -> CronFlavor {
    // BusyBox is almost always installed as a symlink farm pointing at one
    // multi-call binary, so resolving the command names it.
    if let Some(crontab) = which("crontab") {
        let resolved = crontab.canonicalize().unwrap_or(crontab);
        if resolved
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains("busybox"))
        {
            return CronFlavor::BusyBox;
        }
    }
    // Otherwise identify by the daemon shipped alongside. All of these
    // document @reboot; none of them is busybox, which we just excluded.
    for (path, name) in [
        ("/usr/sbin/cron", "vixie cron"),
        ("/usr/sbin/crond", "cronie or dcron"),
        ("/usr/sbin/fcron", "fcron"),
        ("/sbin/cron", "vixie cron"),
        ("/sbin/crond", "cronie or dcron"),
    ] {
        if Path::new(path).is_file() {
            return CronFlavor::RebootCapable(name);
        }
    }
    CronFlavor::Unknown
}

/// Split out so the judgement is testable without a host that has each cron
/// installed on it.
fn availability_for(flavor: CronFlavor) -> Availability {
    match flavor {
        CronFlavor::RebootCapable(_) => Availability::Available,
        CronFlavor::BusyBox => Availability::Caveat(
            "BusyBox crond — whether @reboot fires depends on how it was built, \
             and an unsupported line is accepted silently rather than rejected"
                .into(),
        ),
        CronFlavor::Unknown => Availability::Caveat(
            "couldn't identify this cron — @reboot is a Vixie extension, not POSIX, \
             so reboot survival isn't guaranteed here"
                .into(),
        ),
    }
}

/// `crontab -r` on an already-absent crontab is an error we don't care
/// about — the desired state is "gone" either way.
fn remove_crontab() -> Result<()> {
    let output = Command::new("crontab")
        .arg("-r")
        .output()
        .context("running crontab -r")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    ensure!(
        output.status.success() || stderr.contains("no crontab"),
        "crontab -r failed: {}",
        first_line(&stderr)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

/// The path a backend should start. Canonicalized, because both a unit file
/// and a crontab line outlive the shell that had this on its PATH.
pub fn daemon_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the cued binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    // Both backends are line-oriented — a unit file directive and a crontab
    // entry are each one line — so a newline in the path can't be escaped
    // into either, it can only corrupt the file. Refuse rather than write
    // something that would be read as two directives.
    ensure!(
        !exe.to_string_lossy().chars().any(|c| c.is_control()),
        "the cued binary's path contains a control character ({:?});          a unit file and a crontab line are both one line and can't carry it",
        exe.display().to_string()
    );
    Ok(exe)
}

/// §5.2: the ad-hoc-daemon warning is only worth printing when nothing is
/// supervising. Cheapest check first — a file stat before any subprocess.
pub fn any_installed() -> bool {
    if unit_path().is_ok_and(|path| path.exists()) {
        return true;
    }
    CronReboot.status().unwrap_or(BackendStatus::NotInstalled) == BackendStatus::Installed
}

fn username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| unsafe { libc::getuid() }.to_string())
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

fn in_path(program: &str) -> bool {
    which(program).is_some()
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crontab_entry_is_added_without_touching_other_jobs() {
        let existing = "# my stuff\n0 3 * * * /usr/bin/backup\n";
        let updated = crontab_with_entry(existing, Path::new("/usr/local/bin/cued"));

        assert!(updated.starts_with("# my stuff\n0 3 * * * /usr/bin/backup\n"));
        assert!(updated.contains("@reboot '/usr/local/bin/cued' daemon"));
        assert!(updated.contains(CRON_BEGIN) && updated.contains(CRON_END));
    }

    #[test]
    fn install_is_idempotent_and_teardown_is_exact() {
        let existing = "0 3 * * * /usr/bin/backup\n";
        let once = crontab_with_entry(existing, Path::new("/opt/cued"));
        // Re-running setup replaces our block rather than stacking another.
        let twice = crontab_with_entry(&once, Path::new("/opt/cued"));
        assert_eq!(once, twice);
        assert_eq!(twice.matches("@reboot").count(), 1);

        // A moved binary rewrites in place.
        let moved = crontab_with_entry(&once, Path::new("/usr/bin/cued"));
        assert!(moved.contains("@reboot '/usr/bin/cued' daemon"));
        assert!(!moved.contains("/opt/cued"));

        // §8.2: teardown returns the file to exactly what it was.
        assert_eq!(crontab_without_entry(&moved), existing);
    }

    #[test]
    fn teardown_leaves_an_untouched_crontab_alone() {
        let theirs = "@reboot /usr/bin/something-else\n0 9 * * 1 /usr/bin/weekly\n";
        assert_eq!(crontab_without_entry(theirs), theirs);
    }

    #[test]
    fn crontab_without_a_trailing_newline_still_gets_a_clean_block() {
        // `crontab -l` output is normally newline-terminated, but a
        // hand-edited file need not be, and gluing our marker onto the end
        // of someone's job line would corrupt it.
        let updated = crontab_with_entry("0 3 * * * /usr/bin/backup", Path::new("/opt/cued"));
        assert!(updated.contains("/usr/bin/backup\n# >>> cued"), "{updated}");
    }

    #[test]
    fn only_an_identified_cron_gets_to_promise_reboot_survival() {
        // The bug this guards: `crontab` on PATH was taken as proof that
        // @reboot works, so an unsupported cron would have been reported
        // "available", installed, and silently never fired.
        assert_eq!(
            availability_for(CronFlavor::RebootCapable("vixie cron")),
            Availability::Available
        );
        for unproven in [CronFlavor::BusyBox, CronFlavor::Unknown] {
            let availability = availability_for(unproven);
            assert!(
                matches!(availability, Availability::Caveat(_)),
                "{unproven:?} must not claim reboot survival outright"
            );
            // Still installable — the user may know their host better than
            // our probe does; they just shouldn't be told it's guaranteed.
            assert!(availability.is_available(), "{unproven:?} should stay offerable");
        }
    }

    #[test]
    fn an_absent_tool_is_unavailable_not_merely_caveated() {
        let missing = Availability::Unavailable("crontab is not on PATH".into());
        assert!(!missing.is_available());
    }

    /// codex #11: both generators interpolated the executable path raw. A
    /// path with a space made systemd's `argv[0]` a prefix of itself and the
    /// unit fail to start; cron handed the same split to `sh`.
    #[test]
    fn a_path_with_spaces_stays_one_word_in_both_backends() {
        let spacey = Path::new("/home/riot/my apps/cued");

        let unit = unit_text(spacey);
        assert!(
            unit.contains(r#"ExecStart="/home/riot/my apps/cued" daemon --foreground"#),
            "{unit}"
        );

        let tab = crontab_with_entry("", spacey);
        assert!(tab.contains("@reboot '/home/riot/my apps/cued' daemon"), "{tab}");
    }

    /// The characters each format treats specially, which are not the same
    /// characters. `%` is the interesting one: it is systemd's specifier
    /// sigil *and* cron's newline, so it needs escaping in both — differently.
    #[test]
    fn each_format_escapes_what_it_treats_as_special() {
        // systemd: the quoting layer needs backslash and double-quote
        // escaped, and `%` doubles because specifier expansion runs first.
        assert_eq!(systemd_quote(Path::new("/opt/cued")), r#""/opt/cued""#);
        assert_eq!(systemd_quote(Path::new("/opt/100%/cued")), r#""/opt/100%%/cued""#);
        assert_eq!(systemd_quote(Path::new(r#"/opt/a"b/cued"#)), r#""/opt/a\"b/cued""#);

        // cron: shell single-quoting, then `%` escaped for cron itself —
        // cron eats the backslash before `sh` ever sees the token.
        assert_eq!(cron_quote(Path::new("/opt/cued")), "'/opt/cued'");
        assert_eq!(cron_quote(Path::new("/opt/100%/cued")), r"'/opt/100\%/cued'");
        assert_eq!(cron_quote(Path::new("/opt/it's/cued")), r"'/opt/it'\''s/cued'");
    }

    /// Whatever the path, the block we write into somebody's crontab must
    /// still be one entry that teardown can find and remove exactly (§8.2).
    #[test]
    fn a_hostile_path_does_not_break_the_crontab_block() {
        let nasty = Path::new("/opt/we ird/100%/it's/cued");
        let existing = "0 3 * * * /usr/bin/backup\n";
        let tab = crontab_with_entry(existing, nasty);

        assert_eq!(tab.lines().filter(|l| l.starts_with("@reboot")).count(), 1);
        assert!(tab.starts_with(existing), "somebody else's job was disturbed");
        assert_eq!(crontab_without_entry(&tab), existing, "teardown must be exact");
    }

    #[test]
    fn the_unit_lands_where_systemd_looks_for_user_units() {
        assert_eq!(
            unit_path_in(Path::new("/home/x/.config")),
            Path::new("/home/x/.config/systemd/user/cued.service")
        );
    }

    #[test]
    fn the_unit_runs_the_daemon_in_the_foreground() {
        let text = unit_text(Path::new("/usr/local/bin/cued"));
        // §5.2: systemd captures stderr, so the daemon must not also be
        // redirecting it into its own log file.
        assert!(text.contains(r#"ExecStart="/usr/local/bin/cued" daemon --foreground"#));
        assert!(text.contains("Restart=on-failure"), "supervision is the point");
        assert!(text.contains("WantedBy=default.target"));
    }
}
