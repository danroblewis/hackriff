//! Reading one raw HTTP response off a test's `TcpStream`, **body included**.
//!
//! `hk_api`'s responder writes a response in two `write_all`s, head then body
//! (`http::respond_cached`). A reader that stops at the first `\r\n\r\n` therefore holds the body
//! only if both writes landed before its `read` woke, and whether they did depends on scheduling.
//! Under a loaded gate the read woke between them: the 2026-09-22 bulk gate failed
//! `a_run_that_really_ended_still_says_so` on a correct `410 Gone` because its text was the
//! headers alone (`Content-Length: 27`, zero body bytes), so `contains("finished")` was false.
//!
//! So this reader reads the head, then exactly `Content-Length` body bytes, for every status
//! except `101`. After a `101` the socket carries WebSocket frames, which are not a body and
//! must not be consumed here. A response that closes before its declared body arrives is a
//! server fault, and this reader panics on it instead of returning a truncated text.

use std::io::Read;
use std::net::TcpStream;

/// Returns `(status, the head plus the whole body as text)`. The caller sets the read timeout.
pub fn read_response(mut s: TcpStream) -> (u16, String) {
    let (mut got, mut buf) = (Vec::new(), [0u8; 4096]);
    let head_end = loop {
        if let Some(i) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match s.read(&mut buf) {
            Ok(0) => panic!(
                "the connection closed before the response head ended: {:?}",
                String::from_utf8_lossy(&got)
            ),
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) => panic!("reading the response head: {e}"),
        }
    };
    let head = String::from_utf8_lossy(&got[..head_end]).into_owned();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {head:?} ({} bytes)", got.len()));
    if status != 101 {
        let body_len = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while got.len() < head_end + body_len {
            match s.read(&mut buf) {
                Ok(0) => panic!(
                    "{status}: the connection closed after {} of {body_len} body bytes:\n{}",
                    got.len() - head_end,
                    String::from_utf8_lossy(&got)
                ),
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) => panic!("reading the {status} response body: {e}"),
            }
        }
    }
    (status, String::from_utf8_lossy(&got).into_owned())
}
