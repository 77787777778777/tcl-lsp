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
use std::thread::JoinHandle;

use crossbeam_channel::bounded;
use lsp_server::{Connection, Message};

/// The reader and writer threads, kept so `main` can join them on the way
/// out instead of racing the process exit against the writer's last
/// flush. `lsp_server::Connection::stdio` returns an `IoThreads` for the
/// same reason; this is that, minus the fatal-on-bad-frame behaviour.
pub struct TransportThreads {
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl TransportThreads {
    /// Wait for both threads. The reader is already gone by the time this
    /// is reached (EOF or `exit`); this is really about letting the
    /// writer drain every queued response before the process ends.
    pub fn join(self) {
        let _ = self.reader.join();
        let _ = self.writer.join();
    }
}

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
/// and EOF on stdin ends the reader. Join the returned handles before
/// exiting so the writer's last flush is not cut off by process teardown.
pub fn stdio_connection() -> (Connection, TransportThreads) {
    // `sender` is what the server writes responses/notifications into.
    let (writer_tx, writer_rx) = bounded::<Message>(64);
    let (reader_tx, reader_rx) = bounded::<Message>(64);

    let reader = std::thread::Builder::new()
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
                    // Only a truly unrecoverable stream (no header at all
                    // within the resync window, or a broken pipe) ends the
                    // reader. A single bad header block is skipped by
                    // read_frame itself.
                    Err(e) => {
                        eprintln!("tcl-lsp: stdin framing error, reader stopping: {e}");
                        break;
                    }
                }
            }
            // Dropping reader_tx ends the server loop cleanly.
        })
        .expect("spawn tcl-lsp reader");

    let writer = std::thread::Builder::new()
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
            let _ = stdout.flush();
        })
        .expect("spawn tcl-lsp writer");

    (
        Connection {
            sender: writer_tx,
            receiver: reader_rx,
        },
        TransportThreads { reader, writer },
    )
}

/// Read one `Content-Length`-framed body. `Ok(None)` = EOF before a header.
///
/// A header block that ends with no parseable `Content-Length` is NOT
/// fatal — the reader keeps scanning lines for the next real header, so a
/// stray blank block or a byte-level desync (a previous frame's length
/// was wrong) resyncs on the next well-formed frame instead of taking the
/// server down. Only EOF, a broken pipe, or `RESYNC_CAP` bytes of junk
/// with no header in sight ends it.
fn read_frame(r: &mut impl BufRead) -> io::Result<Option<String>> {
    const RESYNC_CAP: usize = 4 * 1024 * 1024;
    let mut len: Option<usize> = None;
    let mut line = String::new();
    let mut scanned = 0usize;
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        scanned += n;
        let trimmed = line.trim_end().to_ascii_lowercase();
        if let Some(v) = trimmed.strip_prefix("content-length:") {
            if let Ok(parsed) = v.trim().parse::<usize>() {
                len = Some(parsed);
            }
            // A present-but-unparseable value: ignore it and keep looking
            // (this is the desync signature).
        } else if trimmed.is_empty() && len.is_some() {
            break; // end of a header block that gave us a length
        }
        if scanned > RESYNC_CAP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no Content-Length header within the resync window",
            ));
        }
    }
    let len = len.expect("the loop only breaks once len is Some");
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

    fn frame(body: &str) -> String {
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    #[test]
    fn read_frame_reads_one_well_formed_frame() {
        let wire = frame(r#"{"id":1}"#);
        let mut r = io::Cursor::new(wire.into_bytes());
        assert_eq!(read_frame(&mut r).unwrap().as_deref(), Some(r#"{"id":1}"#));
        assert_eq!(read_frame(&mut r).unwrap(), None); // EOF
    }

    #[test]
    fn a_header_block_with_no_content_length_is_skipped_not_fatal() {
        // A stray blank line / an unknown-only header block used to make
        // read_frame return Err and the reader thread exit. It must
        // resync on the next real frame instead.
        let mut wire = String::from("\r\n"); // stray empty block
        wire.push_str("X-Weird: 1\r\n\r\n"); // unknown-only block
        wire.push_str(&frame(r#"{"id":2}"#)); // the real frame
        let mut r = io::Cursor::new(wire.into_bytes());
        assert_eq!(read_frame(&mut r).unwrap().as_deref(), Some(r#"{"id":2}"#));
    }

    #[test]
    fn an_unparseable_content_length_does_not_end_the_reader() {
        // The byte-desync signature: a `Content-Length:` line whose value
        // is not a number. Skip it, keep scanning, land on the next frame.
        let mut wire = String::from("Content-Length: not-a-number\r\n\r\n");
        wire.push_str(&frame(r#"{"id":3}"#));
        let mut r = io::Cursor::new(wire.into_bytes());
        assert_eq!(read_frame(&mut r).unwrap().as_deref(), Some(r#"{"id":3}"#));
    }
}
