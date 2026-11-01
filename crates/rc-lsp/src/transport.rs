//! LSP base protocol framing: `Content-Length` headers followed by a JSON body.

use serde_json::Value;
use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// Upper bound on a single message, to fail fast on a corrupt header.
const MAX_MESSAGE_BYTES: usize = 256 * 1024 * 1024;

/// Reads one message. Returns `Ok(None)` on a clean EOF between messages.
pub async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    let mut saw_header = false;
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return if saw_header {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF inside message header"))
            } else {
                Ok(None)
            };
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            if saw_header {
                break;
            }
            continue; // tolerate stray blank lines between messages
        }
        saw_header = true;
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            let len = value.trim().parse::<usize>().map_err(|_| invalid("bad Content-Length"))?;
            content_length = Some(len);
        }
    }
    let len = content_length.ok_or_else(|| invalid("missing Content-Length header"))?;
    if len > MAX_MESSAGE_BYTES {
        return Err(invalid("message too large"));
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map(Some).map_err(|e| invalid(&format!("invalid JSON: {e}")))
}

/// Encodes one message with its header.
pub fn encode_message(message: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).expect("serde_json::Value always serializes");
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn round_trips_multiple_messages_with_unicode() {
        let a = json!({"jsonrpc": "2.0", "method": "x", "params": {"s": "héllo 😀"}});
        let b = json!({"jsonrpc": "2.0", "id": 1, "result": null});
        let mut bytes = encode_message(&a);
        bytes.extend(encode_message(&b));
        let mut reader = BufReader::new(bytes.as_slice());
        assert_eq!(read_message(&mut reader).await.unwrap(), Some(a));
        assert_eq!(read_message(&mut reader).await.unwrap(), Some(b));
        assert_eq!(read_message(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn accepts_extra_headers_any_case() {
        let raw = b"content-type: application/vscode-jsonrpc; charset=utf-8\r\nCONTENT-LENGTH: 2\r\n\r\n{}";
        let mut reader = BufReader::new(&raw[..]);
        assert_eq!(read_message(&mut reader).await.unwrap(), Some(json!({})));
    }

    #[tokio::test]
    async fn rejects_missing_length_and_truncated_body() {
        let mut reader = BufReader::new(&b"X-Foo: 1\r\n\r\n{}"[..]);
        assert!(read_message(&mut reader).await.is_err());
        let mut reader = BufReader::new(&b"Content-Length: 10\r\n\r\n{}"[..]);
        assert!(read_message(&mut reader).await.is_err());
    }
}
