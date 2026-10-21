//! FlashDB firmware measurement harness -- Rust firmware.
//!
//! Runs the REAL Rust `flashdb` crate (crates/flashdb) over the no_std RAM
//! flash simulator (crates/flashdb-sim) on a bare-metal Cortex-M3 and prints
//! the same RESULT line as the C baseline firmware.
//!
//! Status of the two databases:
//!   * KVDB: real crate, workload identical in shape to the C firmware
//!     (64 keys x 24-byte values, 32 updates, 1500 churn rewrites over 16
//!     keys, 64 verified gets, full iteration).  Partition 16 KiB = 4 x 4 KiB,
//!     matching the C harness.
//!   * TSDB: real crate, workload identical in shape to the C firmware
//!     (200 x append of a 16-byte deterministic payload with fake timestamps,
//!     then full forward iteration + checksum).  Separate 16 KiB partition in
//!     the second half of the `.sim_flash` region; rollover disabled.
//!
//! Everything else -- startup assembly, linker script, semihosting, stack
//! painting, RESULT format, libc shims -- is shared with the C firmware so the
//! two images are directly comparable.
//!
//! Build: scripts/fw_rust.sh (cargo, target thumbv7m-none-eabi).
#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::panic::PanicInfo;

use flashdb::{Config, Flash, Kvdb, Tsdb};

// The SAME startup assembly file as the C firmware.  It is plain GNU-syntax
// ARM assembly, accepted by both arm-none-eabi-gcc and LLVM's integrated
// assembler (which is what global_asm! uses).
core::arch::global_asm!(include_str!("../common/startup_armv7m.S"));

// ---------------------------------------------------------------------------
// Workload shape -- must match harness/firmware/common/workload_spec.h
// ---------------------------------------------------------------------------
const KV_KEYS: u32 = 64;
const KV_UPDATES: u32 = 32;
const KV_CHURN: u32 = 1500;
const KV_CHURN_KEYS: u32 = 16;
const KV_VALUE_LEN: usize = 24;

const TS_APPENDS: u32 = 200;
const TS_PAYLOAD_LEN: usize = 16;
const TS_BASE_TIME: u32 = 1000;
/// Maximum payload accepted by one TSDB append (C harness uses 64; only an
/// upper-bound validation in the Rust API, the on-flash format is identical).
const TS_MAX_LEN: u32 = 32;

const SECTOR_SIZE: u32 = 4096;
const KV_PARTITION_SIZE: u32 = 16 * 1024;
const TS_PARTITION_SIZE: u32 = 16 * 1024;

const STACK_PAINT_WORD: u32 = 0xA5A5_A5A5;

/// Bytes below the current SP left untouched by `stack_repaint()`: no code
/// path pushes more than this between reading SP and the repaint loop.
#[cfg(feature = "stack-phases")]
const STACK_REPAINT_MARGIN: usize = 32;

// Semihosting operation numbers (ARM semihosting spec).
const SYS_WRITE0: u32 = 0x04;
const SYS_EXIT: u32 = 0x18;
const ADP_STOPPED_APP_EXIT: u32 = 0x20026;

/* ======================================================================== */
/* Linker symbols                                                           */
/* ======================================================================== */

extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __data_end: u8;
    static __bss_start: u8;
    static __bss_end: u8;
    static __sim_flash_start: u8;
    static __sim_flash_end: u8;
    static _stack_bottom: u8;
    static _estack: u8;
}

/* ======================================================================== */
/* Semihosting output (no libc, no core::fmt)                               */
/* ======================================================================== */

#[inline(always)]
unsafe fn semihost(op: u32, arg: usize) -> u32 {
    let mut r0 = op;
    core::arch::asm!(
        "bkpt 0xab",
        inout("r0") r0,
        in("r1") arg,
        options(nostack, preserves_flags)
    );
    r0
}

fn write_bytes(s: &[u8]) {
    // SYS_WRITE0 (0x04): r1 = NUL-terminated string.  Call sites pass either
    // byte-string literals that end in 0 or stack buffers whose last byte is 0.
    unsafe {
        semihost(SYS_WRITE0, s.as_ptr() as usize);
    }
}

