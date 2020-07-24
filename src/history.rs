use core::{
    cmp,
    convert::TryFrom,
    mem::{align_of, size_of},
};

use fixed_slice_vec::{
    single::{EmbedValueError, SplitUninitError},
    FixedSliceVec,
};
use race_buffer::writer::OverwrittenEntry;
use static_assertions::{assert_eq_align, assert_eq_size, const_assert, const_assert_eq};

use crate::{
    log::{LogEntry, RaceLog},
    wire::{report::WireReport, WireCausalSnapshot},
    CausalSnapshot, EventId, LogicalClock, MergeError, ModalityProbeInstant, OrdClock, ProbeEpoch,
    ProbeId, ProbeTicks, ProduceError, ReportError, StorageSetupError,
};

pub const MIN_CLOCKS_LEN: usize = 2;
pub const MIN_LOG_LEN: usize = MIN_CLOCKS_LEN * 16;
pub const MIN_HISTORY_SIZE_BYTES: usize = size_of::<DynamicHistory>()
    + 3 * size_of::<u32>()
    + MIN_CLOCKS_LEN * size_of::<LogicalClock>()
    + MIN_LOG_LEN * size_of::<LogEntry>();

const_assert_eq!(align_of::<usize>(), align_of::<DynamicHistory>());
const_assert_eq!(4, align_of::<LogicalClock>());
const_assert_eq!(4, align_of::<LogEntry>());

assert_eq_size!(u64, LogicalClock);
assert_eq_align!(u32, LogicalClock);

assert_eq_size!(u32, LogEntry);
assert_eq_align!(u32, LogEntry);

const_assert_eq!(12, size_of::<CausalSnapshot>());
const_assert_eq!(4, align_of::<CausalSnapshot>());

const_assert_eq!(12, size_of::<ModalityProbeInstant>());
const_assert_eq!(4, align_of::<ModalityProbeInstant>());

// TODO(dan@auxon.io): FIX ME
// const_assert_eq!(
//     10 + size_of::<FixedSliceVec<'_, LogicalClock>>()
//         + size_of::<CompactLogVec<'_>>()
//         + size_of::<ChunkedReportState>(),
//     size_of::<DynamicHistory>()
// );

/// Manages the core of a probe in-memory implementation
/// backed by runtime-sized arrays of current logical clocks
/// and probe log items
#[derive(Debug)]
pub struct DynamicHistory<'a> {
    pub(crate) probe_id: ProbeId,
    /// The number of events seen since the current
    /// probe's logical clock last increased.
    pub(crate) event_count: u32,
    /// Invariants:
    ///   * The first clock is always that of the local probe id
    pub(crate) clocks: FixedSliceVec<'a, LogicalClock>,
    /// Invariants:
    ///   * This log must always contain at least one item
    ///   * The first group of items in the log must always be logical clocks,
    /// starting with the local logical clock.
    pub(crate) log: RaceLog<'a>,
    self_clock: LogicalClock,
    read_cursor: usize,
    report_seq_num: u16,
}

#[derive(Debug)]
struct ClocksFullError;

