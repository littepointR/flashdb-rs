//! Error-type surface tests.
//!
//! `error.rs` is the crate's whole public failure vocabulary, and it was the one
//! module the line-coverage run found completely unexercised (the other test
//! files only ever `matches!` on variants, never render them).  These tests pin
//! the user-visible text of every variant, prove that a flash-driver error is
//! carried through unchanged (the crate's design promise: unlike FlashDB-C's
//! `fdb_err_t`, the caller can still tell *which* operation failed and why), and
//! exercise the `std::error::Error` impl.

use flashdb::{Config, Error, Kvdb};
use flashdb_sim::{RamFlash, SimError};

type Sim = RamFlash<16384, 8>;

#[test]
fn display_covers_every_variant() {
    // Driver errors keep their own Display text and name the failed operation.
    assert_eq!(
        format!("{}", Error::<SimError>::FlashRead(SimError::Injected)),
        "flash read failed: simulated flash failure"
    );
    assert_eq!(
        format!("{}", Error::<SimError>::FlashWrite(SimError::OutOfBounds)),
        "flash write failed: simulated flash access out of bounds"
    );
    assert_eq!(
        format!(
            "{}",
            Error::<SimError>::FlashErase(SimError::UnalignedErase)
        ),
        "flash erase failed: erase not aligned to block"
    );
    // Database-level errors have fixed, stable messages.
    let cases: [(Error<SimError>, &str); 7] = [
        (Error::InvalidConfig, "invalid database configuration"),
        (
            Error::UnsupportedWriteGran,
            "unsupported flash write granularity",
        ),
        (
            Error::NotFormatted,
            "database is not formattable and header is damaged",
        ),
        (Error::SavedFull, "database is full"),
        (Error::InvalidName, "invalid key/name"),
        (Error::ReadCorrupted, "record integrity check failed"),
        (Error::OutOfBounds, "address out of database bounds"),
    ];
    for (err, expected) in cases {
        assert_eq!(format!("{err}"), expected);
    }
}

#[test]
fn driver_error_survives_a_failing_mount() {
    // Arm the simulator so the first flash operation of the mount fails; the
    // error must come back as a typed `Error::Flash*` carrying the simulator's
    // own error, not as a flattened "flash error" code.
    let mut flash = Sim::new(4096);
    flash.fail_after_ops(0);
    let err = match Kvdb::mount(flash, Config::new(4096, 16384)) {
        Ok(_) => panic!("mount succeeded despite an injected flash fault"),
        Err(e) => e,
    };

    assert!(
        matches!(
            err,
            Error::FlashRead(SimError::Injected)
                | Error::FlashWrite(SimError::Injected)
                | Error::FlashErase(SimError::Injected)
        ),
        "unexpected error after injected fault: {err:?}"
    );
    // ... and it is still renderable.
    let text = format!("{err}");
    assert!(text.starts_with("flash "), "unexpected message: {text}");
    assert!(text.contains("simulated flash failure"));
}

#[test]
fn error_is_copy_eq_and_debug_nameable() {
    let a = Error::<SimError>::SavedFull;
    let b = a; // Copy
    assert_eq!(a, b);
    assert_ne!(a, Error::<SimError>::NotFormatted);
    // Debug output names the variant, which is what `unwrap_err()`/`panic!`
    // diagnostics rely on.
    assert!(format!("{a:?}").contains("SavedFull"));
}

#[test]
fn invalid_geometry_is_reported_as_invalid_config() {
    // Sector size greater than the partition cannot be mounted.
    let flash = Sim::new(4096);
    let err = match Kvdb::mount(flash, Config::new(8192, 4096)) {
        Ok(_) => panic!("mount succeeded with an invalid geometry"),
        Err(e) => e,
    };
    assert_eq!(err, Error::InvalidConfig);
}

/// The `std::error::Error` impl is opt-in (`--features std`); when enabled the
/// database errors are plain leaf errors without a source.  This also pins the
/// feature itself: before this test existed the `std` build did not compile
/// (`extern crate std` was `#[cfg(test)]` only).
#[cfg(feature = "std")]
#[test]
fn std_error_impl_is_a_leaf_error() {
    use std::error::Error as _;

    let err = Error::<SimError>::SavedFull;
    assert!(err.source().is_none());
    assert_eq!(format!("{err}"), "database is full");
}
