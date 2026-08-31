//! Lenient stdio transport — the server must not die on bad input.
//!
//! `lsp_server::Connection::stdio` turns one malformed frame into a fatal
//! transport error: its reader thread returns `Err`, `IoThreads::join`
//! propagates it, and the process exits. That is how a single mis-escaped
//! `didClose` from the veles client killed the language server under the
//! editor (2026-08-31): the client kept talking to a corpse, and every
//! hover silently returned nothing. A server that can be killed by what it
//! reads is not a server; here an undecodable frame is logged to stderr and
//! skipped, and only a real EOF or a broken pipe ends the reader.
//!
//! Framing and message types stay the `lsp-server` crate's; only the thread
//! wiring is ours.

use std::io::{self, BufRead, Write};

use crossbeam_channel::bounded;
use lsp_server::{Connection, Message};

/// Decode one frame body. `None` means "not a valid message — drop it and
/// keep serving"; the reason goes to stderr, where /api/logs can see it.
pub fn decode_message(text: &str) -> Option<Message> {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            let snip: String = text.chars().take(200).collect();
            eprintln!("tcl-lsp: dropping malformed frame ({e}): {snip:?}");
            return None;
        }
    };
    match serde_json::from_value::<Message>(value) {
        Ok(m) => Some(m),
        Err(e) => {
            // Valid JSON, but not a JSON-RPC message we understand.
            eprintln!("tcl-lsp: dropping non-message frame ({e})");
            None
        }
    }
}

/// Wire up stdin/stdout as an LSP connection. The reader thread decodes and
/// skips garbage; the writer thread serializes. Both run for the life of
/// the process; when `Connection` is dropped the writer drains and exits,
/// and EOF on stdin ends the reader.
pub fn stdio_connection() -> Connection {
    // `sender` is what the server writes responses/notifications into.
    let (writer_tx, writer_rx) = bounded::<Message>(64);
    let (reader_tx, reader_rx) = bounded::<Message>(64);

    std::thread::Builder::new()
        .name("tcl-lsp-reader".to_string())
        .spawn(move || {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            loop {
                match read_frame(&mut stdin) {
                    Ok(None) => break, // clean EOF: the editor is gone
                    Ok(Some(body)) => {
                        let msg = match decode_message(&body) {
                            Some(m) => m,
                            // The whole point: bad bytes do not end the loop.
                            None => continue,
                        };
                        let is_exit = matches!(&msg, Message::Notification(n)
                            if n.method == "exit");
                        if reader_tx.send(msg).is_err() || is_exit {
                            break;
                        }
                    }
                    // No parseable Content-Length header: the framing itself
                    // is unrecoverable, so stop — matching upstream.
                    Err(e) => {
                        eprintln!("tcl-lsp: stdin framing error, reader stopping: {e}");
                        break;
                    }
                }
            }
            // Dropping reader_tx ends the server loop cleanly.
        })
        .expect("spawn tcl-lsp reader");

    std::thread::Builder::new()
        .name("tcl-lsp-writer".to_string())
        .spawn(move || {
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            // Ends when the Connection's sender is dropped and drained.
            for msg in writer_rx {
                if msg.write(&mut stdout).is_err() {
                    let _ = stdout.flush();
                    break;
                }
            }
        })
        .expect("spawn tcl-lsp writer");

    Connection {
        sender: writer_tx,
        receiver: reader_rx,
    }
}

/// Read one `Content-Length`-framed body. `Ok(None)` = EOF before a header.
fn read_frame(r: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut len: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(None); // EOF
        }
        let trimmed = line.trim_end().to_ascii_lowercase();
        if trimmed.is_empty() {
            break; // end of the header block
        }
        if let Some(v) = trimmed.strip_prefix("content-length:") {
            len = v.trim().parse().ok();
        }
    }
    let Some(len) = len else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame without a parseable Content-Length header",
        ));
    };
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(String::from_utf8_lossy(&body).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_request_decodes() {
        let m = decode_message(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .expect("a request decodes");
        assert!(matches!(m, Message::Request(r) if r.method == "initialize"));
    }

    #[test]
    fn garbage_is_dropped_not_fatal() {
        // The exact shape of the client's bug: JSON syntax broken by an
        // unevaluated Tcl command sitting where a string should be.
        assert!(decode_message(
            r#"{"jsonrpc":"2.0","method":"x","params":{"uri":[_jstr file:///a]}}"#
        )
        .is_none());
        // Non-JSON entirely, and valid JSON that is not a JSON-RPC message.
        assert!(decode_message("}{ not json").is_none());
        assert!(decode_message(r#"{"this":"is not jsonrpc"}"#).is_none());
    }
}
