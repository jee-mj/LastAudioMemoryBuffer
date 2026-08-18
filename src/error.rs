use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum LambError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("validation error: {0}")]
    Validation(String),

    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("persistence failed ({operation}); cleanup also failed ({cleanup})")]
    PersistenceCleanup {
        operation: Box<LambError>,
        cleanup: Box<LambError>,
    },

    #[error("publication outcome is indeterminate after: {operation}")]
    IndeterminatePublication { operation: Box<LambError> },

    #[error("unidentified staging at {path} requires manual removal or recovery")]
    UnidentifiedStagingCleanup { path: PathBuf },

    #[error("control error: {0}")]
    Control(String),

    #[error("capture error: {0}")]
    Capture(String),

    #[error("capture error: {0}")]
    CaptureInvariant(&'static str),

    #[error("export error: {0}")]
    Export(String),

    #[error("export error: {0}")]
    ExportInvariant(&'static str),

    #[error("control error: {0}")]
    ControlInvariant(&'static str),
}

pub type Result<T> = std::result::Result<T, LambError>;

pub fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> LambError {
    LambError::Io {
        path: path.into(),
        source,
    }
}
