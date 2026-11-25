//! The MPD `albumart` binary fetch, shared by every client that shows a cover (the
//! dj-gui TUI and the djg GTK app). Std-only and blocking, like the rest of this
//! crate: the caller runs it off its UI thread.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const ART_TIMEOUT: Duration = Duration::from_secs(3);

/// Read one CRLF/LF-terminated line, stripping the terminator. EOF -> error.
fn read_line(r: &mut impl BufRead) -> std::io::Result<String> {
    let mut s = String::new();
    if r.read_line(&mut s)? == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof"));
    }
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    Ok(s)
}

/// MPD binary-safe quoting for the uri argument.
fn quote(uri: &str) -> String {
    format!("\"{}\"", uri.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Fetch the full cover for `uri` over a dedicated connection, looping the
/// `albumart` offset chunks until the reported total is assembled. `None` on no
/// art / ACK / any IO error.
pub fn fetch_albumart(host: &str, port: u16, uri: &str) -> Option<Vec<u8>> {
    let stream = TcpStream::connect((host, port)).ok()?;
    stream.set_read_timeout(Some(ART_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(ART_TIMEOUT)).ok()?;
    let mut w = stream.try_clone().ok()?;
    let mut r = BufReader::new(stream);
    if !read_line(&mut r).ok()?.starts_with("OK MPD") {
        return None;
    }
    // RAISE THE CHUNK SIZE BEFORE ASKING FOR ANYTHING. The MPD default binary limit is
    // 8 KiB, so a 2 MiB FLAC cover arrived as ~37 request/response round trips - and on
    // this daemon each one re-resolved the cover id server-side, so it was also ~37
    // `getSong` calls against Navidrome for a single image. One cover, one exchange.
    //
    // Best-effort on purpose: an older daemon that does not know `binarylimit` answers
    // ACK, which is simply read and discarded below, and the loop then works exactly as
    // it did before at 8 KiB. There is nothing to fall back to and nothing to detect.
    //
    // 1 MiB sits well under the 8 MiB sanity clamp on the read side below, so a chunk
    // this asks for can never be one that function then refuses - and it is a request
    // for a CEILING, not an allocation: the daemon sends what it has, so a small cover
    // is still one small chunk.
    w.write_all(b"binarylimit 1048576\n").ok()?;
    w.flush().ok()?;
    loop {
        let line = read_line(&mut r).ok()?;
        if line == "OK" || line.starts_with("ACK") {
            break;
        }
    }
    let mut all: Vec<u8> = Vec::new();
    loop {
        w.write_all(format!("albumart {} {}\n", quote(uri), all.len()).as_bytes())
            .ok()?;
        w.flush().ok()?;
        let mut total = 0usize;
        let mut chunk = 0usize;
        loop {
            let line = read_line(&mut r).ok()?;
            if let Some(v) = line.strip_prefix("size: ") {
                total = v.trim().parse().ok()?;
            } else if let Some(v) = line.strip_prefix("binary: ") {
                let n: usize = v.trim().parse().ok()?;
                // Sanity clamp: never trust a wild length into an allocation.
                if n > 8 * 1024 * 1024 {
                    return None;
                }
                let mut buf = vec![0u8; n];
                r.read_exact(&mut buf).ok()?;
                all.extend_from_slice(&buf);
                chunk = n;
                // The raw payload is followed by a lone `\n` then `OK`; the empty
                // line is consumed (and ignored) by the next read_line.
            } else if line == "OK" {
                break;
            } else if line.starts_with("ACK") {
                return if all.is_empty() { None } else { Some(all) };
            }
        }
        if total == 0 || chunk == 0 || all.len() >= total {
            break;
        }
    }
    if all.is_empty() {
        None
    } else {
        Some(all)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_escapes() {
        assert_eq!(quote("song/1"), "\"song/1\"");
        assert_eq!(quote(r#"a"b\c"#), "\"a\\\"b\\\\c\"");
    }
}
