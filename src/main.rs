//! The `cued` binary: parse the CLI, then hand off to either the daemon
//! (`cued daemon`) or the thin client (everything else). The big picture
//! lives here; the details live in the library modules (see src/lib.rs).

use anyhow::Result;
use clap::Parser;

use cued::cli::{Cli, Command};

fn main() -> Result<()> {
    // Rust ignores SIGPIPE, so a closed pipe surfaces as an EPIPE write
    // error and `println!` panics — `cued logs j7 | head` would print a
    // panic instead of exiting. §10.3's scripting surface and the raw,
    // pipeable output `cued logs` goes out of its way to produce both
    // assume the ordinary unix behaviour, so restore it.
    //
    // SAFETY: setting a signal disposition to the system default, before
    // any threads exist.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    match cli.command {
        Command::Mcp => cued::mcp::run(),
        Command::Daemon(args) => cued::daemon::run(args),
        command => cued::client::run(command),
    }
}