impl<'a> DynamicHistory<'a> {
    #[inline]
    pub fn new_at(
        destination: &mut [u8],
        probe_id: ProbeId,
    ) -> Result<&mut DynamicHistory, StorageSetupError> {
        let remaining_bytes = destination.len();
        if remaining_bytes < MIN_HISTORY_SIZE_BYTES {
            return Err(StorageSetupError::UnderMinimumAllowedSize);
        }
        if destination.as_ptr().is_null() {
            return Err(StorageSetupError::NullDestination);
        }
        let history = match fixed_slice_vec::single::embed(destination, |dynamic_region_slice| {
            DynamicHistory::new(dynamic_region_slice, probe_id)
        }) {
            Ok(v) => Ok(v),
            Err(EmbedValueError::SplitUninitError(SplitUninitError::InsufficientSpace)) => {
                Err(StorageSetupError::UnderMinimumAllowedSize)
            }
            Err(EmbedValueError::SplitUninitError(SplitUninitError::Unalignable)) => {
                Err(StorageSetupError::UnderMinimumAllowedSize)
            }
            Err(EmbedValueError::SplitUninitError(SplitUninitError::ZeroSizedTypesUnsupported)) => {
                const_assert!(size_of::<DynamicHistory>() > 0);
                panic!("Static assertions ensure that this structure is not zero sized")
            }
            Err(EmbedValueError::ConstructionError(e)) => Err(e),
        }?;
        {
            let history_ptr = history as *mut DynamicHistory as usize;
            let clocks_ptr = history.clocks.as_slice().as_ptr() as usize;
            assert!(
                 history_ptr + size_of::<DynamicHistory>() <= clocks_ptr,
                "clocks pointer {:#X} should not overlap header [{:#X}..{:#X}] bytes, but they overlapped by {} bytes",
                clocks_ptr, history_ptr, history_ptr + size_of::<DynamicHistory>(), history_ptr + size_of::<DynamicHistory>() - clocks_ptr
            );
            assert!(
                clocks_ptr as usize + history.clocks.capacity() * size_of::<LogicalClock>()
                    <= history.log.as_slice().as_ptr() as usize,
                "log pointer should not overlap clock bytes"
            );
        }

        Ok(history)
    }

    #[inline]
    fn new(
        dynamic_region_slice: &'a mut [u8],
        probe_id: ProbeId,
    ) -> Result<Self, StorageSetupError> {
        let max_n_clocks = cmp::max(
            MIN_CLOCKS_LEN,
            dynamic_region_slice.len() / 8 / size_of::<LogicalClock>(),
        );
        let clocks_region_bytes = max_n_clocks * size_of::<LogicalClock>();
        if clocks_region_bytes > dynamic_region_slice.len() {
            return Err(StorageSetupError::UnderMinimumAllowedSize);
        }
        let (clocks_region, log_region) = dynamic_region_slice.split_at_mut(clocks_region_bytes);
        let mut clocks = FixedSliceVec::from_bytes(clocks_region);
        let log = RaceLog::new_from_bytes(log_region)
            .map_err(|_| StorageSetupError::UnderMinimumAllowedSize)?;
        if clocks.capacity() < MIN_CLOCKS_LEN || log.capacity() < MIN_LOG_LEN {
            return Err(StorageSetupError::UnderMinimumAllowedSize);
        }
        clocks
            .try_push(LogicalClock {
                id: probe_id,
                epoch: ProbeEpoch(0),
                ticks: ProbeTicks(0),
            })
            .expect(
                "The History.clocks field should always contain a clock for this probe instance",
            );
        let history = DynamicHistory {
            read_cursor: 0,
            report_seq_num: 0,
            event_count: 0,
            self_clock: LogicalClock {
                id: probe_id,
                epoch: ProbeEpoch(0),
                ticks: ProbeTicks(0),
            },
            probe_id,
            clocks,
            log,
        };
        Ok(history)
    }

    /// Add an item to the internal log that records this event
    /// occurred.
    ///
    /// Note: this function overwrites older events in the log if it
    /// is full.
    #[inline]
    pub(crate) fn record_event(&mut self, event_id: EventId) {
        // N.B. point for future improvement - basic compression here
        if let OverwrittenEntry::Double(one, two) = self.log.write(LogEntry::event(event_id)) {
            if one.has_clock_bit_set() {
                let (epoch, ticks) = crate::unpack_clock_word(two.raw());
                // If what we get out of the log is garbage, i.e., a
                // zero-valued probe id, just discard it.
                if let Some(id) = ProbeId::new(one.interpret_as_logical_clock_probe_id()) {
                    self.merge_clock(LogicalClock { id, epoch, ticks });
                }
            }
        }
        self.event_count = self.event_count.saturating_add(1);
    }

