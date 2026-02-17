let cfg = vp_metrics::MetricsConfig::default();
let metrics = vp_metrics::MetricsServer::install(cfg.clone())?;
tokio::spawn(metrics.serve());

// Then create metric sets:
let gw_metrics = vp_metrics::gateway::GatewayMetrics::new(cfg.namespace);
let voice_metrics = vp_metrics::voice::VoiceMetricsImpl::new(cfg.namespace, vp_metrics::LabelPolicy::default());
