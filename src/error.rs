use crate::WrongSizeError;
use std::io;
use std::num::TryFromIntError;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Internal error
    #[error("Internal error; unexpectedly aborted")]
    ThreadSend,
    #[error("aborted")]
    Aborted,
    #[error("gifsicle failure")]
    Gifsicle,
    #[error("GIF encoding error: {0}")]
    Gif(gif::EncodingError),
    #[error("Found no usable frames to encode")]
    NoFrames,
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    PNG(String),
    #[error("{0}")]
    WrongSize(String),
    #[error("pngquant error: {0}")]
    Quant(#[from] imagequant::liq_error),
    #[error("gif dispose error: {0}")]
    Pal(#[from] gif_dispose::Error),
}

impl From<TryFromIntError> for Error {
    #[cold]
    fn from(e: TryFromIntError) -> Self {
        Self::WrongSize(e.to_string())
    }
}

impl From<WrongSizeError> for Error {
    #[cold]
    fn from(_: WrongSizeError) -> Self {
        Self::WrongSize("wrong size".to_string())
    }
}

impl From<resize::Error> for Error {
    #[cold]
    fn from(e: resize::Error) -> Self {
        Self::WrongSize(e.to_string())
    }
}

impl From<std::collections::TryReserveError> for Error {
    #[cold]
    fn from(_: std::collections::TryReserveError) -> Self {
        Self::Io(io::ErrorKind::OutOfMemory.into())
    }
}

#[doc(hidden)]
pub type CatResult<T, E = Error> = Result<T, E>;

/// Alias for `Result` with gifski's [`Error`]
pub type GifResult<T, E = Error> = Result<T, E>;

impl From<gif::EncodingError> for Error {
    #[cold]
    fn from(err: gif::EncodingError) -> Self {
        match err {
            gif::EncodingError::Io(err) => err.into(),
            other => Self::Gif(other),
        }
    }
}

impl<T> From<ordered_channel::SendError<T>> for Error {
    #[cold]
    fn from(_: ordered_channel::SendError<T>) -> Self {
        Self::ThreadSend
    }
}

impl From<ordered_channel::RecvError> for Error {
    #[cold]
    fn from(_: ordered_channel::RecvError) -> Self {
        Self::Aborted
    }
}

impl From<Box<dyn std::any::Any + Send>> for Error {
    #[cold]
    fn from(_panic: Box<dyn std::any::Any + Send>) -> Self {
        Self::ThreadSend
    }
}