    /// Add the event and its payload to the internal log, recording
    /// that this event occurred.
    ///
    /// Note: this function overwrites older events in the log if it
    /// is full.
    #[inline]
    pub(crate) fn record_event_with_payload(&mut self, event_id: EventId, payload: u32) {
        let (ev, pay) = LogEntry::event_with_payload(event_id, payload);
        if let OverwrittenEntry::Double(one, two) = self.log.write(ev) {
            let (epoch, ticks) = crate::unpack_clock_word(two.raw());
            // If what we get out of the log is garbage, i.e., a
            // zero-valued probe id, just discard it.
            if let Some(id) = ProbeId::new(one.interpret_as_logical_clock_probe_id()) {
                self.merge_clock(LogicalClock { id, epoch, ticks });
            }
        }
        if let OverwrittenEntry::Double(one, two) = self.log.write(pay) {
            let (epoch, ticks) = crate::unpack_clock_word(two.raw());
            // If what we get out of the log is garbage, i.e., a
            // zero-valued probe id, just discard it.
            if let Some(id) = ProbeId::new(one.interpret_as_logical_clock_probe_id()) {
                self.merge_clock(LogicalClock { id, epoch, ticks });
            }
        }
        self.event_count = self.event_count.saturating_add(1);
    }

    /// Increments the clock in the logical clock corresponding to this probe instance
    #[inline]
    fn increment_local_clock(&mut self) {
        // N.B. We rely on the fact that the first member of the clocks
        // collection is always the clock for this probe
        self.self_clock.increment();
        self.event_count = 0;
        self.report_seq_num = 0;
    }

    #[inline]
    pub(crate) fn produce_snapshot(&mut self) -> Result<CausalSnapshot, ProduceError> {
        let snap = CausalSnapshot {
            clock: self.self_clock,
            reserved_0: 0,
            reserved_1: 0,
        };
        self.increment_local_clock();
        self.write_clocks_to_log(&[self.self_clock]);
        Ok(snap)
    }

    #[inline]
    pub(crate) fn produce_snapshot_bytes(
        &mut self,
        destination: &mut [u8],
    ) -> Result<usize, ProduceError> {
        let mut s = WireCausalSnapshot::new_unchecked(destination);
        s.check_len()?;
        s.set_probe_id(self.self_clock.id);
        s.set_epoch(self.self_clock.epoch);
        s.set_ticks(self.self_clock.ticks);
        s.set_reserved_0(0);
        s.set_reserved_1(0);
        self.increment_local_clock();
        self.write_clocks_to_log(&[self.self_clock]);
        Ok(WireCausalSnapshot::<&[u8]>::min_buffer_len())
    }

    #[inline]
    pub(crate) fn merge_snapshot(
        &mut self,
        external_history: &CausalSnapshot,
    ) -> Result<(), MergeError> {
        self.merge_internal(
            external_history.clock.id,
            external_history.clock.epoch,
            external_history.clock.ticks,
        )
    }

    #[inline]
    pub(crate) fn merge_snapshot_bytes(&mut self, source: &[u8]) -> Result<(), MergeError> {
        let external_history = CausalSnapshot::try_from(source)?;
        self.merge_internal(
            external_history.clock.id,
            external_history.clock.epoch,
            external_history.clock.ticks,
        )
    }

