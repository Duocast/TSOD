use std::{collections::HashSet, net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{anyhow, Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use sqlx::PgPool;
use tokio::{fs, net::TcpListener};
use tracing::{info, warn};
use uuid::Uuid;
use vp_control::ids::ServerId;

#[derive(Clone)]
pub struct UploadHttpConfig {
    pub listen: String,
    pub public_base: String,
    pub upload_dir: PathBuf,
    pub max_file_size_bytes: u64,
    pub allowed_mime: HashSet<String>,
    pub default_server_id: ServerId,
}

#[derive(Clone)]
pub struct UploadHttpServer {
    cfg: UploadHttpConfig,
    pool: PgPool,
}

#[derive(Serialize)]
struct UploadResponse {
    attachment_id: String,
    download_url: String,
    content_type: String,
    size_bytes: u64,
    filename: String,
}

impl UploadHttpServer {
    pub fn new(cfg: UploadHttpConfig, pool: PgPool) -> Self {
        Self { cfg, pool }
    }

    pub async fn serve(self) -> Result<()> {
        fs::create_dir_all(&self.cfg.upload_dir)
            .await
            .context("create upload dir")?;

        let addr: SocketAddr = self.cfg.listen.parse().context("parse upload listen")?;
        let listener = TcpListener::bind(addr).await?;
        info!(%addr, "upload HTTP listening");

        let shared = Arc::new(self);
        loop {
            let (stream, _) = listener.accept().await?;
            let state = shared.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service =
                    hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                        let state = state.clone();
                        async move {
                            match state.route(req).await {
                                Ok(resp) => Ok::<_, hyper::Error>(resp),
                                Err(e) => {
                                    warn!(error=%e, "upload HTTP request failed");
                                    Ok(error_response(
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        "internal error",
                                    ))
                                }
                            }
                        }
                    });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    }

    async fn route(&self, req: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>> {
        match (req.method(), req.uri().path()) {
            (&Method::POST, "/upload") => self.handle_upload(req).await,
            (&Method::GET, path) if path.starts_with("/files/") => {
                let id = path.trim_start_matches("/files/");
                self.handle_download(id).await
            }
            _ => Ok(error_response(StatusCode::NOT_FOUND, "not found")),
        }
    }

    async fn handle_upload(
        &self,
        req: Request<hyper::body::Incoming>,
    ) -> Result<Response<Full<Bytes>>> {
        let headers = req.headers();
        let filename = headers
            .get("x-filename")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("missing x-filename"))?
            .to_string();
        let content_type = headers
            .get("x-content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();

        if !self.cfg.allowed_mime.contains(&content_type) {
            return Ok(error_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported content-type",
            ));
        }

        let channel_id = headers
            .get("x-channel-id")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("missing x-channel-id"))?
            .to_string();
        let uploader_user_id = headers
            .get("x-uploader-user-id")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("missing x-uploader-user-id"))?
            .to_string();

        let body = req.into_body().collect().await?.to_bytes();
        if body.len() as u64 > self.cfg.max_file_size_bytes {
            return Ok(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "file too large",
            ));
        }

        let attachment_id = Uuid::new_v4();
        let file_path = self.cfg.upload_dir.join(attachment_id.to_string());
        fs::write(&file_path, &body)
            .await
            .context("write upload file")?;

        sqlx::query(
            r#"
            INSERT INTO attachments (
              id, server_id, channel_id, uploader_user_id, filename, content_type, size_bytes, storage_path, created_at
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,NOW())
            "#,
        )
        .bind(attachment_id)
        .bind(self.cfg.default_server_id.0)
        .bind(Uuid::parse_str(&channel_id).context("invalid channel id")?)
        .bind(Uuid::parse_str(&uploader_user_id).context("invalid uploader user id")?)
        .bind(&filename)
        .bind(&content_type)
        .bind(body.len() as i64)
        .bind(file_path.to_string_lossy().to_string())
        .execute(&self.pool)
        .await
        .context("insert attachments row")?;

        let payload = UploadResponse {
            attachment_id: attachment_id.to_string(),
            download_url: format!(
                "{}/files/{}",
                self.cfg.public_base.trim_end_matches('/'),
                attachment_id
            ),
            content_type,
            size_bytes: body.len() as u64,
            filename,
        };
        let data = serde_json::to_vec(&payload)?;
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(data)))
            .unwrap())
    }

    async fn handle_download(&self, attachment_id: &str) -> Result<Response<Full<Bytes>>> {
        let id = match Uuid::parse_str(attachment_id) {
            Ok(v) => v,
            Err(_) => {
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid attachment id",
                ))
            }
        };

        let row =
            sqlx::query(r#"SELECT content_type, storage_path FROM attachments WHERE id = $1"#)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;

        let Some(row) = row else {
            return Ok(error_response(StatusCode::NOT_FOUND, "not found"));
        };

        let content_type: String = sqlx::Row::get(&row, "content_type");
        let storage_path: String = sqlx::Row::get(&row, "storage_path");
        let bytes = match fs::read(&storage_path).await {
            Ok(v) => v,
            Err(_) => return Ok(error_response(StatusCode::NOT_FOUND, "missing file")),
        };

        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", content_type)
            .body(Full::new(Bytes::from(bytes)))
            .unwrap())
    }
}

fn error_response(code: StatusCode, message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(code)
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from(message.to_string())))
        .unwrap()
}
