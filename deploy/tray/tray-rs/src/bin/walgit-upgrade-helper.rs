//! Detached Windows tray updater.
//!
//! The tray copies this executable into its writable state directory before
//! launching it, so Inno Setup can replace the copy under the install directory
//! without fighting a running executable's file lock.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    std::process::exit(walgit_tray::upgrade_helper::main_entry());
}