    pub(crate) fn report(&mut self, destination: &mut [u8]) -> Result<usize, ReportError> {
        // Can I get at least two entries in here (just in case the
        // first entry is a clock)?
        if destination.len()
            < WireReport::<&[u8]>::HEADER_LEN
                + (self.clocks.len() * size_of::<LogicalClock>())
                + (size_of::<LogEntry>() * 2)
        {
            todo!("buffer too small");
        }

        let read_curs = self.read_cursor;
        let self_clock = self.self_clock;
        let clocks_len = self.clocks.len();
        let mut report = WireReport::init_from(destination);

        report.set_fingerprint();
        report.set_probe_id(self.probe_id);
        report.set_clock(crate::pack_clock_word(self_clock.epoch, self_clock.ticks));
        report.set_seq_num(self.report_seq_num);
        report.set_n_clocks(clocks_len as u16);

        let payload = report.payload_mut();
        for (c, dest_bytes) in self
            .clocks
            .iter()
            .zip(payload.chunks_exact_mut(size_of::<LogicalClock>()))
        {
            dest_bytes.copy_from_slice(&c.to_le_bytes());
        }

        let n_log_entries_possible = cmp::min(
            payload.len() - (clocks_len * size_of::<LogicalClock>()),
            self.log.write_cursor().saturating_sub(read_curs),
        );

        let mut n_copied = 0;
        let mut clock_id = None;
        let clocks = &mut self.clocks;
        for (entry, dest_bytes) in self.log.iter(read_curs).zip(
            payload[clocks_len * size_of::<LogicalClock>()..]
                .chunks_exact_mut(size_of::<LogEntry>()),
        ) {
            match entry {
                Some(e) => {
                    if e.has_clock_bit_set() {
                        if n_copied <= n_log_entries_possible - 2 {
                            dest_bytes.copy_from_slice(&e.raw().to_le_bytes());
                            clock_id = ProbeId::new(e.interpret_as_logical_clock_probe_id());
                            n_copied += 1;
                        } else {
                            break;
                        }
                    } else {
                        dest_bytes.copy_from_slice(&e.raw().to_le_bytes());
                        if let Some(id) = clock_id {
                            let (epoch, ticks) = crate::unpack_clock_word(e.raw());
                            Self::merge_clocks(clocks, LogicalClock { id, epoch, ticks });
                            clock_id = None;
                        }
                        n_copied += 1;
                    }
                }
                None => todo!("something to do with a missed item"),
            }
        }
        report.set_n_log_entries(n_copied as u32);

        self.read_cursor = read_curs + n_copied;
        self.report_seq_num += 1;
        Ok(WireReport::<&[u8]>::HEADER_LEN
            + (clocks_len * size_of::<LogicalClock>())
            + (n_copied * size_of::<LogEntry>()))
    }

    #[inline]
    fn merge_internal(
        &mut self,
        external_id: ProbeId,
        external_epoch: ProbeEpoch,
        external_clock: ProbeTicks,
    ) -> Result<(), MergeError> {
        self.increment_local_clock();
        self.write_clocks_to_log(&[
            self.self_clock,
            LogicalClock {
                id: external_id,
                epoch: external_epoch,
                ticks: external_clock,
            },
        ]);
        Ok(())
    }

    #[inline]
    fn write_clocks_to_log(&mut self, clocks: &[LogicalClock]) {
        for c in clocks.iter() {
            let (probe_id, clock) = LogEntry::clock(*c);
            self.log.write(probe_id);
            self.log.write(clock);
        }
    }

    pub(crate) fn now(&self) -> ModalityProbeInstant {
        ModalityProbeInstant {
            clock: self.self_clock,
            event_count: self.event_count,
        }
    }

    fn merge_clock(&mut self, ext_clock: LogicalClock) {
        let mut existed = false;
        for c in self.clocks.iter_mut() {
            if c.id == ext_clock.id {
                if OrdClock(ext_clock.epoch, ext_clock.ticks) > OrdClock(c.epoch, c.ticks) {
                    c.epoch = ext_clock.epoch;
                    c.ticks = ext_clock.ticks;
                }
                existed = true;
            }
        }
        if !existed {
            if self.clocks.try_push(ext_clock).is_err() {
                self.record_event(EventId::EVENT_NUM_CLOCKS_OVERFLOWED);
            }
        }
    }

