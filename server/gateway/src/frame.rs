use anyhow::{anyhow, Result};
use bytes::{Bytes, BytesMut};
use prost::Message;

pub const MAX_CONTROL_FRAME_LEN: usize = 64 * 1024;

pub async fn read_delimited<M: Message + Default>(
    recv: &mut quinn::RecvStream,
    max_size: usize,
) -> Result<M> {
    let len = read_varint_u64(recv).await? as usize;
    validate_frame_len(len, max_size)?;

    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(M::decode(&buf[..])?)
}

pub async fn write_delimited<M: Message>(send: &mut quinn::SendStream, msg: &M) -> Result<()> {
    let mut body = BytesMut::with_capacity(msg.encoded_len());
    msg.encode(&mut body)?;
    write_delimited_bytes(send, body.freeze()).await
}

pub async fn write_delimited_bytes(send: &mut quinn::SendStream, body: Bytes) -> Result<()> {
    let mut frame = BytesMut::with_capacity(encoded_varint_len(body.len() as u64) + body.len());
    write_varint_u64_to_buf(&mut frame, body.len() as u64);
    frame.extend_from_slice(&body);
    send.write_all(&frame).await?;
    Ok(())
}

async fn read_varint_u64(recv: &mut quinn::RecvStream) -> Result<u64> {
    let mut buf = [0u8; 10];
    for i in 0..10 {
        recv.read_exact(&mut buf[i..=i]).await.map_err(|e| {
            anyhow!(
                "unexpected EOF while reading varint length after {} bytes: {e}",
                i
            )
        })?;
        if (buf[i] & 0x80) == 0 {
            let (value, _) = parse_varint_u64(&buf[..=i])?;
            return Ok(value);
        }
    }
    Err(anyhow!("varint too long"))
}


fn validate_frame_len(len: usize, max_size: usize) -> Result<()> {
    if len == 0 {
        return Err(anyhow!("zero-length message"));
    }
    if len > max_size {
        return Err(anyhow!("message too large: {} > {}", len, max_size));
    }
    Ok(())
}

fn encoded_varint_len(mut v: u64) -> usize {
    let mut i = 1;
    while v >= 0x80 {
        v >>= 7;
        i += 1;
    }
    i
}

fn write_varint_u64_to_buf(out: &mut BytesMut, mut v: u64) {
    while v >= 0x80 {
        out.extend_from_slice(&[((v as u8) & 0x7f) | 0x80]);
        v >>= 7;
    }
    out.extend_from_slice(&[v as u8]);
}

pub(crate) fn parse_varint_u64(input: &[u8]) -> Result<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    for (idx, byte) in input.iter().copied().enumerate().take(10) {
        result |= ((byte & 0x7f) as u64) << shift;
        if (byte & 0x80) == 0 {
            return Ok((result, idx + 1));
        }
        shift += 7;
    }
    Err(anyhow!("varint too long"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::voiceplatform::v1 as pb;

    #[test]
    fn varint_invalid_rejected() {
        let input = [0x80u8; 10];
        assert!(parse_varint_u64(&input).is_err());
    }

    #[test]
    fn oversized_frame_rejected() {
        let too_big = MAX_CONTROL_FRAME_LEN + 1;
        assert!(too_big > MAX_CONTROL_FRAME_LEN);
    }

    #[test]
    fn delimited_roundtrip_bytes() {
        let msg = pb::ServerToClient {
            request_id: Some(pb::RequestId { value: 9 }),
            ..Default::default()
        };
        let mut body = BytesMut::with_capacity(msg.encoded_len());
        msg.encode(&mut body).unwrap();
        let mut frame = BytesMut::new();
        write_varint_u64_to_buf(&mut frame, body.len() as u64);
        frame.extend_from_slice(&body);

        let (len, consumed) = parse_varint_u64(&frame).unwrap();
        let decoded =
            pb::ServerToClient::decode(&frame[consumed..consumed + len as usize]).unwrap();
        assert_eq!(decoded.request_id.unwrap().value, 9);
    }
}
