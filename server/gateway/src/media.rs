use std::{collections::HashSet, path::PathBuf};

use anyhow::{anyhow, Context, Result};
use image::GenericImageView;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tracing::warn;
use uuid::Uuid;
use vp_control::ids::{ServerId, UserId};

use crate::{
    frame::{read_delimited, write_delimited},
    proto::voiceplatform::v1 as pb,
};

const MEDIA_MAX_MSG: usize = 64 * 1024;
const DEFAULT_MAX_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_MAX_FILE_COUNT: u32 = 2000;
const DEFAULT_MAX_CHUNK: usize = 64 * 1024;
const DEFAULT_THUMB_SIZE: u32 = 320;

#[derive(Clone)]
pub struct MediaService {
    pool: PgPool,
    upload_dir: PathBuf,
    default_server_id: ServerId,
    max_upload_bytes: u64,
    max_total_bytes: u64,
    max_file_count: u32,
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
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_file_count: DEFAULT_MAX_FILE_COUNT,
            inline_safe_mime: HashSet::from([
                "image/png",
                "image/jpeg",
                "image/webp",
                "image/gif",
                "image/avif",
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
            Some(pb::media_request::Payload::ThumbRequest(req)) => {
                self.handle_thumb(&mut send, req, user_id).await
            }
            Some(pb::media_request::Payload::MediaQuotaRequest(_)) => {
                self.handle_get_quota(&mut send, user_id).await
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

        let (used_bytes, file_count) = self.user_quota(user_id.0).await?;
        if file_count >= self.max_file_count {
            return self
                .write_error(send, "attachment count quota exceeded")
                .await;
        }
        if used_bytes.saturating_add(init.size_bytes) > self.max_total_bytes {
            return self
                .write_error(send, "attachment storage quota exceeded")
                .await;
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

        let (thumb_path, width, height, duration_ms) = self
            .extract_media_metadata_and_thumbnail(&path, &mime)
            .await
            .with_context(|| format!("extract metadata for {}", attachment_id))?;

        sqlx::query(
            r#"INSERT INTO attachments
            (id, server_id, channel_id, uploader_user_id, filename, content_type, size_bytes, storage_path, sha256, thumb_path, media_width, media_height, duration_ms, created_at)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,NOW())"#,
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
        .bind(thumb_path.as_deref())
        .bind(width.map(|v| v as i32))
        .bind(height.map(|v| v as i32))
        .bind(duration_ms.map(|v| v as i64))
        .execute(&self.pool)
        .await
        .context("insert attachment row")?;

        let download_uri = format!("vp-media://attachment/{attachment_id}");
        let thumbnail_uri = thumb_path
            .as_ref()
            .map(|_| format!("vp-media://thumb/{attachment_id}"))
            .unwrap_or_default();

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
                    download_uri,
                    thumbnail_uri,
                    width: width.unwrap_or_default(),
                    height: height.unwrap_or_default(),
                    duration_ms: duration_ms.unwrap_or_default(),
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

    async fn handle_thumb(
        &self,
        send: &mut quinn::SendStream,
        req: pb::ThumbRequest,
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
            r#"SELECT channel_id, content_type, storage_path, thumb_path, quarantined
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
        let storage_path: String = row.get("storage_path");
        let mut thumb_path: Option<String> = row.get("thumb_path");

        if thumb_path.is_none() {
            let origin = PathBuf::from(&storage_path);
            let (generated_thumb, width, height, duration_ms) = self
                .extract_media_metadata_and_thumbnail(&origin, &mime)
                .await?;
            if let Some(ref p) = generated_thumb {
                sqlx::query(
                    "UPDATE attachments SET thumb_path=$2, media_width=$3, media_height=$4, duration_ms=$5 WHERE id=$1",
                )
                .bind(id)
                .bind(p)
                .bind(width.map(|v| v as i32))
                .bind(height.map(|v| v as i32))
                .bind(duration_ms.map(|v| v as i64))
                .execute(&self.pool)
                .await?;
            }
            thumb_path = generated_thumb;
        }

        let Some(path) = thumb_path else {
            return self.write_error(send, "thumbnail unavailable").await;
        };

        let mut file = fs::File::open(&path).await.context("open thumbnail file")?;
        let meta = file.metadata().await?;
        let header = pb::MediaResponse {
            payload: Some(pb::media_response::Payload::ThumbMeta(pb::ThumbMeta {
                mime: "image/jpeg".to_string(),
                size_bytes: meta.len(),
            })),
        };
        write_delimited(send, &header).await?;

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

    async fn handle_get_quota(&self, send: &mut quinn::SendStream, user_id: UserId) -> Result<()> {
        let (used_bytes, file_count) = self.user_quota(user_id.0).await?;
        let resp = pb::MediaResponse {
            payload: Some(pb::media_response::Payload::MediaQuotaResponse(
                pb::MediaQuotaResponse {
                    used_bytes,
                    max_bytes: self.max_total_bytes,
                    file_count,
                    max_file_count: self.max_file_count,
                    max_single_file_bytes: self.max_upload_bytes,
                },
            )),
        };
        write_delimited(send, &resp).await?;
        send.finish()?;
        Ok(())
    }

    async fn user_quota(&self, user_id: Uuid) -> Result<(u64, u32)> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(size_bytes), 0) AS used_bytes, COUNT(*) AS file_count FROM attachments WHERE uploader_user_id=$1 AND NOT quarantined",
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await?;
        let used_bytes: i64 = row.get("used_bytes");
        let file_count: i64 = row.get("file_count");
        Ok((used_bytes.max(0) as u64, file_count.max(0) as u32))
    }

    async fn extract_media_metadata_and_thumbnail(
        &self,
        original_path: &PathBuf,
        mime: &str,
    ) -> Result<(Option<String>, Option<u32>, Option<u32>, Option<u64>)> {
        if mime.starts_with("image/") {
            return self
                .extract_image_metadata_and_thumbnail(original_path)
                .await;
        }
        if mime.starts_with("video/") {
            return self
                .extract_video_metadata_and_thumbnail(original_path)
                .await;
        }
        Ok((None, None, None, None))
    }

    async fn extract_image_metadata_and_thumbnail(
        &self,
        original_path: &PathBuf,
    ) -> Result<(Option<String>, Option<u32>, Option<u32>, Option<u64>)> {
        let source = original_path.clone();
        tokio::task::spawn_blocking(move || -> Result<(String, u32, u32)> {
            let image = image::open(&source).context("decode image")?;
            let (width, height) = image.dimensions();
            let thumb = image.thumbnail(DEFAULT_THUMB_SIZE, DEFAULT_THUMB_SIZE);
            let thumb_path = source.with_extension("thumb.jpg");
            thumb
                .save_with_format(&thumb_path, image::ImageFormat::Jpeg)
                .context("write image thumbnail")?;
            Ok((thumb_path.to_string_lossy().to_string(), width, height))
        })
        .await
        .context("image thumbnail task join")?
        .map(|(thumb, width, height)| (Some(thumb), Some(width), Some(height), None))
    }

    async fn extract_video_metadata_and_thumbnail(
        &self,
        original_path: &PathBuf,
    ) -> Result<(Option<String>, Option<u32>, Option<u32>, Option<u64>)> {
        let input = original_path.to_string_lossy().to_string();
        let ffprobe = Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-print_format")
            .arg("json")
            .arg("-show_streams")
            .arg("-show_format")
            .arg(&input)
            .output()
            .await
            .context("run ffprobe")?;
        if !ffprobe.status.success() {
            return Ok((None, None, None, None));
        }

        let parsed: serde_json::Value =
            serde_json::from_slice(&ffprobe.stdout).context("parse ffprobe json")?;
        let mut width = None;
        let mut height = None;
        if let Some(streams) = parsed.get("streams").and_then(|s| s.as_array()) {
            for stream in streams {
                if stream
                    .get("codec_type")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s == "video")
                {
                    width = stream
                        .get("width")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32);
                    height = stream
                        .get("height")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32);
                    break;
                }
            }
        }
        let duration_ms = parsed
            .get("format")
            .and_then(|f| f.get("duration"))
            .and_then(|d| d.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .map(|secs| (secs * 1000.0) as u64);

        let thumb_path = original_path.with_extension("thumb.jpg");
        let ffmpeg = Command::new("ffmpeg")
            .arg("-v")
            .arg("error")
            .arg("-y")
            .arg("-i")
            .arg(&input)
            .arg("-frames:v")
            .arg("1")
            .arg("-vf")
            .arg(format!(
                "thumbnail,scale='min({},iw)':-1",
                DEFAULT_THUMB_SIZE
            ))
            .arg(thumb_path.to_string_lossy().to_string())
            .output()
            .await
            .context("run ffmpeg")?;
        if !ffmpeg.status.success() {
            return Ok((None, width, height, duration_ms));
        }

        Ok((
            Some(thumb_path.to_string_lossy().to_string()),
            width,
            height,
            duration_ms,
        ))
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
