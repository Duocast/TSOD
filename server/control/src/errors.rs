use thiserror::Error;

pub type ControlResult<T> = Result<T, ControlError>;

#[derive(Error, Debug)]
pub enum ControlError {
    #[error("not found: {0}")]
    NotFound(&'static str),

    #[error("already exists: {0}")]
    AlreadyExists(&'static str),

    #[error("permission denied: {0}")]
    PermissionDenied(&'static str),

    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),

    #[error("failed precondition: {0}")]
    FailedPrecondition(&'static str),
}