fn write_u32(mut value: u32) {
    // Decimal digits without integer division (same algorithm as the C side).
    const POW10: [u32; 10] = [
        1_000_000_000, 100_000_000, 10_000_000, 1_000_000, 100_000, 10_000, 1_000, 100, 10, 1,
    ];
    let mut buf = [b' '; 11];
    let mut started = false;

    for (i, &p) in POW10.iter().enumerate() {
        let mut digit = 0u8;
        while value >= p {
            value -= p;
            digit += 1;
        }
        if digit != 0 || started || p == 1 {
            buf[i] = b'0' + digit;
            started = true;
        }
    }
    buf[10] = 0;
    let mut start = 0;
    while start < 10 && buf[start] == b' ' {
        start += 1;
    }
    write_bytes(&buf[start..]);
}

fn write_hex32(value: u32) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 11];

    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..8 {
        buf[2 + i] = HEX[((value >> (28 - 4 * i)) & 0xf) as usize];
    }
    buf[10] = 0;
    write_bytes(&buf[..]);
}

fn exit() -> ! {
    unsafe {
        semihost(SYS_EXIT, ADP_STOPPED_APP_EXIT as usize);
    }
    loop {}
}

fn fail(stage: &[u8], code: u32) -> ! {
    write_bytes(b"ERROR stage=\0");
    write_bytes(stage);
    write_bytes(b" code=\0");
    write_u32(code);
    write_bytes(b"\nRESULT rc=1\n\0");
    exit()
}

/* ======================================================================== */
/* Panic handler                                                            */
/*                                                                          */
/* Deliberately minimal: a panic means a firmware bug, and a plain `loop {}` */
/* keeps core::fmt/panicking machinery out of the image (the C baseline's   */
/* FDB_ASSERT does the equivalent `while (1);` when logging is compiled     */
/* out).  A hang is reported by the host script's QEMU timeout.             */
/* ======================================================================== */

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

/* ======================================================================== */
/* Minimal libc + compiler-builtins overrides                               */
/*                                                                          */
/* On ARM, LLVM lowers memcpy/memset/memmove intrinsics to __aeabi_* calls.  */
/* By default those resolve to compiler-builtins' generic implementations    */
/* (~1.5 KiB of ROM: memcpy 744 B, __aeabi_memset 220 B, __aeabi_memset4    */
/* 184 B, __aeabi_memclr4 174 B, __aeabi_memcpy4 156 B, __aeabi_memcpy 12 B).*/
/* The C baseline links tiny hand-written stubs instead (c/libc_stubs.c,     */
/* ~230 B of code for the same set), so the Rust firmware supplies strong    */
/* definitions here: compiler-builtins' symbols are weak and lose.  This is  */
/* harness-only code -- the library under measurement is unchanged.          */
/*                                                                          */
/* Volatile accesses stop LLVM from recognising these loops and turning them */
/* back into calls to themselves (a classic no_std memcpy pitfall).          */
/* ======================================================================== */

#[inline(never)]
unsafe fn copy_fwd(dst: *mut u8, src: *const u8, n: usize) {
    let mut i = 0;
    while i < n {
        core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
        i += 1;
    }
}

#[inline(never)]
unsafe fn copy_back(dst: *mut u8, src: *const u8, n: usize) {
    let mut i = n;
    while i > 0 {
        i -= 1;
        core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
    }
}

#[inline(never)]
unsafe fn fill(dst: *mut u8, byte: u8, n: usize) {
    let mut i = 0;
    while i < n {
        core::ptr::write_volatile(dst.add(i), byte);
        i += 1;
    }
}

#[inline(never)]
unsafe fn compare(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let va = core::ptr::read_volatile(a.add(i));
        let vb = core::ptr::read_volatile(b.add(i));
        if va != vb {
            return va as i32 - vb as i32;
        }
        i += 1;
    }
    0
}

/* ---- C standard names (memcpy & friends) ------------------------------- */

#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    copy_fwd(dst, src, n);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if (dst as usize) <= (src as usize) {
        copy_fwd(dst, src, n);
    } else {
        copy_back(dst, src, n);
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut u8, c: i32, n: usize) -> *mut u8 {
    fill(dst, c as u8, n);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    compare(a, b, n)
}

#[no_mangle]
pub unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    compare(a, b, n)
}

