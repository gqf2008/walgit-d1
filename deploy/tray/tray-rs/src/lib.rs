//! Shared library for the walgit tray and its detached updater helper.
//!
//! The Windows updater is a separate process because Inno Setup must replace
//! `walgit-tray.exe` while the old tray is exiting. Keeping release parsing and
//! updater orchestration in this library gives the tray and helper one contract
//! while the two binaries retain separate process lifetimes.

pub mod release;
pub mod upgrade_helper;
