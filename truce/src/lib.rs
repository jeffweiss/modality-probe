//#![no_std] // TODO - RESTORE - DEBUG

use static_assertions::assert_cfg;
assert_cfg!(not(target_pointer_width = "16"));

mod compact_log;
mod history;

use history::DynamicHistory;

use core::mem::{align_of, size_of};
use core::num::NonZeroU32;

pub const PRODUCED_BACKEND_LOG_REPORT: EventId =
    EventId(unsafe { NonZeroU32::new_unchecked(EventId::MAX_RAW_ID - 1) });
pub const LOG_OVERFLOWED: EventId =
    EventId(unsafe { NonZeroU32::new_unchecked(EventId::MAX_RAW_ID - 2) });
pub const LOGICAL_CLOCK_OVERFLOWED: EventId =
    EventId(unsafe { NonZeroU32::new_unchecked(EventId::MAX_RAW_ID - 3) });

/// Snapshot of causal history for transmission around the system
///
/// Note the use of bare integer types rather than the safety-oriented
/// wrappers (TracerId, NonZero*) for C representation reasons.
#[repr(C)]
#[derive(Clone)]
pub struct CausalSnapshot {
    /// The tracer node at which this history snapshot was created
    pub tracer_id: u32,

    /// Mapping between tracer_ids and event-counts at each location
    pub buckets: [LogicalClockBucket; 256],
    pub buckets_len: u8,
}

#[derive(Copy, Clone, Default, Debug, Ord, PartialOrd, Eq, PartialEq)]
#[repr(C)]
pub struct LogicalClockBucket {
    /// The tracer node that this clock is tracking
    pub id: u32,
    /// Clock tick count
    pub count: u32,
}

/// Ought to uniquely identify a location for where events occur within a system under test.
///
/// Typically represents a single thread.
///
/// Must be backed by a value greater than 0 and less than 0b1000_0000_0000_0000
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct TracerId(NonZeroU32);

impl TracerId {
    const MAX_RAW_ID: u32 = 0b0111_1111_1111_1111;

    /// raw_id must be greater than 0 and less than 0b1000_0000_0000_0000
    #[inline]
    pub fn new(raw_id: u32) -> Option<Self> {
        if raw_id > Self::MAX_RAW_ID {
            return None;
        }
        NonZeroU32::new(raw_id).map(|id| Self(id))
    }

    #[inline]
    pub fn get(&self) -> NonZeroU32 {
        self.0
    }

    #[inline]
    pub fn get_raw(&self) -> u32 {
        self.0.get()
    }
}

/// Uniquely identify an event or kind of event.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct EventId(NonZeroU32);

impl EventId {
    const MAX_RAW_ID: u32 = 0b0111_1111_1111_1111;
    const NUM_RESERVED_IDS: u32 = 256;
    const MAX_ID: u32 = EventId::MAX_RAW_ID - EventId::NUM_RESERVED_IDS;

    /// raw_id must be greater than 0 and less than 0b1000_0000_0000_0000
    #[inline]
    pub fn new(raw_id: u32) -> Option<Self> {
        if raw_id > Self::MAX_ID {
            return None;
        }
        NonZeroU32::new(raw_id).map(|id| Self(id))
    }

    #[inline]
    pub fn get(&self) -> NonZeroU32 {
        self.0
    }

    #[inline]
    pub fn get_raw(&self) -> u32 {
        self.0.get()
    }
}

#[derive(Debug)]
pub enum LocalStorageCreationError {
    UnderMinimumAllowedSize,
    ExceededMaximumAddressableSize,
    NullDestination,
}

/// Public interface to tracing.
#[derive(Debug)]
#[repr(C)]
pub struct Tracer<'a> {
    id: TracerId,
    history: &'a mut DynamicHistory,
}

/// Trace data collection interface
pub trait Backend: core::fmt::Debug {
    /// Transmits a Tracer's internal state according to the
    /// tracing data protocol to some storage or presentation
    /// or retransmission backend.
    ///
    /// Returns `true` if the data was successfully transmitted
    fn send_tracer_data(&mut self, data: &[u8]) -> bool;
}

