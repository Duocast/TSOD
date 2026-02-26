use std::{collections::HashSet, path::PathBuf, sync::Arc};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};
use tracing::warn;
use uuid::Uuid;
use vp_control::ids::{ChannelId, ServerId, UserId};

use crate::{
    frame::{read_delimited, write_delimited},
    proto::voiceplatform::v1 as pb,
};

const MEDIA_MAX_MSG: usize = 64 * 1024;
const DEFAULT_MAX_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;
const DEFAULT_MAX_CHUNK: usize = 64 * 1024;

#[derive(Clone)]
pub struct MediaService {
    pool: PgPool,
    upload_dir: PathBuf,
    default_server_id: ServerId,
    max_upload_bytes: u64,
    inline_safe_mime: HashSet<&'static str>,
}

impl MediaService {
    pub async fn new(
        pool: PgPool,
        upload_dir: PathBuf,
        default_server_id: ServerId,
    ) -> Result<Self> {
        fs::create_dir_all(&upload_dir)
            .await
            .context("create media upload dir")?;
        Ok(Self {
            pool,
            upload_dir,
            default_server_id,
            max_upload_bytes: DEFAULT_MAX_UPLOAD_BYTES,
            inline_safe_mime: HashSet::from([
                "image/png",
                "image/jpeg",
                "image/webp",
                "image/gif",
                "text/plain",
            ]),
        })
    }

    pub async fn handle_stream(
        &self,
        mut send: quinn::SendStream,
        mut recv: quinn::RecvStream,
        user_id: UserId,
    ) -> Result<()> {
        let req: pb::MediaRequest = read_delimited(&mut recv, MEDIA_MAX_MSG).await?;
        match req.payload {
            Some(pb::media_request::Payload::UploadInit(init)) => {
                self.handle_upload(&mut send, &mut recv, user_id, init)
                    .await
            }
            Some(pb::media_request::Payload::DownloadRequest(req)) => {
                self.handle_download(&mut send, req, user_id).await
            }
            Some(pb::media_request::Payload::ThumbRequest(_)) => {
                let resp = pb::MediaResponse {
                    payload: Some(pb::media_response::Payload::Error(pb::MediaError {
                        message: "thumbnail generation not implemented".into(),
                    })),
                };
                write_delimited(&mut send, &resp).await?;
                send.finish()?;
                Ok(())
            }
            None => Err(anyhow!("empty media request")),
        }
    }

    async fn handle_upload(
        &self,
        send: &mut quinn::SendStream,
        recv: &mut quinn::RecvStream,
        user_id: UserId,
        init: pb::UploadInit,
    ) -> Result<()> {
        let channel_id = init
            .channel_id
            .as_ref()
            .ok_or_else(|| anyhow!("missing channel_id"))?
            .value
            .clone();
        let channel_uuid = Uuid::parse_str(&channel_id).context("invalid channel id")?;
        if !self.user_in_channel(channel_uuid, user_id.0).await? {
            return self.write_error(send, "not authorized for channel").await;
        }
        if init.size_bytes == 0 || init.size_bytes > self.max_upload_bytes {
            return self.write_error(send, "invalid upload size").await;
        }

        let attachment_id = Uuid::new_v4();
        let shard = &attachment_id.to_string()[..2];
        let dir = self.upload_dir.join(shard);
        fs::create_dir_all(&dir).await?;
        let path = dir.join(attachment_id.to_string());
        let mut file = fs::File::create(&path)
            .await
            .context("create upload file")?;

        let ready = pb::MediaResponse {
            payload: Some(pb::media_response::Payload::UploadReady(pb::UploadReady {
                attachment_id: Some(pb::AssetId {
                    value: attachment_id.to_string(),
                }),
                max_chunk: DEFAULT_MAX_CHUNK as u32,
            })),
        };
        write_delimited(send, &ready).await?;

        let mut remaining = init.size_bytes;
        let mut total = 0u64;
        let mut hasher = Sha256::new();
        let mut sniff_buf = Vec::with_capacity(1024);
        let mut buf = vec![0u8; DEFAULT_MAX_CHUNK];

        while remaining > 0 {
            let want = usize::min(DEFAULT_MAX_CHUNK, remaining as usize);
            recv.read_exact(&mut buf[..want])
                .await
                .context("read upload bytes")?;
            let chunk = &buf[..want];
            file.write_all(chunk).await.context("write upload bytes")?;
            hasher.update(chunk);
            if sniff_buf.len() < 1024 {
                let copy = usize::min(1024 - sniff_buf.len(), chunk.len());
                sniff_buf.extend_from_slice(&chunk[..copy]);
            }
            total += want as u64;
            remaining -= want as u64;
            if total > self.max_upload_bytes {
                return self.write_error(send, "upload exceeds server limit").await;
            }
        }
        file.flush().await?;

        let sniffed = infer::get(&sniff_buf).map(|k| k.mime_type().to_string());
        let mime = sniffed.unwrap_or_else(|| init.mime.clone());
        let hash = hex::encode(hasher.finalize());

        sqlx::query(
            r#"INSERT INTO attachments
            (id, server_id, channel_id, uploader_user_id, filename, content_type, size_bytes, storage_path, sha256, created_at)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,NOW())"#,
        )
        .bind(attachment_id)
        .bind(self.default_server_id.0)
        .bind(channel_uuid)
        .bind(user_id.0)
        .bind(&init.filename)
        .bind(&mime)
        .bind(total as i64)
        .bind(path.to_string_lossy().to_string())
        .bind(&hash)
        .execute(&self.pool)
        .await
        .context("insert attachment row")?;

