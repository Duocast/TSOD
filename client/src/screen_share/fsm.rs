use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum StopReason {
    UserRequested,
    RemoteRequest,
    CaptureEnded,
    EncoderFailed(String),
    TransportFailed(String),
}

#[derive(Debug, Clone)]
pub enum ShareError {
    CapabilityProbeFailed(String),
    StartRequestRejected(String),
    MissingStartResponsePayload,
    NoNegotiatedStreams,
    NoSupportedEncoder,
    CaptureBackendInit(String),
}

#[derive(Debug, Clone)]
pub enum ShareState {
    Idle,
    Starting { request_id: Uuid },
    Active { stream_id: String },
    Stopping { reason: StopReason },
    Error { reason: ShareError, retriable: bool },
}
