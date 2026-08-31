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
mod transport;

use anyhow::Result;

fn main() -> Result<()> {
    // stderr, never stdout.
    eprintln!("tcl-lsp {} starting", env!("CARGO_PKG_VERSION"));

    // Our transport, not `lsp_server::Connection::stdio`: one malformed
    // frame must not take the process down. See transport.rs.
    let connection = transport::stdio_connection();
    let result = server::run(&connection);

    // Dropping the Connection's sender ends the writer thread; the reader
    // stops when stdin hits EOF (the editor's side of the pipe closing).
    drop(connection);
    result?;
    eprintln!("tcl-lsp exiting");
    Ok(())
}
