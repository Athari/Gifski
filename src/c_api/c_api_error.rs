use crate::GifResult;
use std::fmt;
use std::io;
use std::os::raw::c_int;

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, strum::FromRepr)]
#[allow(non_camel_case_types)]
#[allow(clippy::upper_case_acronyms)]
pub enum GifskiError {
    OK = 0,
    NULL_ARG,
    INVALID_STATE,
    QUANT,
    GIF,
    THREAD_LOST,
    NOT_FOUND,
    PERMISSION_DENIED,
    ALREADY_EXISTS,
    INVALID_INPUT,
    TIMED_OUT,
    WRITE_ZERO,
    INTERRUPTED,
    UNEXPECTED_EOF,
    ABORTED,
    OTHER,
}

impl From<GifskiError> for io::Error {
    #[cold]
    fn from(g: GifskiError) -> Self {
        use GifskiError::*;
        use std::io::ErrorKind as EK;
        match g {
            OK => panic!("wrong err code"),
            NOT_FOUND => EK::NotFound,
            PERMISSION_DENIED => EK::PermissionDenied,
            ALREADY_EXISTS => EK::AlreadyExists,
            INVALID_INPUT => EK::InvalidInput,
            TIMED_OUT => EK::TimedOut,
            WRITE_ZERO => EK::WriteZero,
            INTERRUPTED => EK::Interrupted,
            UNEXPECTED_EOF => EK::UnexpectedEof,
            _ => return Self::other(g),
        }.into()
    }
}

impl From<c_int> for GifskiError {
    #[cold]
    fn from(res: c_int) -> Self {
        usize::try_from(res).ok().and_then(Self::from_repr).unwrap_or(Self::OTHER)
    }
}

impl From<GifResult<()>> for GifskiError {
    #[cold]
    fn from(res: GifResult<()>) -> Self {
        use crate::error::Error::*;
        match res {
            Ok(()) => Self::OK,
            Err(err) => match err {
                Quant(_) => Self::QUANT,
                Pal(_) => Self::GIF,
                ThreadSend => Self::THREAD_LOST,
                Io(ref err) => err.kind().into(),
                Aborted => Self::ABORTED,
                Gifsicle | Gif(_) => Self::GIF,
                NoFrames => Self::INVALID_STATE,
                WrongSize(_) => Self::INVALID_INPUT,
                PNG(_) => Self::OTHER,
            },
        }
    }
}

impl From<io::ErrorKind> for GifskiError {
    #[cold]
    fn from(res: io::ErrorKind) -> Self {
        use std::io::ErrorKind as EK;
        match res {
            EK::NotFound => Self::NOT_FOUND,
            EK::PermissionDenied => Self::PERMISSION_DENIED,
            EK::AlreadyExists => Self::ALREADY_EXISTS,
            EK::InvalidInput | EK::InvalidData => Self::INVALID_INPUT,
            EK::TimedOut => Self::TIMED_OUT,
            EK::WriteZero => Self::WRITE_ZERO,
            EK::Interrupted => Self::INTERRUPTED,
            EK::UnexpectedEof => Self::UNEXPECTED_EOF,
            _ => Self::OTHER,
        }
    }
}

impl std::error::Error for GifskiError {}

impl fmt::Display for GifskiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}
