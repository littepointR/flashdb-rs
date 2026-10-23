//! # FlashDB-rs — a Rust rewrite of FlashDB
//!
//! FlashDB is an ultra-lightweight embedded database with two features:
//! a key-value store (**KVDB**) and a time-series log store (**TSDB**), both
//! designed to live directly on raw NOR/NAND flash without a file system.
//!
//! This crate is a from-scratch Rust rewrite. It is *not* a line-by-line
//! translation of the C code; it keeps the proven on-flash format (so data
//! written by FlashDB-C can be mounted, and vice versa) but rebuilds the
//! internals around Rust's strengths:
//!
//! * **No global state, no `void *`.** A database owns its flash through
//!   generics (`Kvdb<F>`), so multiple databases and multiple flash drivers can
//!   coexist without a registry or a global lock.
//! * **Typed errors.** [`Error`] carries the flash driver's own error type and
//!   says which operation failed, instead of collapsing everything into
//!   `FDB_READ_ERR`.
//! * **`no_std`, no allocation, no global state.** All buffers are stack or
//!   caller-provided. The crate is `#![deny(unsafe_code)]` with a single
//!   audited exception: placement initialization of a database inside
//!   `MaybeUninit` (`mount_into`), so static databases need no stack copy.
//! * **Iterators instead of callbacks.** KV and TS iteration are `Iterator`s
//!   (and TS iterators support forward, reverse and by-time modes) instead of
//!   C callbacks taking `void *args`.
//! * **Compile-time configuration.** Write granularity is a constant of the
//!   flash driver, not a build-wide `#define`; layout tables, buffer sizes and
//!   status encodings are folded per flash type.
//! * **Smaller RAM.** Direct-mapped KV name cache (O(1) lookup instead of a
//!   64-entry linear scan), no sector-cache table, leaner DB structs.
//! * **Fewer flash reads.** Lookups validate headers and match names without
//!   CRC-ing every value; `get` computes the CRC while copying into the caller
//!   buffer instead of reading the value twice (see `docs/DESIGN.md`).
//!
//! ## Example
//!
//! ```
//! use flashdb::{Config, Flash, Kvdb};
//!
//! # struct MyFlash;
//! # impl Flash for MyFlash {
//! #     type Error = core::convert::Infallible;
//! #     fn read(&mut self, _: u32, _: &mut [u8]) -> Result<(), Self::Error> { Ok(()) }
//! #     fn write(&mut self, _: u32, _: &[u8]) -> Result<(), Self::Error> { Ok(()) }
//! #     fn erase(&mut self, _: u32, _: u32) -> Result<(), Self::Error> { Ok(()) }
//! # }
//! # fn demo(flash: MyFlash) -> Result<(), flashdb::Error<core::convert::Infallible>> {
//! // 4 sectors of 4 KiB: one 16 KiB partition.
//! let mut db = Kvdb::mount(flash, Config::new(4096, 16384))?;
//!
//! db.set(b"boot_count", &42u32.to_le_bytes())?;
//!
//! let mut buf = [0u8; 16];
//! let n = db.get(b"boot_count", &mut buf)?.unwrap();
//! assert_eq!(&buf[..n], &42u32.to_le_bytes());
//! # Ok(())
//! # }
//! ```
//!
//! ## Flash driver contract
//!
//! Implement [`Flash`] for the partition that backs a database. Addresses are
//! relative to the partition; writes may only clear bits; erase sets bytes to
//! `0xFF`. The write granularity is declared per driver and decides the
//! on-flash status encoding (1 = bit level, 8/32/... = one write granule per
//! status), exactly like FlashDB's `FDB_WRITE_GRAN`.
//!
//! ## Format compatibility
//!
//! For the same sector size, write granularity and timestamp width, the
//! on-flash format matches FlashDB 2.2.x byte for byte, including padding and
//! status encodings. This is validated by differential tests against the C
//! implementation (`harness/`).

#![no_std]
#![deny(unsafe_code)]
#![deny(missing_docs)]

#[cfg(any(test, feature = "std"))]
extern crate std;

mod error;
mod flash;
mod kvdb;
mod low;
mod storage;
mod tsdb;

pub use error::{Error, Result};
pub use flash::{Flash, Partition};
pub use kvdb::{KvEntry, KvIterator, KvStats, Kvdb, KvdbImpl, KvdbNoCache, KvdbSmallCache};
pub use low::{
    crc32, KvStatus, SectorDirtyStatus, SectorStoreStatus, TslStatus, KV_NAME_MAX,
    STR_KV_VALUE_MAX_SIZE, VER_NUM_KV_NAME,
};
pub use storage::Config;
pub use tsdb::{Timestamp, Tsdb, Tsdb64, TsdbImpl, TslEntry, TslIterator};