        let complete = pb::MediaResponse {
            payload: Some(pb::media_response::Payload::UploadComplete(
                pb::UploadComplete {
                    attachment_id: Some(pb::AssetId {
                        value: attachment_id.to_string(),
                    }),
                    size_bytes: total,
                    sha256: hash,
                    mime,
                    filename: init.filename,
                },
            )),
        };
        write_delimited(send, &complete).await?;
        send.finish()?;
        Ok(())
    }

    async fn handle_download(
        &self,
        send: &mut quinn::SendStream,
        req: pb::DownloadRequest,
        user_id: UserId,
    ) -> Result<()> {
        let attachment_id = req
            .attachment_id
            .as_ref()
            .ok_or_else(|| anyhow!("missing attachment id"))?
            .value
            .clone();
        let id = Uuid::parse_str(&attachment_id).context("invalid attachment id")?;

        let row = sqlx::query(
            r#"SELECT channel_id, filename, content_type, size_bytes, storage_path, sha256, quarantined
               FROM attachments WHERE id=$1"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return self.write_error(send, "attachment not found").await;
        };

        let channel_id: Uuid = row.get("channel_id");
        if !self.user_in_channel(channel_id, user_id.0).await? {
            return self.write_error(send, "not authorized").await;
        }
        let quarantined: bool = row.get("quarantined");
        if quarantined {
            return self.write_error(send, "attachment quarantined").await;
        }

        let mime: String = row.get("content_type");
        let filename: String = row.get("filename");
        let size_bytes: i64 = row.get("size_bytes");
        let storage_path: String = row.get("storage_path");
        let sha256: Option<String> = row.get("sha256");

        let meta = pb::MediaResponse {
            payload: Some(pb::media_response::Payload::DownloadMeta(
                pb::DownloadMeta {
                    mime: mime.clone(),
                    filename,
                    size_bytes: size_bytes as u64,
                    sha256: sha256.unwrap_or_default(),
                    safe_inline: self.inline_safe_mime.contains(mime.as_str()),
                },
            )),
        };
        write_delimited(send, &meta).await?;

        let mut file = fs::File::open(&storage_path)
            .await
            .context("open attachment file")?;
        let mut buf = vec![0u8; DEFAULT_MAX_CHUNK];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            send.write_all(&buf[..n]).await?;
        }
        send.finish()?;
        Ok(())
    }

    async fn user_in_channel(&self, channel_id: Uuid, user_id: Uuid) -> Result<bool> {
        let exists = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM channel_members WHERE channel_id=$1 AND user_id=$2 LIMIT 1",
        )
        .bind(channel_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        Ok(exists)
    }

    async fn write_error(&self, send: &mut quinn::SendStream, message: &str) -> Result<()> {
        warn!(%message, "media request rejected");
        let resp = pb::MediaResponse {
            payload: Some(pb::media_response::Payload::Error(pb::MediaError {
                message: message.to_string(),
            })),
        };
        write_delimited(send, &resp).await?;
        send.finish()?;
        Ok(())
    }
}
