//! Litter-side JSON-line wire for the upstream `RemoteAppServerClient`.
//!
//! Upstream's `RemoteAppServerClient` only ships WebSocket transports (`connect`,
//! `connect_websocket_stream`). Pi/non-Codex servers and the SSH-bridge bootstrap path
//! talk plain JSON-RPC over a raw byte stream (one JSON object per line). The patch in
//! `patches/codex/remote-app-server-websocket-cap.patch` exposes a [`JsonRpcWire`] trait
//! and a public `RemoteAppServerClient::connect_with_wire` constructor so we can drive
//! the same dispatch loop over any wire. This module implements that wire for raw
//! line-delimited JSON-RPC and exposes a `connect_json_line_stream` helper to mirror
//! the upstream `connect_websocket_stream` API.
use std::future::Future;
use std::io::{Error as IoError, Result as IoResult};

use codex_app_server_client::{JsonRpcWire, RemoteAppServerClient, RemoteAppServerConnectArgs};
use codex_app_server_protocol::JSONRPCMessage;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Per-read scratch buffer size used when accumulating bytes from the
/// underlying reader. Chosen large enough to amortize syscall overhead on
/// healthy streams while small enough that a fully-buffered SSH exec data
/// event (which can be tens of KB) still gets drained in a handful of
/// reads. The value is unrelated to JSON-RPC frame size — frames are
/// accumulated across as many reads as needed until a newline is seen.
const READ_CHUNK: usize = 8 * 1024;

struct JsonLineWire<R, W> {
    reader: R,
    /// Accumulator of unread bytes already pulled from `reader`. Bytes
    /// stay here until a `\n` boundary is found so that JSON-RPC frames
    /// split across arbitrary chunk boundaries (e.g. russh's per-data
    /// event window) parse cleanly.
    read_buf: Vec<u8>,
    writer: W,
}

impl<R, W> JsonRpcWire for JsonLineWire<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    fn send_message<'a>(
        &'a mut self,
        message: JSONRPCMessage,
        label: &'a str,
    ) -> impl Future<Output = IoResult<()>> + Send + 'a {
        async move {
            // Inject the JSON-RPC 2.0 envelope marker. Upstream's
            // `JSONRPCMessage` mirrors codex-app-server's "JSON-RPC lite"
            // shape and intentionally omits the `jsonrpc: "2.0"` field
            // (see `codex-rs/app-server-protocol/src/jsonrpc_lite.rs`).
            // Strict JSON-RPC 2.0 peers — notably the pi 0.1.x ACP
            // server — reject any frame that lacks the version marker
            // with `Invalid request: missing field jsonrpc`, which from
            // the dispatch loop's perspective looks like a parse error
            // on the *response* path. Adding the field here keeps the
            // wire compatible with strict peers while leaving upstream's
            // internal types untouched.
            let mut value = serde_json::to_value(&message).map_err(IoError::other)?;
            if let Some(obj) = value.as_object_mut() {
                obj.entry("jsonrpc")
                    .or_insert_with(|| serde_json::Value::String("2.0".to_string()));
            }
            let payload = serde_json::to_vec(&value).map_err(IoError::other)?;
            self.writer.write_all(&payload).await.map_err(|err| {
                IoError::other(format!(
                    "failed to write JSON-lines message to `{label}`: {err}"
                ))
            })?;
            self.writer.write_all(b"\n").await.map_err(|err| {
                IoError::other(format!(
                    "failed to finish JSON-lines message to `{label}`: {err}"
                ))
            })?;
            self.writer.flush().await.map_err(|err| {
                IoError::other(format!(
                    "failed to flush JSON-lines message to `{label}`: {err}"
                ))
            })
        }
    }

    fn next_message<'a>(
        &'a mut self,
        label: &'a str,
    ) -> impl Future<Output = IoResult<Option<JSONRPCMessage>>> + Send + 'a {
        async move {
            // Pull bytes until we find a newline boundary. This replaces
            // `BufReader::read_line` because some `AsyncRead` impls (notably
            // the russh exec channel wrapper used by the pi SSH bootstrap)
            // hand back data in arbitrary chunk sizes that don't align with
            // JSON-RPC frame boundaries. `read_line` builds on
            // `read_until` and is correct in principle, but historical
            // breakage there motivates an explicit, easy-to-audit
            // accumulator scoped to this transport.
            loop {
                if let Some(newline_idx) =
                    self.read_buf.iter().position(|byte| *byte == b'\n')
                {
                    // Split off the line, preserve the remainder for the
                    // next call.
                    let remainder = self.read_buf.split_off(newline_idx + 1);
                    let mut line_bytes =
                        std::mem::replace(&mut self.read_buf, remainder);
                    // Drop the trailing newline (and an optional CR) so
                    // `serde_json::from_slice` sees a tight frame.
                    debug_assert_eq!(line_bytes.last(), Some(&b'\n'));
                    line_bytes.pop();
                    if line_bytes.last() == Some(&b'\r') {
                        line_bytes.pop();
                    }
                    if line_bytes.is_empty() {
                        // Tolerate stray empty lines from servers that
                        // emit a trailing newline after their final frame.
                        continue;
                    }
                    return serde_json::from_slice::<JSONRPCMessage>(&line_bytes)
                        .map(Some)
                        .map_err(|err| {
                            IoError::other(format!(
                                "remote app server at `{label}` sent invalid JSON-RPC: {err}"
                            ))
                        });
                }

                // No complete line yet; pull more bytes.
                let mut chunk = [0u8; READ_CHUNK];
                let read = self.reader.read(&mut chunk).await.map_err(|err| {
                    IoError::other(format!(
                        "failed to read JSON-lines message from `{label}`: {err}"
                    ))
                })?;
                if read == 0 {
                    // EOF. If we have a partial line buffered the peer
                    // truncated mid-frame; surface that as None so the
                    // upstream dispatcher reports a clean disconnect
                    // rather than a misleading parse error. (Real
                    // disconnects from pi acp always end on a frame
                    // boundary because each response is flushed
                    // explicitly.)
                    return Ok(None);
                }
                self.read_buf.extend_from_slice(&chunk[..read]);
            }
        }
    }

    fn close<'a>(&'a mut self, label: &'a str) -> impl Future<Output = IoResult<()>> + Send + 'a {
        async move {
            self.writer.shutdown().await.map_err(|err| {
                IoError::other(format!(
                    "failed to close JSON-lines app server `{label}`: {err}"
                ))
            })
        }
    }
}

