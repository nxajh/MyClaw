//! CLI signal utilities — re-exports from the library crate.

// Re-export from the library crate so CLI subcommands can use them.
pub use myclaw::signal::{find_daemon_pid, send_signal, send_sighup, send_sigusr1};
