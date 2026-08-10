//! `tcl-lsp` — a language server for Tcl and Tk.
//!
//! stdin/stdout carry JSON-RPC and nothing else. Every diagnostic message this
//! process emits about itself goes to stderr; the `clippy::print_stdout` lint below
//! makes that a compile error rather than a convention, because writing to stdout is
//! the single most common way to break a language server over stdio.

#![forbid(unsafe_code)]
#![deny(clippy::print_stdout)]

mod config;
mod external;
mod semantic;
mod server;

use anyhow::Result;

fn main() -> Result<()> {
    // stderr, never stdout.
    eprintln!("tcl-lsp {} starting", env!("CARGO_PKG_VERSION"));

    let (connection, io_threads) = lsp_server::Connection::stdio();
    let result = server::run(&connection);

    // Always join the IO threads so the process exits cleanly even on error.
    drop(connection);
    io_threads.join()?;
    result?;
    eprintln!("tcl-lsp exiting");
    Ok(())
}
