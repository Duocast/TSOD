use anyhow::{anyhow, Result};
use bytes::{Buf, BufMut, BytesMut};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Read a single length-delimited protobuf message.
pub async fn read_delimited<M: Message + Default>(
    recv: &mut quinn::RecvStream,
    max_size: usize,
) -> Result<M> {
    let len = read_varint_u64(recv).await? as usize;
    if len == 0 {
        return Err(anyhow!("zero-length message"));
    }
    if len > max_size {
        return Err(anyhow!("message too large: {} > {}", len, max_size));
    }

    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(M::decode(&buf[..])?)
}

/// Write a single length-delimited protobuf message.
pub async fn write_delimited<M: Message>(
    send: &mut quinn::SendStream,
    msg: &M,
) -> Result<()> {
    let mut body = BytesMut::with_capacity(msg.encoded_len());
    msg.encode(&mut body)?;

    write_varint_u64(send, body.len() as u64).await?;
    send.write_all(&body).await?;
    send.flush().await?;
    Ok(())
}

async fn read_varint_u64(recv: &mut quinn::RecvStream) -> Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;

    for _ in 0..10 {
        let mut b = [0u8; 1];
        recv.read_exact(&mut b).await?;
        let byte = b[0];

        result |= ((byte & 0x7f) as u64) << shift;
        if (byte & 0x80) == 0 {
            return Ok(result);
        }
        shift += 7;
    }
    Err(anyhow!("varint too long"))
}

async fn write_varint_u64(send: &mut quinn::SendStream, mut v: u64) -> Result<()> {
    let mut buf = [0u8; 10];
    let mut i = 0;

    while v >= 0x80 {
        buf[i] = (v as u8) | 0x80;
        v >>= 7;
        i += 1;
    }
    buf[i] = v as u8;
    i += 1;

    send.write_all(&buf[..i]).await?;
    Ok(())
}
