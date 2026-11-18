//! Capture core shared by the Rust Screen GUI and the `rust-screen-mcp` server:
//! Win32 desktop capture, monitor/window discovery, the interactive selector,
//! PNG/clipboard export and the persisted settings.
#[cfg(not(windows))]
compile_error!("Rust Screen currently supports Windows 10/11 only.");

pub mod config;
pub mod enumerate;
pub mod export;
pub mod geometry;
pub mod platform;
pub mod selector;
