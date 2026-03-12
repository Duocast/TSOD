use crate::screen_share::runtime_probe::EncodeBackendKind;

pub fn supports_backend(kind: EncodeBackendKind) -> bool {
    matches!(
        kind,
        EncodeBackendKind::MfHwVp9 | EncodeBackendKind::MfHwAv1
    )
}
