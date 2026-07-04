//! Loopback HTTP video server — Linux-only workaround for WebKitGTK.
//!
//! WebKitGTK's GStreamer media backend never routes `<video>`/`<audio>` loads
//! through custom URI scheme handlers (upstream WebKit bug 146351, still open),
//! so `stingle://` video URLs spin and then show the broken-player icon on
//! Linux even though images over the same protocol work. Videos are instead
//! streamed over plain HTTP on 127.0.0.1, which GStreamer handles natively —
//! including Range requests, which map onto the same chunked
//! [`Account::media_response`] decryption the stingle:// protocol uses.
//!
//! SECURITY: identical guarantees to the protocol handler — decrypted bytes
//! exist only in memory and are streamed straight to the socket in ≤4 MB
//! chunks (never a whole-file plaintext buffer, never disk). The listener
//! binds 127.0.0.1 only, and every request must present the per-run random
//! token as its first path segment, so other local processes can't fetch
//! media without it. Responses carry `Cache-Control: no-store`.

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use tauri::Manager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::AppState;

/// Longest request head we accept; anything bigger is dropped.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// Bind the server on an ephemeral loopback port and start accepting.
/// Returns `(port, token)` for building `http://127.0.0.1:<port>/<token>/…`.
pub async fn start(app: tauri::AppHandle) -> std::io::Result<(u16, String)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let token = URL_SAFE_NO_PAD.encode(
        stingle_crypto::sodium::random_bytes(24)
            .map_err(|e| std::io::Error::other(format!("token: {e}")))?,
    );
    let token_arc = Arc::new(token.clone());
    tauri::async_runtime::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((sock, _)) => {
                    let app = app.clone();
                    let token = token_arc.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = handle_connection(app, sock, &token).await;
                    });
                }
                Err(err) => tracing::warn!("video server accept failed: {err}"),
            }
        }
    });
    Ok((port, token))
}

/// Compare the URL token against ours in constant time; a loopback attacker
/// can issue unlimited requests, so don't leak prefix length via timing.
fn token_ok(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

async fn write_error(sock: &mut TcpStream, status: u16, reason: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    sock.write_all(head.as_bytes()).await?;
    sock.shutdown().await
}

async fn handle_connection(
    app: tauri::AppHandle,
    mut sock: TcpStream,
    token: &str,
) -> std::io::Result<()> {
    // Read the request head, bounded in size and time.
    let head = match tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut buf = Vec::with_capacity(2048);
        let mut tmp = [0u8; 2048];
        loop {
            let n = sock.read(&mut tmp).await?;
            if n == 0 {
                return Err(std::io::Error::other("connection closed mid-head"));
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                buf.truncate(pos);
                return Ok(String::from_utf8_lossy(&buf).into_owned());
            }
            if buf.len() > MAX_HEAD_BYTES {
                return Err(std::io::Error::other("request head too large"));
            }
        }
    })
    .await
    {
        Ok(Ok(head)) => head,
        _ => return Ok(()),
    };

    let mut lines = head.split("\r\n");
    let mut req_line = lines.next().unwrap_or("").split_whitespace();
    let method = req_line.next().unwrap_or("");
    let target = req_line.next().unwrap_or("");
    if method != "GET" {
        return write_error(&mut sock, 405, "Method Not Allowed").await;
    }
    let range = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("range"))
        .and_then(|(_, v)| crate::parse_range(v));

    // Path: `/<token>/<set>!<isThumb>!<album-or-->!<filename>` — the payload
    // uses the exact format of the stingle:// protocol.
    let path = target.split('?').next().unwrap_or("");
    let rest = path.strip_prefix('/').unwrap_or(path);
    let (given_token, payload) = match rest.split_once('/') {
        Some(p) => p,
        None => return write_error(&mut sock, 404, "Not Found").await,
    };
    if !token_ok(given_token, token) {
        return write_error(&mut sock, 404, "Not Found").await;
    }

    serve_media(&app, &mut sock, payload, range).await
}

/// Stream one media file (or the requested byte range of it) as a single
/// response, decrypting sequential ≤4 MB windows so a multi-GB video never
/// materializes in memory at once. GStreamer opens with no Range header
/// (→ 200, whole file) and seeks with `bytes=N-` (→ 206, N to EOF).
async fn serve_media(
    app: &tauri::AppHandle,
    sock: &mut TcpStream,
    payload: &str,
    range: Option<(u64, Option<u64>)>,
) -> std::io::Result<()> {
    let Some((set, is_thumb, album, filename)) = crate::parse_media_path(payload) else {
        return write_error(sock, 404, "Not Found").await;
    };
    let Some(acc) = app.state::<AppState>().current().await else {
        return write_error(sock, 404, "Not Found").await;
    };

    let (start, req_end) = range.unwrap_or((0, None));
    // The first chunk also reveals the file's total size and content type,
    // which the response head needs before any body bytes go out.
    let first = match acc
        .media_response(set, album.as_deref(), &filename, is_thumb, Some((start, req_end)))
        .await
    {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!("video server: media error for {filename}: {err}");
            return write_error(sock, 404, "Not Found").await;
        }
    };
    let total = first.total_size;
    let end = req_end.map(|e| e.min(total - 1)).unwrap_or(total - 1);

    let mut head = if range.is_some() {
        format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\n")
    } else {
        "HTTP/1.1 200 OK\r\n".to_string()
    };
    head.push_str(&format!(
        "Content-Type: {}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        first.content_type,
        end - start + 1
    ));
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(&first.body).await?;

    let mut pos = match first.range {
        Some((_, e)) => e + 1,
        None => end + 1,
    };
    while pos <= end {
        let m = match acc
            .media_response(set, album.as_deref(), &filename, is_thumb, Some((pos, Some(end))))
            .await
        {
            Ok(m) => m,
            // Mid-stream failure: the head is already sent, so all we can do
            // is close early — the player sees a truncated body and re-requests.
            Err(err) => {
                tracing::warn!("video server: stream error for {filename}: {err}");
                break;
            }
        };
        if m.body.is_empty() {
            break;
        }
        sock.write_all(&m.body).await?;
        pos = match m.range {
            Some((_, e)) => e + 1,
            None => end + 1,
        };
    }
    sock.shutdown().await
}
