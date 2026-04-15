use thiserror::{self, Error};

#[derive(Error, Debug)]
pub enum VoxError {
    /// File could not be opened
    #[error("Failed to open file: {0}")]
    FileOpen(String),

    // Audio output error
    #[error("output error: {0}")]
    Output(String),

    #[error("decoder error: {0}")]
    Decoder(String),

    #[error("resampler error: {0}")]
    Resampler(String),

    #[error("seek error: {0}")]
    Seek(String),

    #[error("Vox channel closed")]
    ChannelClosed,
}

pub type Result<T> = std::result::Result<T, VoxError>;
