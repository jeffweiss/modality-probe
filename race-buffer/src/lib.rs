//! RaceBuffer, a single producer, single consumer, shared memory ring buffer which maintains
//! consistency while operating under race conditions.
#![cfg_attr(not(feature = "std"), no_std)]
#![deny(warnings)]
#![deny(missing_docs)]

use core::marker::Copy;
use core::ops::{Add, AddAssign, Sub};
use core::sync::atomic::{fence, Ordering};

/// The index of an entry in the sequence of all entries written to the buffer
/// Note: This struct is 2 separate words so they can be read in one cpu instruction on 32 bit machines,
/// for asynchronous reading
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
#[repr(C)]
pub(crate) struct SeqNum {
    high: u32,
    low: u32,
}

impl SeqNum {
    const UPDATING_HIGH_MASK: u32 = 0b1000_0000_0000_0000_0000_0000_0000_0000;

    pub(crate) fn new(high: u32, low: u32) -> Self {
        debug_assert!(high < Self::UPDATING_HIGH_MASK);
        Self { high, low }
    }

    #[cfg(feature = "std")]
    pub(crate) fn has_updating_high_bit_set(high: u32) -> bool {
        high & Self::UPDATING_HIGH_MASK != 0
    }

    /// Set the high bit of the high word so the asynchronous reader does not try to use the sequence number
    /// in an inconsistent state
    fn set_updating_high_bit(&mut self) {
        self.high |= Self::UPDATING_HIGH_MASK;
    }

    /// Increment the sequence number by the given amount, using an "updating" flag to ensure the asynchronous
    /// reader does not use the sequence number in an inconsistent state
    pub(crate) fn increment(&mut self, n: Self) {
        let (new_low, overflowed) = self.low.overflowing_add(n.low);
        let high_increment = n.high + if overflowed { 1 } else { 0 };
        if high_increment > 0 {
            let new_high = self.high + high_increment;
            // Set the updating bit before modifying the value of the sequence number.
            // While this bit is set, the reader will retry reading the high word.
            // Fences are used to prevent write reorders
            self.set_updating_high_bit();
            fence(Ordering::Release);
            self.low = new_low;
            fence(Ordering::Release);
            self.high = new_high;
            fence(Ordering::Release);
        } else {
            self.low = new_low;
            fence(Ordering::Release);
        }
    }
}

impl Into<u64> for SeqNum {
    fn into(self) -> u64 {
        debug_assert!(self.high < Self::UPDATING_HIGH_MASK);
        ((self.high as u64) << 32) + self.low as u64
    }
}

impl From<u64> for SeqNum {
    fn from(n: u64) -> Self {
        let high = (n >> 32) as u32;
        debug_assert!(high < Self::UPDATING_HIGH_MASK);
        Self {
            high,
            low: n as u32,
        }
    }
}

impl Add<Self> for SeqNum {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        (Into::<u64>::into(self) + Into::<u64>::into(rhs)).into()
    }
}

impl AddAssign<Self> for SeqNum {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs
    }
}

impl Sub<Self> for SeqNum {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        (Into::<u64>::into(self) - Into::<u64>::into(rhs)).into()
    }
}

impl Add<u64> for SeqNum {
    type Output = Self;

    fn add(self, rhs: u64) -> Self::Output {
        (Into::<u64>::into(self) + rhs).into()
    }
}

impl AddAssign<u64> for SeqNum {
    fn add_assign(&mut self, rhs: u64) {
        *self = *self + rhs
    }
}

impl Sub<u64> for SeqNum {
    type Output = Self;

    fn sub(self, rhs: u64) -> Self::Output {
        (Into::<u64>::into(self) - rhs).into()
    }
}

/// Returns the corresponding index in backing storage to given sequence number
#[inline]
fn get_seqn_index(storage_cap: usize, seqn: SeqNum, use_base_2_indexing: bool) -> usize {
    // Cast to usize safe because index in storage slice cannot be greater than usize
    if use_base_2_indexing {
        // Index is lowest n bits of seqn where storage_cap is 2^n
        (Into::<u64>::into(seqn) & (storage_cap as u64 - 1)) as usize
    } else {
        (Into::<u64>::into(seqn) % storage_cap as u64) as usize
    }
}

