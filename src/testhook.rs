//! Deterministic fault points for real-process tests (DESIGN.md §11).
//!
//! Compiled to nothing unless the `test-hooks` feature is on, so a release
//! build without that feature carries no active hooks. With the feature, a harness that sets
//! `CUED_TEST_HOOK_DIR` can hold an attempt at a named point in its
//! execution — between the store claim and the registry entry, just before
//! the spawn, after the process exits but before its outcome is durable —
//! and do something there (cancel, pause, retry, kill the daemon) that a
//! real race would only hit by luck.
//!
//! Protocol, all plain files in that directory:
//!   - `<point>.hold` arms the point; without it the point is a no-op.
//!   - On arrival the attempt writes `<point>-j<job>-r<run>-a<attempt>.reached`
//!     (holding the daemon's pid) and waits.
//!   - The harness releases it by creating the same name with `.release`.

use crate::model::{JobId, RunId};

#[cfg(feature = "test-hooks")]
pub async fn checkpoint(point: &str, job: JobId, run: RunId, attempt: u32) {
    use std::path::PathBuf;
    use std::time::Duration;

    let Some(dir) = std::env::var_os("CUED_TEST_HOOK_DIR").map(PathBuf::from) else {
        return;
    };
    if !dir.join(format!("{point}.hold")).exists() {
        return;
    }
    let tag = format!("{point}-j{}-r{}-a{attempt}", job.0, run.0);
    let _ = std::fs::write(
        dir.join(format!("{tag}.reached")),
        std::process::id().to_string(),
    );
    eprintln!("cued: test hook holding at {tag}");
    while !dir.join(format!("{tag}.release")).exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    eprintln!("cued: test hook released {tag}");
}

#[cfg(not(feature = "test-hooks"))]
#[inline(always)]
pub async fn checkpoint(_point: &str, _job: JobId, _run: RunId, _attempt: u32) {}
