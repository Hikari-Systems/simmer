//! §8.1 — the `DATA` buffer.
//!
//! **This is not a spool.** It is a transient buffer that exists for the length
//! of one client connection, and §2.2 depends on it staying that way. It is in
//! memory up to [`SPILL_THRESHOLD`] and in an unlinked temporary file above it,
//! and it is not recovered after a crash — at the point of a crash no `250` has
//! been returned to the client, so nothing has been promised.
//!
//! The `DATA` payload cannot be streamed through to the downstream because
//! headers are rewritten (phase 4) and the routing decision may depend on the
//! body's `From:` header (§5.4). Buffering is forced, not chosen.
//!
//! ## What is stored
//!
//! The **unstuffed, CRLF-normalised** message: transparency dots removed, bare
//! `LF` promoted to `CRLF`. That is the canonical RFC 5322 message, which is
//! what phase 4 needs to parse and what phase 5 needs to rewrite. Re-stuffing
//! happens on transmit, in [`crate::downstream`].

use std::io;
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// §8.1: "buffered in memory up to a threshold (default 1 MiB) and spilled to a
/// temporary file above it".
///
/// Not in the §4.1 schema, so not configurable. The container gives `/tmp` a
/// tmpfs (see `docker-compose.yml`), which is what makes "not durable" a
/// property of the deployment rather than a promise in a comment.
pub const SPILL_THRESHOLD: usize = 1024 * 1024;

/// Where a spilled buffer goes. `TMPDIR` if set, else `/tmp`.
fn spill_dir() -> std::path::PathBuf {
    std::env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new("/tmp").to_path_buf())
}

/// The accumulating message body.
pub enum MessageBuffer {
    Memory(Vec<u8>),
    /// An **unlinked** temporary file. `tempfile::tempfile()` removes the
    /// directory entry immediately, so §8.1's "deleted when the session ends" is
    /// enforced by the kernel on last-close and survives a panic anywhere
    /// between here and the end of the session.
    Spilled {
        file: tokio::fs::File,
        len: usize,
    },
}

impl MessageBuffer {
    pub fn new() -> Self {
        MessageBuffer::Memory(Vec::with_capacity(8 * 1024))
    }

