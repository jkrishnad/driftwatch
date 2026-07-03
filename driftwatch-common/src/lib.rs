#![no_std]

/// HashMap key matching an issue to its complete: same (dev, sector) pair is
/// present at identical offsets in both tracepoints. dev widened to u64 so the
/// struct is exactly 16 bytes with zero padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskKey {
    pub sector: u64,
    pub dev: u64,
}

/// One completed block request, shipped kernel -> daemon over the ringbuf.
/// 32 bytes, no padding holes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DiskEvent {
    pub sector: u64,
    pub latency_ns: u64, // complete_ts - issue_ts (device service time)
    pub dev: u32,        // kernel dev_t: major = dev >> 20, minor = dev & 0xfffff
    pub bytes: u32,      // request size (nr_sector * 512)
    pub error: i32,      // block layer errno, 0 = ok (free hard-fault signal)
    pub rw: u8,          // 0 = read, 1 = write, 2 = other (discard/flush/...)
    pub _pad: [u8; 3],
}

pub const RW_READ: u8 = 0;
pub const RW_WRITE: u8 = 1;
pub const RW_OTHER: u8 = 2;

#[cfg(feature = "user")]
mod user {
    use super::*;

    unsafe impl aya::Pod for DiskKey {}
    unsafe impl aya::Pod for DiskEvent {}
}
