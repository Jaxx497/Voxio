use thiserror::Error;

/// Errors that can occur during playback, decoding, seeking, or device I/O.
#[derive(Clone, Error, Debug, PartialEq)]
pub enum VoxError {
    /// The file could not be opened or is not a regular file.
    #[error("Failed to open file: {0}")]
    FileOpen(String),

    /// The output device could not be opened or rebuilt.
    #[error("output error: {0}")]
    Device(String),

    /// The track could not be decoded.
    #[error("decoder error: {0}")]
    Decoder(String),

    /// Resampling to the output rate failed.
    #[error("resampler error: {0}")]
    Resampler(String),

    /// A seek could not be completed.
    #[error("seek error: {0}")]
    Seek(String),

    /// No active track to queue after; the engine is idle.
    #[error("no active track to queue after")]
    NotActive,

    /// The engine has shut down and no longer accepts commands.
    #[error("Vox channel closed")]
    ChannelClosed,
}

pub type Result<T> = std::result::Result<T, VoxError>;