#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const u8) -> usize {
    let mut n = 0;
    while core::ptr::read_volatile(s.add(n)) != 0 {
        n += 1;
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn strcmp(a: *const u8, b: *const u8) -> i32 {
    let mut i = 0;
    loop {
        let ca = core::ptr::read_volatile(a.add(i));
        let cb = core::ptr::read_volatile(b.add(i));
        if ca != cb || ca == 0 {
            return ca as i32 - cb as i32;
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn strncmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let ca = core::ptr::read_volatile(a.add(i));
        let cb = core::ptr::read_volatile(b.add(i));
        if ca != cb || ca == 0 {
            return ca as i32 - cb as i32;
        }
        i += 1;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn strncpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        let c = core::ptr::read_volatile(src.add(i));
        core::ptr::write_volatile(dst.add(i), c);
        if c == 0 {
            i += 1;
            while i < n {
                core::ptr::write_volatile(dst.add(i), 0);
                i += 1;
            }
            break;
        }
        i += 1;
    }
    dst
}

/* ---- AAPCS runtime helpers (__aeabi_*): compiler-builtins are weak ----- */

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memcpy(dst: *mut u8, src: *const u8, n: usize) {
    copy_fwd(dst, src, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memcpy4(dst: *mut u8, src: *const u8, n: usize) {
    copy_fwd(dst, src, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memcpy8(dst: *mut u8, src: *const u8, n: usize) {
    copy_fwd(dst, src, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memmove(dst: *mut u8, src: *const u8, n: usize) {
    memmove(dst, src, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memmove4(dst: *mut u8, src: *const u8, n: usize) {
    memmove(dst, src, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memmove8(dst: *mut u8, src: *const u8, n: usize) {
    memmove(dst, src, n);
}

/* Note the AAPCS argument order: (dest, length, value). */
#[no_mangle]
pub unsafe extern "C" fn __aeabi_memset(dst: *mut u8, n: usize, c: i32) {
    fill(dst, c as u8, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memset4(dst: *mut u8, n: usize, c: i32) {
    fill(dst, c as u8, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memset8(dst: *mut u8, n: usize, c: i32) {
    fill(dst, c as u8, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memclr(dst: *mut u8, n: usize) {
    fill(dst, 0, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memclr4(dst: *mut u8, n: usize) {
    fill(dst, 0, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memclr8(dst: *mut u8, n: usize) {
    fill(dst, 0, n);
}

/* ======================================================================== */
/* Section sizes + stack high-water (identical method to the C firmware)    */
/* ======================================================================== */

struct Metrics {
    text: u32,
    rodata: u32,
    data: u32,
    bss: u32,
    sim_flash: u32,
    stack: u32,
    stack_init: u32,   /* mount/init phase (paint is fresh before main) */
    stack_limit: u32,
    #[cfg(feature = "stack-phases")]
    st_set: u32,       /* isolated phase peaks, see stack_diagnostics() */
    #[cfg(feature = "stack-phases")]
    st_get: u32,
    #[cfg(feature = "stack-phases")]
    st_iter: u32,
    #[cfg(feature = "stack-phases")]
    st_gc: u32,
    #[cfg(feature = "stack-phases")]
    st_ts_append: u32,
    #[cfg(feature = "stack-phases")]
    st_ts_iter: u32,
    kv_writes: u32,
    kv_gets: u32,
    kv_iterated: u32,
    kv_checksum: u32,
    kv_iter_checksum: u32,
    ts_appends: u32,
    ts_iterated: u32,
    ts_checksum: u32,
}

impl Metrics {
    const fn zeroed() -> Self {
        Metrics {
            text: 0,
            rodata: 0,
            data: 0,
            bss: 0,
            sim_flash: 0,
            stack: 0,
            stack_init: 0,
            stack_limit: 0,
            #[cfg(feature = "stack-phases")]
            st_set: 0,
            #[cfg(feature = "stack-phases")]
            st_get: 0,
            #[cfg(feature = "stack-phases")]
            st_iter: 0,
            #[cfg(feature = "stack-phases")]
            st_gc: 0,
            #[cfg(feature = "stack-phases")]
            st_ts_append: 0,
            #[cfg(feature = "stack-phases")]
            st_ts_iter: 0,
            kv_writes: 0,
            kv_gets: 0,
            kv_iterated: 0,
            kv_checksum: 0,
            kv_iter_checksum: 0,
            ts_appends: 0,
            ts_iterated: 0,
            ts_checksum: 0,
        }
    }
}

fn sym_addr(sym: *const u8) -> usize {
    sym as usize
}

fn fill_section_sizes(m: &mut Metrics) {
    unsafe {
        m.text = (sym_addr(&__text_end) - sym_addr(&__text_start)) as u32;
        m.rodata = (sym_addr(&__rodata_end) - sym_addr(&__rodata_start)) as u32;
        m.data = (sym_addr(&__data_end) - sym_addr(&__data_start)) as u32;
        m.bss = (sym_addr(&__bss_end) - sym_addr(&__bss_start)) as u32;
        m.sim_flash = (sym_addr(&__sim_flash_end) - sym_addr(&__sim_flash_start)) as u32;
    }
}

fn stack_limit_bytes() -> u32 {
    unsafe { (sym_addr(&_estack) - sym_addr(&_stack_bottom)) as u32 }
}

fn stack_high_water_bytes() -> u32 {
    unsafe {
        let bottom = sym_addr(&_stack_bottom) as *const u32;
        let top = sym_addr(&_estack) as *const u32;
        let mut p = bottom;

        while p < top && core::ptr::read_volatile(p) == STACK_PAINT_WORD {
            p = p.add(1);
        }
        let used = (top as usize - p as usize) as u32;
        (used + 7) & !7
    }
}

/// Repaint the unused part of the stack below the current SP with 0xA5.
///
/// Used for per-phase stack attribution: after repainting, a high-water scan
/// reports the peak of the phase that runs next (plus the live caller frames),
/// without marks left by earlier phases.  Only memory below the current SP is
/// touched, so live frames are preserved.
#[cfg(feature = "stack-phases")]
#[inline(never)]
fn stack_repaint() {
    unsafe {
        let sp: usize;
        core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
        let bottom = sym_addr(&_stack_bottom);
        let end = sp.saturating_sub(STACK_REPAINT_MARGIN);
        let mut p = bottom;
        while p < end {
            core::ptr::write_volatile(p as *mut u32, STACK_PAINT_WORD);
            p += 4;
        }
    }
}

/* ======================================================================== */
/* Static storage for the databases                                         */
/*                                                                          */
/* The C firmware keeps its fdb_kvdb/fdb_tsdb structs in .bss, not on the    */
/* stack.  Do the same here so the stack high-water comparison is fair: the  */
/* flash simulator and the mounted database live in .bss, and main() only    */
/* holds a shallow reference to them.                                       */
/* ======================================================================== */

/// One 16 KiB simulated NOR partition (4 x 4 KiB erase blocks).
///
/// Both stores live in the `.sim_flash` NOLOAD memory region of the linker
/// script (32 KiB total: KV at +0, TS at +16 KiB) -- the same region the C
/// firmware's FAL simulator uses -- so they are neither part of ROM nor
/// counted in .data/.bss: they model external NOR parts.
#[repr(C, align(4096))]
struct SimFlashStore<const N: usize>([u8; N]);

#[link_section = ".sim_flash"]
static mut KV_SIM_FLASH: SimFlashStore<{ KV_PARTITION_SIZE as usize }> =
    SimFlashStore([0u8; KV_PARTITION_SIZE as usize]);

#[link_section = ".sim_flash"]
static mut TS_SIM_FLASH: SimFlashStore<{ TS_PARTITION_SIZE as usize }> =
    SimFlashStore([0u8; TS_PARTITION_SIZE as usize]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimFlashError {
    OutOfBounds,
    UnalignedErase,
}

/// Minimal NOR flash driver over one [`SimFlashStore`] region.
///
/// `flashdb-sim::RamFlash` is the same simulator used by the host benchmarks,
/// but it owns its buffer by value; in a bare-metal image that buffer would
/// land in `.data` (16 KiB of ROM!) and in the static-RAM metric.  For the
/// footprint measurement the buffers must live in the detached `.sim_flash`
/// region, so the firmware implements the small `Flash` trait here.  One
/// implementation serves both partitions through a base pointer; the driver is
/// harness code present in the C image too (fal_ram_sim.c), so it does not tilt
/// the comparison.
#[derive(Clone, Copy)]
struct SimFlash {
    base: *mut u8,
    len: usize,
}

impl SimFlash {
    const fn new(base: *mut u8, len: usize) -> Self {
        Self { base, len }
    }

    fn check(&self, addr: u32, len: usize) -> Result<usize, SimFlashError> {
        let start = addr as usize;
        let end = start.checked_add(len).ok_or(SimFlashError::OutOfBounds)?;
        if end > self.len {
            return Err(SimFlashError::OutOfBounds);
        }
        Ok(start)
    }

    /// Put the simulated part into its factory-fresh (erased) state.
    #[inline(never)]
    fn erase_all(&self) {
        unsafe {
            core::ptr::write_bytes(self.base, 0xFF, self.len);
        }
    }
}

impl Flash for SimFlash {
    type Error = SimFlashError;

    const WRITE_GRAN_BITS: u32 = 8;

    #[inline(never)]
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        let start = self.check(addr, buf.len())?;
        unsafe {
            core::ptr::copy_nonoverlapping(self.base.add(start), buf.as_mut_ptr(), buf.len());
        }
        Ok(())
    }

    #[inline(never)]
    fn write(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        let start = self.check(addr, buf.len())?;
        let mut i = 0;
        while i < buf.len() {
            unsafe {
                // NOR programming can only clear bits (1 -> 0).
                let old = core::ptr::read_volatile(self.base.add(start + i));
                core::ptr::write_volatile(self.base.add(start + i), old & buf[i]);
            }
            i += 1;
        }
        Ok(())
    }

    #[inline(never)]
    fn erase(&mut self, addr: u32, len: u32) -> Result<(), Self::Error> {
        if addr % SECTOR_SIZE != 0 || len % SECTOR_SIZE != 0 {
            return Err(SimFlashError::UnalignedErase);
        }
        let start = self.check(addr, len as usize)?;
        unsafe {
            core::ptr::write_bytes(self.base.add(start), 0xFF, len as usize);
        }
        Ok(())
    }
}

fn kv_flash() -> SimFlash {
    unsafe {
        SimFlash::new(
            core::ptr::addr_of_mut!(KV_SIM_FLASH.0) as *mut u8,
            KV_PARTITION_SIZE as usize,
        )
    }
}

fn ts_flash() -> SimFlash {
    unsafe {
        SimFlash::new(
            core::ptr::addr_of_mut!(TS_SIM_FLASH.0) as *mut u8,
            TS_PARTITION_SIZE as usize,
        )
    }
}

// The database structs live in .bss, mirroring the C firmware's static
// fdb_kvdb/fdb_tsdb, so main()'s frame stays shallow and the stack measurement
// is comparable.
type KvDb = Kvdb<SimFlash>;
type TsDb = Tsdb<SimFlash>;

struct DbCell<T>(UnsafeCell<MaybeUninit<T>>);
unsafe impl<T> Sync for DbCell<T> {}

static KV_DB: DbCell<KvDb> = DbCell(UnsafeCell::new(MaybeUninit::uninit()));
static TS_DB: DbCell<TsDb> = DbCell(UnsafeCell::new(MaybeUninit::uninit()));

#[inline(never)]
fn kv_mount() {
    let flash = kv_flash();
    flash.erase_all();
    // Placement initialization: the database (including its 512-byte name
    // cache) is built directly in .bss, so mounting costs no stack copy.
    // FlashDB-C has the same property because fdb_kvdb_init() takes a pointer.
    let slot = unsafe { &mut *KV_DB.0.get() };
    match Kvdb::mount_into(slot, flash, Config::new(SECTOR_SIZE, KV_PARTITION_SIZE)) {
        Ok(()) => {}
        Err(_) => fail(b"kv_mount\0", 0),
    }
}

#[inline(never)]
fn ts_mount() {
    let flash = ts_flash();
    flash.erase_all();
    match Tsdb::mount(flash, Config::new(SECTOR_SIZE, TS_PARTITION_SIZE), TS_MAX_LEN) {
        Ok(mut db) => {
            db.set_rollover(false);
            unsafe {
                (*TS_DB.0.get()).write(db);
            }
        }
        Err(_) => fail(b"ts_mount\0", 0),
    }
}

fn kv() -> &'static mut KvDb {
    unsafe { &mut *(*KV_DB.0.get()).as_mut_ptr() }
}

fn ts() -> &'static mut TsDb {
    unsafe { &mut *(*TS_DB.0.get()).as_mut_ptr() }
}

/* ======================================================================== */
/* Deterministic data generation (byte-identical to c/main.c)               */
/* ======================================================================== */

const HEX: &[u8; 16] = b"0123456789abcdef";

fn make_kv_key(out: &mut [u8; 5], idx: u32) {
    out[0] = b'k';
    out[1] = b'v';
    out[2] = b'.';
    out[3] = HEX[((idx >> 4) & 0xf) as usize];
    out[4] = HEX[(idx & 0xf) as usize];
}

fn make_kv_value(out: &mut [u8; KV_VALUE_LEN], idx: u32, gen: u32) {
    out[0] = b'v';
    out[1] = HEX[((gen >> 12) & 0xf) as usize];
    out[2] = HEX[((gen >> 8) & 0xf) as usize];
    out[3] = HEX[((gen >> 4) & 0xf) as usize];
    out[4] = HEX[(gen & 0xf) as usize];
    out[5] = b'-';
    out[6] = HEX[((idx >> 4) & 0xf) as usize];
    out[7] = HEX[(idx & 0xf) as usize];
    out[8] = b'-';
    let mut k = 9;
    while k < KV_VALUE_LEN {
        out[k] = HEX[((idx.wrapping_mul(7) + gen.wrapping_mul(13) + k as u32) & 0xf) as usize];
        k += 1;
    }
}

fn make_ts_payload(out: &mut [u8; TS_PAYLOAD_LEN], idx: u32) {
    let tick = TS_BASE_TIME + idx;
    out[0] = idx as u8;
    out[1] = (idx >> 8) as u8;
    out[2] = (idx >> 16) as u8;
    out[3] = (idx >> 24) as u8;
    out[4] = tick as u8;
    out[5] = (tick >> 8) as u8;
    out[6] = (tick >> 16) as u8;
    out[7] = (tick >> 24) as u8;
    let mut k = 8;
    while k < TS_PAYLOAD_LEN {
        out[k] = (idx.wrapping_mul(31) + (k as u32).wrapping_mul(7)) as u8;
        k += 1;
    }
}

/// The generation stored for `idx` after the whole KV phase; mirrors c/main.c.
fn expected_gen(idx: u32) -> u32 {
    if idx < KV_CHURN_KEYS {
        let last_n = (KV_CHURN - 1) - ((KV_CHURN - 1 - idx) % KV_CHURN_KEYS);
        2 + last_n
    } else if idx < KV_UPDATES {
        1
    } else {
        0
    }
}

fn fnv1a(mut h: u32, data: &[u8]) -> u32 {
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(16_777_619);
    }
    h
}

/* ======================================================================== */
/* KVDB phase -- real Rust flashdb crate                                    */
/* ======================================================================== */

#[inline(never)]
fn kv_phase(m: &mut Metrics) {
    let db = kv();
    let mut key = [0u8; 5];
    let mut value = [0u8; KV_VALUE_LEN];
    let mut read_buf = [0u8; KV_VALUE_LEN];
    let mut iter_buf = [0u8; KV_VALUE_LEN];

    // 1. initial set of all keys
    let mut i = 0u32;
    while i < KV_KEYS {
        make_kv_key(&mut key, i);
        make_kv_value(&mut value, i, 0);
        if db.set(&key, &value).is_err() {
            fail(b"kv_set\0", i);
        }
        m.kv_writes += 1;
        i += 1;
    }

    // 2. update half of them
    i = 0;
    while i < KV_UPDATES {
        make_kv_key(&mut key, i);
        make_kv_value(&mut value, i, 1);
        if db.set(&key, &value).is_err() {
            fail(b"kv_update\0", i);
        }
        m.kv_writes += 1;
        i += 1;
    }

    // 3. GC churn: rewrite a small key set many times so the database has to
    //    erase and migrate sectors (same pattern as the C workload).
    let mut n = 0u32;
    while n < KV_CHURN {
        let idx = n % KV_CHURN_KEYS;
        make_kv_key(&mut key, idx);
        make_kv_value(&mut value, idx, 2 + n);
        if db.set(&key, &value).is_err() {
            fail(b"kv_churn\0", n);
        }
        m.kv_writes += 1;
        n += 1;
    }

    // 4. get every key, verify and checksum its value (index order, so this
    //    checksum is implementation-order independent and matches the C side).
    i = 0;
    while i < KV_KEYS {
        let gen = expected_gen(i);
        let mut expected = [0u8; KV_VALUE_LEN];
        make_kv_key(&mut key, i);
        make_kv_value(&mut expected, i, gen);
        match db.get(&key, &mut read_buf) {
            Ok(Some(got)) if got == KV_VALUE_LEN => {}
            _ => fail(b"kv_get_len\0", i),
        }
        if unsafe { memcmp(read_buf.as_ptr(), expected.as_ptr(), KV_VALUE_LEN) } != 0 {
            fail(b"kv_get_data\0", i);
        }
        m.kv_gets += 1;
        m.kv_checksum = fnv1a(m.kv_checksum, &read_buf);
        i += 1;
    }

    // 5. iterate the whole database (different code path from point lookup).
    //    This checksum depends on iteration order, so it is reported
    //    separately from kv_checksum.
    let mut it = db.iter();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => fail(b"kv_iter\0", m.kv_iterated),
        };
        m.kv_iter_checksum = fnv1a(m.kv_iter_checksum, entry.name());
        let got = match it.read_value(&entry, &mut iter_buf) {
            Ok(n) => n,
            Err(_) => fail(b"kv_iter_read\0", m.kv_iterated),
        };
        m.kv_iter_checksum = fnv1a(m.kv_iter_checksum, &iter_buf[..got]);
        m.kv_iterated += 1;
    }

    if m.kv_iterated != KV_KEYS {
        fail(b"kv_iter_count\0", m.kv_iterated);
    }
}

/* ======================================================================== */
/* TSDB phase -- real Rust flashdb crate                                    */
/*                                                                          */
/* 200 deterministic 16-byte payloads with fake timestamps (1000..1199),    */
/* then a full forward iteration that reads each payload back and checksums */
/* it in iteration order.  This mirrors the C workload exactly, so         */
/* ts_checksum is directly comparable.                                      */
/* ======================================================================== */

#[inline(never)]
fn tsdb_phase(m: &mut Metrics) {
    let db = ts();
    let mut payload = [0u8; TS_PAYLOAD_LEN];
    let mut read_buf = [0u8; TS_PAYLOAD_LEN];
    let mut i = 0u32;

    // append phase
    while i < TS_APPENDS {
        make_ts_payload(&mut payload, i);
        if db.append((TS_BASE_TIME + i) as i32, &payload).is_err() {
            fail(b"ts_append\0", i);
        }
        m.ts_appends += 1;
        i += 1;
    }

    // forward iteration phase
    let mut it = db.iter();
    let mut n = 0u32;
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => fail(b"ts_iter\0", n),
        };
        let got = match it.read_blob(&entry, &mut read_buf) {
            Ok(g) => g,
            Err(_) => fail(b"ts_iter_read\0", n),
        };
        if got != TS_PAYLOAD_LEN {
            fail(b"ts_iter_len\0", got as u32);
        }
        m.ts_iterated += 1;
        m.ts_checksum = fnv1a(m.ts_checksum, &read_buf[..got]);
        n += 1;
    }
    if n != TS_APPENDS {
        fail(b"ts_iter_count\0", n);
    }
}

/* ======================================================================== */
/* Entry point                                                              */
/* ======================================================================== */

/// Per-phase stack attribution.
///
/// Runs AFTER the headline scan, so it never changes the reported `stack`
/// (the stack is repainted between phases, which would otherwise erase the
/// headline high-water marks).  Each phase is measured from a freshly painted
/// stack, so the number is that phase's peak plus the live caller frames.
/// Database state: the full workload has already run, so iteration sees all
/// 64 keys; the churn phase re-runs the same 1500-write GC pattern.
#[cfg(feature = "stack-phases")]
#[inline(never)]
fn stack_diagnostics(m: &mut Metrics) {
    let db = kv();
    let mut key = [0u8; 5];
    let mut value = [0u8; KV_VALUE_LEN];
    let mut read_buf = [0u8; KV_VALUE_LEN];
    let mut iter_buf = [0u8; KV_VALUE_LEN];

    // (b) one set
    stack_repaint();
    make_kv_key(&mut key, 0);
    make_kv_value(&mut value, 0, 0x3000);
    if db.set(&key, &value).is_err() {
        fail(b"st_set\0", 0);
    }
    m.st_set = stack_high_water_bytes();

    // (c) one get
    stack_repaint();
    match db.get(&key, &mut read_buf) {
        Ok(Some(got)) if got == KV_VALUE_LEN => {}
        _ => fail(b"st_get\0", 0),
    }
    m.st_get = stack_high_water_bytes();

    // (d) full iteration
    stack_repaint();
    let mut it = db.iter();
    let mut n = 0u32;
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => fail(b"st_iter\0", n),
        };
        if it.read_value(&entry, &mut iter_buf).is_err() {
            fail(b"st_iter_read\0", n);
        }
        n += 1;
    }
    if n != KV_KEYS {
        fail(b"st_iter_count\0", n);
    }
    m.st_iter = stack_high_water_bytes();

    // (e) GC churn (same write pattern as the measured workload)
    stack_repaint();
    n = 0;
    while n < KV_CHURN {
        let idx = n % KV_CHURN_KEYS;
        make_kv_key(&mut key, idx);
        make_kv_value(&mut value, idx, 0x4000 + n);
        if db.set(&key, &value).is_err() {
            fail(b"st_gc\0", n);
        }
        n += 1;
    }
    m.st_gc = stack_high_water_bytes();

    // (f) one TSDB append, (g) full TSDB iteration.  The TS database has all
    //     200 records from the workload at this point; the diagnostic append
    //     uses the next timestamp so the iteration then sees 201 entries.
    let tsdb = ts();
    let mut ts_payload = [0u8; TS_PAYLOAD_LEN];
    let mut ts_buf = [0u8; TS_PAYLOAD_LEN];

    stack_repaint();
    make_ts_payload(&mut ts_payload, TS_APPENDS);
    if tsdb
        .append((TS_BASE_TIME + TS_APPENDS) as i32, &ts_payload)
        .is_err()
    {
        fail(b"st_ts_append\0", 0);
    }
    m.st_ts_append = stack_high_water_bytes();

    stack_repaint();
    let mut ts_it = tsdb.iter();
    let mut ts_n = 0u32;
    while let Some(entry) = ts_it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => fail(b"st_ts_iter\0", ts_n),
        };
        let got = match ts_it.read_blob(&entry, &mut ts_buf) {
            Ok(g) => g,
            Err(_) => fail(b"st_ts_iter_read\0", ts_n),
        };
        if got != TS_PAYLOAD_LEN {
            fail(b"st_ts_iter_len\0", got as u32);
        }
        ts_n += 1;
    }
    if ts_n != TS_APPENDS + 1 {
        fail(b"st_ts_iter_count\0", ts_n);
    }
    m.st_ts_iter = stack_high_water_bytes();
}

#[inline(never)]
fn print_result(m: &Metrics) {
    write_bytes(b"RESULT text=\0");
    write_u32(m.text);
    write_bytes(b" rodata=\0");
    write_u32(m.rodata);
    write_bytes(b" data=\0");
    write_u32(m.data);
    write_bytes(b" bss=\0");
    write_u32(m.bss);
    write_bytes(b" sim_flash=\0");
    write_u32(m.sim_flash);
    write_bytes(b" stack=\0");
    write_u32(m.stack);
    write_bytes(b" stack_init=\0");
    write_u32(m.stack_init);
    write_bytes(b" stack_limit=\0");
    write_u32(m.stack_limit);
    #[cfg(feature = "stack-phases")]
    {
        write_bytes(b" st_set=\0");
        write_u32(m.st_set);
        write_bytes(b" st_get=\0");
        write_u32(m.st_get);
        write_bytes(b" st_iter=\0");
        write_u32(m.st_iter);
        write_bytes(b" st_gc=\0");
        write_u32(m.st_gc);
        write_bytes(b" st_ts_append=\0");
        write_u32(m.st_ts_append);
        write_bytes(b" st_ts_iter=\0");
        write_u32(m.st_ts_iter);
    }
    write_bytes(b" kv_writes=\0");
    write_u32(m.kv_writes);
    write_bytes(b" kv_gets=\0");
    write_u32(m.kv_gets);
    write_bytes(b" kv_iterated=\0");
    write_u32(m.kv_iterated);
    write_bytes(b" kv_checksum=\0");
    write_hex32(m.kv_checksum);
    write_bytes(b" kv_iter_checksum=\0");
    write_hex32(m.kv_iter_checksum);
    write_bytes(b" ts_appends=\0");
    write_u32(m.ts_appends);
    write_bytes(b" ts_iterated=\0");
    write_u32(m.ts_iterated);
    write_bytes(b" ts_checksum=\0");
    write_hex32(m.ts_checksum);
    write_bytes(b" rc=0\n\0");
}

#[no_mangle]
pub extern "C" fn main() -> ! {
    let mut m = Metrics::zeroed();
    fill_section_sizes(&mut m);
    m.stack_limit = stack_limit_bytes();

    // Database init (compares with the C fdb_kvdb_init/fdb_tsdb_init phase).
    kv_mount();
    ts_mount();

    // High-water reached by initialisation, before the workload starts.
    m.stack_init = stack_high_water_bytes();

    kv_phase(&mut m);
    tsdb_phase(&mut m);

    // Final scan after both phases have returned to this shallow frame.
    m.stack = stack_high_water_bytes();

    // Per-phase attribution (does not affect m.stack: repaints happen after).
    #[cfg(feature = "stack-phases")]
    stack_diagnostics(&mut m);

    print_result(&m);
    exit()
}
