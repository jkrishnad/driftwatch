#![no_std]

/// Key that pairs an issue event with its complete event.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskKey {
    pub sector: u64, // where on the disk the request starts
    pub dev: u64,    // which disk
}

/// One completed disk request, sent from kernel to daemon.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DiskEvent {
    pub sector: u64,     // where on the disk
    pub latency_ns: u64, // how long the disk took
    pub dev: u32,        // which disk (major:minor packed into one number)
    pub bytes: u32,      // how much data
    pub error: i32,      // 0 = success
    pub rw: u8,          // read, write or other (see consts below)
    pub _pad: [u8; 3],   // filler so the struct has no gaps
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
