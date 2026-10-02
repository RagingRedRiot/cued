//! cued — durable per-user scheduling of one-off and recurring commands,
//! reminders, and multi-step workflows.
//!
//! Module map (each maps onto a DESIGN.md section):
//!
//! - [`model`]     — the canonical types: Job/Run/Step/Transition/Schedule (§2–§4)
//! - [`schedule`]  — next-fire computation: Every arithmetic, Calendar walk (§4.1, §9)
//! - [`store`]     — SQLite via sqlx; source of truth (§5.3)
//! - [`proto`]     — the CLI↔daemon wire protocol (§5.1)
//! - [`daemon`]    — the event loop: heap, tick, reconciliation (§3.4, §4.2, §5.2)
//! - [`client`]    — the thin CLI side: connect, auto-spawn, render (§5.2)
//! - [`cli`]       — clap surface (§6.1)
//! - [`submit`]    — TOML → canonical graph desugaring + validation (§6.2–§6.3)
//! - [`timeparse`] — the time grammar: instants, durations, calendar rules (§9)
//! - [`exec`]      — the `Spawner` trait and process-group execution (§2.2, §11)
//! - [`export`]    — a stored job rendered back as a §6.2 TOML workflow file
//! - [`clock`]     — the `Clock` trait: injected time (§11)
//! - [`notify`]    — durable notification queue + delivery (§3.5)
//! - [`persist`]   — `PersistenceBackend`: systemd --user / cron @reboot (§8)
//! - [`config`]    — `~/.config/cued/config.toml` defaults (§10.1)
//! - [`desktop`]   — the status window's launcher entry and icons (§10.3)
//! - [`paths`]     — XDG locations for socket, store, logs, config (§5, §7.4)
//! - [`testhook`]  — feature-gated fault points for real-process tests (§11)

pub mod cli;
pub mod client;
pub mod clock;
pub mod config;
pub mod daemon;
pub mod desktop;
pub mod exec;
pub mod export;
pub mod model;
pub mod notify;
pub mod paths;
pub mod persist;
pub mod proto;
pub mod schedule;
pub mod store;
pub mod submit;
pub mod testhook;
pub mod timeparse;

pub mod mcp;