/// Calculate the number of entries missed based on the read and overwrite sequence numbers
#[inline]
fn num_missed(read_seqn: SeqNum, overwrite_seqn: SeqNum) -> SeqNum {
    if overwrite_seqn < read_seqn {
        0.into()
    } else {
        overwrite_seqn - read_seqn
    }
}

/// Entry that can be stored in a RaceBuffer
pub trait Entry: Copy + PartialEq {
    /// Return true if entry is the first in a double entry
    fn is_prefix(&self) -> bool;
}

/// An entry or double entry that has just been overwritten
#[derive(PartialEq, Copy, Clone, Debug)]
pub enum WholeEntry<E>
where
    E: Entry,
{
    /// Single entry overwritten
    Single(E),
    /// Double entry overwritten
    Double(E, E),
}

impl<E> WholeEntry<E>
where
    E: Entry,
{
    /// 1 if entry is single, 2 if double
    pub fn size(&self) -> u64 {
        match self {
            Self::Single(_) => 1,
            _ => 2,
        }
    }
}

#[cfg(feature = "std")]
pub mod async_reader;

pub mod buffer;
pub use buffer::RaceBuffer;

#[cfg(all(feature = "std", test))]
mod util;

#[cfg(all(feature = "std", test))]
pub mod tests {
    use super::*;

    use core::mem::MaybeUninit;
    use crossbeam;
    use rand::Rng;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;
    use util::{OrderedEntry, OutputOrderedEntry, RawPtrSnapper};

    #[test]
    fn seq_nums() {
        assert_eq!(SeqNum::new(u32::MAX >> 1, u32::MAX), (u64::MAX >> 1).into());
        assert_eq!(
            Into::<u64>::into(SeqNum::new(u32::MAX >> 1, u32::MAX)),
            u64::MAX >> 1
        );
        assert_eq!(
            SeqNum::new(u32::MAX >> 1, 0),
            (0b01111111_11111111_11111111_11111111_00000000_00000000_00000000_00000000).into()
        );
        assert_eq!(
            Into::<u64>::into(SeqNum::new(u32::MAX >> 1, 0)),
            0b01111111_11111111_11111111_11111111_00000000_00000000_00000000_00000000
        );

        assert_eq!(
            SeqNum::new(1, 2) + SeqNum::new(2, u32::MAX),
            SeqNum::new(4, 1)
        );
        assert_eq!(
            SeqNum::new(1, 2) + (u32::MAX as u64 * 3 + 2),
            SeqNum::new(4, 1)
        );

        assert!(SeqNum::new(1, 0) > SeqNum::new(0, 2));
        assert!(SeqNum::new(1, 3) > SeqNum::new(1, 2))
    }

    // Perform many writes and reads concurrently,
    // check if reader output is in order and consistent
    #[test]
    fn async_output() {
        const NUM_WRITES: u32 = 160;
        const STORAGE_CAP: usize = 8;

        let (ptr_s, ptr_r) = crossbeam::crossbeam_channel::bounded(0);
        let barr_r = Arc::new(Barrier::new(2));
        let barr_w = barr_r.clone();
        let num_read_r = Arc::new(AtomicU32::new(0));
        let num_read_w = num_read_r.clone();
        crossbeam::thread::scope(|s| {
            s.spawn(move |_| {
                let mut storage = [MaybeUninit::uninit(); STORAGE_CAP];

                let mut buf = RaceBuffer::new(&mut storage[..], false).unwrap();
                assert!(buf.capacity() == STORAGE_CAP);
                let buf_ptr = &buf as *const RaceBuffer<'_, OrderedEntry> as usize;
                ptr_s.send(buf_ptr).unwrap();

                for i in 0..NUM_WRITES {
                    buf.push(OrderedEntry::from_index(i));
                    std::thread::sleep(Duration::from_millis(10));
                }
                barr_r.wait();
            });
            s.spawn(move |_| {
                let buf_ptr = ptr_r.recv().unwrap() as *const RaceBuffer<'_, OrderedEntry>;
                let snapper = RawPtrSnapper::new(buf_ptr);
                let mut output_buf = Vec::new();
                let mut buf_reader = async_reader::RaceReader::new(snapper, STORAGE_CAP);

                let mut total_items_read = 0;
                while total_items_read < NUM_WRITES as usize {
                    let mut temp_buf = Vec::new();
                    let n_missed = buf_reader.read(&mut temp_buf).unwrap();
                    total_items_read += n_missed as usize + temp_buf.len();

                    output_buf.push(OutputOrderedEntry::Missed(n_missed));
                    output_buf.extend(temp_buf.iter().map(|e| OutputOrderedEntry::Present(*e)));

                    thread::sleep(Duration::from_millis(10));
                }
                OutputOrderedEntry::check_entries_correct_index(&output_buf[..]);
                barr_w.wait();
            });
        })
        .unwrap();
    }

