use anyhow::Result;
use http_body_util::Full;
use hyper::{body::Bytes, Request, Response};
use hyper_util::rt::TokioIo;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use std::{net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;
use tracing::info;

use crate::MetricsConfig;

pub struct MetricsServer {
    handle: PrometheusHandle,
    cfg: MetricsConfig,
}

impl MetricsServer {
    pub fn install(cfg: MetricsConfig) -> Result<Self> {
        // Install global recorder once. Panics if installed twice; call from main init.
        let handle = PrometheusBuilder::new()
            // Optional: allow only vp_* metrics or keep default.
            .set_buckets_for_metric(
                Matcher::Prefix(format!("{}_voice_", cfg.namespace)),
                &[0.001, 0.005, 0.01, 0.02, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0],
            )?
            .install_recorder()?;

        Ok(Self { handle, cfg })
    }

    pub async fn serve(self) -> Result<()> {
        let addr: SocketAddr = self.cfg.listen.parse()?;
        let listener = TcpListener::bind(addr).await?;
        info!("metrics listening on http://{}/metrics", addr);

        let handle = Arc::new(self.handle);

        loop {
            let (stream, _) = listener.accept().await?;
            let handle = handle.clone();

            tokio::spawn(async move {
                let io = TokioIo::new(stream);

                let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                    let handle = handle.clone();
                    async move { metrics_handler(req, handle).await }
                });

                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    }
}

async fn metrics_handler(
    req: Request<hyper::body::Incoming>,
    handle: Arc<PrometheusHandle>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    if req.uri().path() != "/metrics" {
        return Ok(Response::builder()
            .status(404)
            .body(Full::new(Bytes::from("not found")))
            .unwrap());
    }

    let body = handle.render();
    Ok(Response::builder()
        .status(200)
        .header("content-type", "text/plain; version=0.0.4")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}
