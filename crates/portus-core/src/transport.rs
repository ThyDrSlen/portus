use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Length-prefixed framing: 4-byte big-endian length prefix followed by JSON payload.
/// Max message size: 1 MB.
const MAX_MESSAGE_SIZE: u32 = 1_048_576;

/// Send a length-prefixed message over an async writer.
pub async fn send_message<W: AsyncWriteExt + Unpin>(writer: &mut W, data: &[u8]) -> Result<()> {
    let len = data.len() as u32;
    if len > MAX_MESSAGE_SIZE {
        anyhow::bail!("message too large: {} bytes (max {})", len, MAX_MESSAGE_SIZE);
    }
    writer
        .write_all(&len.to_be_bytes())
        .await
        .context("failed to write message length")?;
    writer
        .write_all(data)
        .await
        .context("failed to write message body")?;
    writer.flush().await.context("failed to flush")?;
    Ok(())
}

/// Receive a length-prefixed message from an async reader.
/// Returns None on clean EOF (connection closed).
pub async fn recv_message<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("failed to read message length"),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_MESSAGE_SIZE {
        anyhow::bail!(
            "incoming message too large: {} bytes (max {})",
            len,
            MAX_MESSAGE_SIZE
        );
    }
    let mut buf = vec![0u8; len as usize];
    reader
        .read_exact(&mut buf)
        .await
        .context("failed to read message body")?;
    Ok(Some(buf))
}

/// Serialize a request/response to JSON bytes and send it.
pub async fn send_json<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: serde::Serialize,
{
    let data = serde_json::to_vec(value).context("failed to serialize message")?;
    send_message(writer, &data).await
}

/// Receive a length-prefixed message and deserialize from JSON.
pub async fn recv_json<R, T>(reader: &mut R) -> Result<Option<T>>
where
    R: AsyncReadExt + Unpin,
    T: serde::de::DeserializeOwned,
{
    match recv_message(reader).await? {
        Some(data) => {
            let value =
                serde_json::from_slice(&data).context("failed to deserialize message")?;
            Ok(Some(value))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_message() {
        let (mut client, mut server) = tokio::io::duplex(1024);

        let payload = b"hello portus";
        send_message(&mut client, payload).await.unwrap();

        let received = recv_message(&mut server).await.unwrap().unwrap();
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn roundtrip_json() {
        use crate::protocol::Request;

        let (mut client, mut server) = tokio::io::duplex(4096);

        let req = Request::Status;
        send_json(&mut client, &req).await.unwrap();

        let received: Request = recv_json(&mut server).await.unwrap().unwrap();
        match received {
            Request::Status => {}
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn eof_returns_none() {
        let (client, mut server) = tokio::io::duplex(1024);
        drop(client); // close the write end

        let result = recv_message(&mut server).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn send_rejects_oversized_message() {
        let (mut client, _server) = tokio::io::duplex(2 * MAX_MESSAGE_SIZE as usize);
        let oversized = vec![0u8; MAX_MESSAGE_SIZE as usize + 1];
        let result = send_message(&mut client, &oversized).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("too large"), "error should mention size: got '{}'", err);
    }

    #[tokio::test]
    async fn recv_rejects_oversized_length_prefix() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let fake_len = (MAX_MESSAGE_SIZE + 1).to_be_bytes();
        tokio::io::AsyncWriteExt::write_all(&mut client, &fake_len)
            .await
            .unwrap();
        let result = recv_message(&mut server).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("too large"), "error should mention size: got '{}'", err);
    }

    #[tokio::test]
    async fn send_and_recv_empty_message() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        send_message(&mut client, &[]).await.unwrap();
        let received = recv_message(&mut server).await.unwrap().unwrap();
        assert!(received.is_empty());
    }

    #[tokio::test]
    async fn send_and_recv_at_max_size() {
        let (mut client, mut server) = tokio::io::duplex(MAX_MESSAGE_SIZE as usize + 64);
        let data = vec![42u8; MAX_MESSAGE_SIZE as usize];
        send_message(&mut client, &data).await.unwrap();
        let received = recv_message(&mut server).await.unwrap().unwrap();
        assert_eq!(received.len(), MAX_MESSAGE_SIZE as usize);
    }
}
