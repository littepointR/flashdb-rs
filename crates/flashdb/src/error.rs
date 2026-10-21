//! Error types shared by the KV and TS databases.
//!
//! FlashDB-C reports failures with a C enum (`fdb_err_t`) and loses the
//! underlying flash driver error. The Rust design keeps the driver error
//! (`E`) and distinguishes *which* flash operation failed, so that a caller
//! driving an SPI NOR chip can decide whether a retry makes sense.

use core::fmt;

/// Result alias used throughout the crate.
pub type Result<T, E> = core::result::Result<T, Error<E>>;

/// Database error type, generic over the flash driver error.
///
/// `E` is [`Flash::Error`](crate::Flash::Error); it is carried through
/// unchanged instead of being flattened into a single "flash error" code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error<E> {
    /// The flash driver rejected a read.
    FlashRead(E),
    /// The flash driver rejected a write/program operation.
    FlashWrite(E),
    /// The flash driver rejected an erase.
    FlashErase(E),
    /// The database geometry in [`Config`](crate::Config) is invalid.
    InvalidConfig,
    /// The flash write granularity is not one of 1, 8, 32, 64, 128 or 256 bits.
    UnsupportedWriteGran,
    /// The database is marked "not formattable" and its on-flash headers are
    /// damaged, so mounting refuses to destroy existing data.
    NotFormatted,
    /// There is no room left for the record, even after garbage collection.
    SavedFull,
    /// The key / name is too long or empty where a name is required.
    InvalidName,
    /// A persisted record failed its integrity check.
    ReadCorrupted,
    /// An address range fell outside the database partition.
    ///
    /// This is a Rust-side safety net: FlashDB-C relies on the FAL partition
    /// layer to bound-check, which this crate deliberately does not require.
    OutOfBounds,
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::FlashRead(e) => write!(f, "flash read failed: {e}"),
            Error::FlashWrite(e) => write!(f, "flash write failed: {e}"),
            Error::FlashErase(e) => write!(f, "flash erase failed: {e}"),
            Error::InvalidConfig => f.write_str("invalid database configuration"),
            Error::UnsupportedWriteGran => f.write_str("unsupported flash write granularity"),
            Error::NotFormatted => f.write_str("database is not formattable and header is damaged"),
            Error::SavedFull => f.write_str("database is full"),
            Error::InvalidName => f.write_str("invalid key/name"),
            Error::ReadCorrupted => f.write_str("record integrity check failed"),
            Error::OutOfBounds => f.write_str("address out of database bounds"),
        }
    }
}

#[cfg(feature = "std")]
impl<E: fmt::Display + fmt::Debug> std::error::Error for Error<E> {}
