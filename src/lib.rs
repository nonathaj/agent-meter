//! agent-meter: manage multiple AI coding-agent accounts and switch between
//! them before a usage limit is hit.
//!
//! The crate is layered:
//! - [`provider`] knows where each agent CLI keeps its credentials, how to
//!   install a stored account, and how to talk to the provider's usage and
//!   token endpoints.
//! - [`store`] persists accounts and cached usage in agent-meter's data dir.
//! - [`policy`] is the pure decision logic for automatic switching.
//! - [`engine`] composes the above into the operations the CLI and TUI expose.
//! - [`remote`] keeps another machine's store in step with this one.

pub mod account;
pub mod cli;
pub mod config;
pub mod engine;
pub mod fleet;
pub mod foreign;
pub mod fsutil;
pub mod http;
pub mod jwt;
pub mod lock;
pub mod paths;
pub mod policy;
pub mod provider;
pub mod remote;
pub mod store;
pub mod timefmt;
pub mod tui;
pub mod usage;