    // TODO(dan@auxon.io): Combine these
    fn merge_clocks<'c>(clocks: &mut FixedSliceVec<'c, LogicalClock>, ext_clock: LogicalClock) {
        let mut existed = false;
        for c in clocks.iter_mut() {
            if c.id == ext_clock.id {
                if OrdClock(ext_clock.epoch, ext_clock.ticks) > OrdClock(c.epoch, c.ticks) {
                    c.epoch = ext_clock.epoch;
                    c.ticks = ext_clock.ticks;
                }
                existed = true;
            }
        }
        if !existed {
            let _ = clocks.try_push(ext_clock);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    impl std::ops::Sub for ProbeEpoch {
        type Output = Self;
        fn sub(self, other: Self) -> Self {
            ProbeEpoch(self.0 - other.0)
        }
    }

    #[test]
    fn epoch_rollover() {
        let probe_a = ProbeId::new(1).unwrap();
        let probe_b = ProbeId::new(2).unwrap();

        let lc = |id: ProbeId, epoch: u16, clock: u16| LogicalClock {
            id,
            epoch: ProbeEpoch(epoch),
            ticks: ProbeTicks(clock),
        };

        let find_clock =
            |h: &DynamicHistory, id: ProbeId| h.clocks.iter().find(|c| c.id == id).cloned();

        {
            let mut storage_a = [0u8; 512];
            let h = DynamicHistory::new_at(&mut storage_a, probe_a).unwrap();

            h.merge_internal(probe_b, ProbeEpoch(1), ProbeTicks(1))
                .unwrap();
            assert_eq!(find_clock(&h, probe_b), Some(lc(probe_b, 1, 1)));

            // Sanity check just the clock tick
            h.merge_internal(probe_b, ProbeEpoch(1), ProbeTicks(2))
                .unwrap();
            assert_eq!(find_clock(&h, probe_b), Some(lc(probe_b, 1, 2)));

            // Sanity check the epoch tick
            h.merge_internal(probe_b, ProbeEpoch(2), ProbeTicks(2))
                .unwrap();
            assert_eq!(find_clock(&h, probe_b), Some(lc(probe_b, 2, 2)));

            // Can't roll back the clock
            h.merge_internal(probe_b, ProbeEpoch(2), ProbeTicks(1))
                .unwrap();
            assert_eq!(find_clock(&h, probe_b), Some(lc(probe_b, 2, 2)));

            // Can't roll back the epoch
            h.merge_internal(probe_b, ProbeEpoch(1), ProbeTicks(3))
                .unwrap();
            assert_eq!(find_clock(&h, probe_b), Some(lc(probe_b, 2, 2)));

            // Go to the very edge of the epoch's range
            let emax = ProbeEpoch::MAX;
            let cmax = ProbeTicks::MAX;
            h.merge_internal(probe_b, emax, cmax).unwrap();
            assert_eq!(find_clock(&h, probe_b), Some(lc(probe_b, emax.0, cmax.0)));

            // Wraparound to 1 is now allowed
            h.merge_internal(probe_b, ProbeEpoch(1), ProbeTicks(1))
                .unwrap();
            assert_eq!(find_clock(&h, probe_b), Some(lc(probe_b, 1, 1)));
        }

        // Wrap around can happen even if a few messages were missed (because of
        // repeated restarts)
        {
            let mut storage_a = [0u8; 512];
            let h = DynamicHistory::new_at(&mut storage_a, probe_a).unwrap();

            h.merge_internal(probe_b, ProbeEpoch::MAX - ProbeEpoch(2), ProbeTicks(1))
                .unwrap();
            h.merge_internal(probe_b, ProbeEpoch(2), ProbeTicks(1))
                .unwrap();
            assert_eq!(find_clock(&h, probe_b), Some(lc(probe_b, 2, 1)));
        }

        // But not outside the threshold
        {
            let mut storage_a = [0u8; 512];
            let h = DynamicHistory::new_at(&mut storage_a, probe_a).unwrap();

            h.merge_internal(probe_b, ProbeEpoch::MAX - ProbeEpoch(2), ProbeTicks(1))
                .unwrap();
            h.merge_internal(probe_b, ProbeEpoch(5), ProbeTicks(1))
                .unwrap();
            assert_eq!(
                find_clock(&h, probe_b),
                Some(lc(probe_b, ProbeEpoch::MAX.0 - 2, 1))
            );
        }
    }
}