    // Perform many reads and writes concurrently with random timeouts,
    // check if read buffer is in order and consistent
    #[test]
    fn async_output_timeouts() {
        const NUM_WRITES: u32 = 100_000;
        const STORAGE_CAP: usize = 8;

        let (ptr_s, ptr_r) = crossbeam::crossbeam_channel::bounded(0);
        let barr_r = Arc::new(Barrier::new(2));
        let barr_w = barr_r.clone();
        let num_read_r = Arc::new(AtomicU32::new(0));
        let num_read_w = num_read_r.clone();
        crossbeam::thread::scope(|s| {
            s.spawn(move |_| {
                let mut storage = [MaybeUninit::uninit(); STORAGE_CAP];
                let mut buf = RaceBuffer::new(&mut storage[..], true).unwrap();
                assert!(buf.capacity() == STORAGE_CAP);
                let buf_ptr = &buf as *const RaceBuffer<'_, OrderedEntry> as usize;
                ptr_s.send(buf_ptr).unwrap();
                let mut rng = rand::thread_rng();
                let mut last_prefix = false;

                for i in 0..NUM_WRITES {
                    while i - num_read_w.load(Ordering::SeqCst) >= buf.get_seqn_mod() - 1 {}
                    if last_prefix {
                        buf.push(OrderedEntry::from_index_suffix(i));
                        last_prefix = false;
                    } else {
                        if rng.gen::<f64>() < 0.33 {
                            buf.push(OrderedEntry::from_index_prefix(i));
                            last_prefix = true;
                        } else {
                            buf.push(OrderedEntry::from_index(i));
                        }
                    }
                    let sleep_time: u64 = rng.gen::<u64>() % 1000;
                    std::thread::sleep(Duration::from_nanos(sleep_time));
                }
                barr_r.wait();
            });
            s.spawn(move |_| {
                let buf_ptr = ptr_r.recv().unwrap() as *const RaceBuffer<'_, OrderedEntry>;
                let snapper = RawPtrSnapper::new(buf_ptr);
                let mut output_buf = Vec::new();
                let mut buf_reader = async_reader::RaceReader::new(snapper, STORAGE_CAP);
                let mut rng = rand::thread_rng();

                let mut total_items_read = 0;
                while total_items_read < (NUM_WRITES - 1) as u64 {
                    let mut temp_buf = Vec::new();
                    let n_missed = buf_reader.read(&mut temp_buf).unwrap();
                    let n_read: u64 = temp_buf.iter().map(|we| we.size()).sum();
                    total_items_read += n_missed + n_read;

                    output_buf.push(OutputOrderedEntry::Missed(n_missed));
                    output_buf.extend(temp_buf.iter().map(|e| OutputOrderedEntry::Present(*e)));

                    let sleep_time: u64 = rng.gen::<u64>() % 3000;
                    std::thread::sleep(Duration::from_nanos(sleep_time));
                }
                OutputOrderedEntry::check_entries_correct_index(&output_buf[..]);
                OutputOrderedEntry::check_double_entries_consistent(&output_buf[..]);
                barr_w.wait();
            });
        })
        .unwrap();
    }
}
