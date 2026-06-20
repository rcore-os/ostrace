#![no_std]

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RecordKind {
    Instant = 1,
    DurationBegin = 2,
    DurationEnd = 3,
    Counter = 4,
    Metadata = 5,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct RecordHeader {
    pub len: u16,
    pub kind: RecordKind,
    pub flags: u8,
    pub event_id: u32,
    pub timestamp: u64,
}

pub const RECORD_HEADER_SIZE: usize = core::mem::size_of::<RecordHeader>();

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_header_size_is_stable() {
        assert_eq!(RECORD_HEADER_SIZE, 16);
    }
}