    pub fn len(&self) -> usize {
        match self {
            MessageBuffer::Memory(v) => v.len(),
            MessageBuffer::Spilled { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_spilled(&self) -> bool {
        matches!(self, MessageBuffer::Spilled { .. })
    }

    /// Append bytes, spilling to a temporary file on first crossing of
    /// [`SPILL_THRESHOLD`].
    pub async fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            MessageBuffer::Memory(v) if v.len() + bytes.len() <= SPILL_THRESHOLD => {
                v.extend_from_slice(bytes);
                Ok(())
            }
            MessageBuffer::Memory(v) => {
                let existing = std::mem::take(v);
                let file = tempfile::tempfile_in(spill_dir())?;
                let mut file = tokio::fs::File::from_std(file);
                file.write_all(&existing).await?;
                file.write_all(bytes).await?;
                *self = MessageBuffer::Spilled {
                    file,
                    len: existing.len() + bytes.len(),
                };
                Ok(())
            }
            MessageBuffer::Spilled { file, len } => {
                file.write_all(bytes).await?;
                *len += bytes.len();
                Ok(())
            }
        }
    }

    /// The leading header block, up to and including the blank line that ends it.
    ///
    /// §5.4 needs the first `From:` address to pick a route, and a spilled
    /// 25 MiB message must not be read back into memory in its entirety to
    /// answer that. Capped at `max` bytes: a message whose headers exceed that
    /// is malformed by any reasonable standard, and the cap is what stops a
    /// header-only message from defeating the point of spilling.
    pub async fn header_block(&mut self, max: usize) -> io::Result<Vec<u8>> {
        let prefix = match self {
            MessageBuffer::Memory(v) => v[..v.len().min(max)].to_vec(),
            MessageBuffer::Spilled { file, .. } => {
                file.seek(io::SeekFrom::Start(0)).await?;
                let mut buf = vec![0u8; max];
                let n = read_up_to(file, &mut buf).await?;
                buf.truncate(n);
                buf
            }
        };

        // Cut at the header/body separator if it is within the prefix. If it is
        // not, hand back what we have — a truncated header block still parses,
        // and `From:` is conventionally near the top.
        let end = find_header_end(&prefix).unwrap_or(prefix.len());
        Ok(prefix[..end].to_vec())
    }

    /// Read the whole buffer back. Used to transmit, and by tests.
    ///
    /// Phase 2 forwards verbatim so this is the transmit path. Phase 4 parses
    /// and rewrites, which needs the bytes in memory anyway.
    pub async fn read_all(&mut self) -> io::Result<Vec<u8>> {
        match self {
            MessageBuffer::Memory(v) => Ok(v.clone()),
            MessageBuffer::Spilled { file, len } => {
                file.seek(io::SeekFrom::Start(0)).await?;
                let mut out = Vec::with_capacity(*len);
                file.read_to_end(&mut out).await?;
                Ok(out)
            }
        }
    }
}

impl Default for MessageBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// `read_exact` that tolerates a short file.
async fn read_up_to(file: &mut tokio::fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let n = file.read(&mut buf[total..]).await?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

/// Offset just past the CRLFCRLF (or LFLF) that ends the header block.
fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .or_else(|| bytes.windows(2).position(|w| w == b"\n\n").map(|i| i + 2))
}

// ---------------------------------------------------------------------------
// Dot transparency and line endings (RFC 5321 §4.5.2)
// ---------------------------------------------------------------------------

/// Strip one leading transparency dot from a `DATA` line, if present.
///
/// The client doubles a leading `.` so that a body line reading `.` cannot be
/// mistaken for the terminator. Undoing it here means the stored message is the
/// real one — which matters as soon as phase 5 runs a regex over the body, where
/// a stray `.` at the start of a line would silently break a match.
pub fn unstuff(line: &[u8]) -> &[u8] {
    if let Some(rest) = line.strip_prefix(b".") {
        rest
    } else {
        line
    }
}

/// Add transparency dots and terminate, for transmission.
///
/// The inverse of [`unstuff`], applied per line on the way out. A message that
/// arrived stuffed and leaves stuffed is byte-identical, which is the property
/// the phase-2 end-to-end test asserts and phase 4 must preserve.
pub fn stuff_into(out: &mut Vec<u8>, message: &[u8]) {
    for line in split_crlf_lines(message) {
        if line.starts_with(b".") {
            out.push(b'.');
        }
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
}

/// Split on CRLF, dropping a trailing empty segment so a message that already
/// ends in CRLF does not gain a blank line on every hop.
fn split_crlf_lines(message: &[u8]) -> Vec<&[u8]> {
    if message.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + 1 < message.len() {
        if message[i] == b'\r' && message[i + 1] == b'\n' {
            out.push(&message[start..i]);
            i += 2;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < message.len() {
        out.push(&message[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stays_in_memory_below_the_threshold() {
        let mut b = MessageBuffer::new();
        b.append(b"hello").await.unwrap();
        assert!(!b.is_spilled());
        assert_eq!(b.len(), 5);
        assert_eq!(b.read_all().await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn spills_on_crossing_the_threshold_and_keeps_everything() {
        let mut b = MessageBuffer::new();

        // Just under, in one write.
        let first = vec![b'a'; SPILL_THRESHOLD - 10];
        b.append(&first).await.unwrap();
        assert!(!b.is_spilled(), "must not spill below the threshold");

        // The write that crosses it.
        let second = vec![b'b'; 100];
        b.append(&second).await.unwrap();
        assert!(b.is_spilled(), "must spill above the threshold");

        assert_eq!(b.len(), first.len() + second.len());
        let all = b.read_all().await.unwrap();
        assert_eq!(all.len(), first.len() + second.len());
        // The bytes buffered before the spill must survive it — the bug this
        // guards is dropping the in-memory prefix when opening the file.
        assert!(all[..first.len()].iter().all(|c| *c == b'a'));
        assert!(all[first.len()..].iter().all(|c| *c == b'b'));
    }

    #[tokio::test]
    async fn appends_after_a_spill_go_to_the_file() {
        let mut b = MessageBuffer::new();
        b.append(&vec![b'x'; SPILL_THRESHOLD + 1]).await.unwrap();
        b.append(b"tail").await.unwrap();
        assert!(b.is_spilled());
        let all = b.read_all().await.unwrap();
        assert_eq!(&all[all.len() - 4..], b"tail");
        assert_eq!(b.len(), SPILL_THRESHOLD + 5);
    }

    #[tokio::test]
    async fn header_block_stops_at_the_blank_line() {
        let mut b = MessageBuffer::new();
        b.append(b"From: a@b.com\r\nSubject: hi\r\n\r\nbody here\r\n")
            .await
            .unwrap();
        assert_eq!(
            b.header_block(64 * 1024).await.unwrap(),
            b"From: a@b.com\r\nSubject: hi\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn header_block_works_on_a_spilled_buffer() {
        // The case that matters: a 25 MiB message must not be read back whole
        // just to find From:.
        let mut b = MessageBuffer::new();
        b.append(b"From: a@b.com\r\n\r\n").await.unwrap();
        b.append(&vec![b'x'; SPILL_THRESHOLD + 1]).await.unwrap();
        assert!(b.is_spilled());
        assert_eq!(
            b.header_block(64 * 1024).await.unwrap(),
            b"From: a@b.com\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn header_block_tolerates_a_message_with_no_body_separator() {
        let mut b = MessageBuffer::new();
        b.append(b"From: a@b.com\r\n").await.unwrap();
        assert_eq!(
            b.header_block(64 * 1024).await.unwrap(),
            b"From: a@b.com\r\n"
        );
    }

    #[tokio::test]
    async fn header_block_is_capped() {
        let mut b = MessageBuffer::new();
        b.append(&vec![b'h'; 4096]).await.unwrap();
        assert_eq!(b.header_block(100).await.unwrap().len(), 100);
    }

    // -- dot transparency -------------------------------------------------

    #[test]
    fn unstuffs_a_leading_dot() {
        // Exactly one dot comes off: "..hidden" on the wire is ".hidden" in the
        // message. Stripping all of them would corrupt a body line of dots.
        assert_eq!(unstuff(b"..hidden"), b".hidden");
        assert_eq!(unstuff(b"...x"), b"..x");
        assert_eq!(unstuff(b"."), b"");
        assert_eq!(unstuff(b"normal"), b"normal");
        assert_eq!(unstuff(b""), b"");
    }

    #[test]
    fn stuffing_round_trips_with_unstuffing() {
        // The property that keeps a body byte-identical across the relay.
        let message: &[u8] = b"Subject: t\r\n\r\n.leading dot\r\n..two dots\r\nplain\r\n";

        let mut wire = Vec::new();
        stuff_into(&mut wire, message);
        assert_eq!(
            wire,
            b"Subject: t\r\n\r\n..leading dot\r\n...two dots\r\nplain\r\n.\r\n".to_vec()
        );

        // Now undo it the way the receiving side would.
        let body = &wire[..wire.len() - 3]; // drop the terminating ".\r\n"
        let mut back = Vec::new();
        for line in split_crlf_lines(body) {
            back.extend_from_slice(unstuff(line));
            back.extend_from_slice(b"\r\n");
        }
        assert_eq!(back, message);
    }

    #[test]
    fn stuffing_terminates_even_an_empty_message() {
        let mut out = Vec::new();
        stuff_into(&mut out, b"");
        assert_eq!(out, b".\r\n");
    }

    #[test]
    fn stuffing_does_not_add_a_blank_line_to_a_crlf_terminated_message() {
        let mut out = Vec::new();
        stuff_into(&mut out, b"a\r\nb\r\n");
        assert_eq!(out, b"a\r\nb\r\n.\r\n");
    }

    #[test]
    fn stuffing_terminates_a_message_with_no_final_crlf() {
        let mut out = Vec::new();
        stuff_into(&mut out, b"a\r\nb");
        assert_eq!(out, b"a\r\nb\r\n.\r\n");
    }
}