impl<'a> Tracer<'a> {
    /// Initialize tracing for this location.
    /// `tracer_id` ought to be unique throughout the system.
    pub fn initialize_at(
        memory: &'a mut [u8],
        tracer_id: TracerId,
    ) -> Result<&'a mut Tracer<'a>, LocalStorageCreationError> {
        let tracer_align_offset = memory.as_mut_ptr().align_offset(align_of::<Tracer<'_>>());
        let tracer_bytes = tracer_align_offset + size_of::<Tracer<'_>>();
        if tracer_bytes > memory.len() {
            return Err(LocalStorageCreationError::UnderMinimumAllowedSize);
        }
        let tracer_ptr = unsafe { memory.as_mut_ptr().add(tracer_align_offset) as *mut Tracer<'_> };
        unsafe {
            *tracer_ptr = Tracer::new_with_storage(&mut memory[tracer_bytes..], tracer_id)?;
            Ok(tracer_ptr
                .as_mut()
                .expect("Tracer pointer should not be null"))
        }
    }

    pub fn new_with_storage(
        history_memory: &'a mut [u8],
        tracer_id: TracerId,
    ) -> Result<Tracer<'a>, LocalStorageCreationError> {
        let t = Tracer::<'a> {
            id: tracer_id,
            history: DynamicHistory::new_at(history_memory, tracer_id)?,
        };
        Ok(t)
    }

    /// Record that an event occurred. The end user is responsible
    /// for associating meaning with each event_id.
    #[inline]
    pub fn record_event(&mut self, event_id: EventId) {
        self.history.record_event(event_id);
    }

    /// Conduct necessary background activities and write
    /// the recorded reporting log to a collection backend.
    ///
    /// Writes the Tracer's internal state according to the
    /// log reporting schema.
    ///
    /// If the write was successful, returns the number of bytes written
    pub fn write_reporting(&mut self, destination: &mut [u8]) -> Result<usize, ()> {
        self.history.write_lcm_log_report(destination)
    }

    /// Write a summary of this tracer's causal history for use
    /// by another Tracer elsewhere in the system.
    ///
    /// This summary can be treated as an opaque blob of data
    /// that ought to be passed around to be `merge`d, though
    /// it will conform to an internal schema for the interested.
    ///
    /// Pre-pruned to the causal history of just this node
    ///  and its immediate inbound neighbors.
    ///
    /// If the write was successful, returns the number of bytes written
    pub fn share_history(&mut self, destination: &mut [u8]) -> Result<usize, ShareError> {
        self.history.write_lcm_logical_clock(destination)
    }

    /// Consume a causal history summary structure provided
    /// by some other Tracer.
    pub fn merge_history(&mut self, source: &[u8]) -> Result<(), MergeError> {
        self.history.merge_from_bytes(source)
    }

    /// Produce a transmittable summary of this tracer's
    /// causal history for use by another Tracer elsewhere
    /// in the system.
    ///
    /// Pre-pruned to the causal history of just this node
    ///  and its immediate inbound neighbors.
    pub fn share_fixed_size_history(&mut self) -> Result<CausalSnapshot, ShareError> {
        self.history.write_fixed_size_logical_clock()
    }

    /// Consume a fixed-sized causal history summary structure provided
    /// by some other Tracer.
    pub fn merge_fixed_size_history(
        &mut self,
        external_history: &CausalSnapshot,
    ) -> Result<(), MergeError> {
        self.history.merge_fixed_size(external_history)
    }
}

/// The errors than can occur when sharing (exporting / serializing)
/// a tracer's causal history for use by some other tracer instance.
#[derive(Debug, Clone, Copy)]
pub enum ShareError {
    /// The destination that is receiving the history is not big enough.
    ///
    /// Indicates that the end user should provide a larger destination buffer.
    InsufficientDestinationSize,
    /// An unexpected error occurred while writing out causal history.
    ///
    /// Indicates a logical error in the implementation of this library
    /// (or its dependencies).
    Encoding,
}

/// The errors than can occur when merging in the causal history from some
/// other tracer instance.
#[derive(Debug, Clone, Copy)]
pub enum MergeError {
    /// The local tracer does not have enough space to track all
    /// of direct neighbors attempting to communicate with it.
    ExceededAvailableClocks,
    /// The the external history we attempted to merge was encoded
    /// in an invalid fashion.
    ExternalHistoryEncoding,
    /// The external history violated a semantic rule of the protocol,
    /// such as by having a tracer_id out of the allowed value range.
    ExternalHistorySemantics,
}
