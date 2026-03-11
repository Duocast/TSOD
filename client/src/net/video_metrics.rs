#[derive(Debug, Default, Clone)]
pub struct VideoMetrics {
    pub capture_frames: u64,
    pub capture_queue_overflows: u64,
    pub encode_frames: u64,
    pub encode_errors: u64,
    pub decode_frames: u64,
    pub decode_errors: u64,
    pub tx_bitrate_bps: u64,
    pub rx_bitrate_bps: u64,
    pub encode_p50_ms: f32,
    pub encode_p95_ms: f32,
    pub decode_p50_ms: f32,
    pub decode_p95_ms: f32,
    pub freeze_count: u64,
    pub freeze_ms_p95: f32,
    pub active_layer: u8,
    pub backend_label: String,
}

impl VideoMetrics {
    pub fn record_capture_frame(&mut self) {
        self.capture_frames = self.capture_frames.saturating_add(1);
    }

    pub fn record_capture_queue_overflow(&mut self, count: u64) {
        self.capture_queue_overflows = self.capture_queue_overflows.saturating_add(count);
    }

    pub fn record_encode_frame(&mut self) {
        self.encode_frames = self.encode_frames.saturating_add(1);
    }

    pub fn record_encode_error(&mut self) {
        self.encode_errors = self.encode_errors.saturating_add(1);
    }

    pub fn record_decode_frame(&mut self) {
        self.decode_frames = self.decode_frames.saturating_add(1);
    }

    pub fn record_decode_error(&mut self) {
        self.decode_errors = self.decode_errors.saturating_add(1);
    }

    pub fn set_tx_bitrate_bps(&mut self, bitrate_bps: u64) {
        self.tx_bitrate_bps = bitrate_bps;
    }

    pub fn set_rx_bitrate_bps(&mut self, bitrate_bps: u64) {
        self.rx_bitrate_bps = bitrate_bps;
    }

    pub fn set_encode_latencies(&mut self, p50_ms: f32, p95_ms: f32) {
        self.encode_p50_ms = p50_ms.max(0.0);
        self.encode_p95_ms = p95_ms.max(0.0);
    }

    pub fn set_decode_latencies(&mut self, p50_ms: f32, p95_ms: f32) {
        self.decode_p50_ms = p50_ms.max(0.0);
        self.decode_p95_ms = p95_ms.max(0.0);
    }

    pub fn record_freeze(&mut self) {
        self.freeze_count = self.freeze_count.saturating_add(1);
    }

    pub fn set_freeze_ms_p95(&mut self, freeze_ms_p95: f32) {
        self.freeze_ms_p95 = freeze_ms_p95.max(0.0);
    }

    pub fn set_active_layer(&mut self, active_layer: u8) {
        self.active_layer = active_layer;
    }

    pub fn set_backend_label(&mut self, backend_label: impl Into<String>) {
        self.backend_label = backend_label.into();
    }
}