/// Connect a [`RemoteAppServerClient`] over an arbitrary line-delimited JSON-RPC
/// stream. Mirrors the API shape of upstream's `connect_websocket_stream`.
pub async fn connect_json_line_stream<S>(
    stream: S,
    args: RemoteAppServerConnectArgs,
    label: String,
) -> IoResult<RemoteAppServerClient>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    RemoteAppServerClient::connect_with_wire(
        args,
        label,
        JsonLineWire {
            reader,
            read_buf: Vec::with_capacity(READ_CHUNK),
            writer,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    //! Unit coverage for the JsonLineWire accumulator. The byte-at-a-time
    //! cases simulate the worst-case `AsyncRead` behavior of the russh
    //! exec channel adapter used by `bootstrap_pi_server`, which can
    //! deliver a multi-KB JSON-RPC frame across many small `poll_read`
    //! returns. Prior to the chunk-boundary fix, `BufReader::read_line`
    //! on top of those reads would yield a partial line and the upstream
    //! dispatcher would log `invalid JSON-RPC: data did not match any
    //! variant of untagged enum JSONRPCMessage`.
    use super::{JsonLineWire, READ_CHUNK};
    use codex_app_server_client::JsonRpcWire;
    use codex_app_server_protocol::JSONRPCMessage;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    /// Reader that hands out at most `chunk_size` bytes per `poll_read`,
    /// modeling an `AsyncRead` adapter (like the SSH exec channel) that
    /// fragments frames across many small reads.
    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        chunk_size: usize,
    }

    impl AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.pos >= self.data.len() {
                return Poll::Ready(Ok(()));
            }
            let remaining = self.data.len() - self.pos;
            let take = remaining.min(self.chunk_size).min(buf.remaining());
            let end = self.pos + take;
            buf.put_slice(&self.data[self.pos..end]);
            self.pos = end;
            Poll::Ready(Ok(()))
        }
    }

    /// Throwaway writer; the next_message tests never call send.
    struct NullWriter;
    impl AsyncWrite for NullWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Build a realistic pi-style initialize response payload of roughly
    /// `target_bytes` plus the surrounding JSON-RPC envelope. The padding
    /// is uniform ASCII so the parser sees a single well-formed object.
    fn synthetic_initialize_response(target_bytes: usize) -> Vec<u8> {
        // Include a `userAgent` field with a long string so the parsed
        // payload looks plausibly like a real `pi acp` initialize result
        // (which contains capability descriptors that can run into
        // several KB on real devices).
        let padding = "x".repeat(target_bytes);
        let body = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"userAgent\":\"{padding}\"}}}}\n"
        );
        body.into_bytes()
    }

    fn make_wire(data: Vec<u8>, chunk_size: usize) -> JsonLineWire<ChunkedReader, NullWriter> {
        JsonLineWire {
            reader: ChunkedReader {
                data,
                pos: 0,
                chunk_size,
            },
            read_buf: Vec::with_capacity(READ_CHUNK),
            writer: NullWriter,
        }
    }

    /// Byte-at-a-time worst case: a 3KB initialize response delivered one
    /// byte per `poll_read` must still parse as a single JSONRPCMessage.
    #[tokio::test]
    async fn parses_multi_kb_frame_delivered_one_byte_at_a_time() {
        let payload = synthetic_initialize_response(3 * 1024);
        let mut wire = make_wire(payload, 1);
        let msg = wire
            .next_message("test")
            .await
            .expect("next_message ok")
            .expect("a frame should be available");
        match msg {
            JSONRPCMessage::Response(resp) => {
                let ua = resp.result["userAgent"].as_str().unwrap_or_default();
                assert_eq!(ua.len(), 3 * 1024, "userAgent payload preserved verbatim");
            }
            other => panic!("expected JSONRPCResponse, got {other:?}"),
        }
    }

    /// Two-halves case: the frame split in the middle of the JSON body.
    /// `next_message` must accumulate both halves before parsing.
    #[tokio::test]
    async fn parses_frame_split_in_two_halves() {
        let payload = synthetic_initialize_response(2 * 1024);
        let half = payload.len() / 2;
        let mut wire = make_wire(payload, half);
        let msg = wire
            .next_message("test")
            .await
            .expect("next_message ok")
            .expect("a frame should be available");
        assert!(matches!(msg, JSONRPCMessage::Response(_)));
    }

    /// Multiple frames in a single read: `next_message` must return the
    /// first frame and leave the rest in `read_buf` for the next call.
    #[tokio::test]
    async fn returns_frames_one_at_a_time_with_buffered_remainder() {
        let mut payload = synthetic_initialize_response(64);
        payload.extend_from_slice(&synthetic_initialize_response(64));
        let mut wire = make_wire(payload, READ_CHUNK); // single big read
        let _first = wire.next_message("test").await.unwrap().unwrap();
        let _second = wire.next_message("test").await.unwrap().unwrap();
        // Third call should hit EOF cleanly.
        assert!(wire.next_message("test").await.unwrap().is_none());
    }

    /// Writer that captures every byte handed to it so the test can
    /// inspect what was actually serialized onto the wire.
    #[derive(Default)]
    struct CapturingWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl AsyncWrite for CapturingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Strict JSON-RPC 2.0 peers (e.g. pi 0.1.x) reject frames that
    /// lack the `"jsonrpc": "2.0"` field. The wire must inject the
    /// marker on outbound frames even though upstream's
    /// `JSONRPCMessage` omits it from its serialized representation.
    #[tokio::test]
    async fn outbound_frames_include_jsonrpc_2_0_envelope() {
        use codex_app_server_protocol::{JSONRPCMessage, JSONRPCRequest, RequestId};

        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = CapturingWriter(captured.clone());
        // The reader side is unused for send_message tests.
        let reader = ChunkedReader {
            data: Vec::new(),
            pos: 0,
            chunk_size: 0,
        };
        let mut wire = JsonLineWire {
            reader,
            read_buf: Vec::new(),
            writer,
        };

        let request = JSONRPCRequest {
            id: RequestId::Integer(7),
            method: "initialize".to_string(),
            params: None,
            trace: None,
        };
        wire.send_message(JSONRPCMessage::Request(request), "test")
            .await
            .expect("send_message ok");

        let bytes = captured.lock().expect("lock").clone();
        let line = std::str::from_utf8(&bytes).expect("utf-8");
        let line = line.trim_end_matches('\n');
        let value: serde_json::Value = serde_json::from_str(line).expect("valid json");
        assert_eq!(
            value["jsonrpc"], "2.0",
            "outbound frame must carry the JSON-RPC 2.0 marker so strict peers (pi 0.1.x) accept it; got {value}"
        );
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["id"], 7);
    }
}
