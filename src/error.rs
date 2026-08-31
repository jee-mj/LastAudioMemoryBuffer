use std::path::PathBuf;

pub const EX_CONFIG: i32 = 78;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Failure,
    PermanentBootstrap,
}

impl ExitCode {
    pub const fn as_i32(self) -> i32 {
        match self {
            Self::Failure => 1,
            Self::PermanentBootstrap => EX_CONFIG,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LambError {
    #[error("cannot establish inspectable daemon: {0}")]
    NonRestartableBootstrap(#[source] Box<LambError>),

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

    #[error("fatal daemon error: {0}")]
    DaemonFatal(String),

    #[error("capture error: {0}")]
    CaptureInvariant(&'static str),

    #[error("export error: {0}")]
    Export(String),

    #[error("export error: {0}")]
    ExportInvariant(&'static str),

    #[error("control error: {0}")]
    ControlInvariant(&'static str),
}

impl LambError {
    pub fn non_restartable_bootstrap(source: LambError) -> Self {
        Self::NonRestartableBootstrap(Box::new(source))
    }

    pub fn process_exit_code(&self) -> i32 {
        match self {
            Self::NonRestartableBootstrap(_) => ExitCode::PermanentBootstrap.as_i32(),
            _ => ExitCode::Failure.as_i32(),
        }
    }
}

pub type Result<T> = std::result::Result<T, LambError>;

pub fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> LambError {
    LambError::Io {
        path: path.into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_non_restartable_bootstrap_maps_to_exit_78() {
        let bootstrap = LambError::non_restartable_bootstrap(LambError::Control(
            "cannot bind listener".to_string(),
        ));
        assert_eq!(bootstrap.process_exit_code(), 78);
        assert_eq!(ExitCode::PermanentBootstrap.as_i32(), 78);
        assert_eq!(
            LambError::ControlInvariant("worker failed").process_exit_code(),
            1
        );
        assert_eq!(
            LambError::Config("bad cli input".to_string()).process_exit_code(),
            1
        );
    }
}
