//! A wire protocol for representing Modality probe log reports
//! that are fragmented into multiple chunks due to sizing
//! constraints

use crate::compact_log::CompactLogItem;
use crate::history::DynamicHistory;
use crate::wire::{ChunkPayloadDataType, ChunkedReportWireError, WireChunkedReport};
use crate::ProbeId;
use crate::ReportError;
use core::borrow::Borrow;
use core::mem::{size_of, MaybeUninit};

/// The size of a chunk in u32s, the 4-byte pieces we align these messages to.
pub const MAX_CHUNK_U32_WORDS: usize = 256 / size_of::<u32>();
/// The maximum number of compact log items (events or clocks)
/// that could fit in a chunk.
pub const MAX_PAYLOAD_COMPACT_LOG_ITEMS_PER_CHUNK: usize =
    WireChunkedReport::<&[u8]>::MAX_PAYLOAD_BYTES_PER_CHUNK / size_of::<CompactLogItem>();

/// The slice input was an incorrect length.
#[derive(Debug, PartialEq, Eq)]
pub struct IncorrectLengthSlice;

/// The things that can go wrong when writing a chunked report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChunkedReportError {
    /// One of the general-case errors that can occur
    /// in producing a report.
    ReportError(crate::ReportError),
    /// No chunked report transaction has been started.
    NoChunkedReportInProgress,
}

/// Correlation value threaded through the steps of a chunked
/// report in order to prevent data corruption.
#[repr(transparent)]
#[derive(Debug, PartialEq, Eq)]
pub struct ChunkedReportToken {
    /// The identifier for this report / group-of-chunks
    group_id: u16,
}

/// Write reports in 1+ chunks.
///
/// Call `start_chunked_report` once at the start of a report.
/// Call `write_next_report_chunk` as many times as needed to
/// produce all the report pieces, transmitting each piece as you go.
/// Call `finish_chunked_report` when all the chunks have been made.
pub trait ChunkedReporter {
    /// Prepare for chunked reporting, beginning a chunked reporting transaction.
    /// Returns a token that should be re-used within this chunked reporting transaction.
    ///
    /// Lock out instance internal mutation to a degree necessary to prevent data corruption.
    fn start_chunked_report(&mut self) -> Result<ChunkedReportToken, ChunkedReportError>;

    /// Write up to MAX_CHUNK_BYTES bytes of the chunked format into the provided buffer.
    ///
    /// Return number of bytes written on success.
    ///
    /// A successful return value of 0 bytes indicates that no payload bytes could be written
    /// because all chunks for this report transaction have been handled.
    fn write_next_report_chunk(
        &mut self,
        token: &ChunkedReportToken,
        destination: &mut [u8],
    ) -> Result<usize, ChunkedReportError>;

    /// Idempotent cessation and cleanup of a (possibly error'd out)
    /// chunked reporting transaction.
    /// Undo any internal mutation locks related to reporting.
    fn finish_chunked_report(
        &mut self,
        token: ChunkedReportToken,
    ) -> Result<(), ChunkedReportError>;
}

#[derive(Debug)]
pub(crate) struct ChunkedReportState {
    /// Use the `false` value to indicate "no chunked report in progress"
    is_report_in_progress: bool,
    /// Indicate the group id of the most recently or currently
    /// active report.
    pub most_recent_group_id: u16,
    /// How many chunks have been written for the report in progress
    /// already.
    pub n_written_chunks: u16,
}

impl ChunkedReportState {
    pub(crate) fn is_report_in_progress(&self) -> bool {
        self.is_report_in_progress
    }
}

impl Default for ChunkedReportState {
    fn default() -> Self {
        ChunkedReportState {
            is_report_in_progress: false,
            most_recent_group_id: 0,
            n_written_chunks: 0,
        }
    }
}

impl<'data> ChunkedReporter for DynamicHistory<'data> {
    fn start_chunked_report(&mut self) -> Result<ChunkedReportToken, ChunkedReportError> {
        if self.chunked_report_state.is_report_in_progress() {
            return Err(ChunkedReportError::ReportError(
                ReportError::ReportLockConflict,
            ));
        }
        self.chunked_report_state.is_report_in_progress = true;
        let (group_id, _overflowed) = self
            .chunked_report_state
            .most_recent_group_id
            .overflowing_add(1);
        self.chunked_report_state.most_recent_group_id = group_id;
        self.chunked_report_state.n_written_chunks = 0;
        Ok(ChunkedReportToken { group_id })
    }

    fn write_next_report_chunk(
        &mut self,
        token: &ChunkedReportToken,
        destination: &mut [u8],
    ) -> Result<usize, ChunkedReportError> {
        if !self.chunked_report_state.is_report_in_progress() {
            return Err(ChunkedReportError::NoChunkedReportInProgress);
        }
        if token.group_id != self.chunked_report_state.most_recent_group_id {
            return Err(ChunkedReportError::ReportError(
                ReportError::ReportLockConflict,
            ));
        }
        let current_chunk_index = self.chunked_report_state.n_written_chunks;

        let (log_index, is_last_chunk, items_for_current_chunk) = {
            let curr_log_len = self.compact_log.len();
            let possible_log_index = usize::from(self.chunked_report_state.n_written_chunks)
                * MAX_PAYLOAD_COMPACT_LOG_ITEMS_PER_CHUNK;
            let (log_index, n_log_items_left) = if possible_log_index > curr_log_len {
                (0, 0)
            } else {
                (possible_log_index, curr_log_len - possible_log_index)
            };
            let is_last_chunk =
                n_log_items_left == 0 || n_log_items_left < MAX_PAYLOAD_COMPACT_LOG_ITEMS_PER_CHUNK;
            let items_for_current_chunk =
                core::cmp::min(n_log_items_left, MAX_PAYLOAD_COMPACT_LOG_ITEMS_PER_CHUNK);
            (log_index, is_last_chunk, items_for_current_chunk)
        };
        let n_chunk_payload_bytes = items_for_current_chunk * size_of::<CompactLogItem>();
        debug_assert!(n_chunk_payload_bytes <= core::u8::MAX as usize);

        if n_chunk_payload_bytes == 0 {
            self.chunked_report_state.n_written_chunks = current_chunk_index.saturating_add(1);
            return Ok(0);
        }

        let required_bytes = WireChunkedReport::<&[u8]>::buffer_len(n_chunk_payload_bytes);
        if destination.len() < required_bytes {
            return Err(ChunkedReportError::ReportError(
                ReportError::InsufficientDestinationSize,
            ));
        }

        let log_slice =
            &self.compact_log.as_slice()[log_index..log_index + items_for_current_chunk];

        let mut report = WireChunkedReport::new_unchecked(&mut destination[..]);
        report.set_fingerprint();
        report.set_probe_id(self.probe_id);
        report.set_chunk_group_id(token.group_id);
        report.set_chunk_index(current_chunk_index);
        report.set_payload_data_type(ChunkPayloadDataType::Log);
        report.set_is_last_chunk(is_last_chunk);
        report.set_reserved(0);
        report.set_n_chunk_payload_bytes(n_chunk_payload_bytes as u8);

        let payload_destination = report.payload_mut();
        super::write_log_as_little_endian_bytes(payload_destination, log_slice)
            .map_err(ChunkedReportError::ReportError)?;

        self.chunked_report_state.n_written_chunks = current_chunk_index.saturating_add(1);
        Ok(required_bytes)
    }

    fn finish_chunked_report(
        &mut self,
        token: ChunkedReportToken,
    ) -> Result<(), ChunkedReportError> {
        if !self.chunked_report_state.is_report_in_progress() {
            return Err(ChunkedReportError::NoChunkedReportInProgress);
        }
        if token.group_id != self.chunked_report_state.most_recent_group_id {
            return Err(ChunkedReportError::ReportError(
                ReportError::ReportLockConflict,
            ));
        }

        // This field is effectively used as our lock
        self.chunked_report_state.is_report_in_progress = false;
        // Reset tracking state
        self.chunked_report_state.n_written_chunks = 0;
        debug_assert!(!self.chunked_report_state.is_report_in_progress());
        self.finished_report_logging();

        Ok(())
    }
}

/// An interpreted version of the chunk format
/// which represents the values in the correct
/// endianness for the executing platform.
#[derive(PartialEq)]
pub enum NativeChunk {
    /// A chunk containing values in the compact log format.
    Log {
        /// Chunk framing information
        header: NativeChunkHeader,
        /// The contents of the chunk, interpreted as the compact log format
        contents: NativeChunkLogContents,
    },
    /// A chunk containing arbitrary extension bytes
    Extension {
        /// Chunk framing information
        header: NativeChunkHeader,
        /// The contents of the chunk, interpreted as bare extension bytes
        contents: NativeChunkExtensionContents,
    },
}

impl NativeChunk {
    /// Extract the common framing header information
    pub fn header(&self) -> &NativeChunkHeader {
        match self {
            NativeChunk::Log { header, .. } => header,
            NativeChunk::Extension { header, .. } => header,
        }
    }

    /// How many bytes are in the payload?
    pub fn n_chunk_payload_bytes(&self) -> usize {
        match self {
            NativeChunk::Log { contents, .. } => {
                usize::from(contents.n_chunk_payload_items) * size_of::<CompactLogItem>()
            }
            NativeChunk::Extension { contents, .. } => usize::from(contents.n_chunk_payload_bytes),
        }
    }

    /// Produce an owned natively-usable interpretation of a chunked report
    /// from the barely-structured on-the-wire representation
    pub fn from_wire_bytes<B: Borrow<[u8]>>(
        borrow_wire_bytes: B,
    ) -> Result<NativeChunk, ChunkedReportWireError> {
        let wire_bytes = borrow_wire_bytes.borrow();

        let report = WireChunkedReport::new(&wire_bytes[..])?;

        let probe_id = report.probe_id()?;
        let chunk_group_id = report.chunk_group_id();
        let chunk_index = report.chunk_index();
        let is_last_chunk = report.is_last_chunk();
        let reserved = report.reserved();
        let n_payload_bytes = report.n_chunk_payload_bytes();
        let data_type = report.payload_data_type()?;

        let header = NativeChunkHeader {
            probe_id,
            chunk_group_id,
            chunk_index,
            is_last_chunk,
            reserved,
        };
        let payload_bytes = &report.payload()[..usize::from(n_payload_bytes)];
        Ok(match data_type {
            ChunkPayloadDataType::Log => {
                // Assuming init is always safe when initializing an array of MaybeUninit values
                let mut payload: [MaybeUninit<CompactLogItem>;
                    MAX_PAYLOAD_COMPACT_LOG_ITEMS_PER_CHUNK] =
                    unsafe { MaybeUninit::uninit().assume_init() };
                debug_assert!(core::mem::size_of_val(&payload) >= usize::from(n_payload_bytes));
                debug_assert!(core::mem::size_of_val(&payload) >= payload_bytes.len());
                let n_payload_items = n_payload_bytes / (size_of::<CompactLogItem>() as u8);
                for (source, dest) in payload_bytes
                    .chunks_exact(size_of::<CompactLogItem>())
                    .zip(payload.iter_mut())
                {
                    *dest = MaybeUninit::new(CompactLogItem::from_raw(u32::from_le_bytes([
                        source[0], source[1], source[2], source[3],
                    ])));
                }
                NativeChunk::Log {
                    header,
                    contents: NativeChunkLogContents {
                        n_chunk_payload_items: n_payload_items,
                        payload,
                    },
                }
            }
            ChunkPayloadDataType::Extension => {
                // Assuming init is always safe when initializing an array of MaybeUninit values
                let mut payload: [MaybeUninit<u8>;
                    WireChunkedReport::<&[u8]>::MAX_PAYLOAD_BYTES_PER_CHUNK] =
                    unsafe { MaybeUninit::uninit().assume_init() };
                for (source, sink) in payload_bytes.iter().zip(payload.iter_mut()) {
                    *sink = MaybeUninit::new(*source);
                }
                NativeChunk::Extension {
                    header,
                    contents: NativeChunkExtensionContents {
                        n_chunk_payload_bytes: n_payload_bytes,
                        payload,
                    },
                }
            }
        })
    }
}

/// Chunk framing information, owned and accessible in the native
/// endianness.
#[derive(Debug, PartialEq)]
pub struct NativeChunkHeader {
    /// The probe_id of the
    /// Modality probe instance producing this report.
    pub probe_id: ProbeId,
    /// A u16 representing the group of report chunks to which
    /// this chunk belongs. Determined by the Modality probe instance.
    /// Expected, but not guaranteed to be increasing
    /// and wrapping-overflowing during the lifetime of an Modality probe
    /// instance.
    pub chunk_group_id: u16,
    /// In what ordinal position does this chunk's payload live relative to the other chunks.
    pub chunk_index: u16,
    /// Is this chunk the last chunk in the report (0001) or not (0000)?
    pub is_last_chunk: bool,
    /// Reserved for future enhancements and to make the payload 4-byte aligned
    pub reserved: u8,
}

/// The contents of the chunk, interpreted as the compact log format
///
/// This struct maintains the invariant that the members of the `payload`
/// field are initialized up to `n_chunk_payload_items` value
pub struct NativeChunkLogContents {
    /// How many of the payload items are populated?
    n_chunk_payload_items: u8,
    /// The content of the report chunk
    payload: [MaybeUninit<CompactLogItem>; MAX_PAYLOAD_COMPACT_LOG_ITEMS_PER_CHUNK],
}

impl PartialEq for NativeChunkLogContents {
    fn eq(&self, other: &Self) -> bool {
        self.n_chunk_payload_items == other.n_chunk_payload_items
            && self
                .payload
                .iter()
                .take(usize::from(self.n_chunk_payload_items))
                .zip(
                    other
                        .payload
                        .iter()
                        .take(usize::from(other.n_chunk_payload_items)),
                )
                .all(|(s, o)| unsafe { s.assume_init() == o.assume_init() })
    }
}

impl NativeChunkLogContents {
    /// Access to the log as an immutable slice
    pub fn log_slice(&self) -> &[CompactLogItem] {
        let slice: &[MaybeUninit<CompactLogItem>] =
            &self.payload[..usize::from(self.n_chunk_payload_items)];
        // Due to the promises of MaybeUninit, it is sound to pointer-transmute here
        unsafe { &*(slice as *const [MaybeUninit<CompactLogItem>] as *const [CompactLogItem]) }
    }
    /// Access to the log as a mutable slice
    pub fn log_slice_mut(&mut self) -> &mut [CompactLogItem] {
        let slice: &mut [MaybeUninit<CompactLogItem>] =
            &mut self.payload[..usize::from(self.n_chunk_payload_items)];
        // Due to the promises of MaybeUninit, it is sound to pointer-transmute here
        unsafe { &mut *(slice as *mut [MaybeUninit<CompactLogItem>] as *mut [CompactLogItem]) }
    }
}

/// The contents of the chunk, interpreted as bare extension bytes
///
/// This struct maintains the invariant that the members of the `payload`
/// field are initialized up to `n_chunk_payload_bytes` value
pub struct NativeChunkExtensionContents {
    /// How many of the payload bytes are populated?
    n_chunk_payload_bytes: u8,
    /// The content of the report chunk
    payload: [MaybeUninit<u8>; WireChunkedReport::<&[u8]>::MAX_PAYLOAD_BYTES_PER_CHUNK],
}

impl PartialEq for NativeChunkExtensionContents {
    fn eq(&self, other: &Self) -> bool {
        self.n_chunk_payload_bytes == other.n_chunk_payload_bytes
            && self
                .payload
                .iter()
                .take(usize::from(self.n_chunk_payload_bytes))
                .zip(
                    other
                        .payload
                        .iter()
                        .take(usize::from(other.n_chunk_payload_bytes)),
                )
                .all(|(s, o)| unsafe { s.assume_init() == o.assume_init() })
    }
}

impl NativeChunkExtensionContents {
    /// Access to the payload as an immutable slice
    pub fn payload_slice(&self) -> &[u8] {
        let slice: &[MaybeUninit<u8>] = &self.payload[..usize::from(self.n_chunk_payload_bytes)];
        // Due to the promises of MaybeUninit, it is sound to pointer-transmute here
        unsafe { &*(slice as *const [MaybeUninit<u8>] as *const [u8]) }
    }
    /// Access to the payload as a mutable slice
    pub fn payload_slice_mut(&mut self) -> &mut [u8] {
        let slice: &mut [MaybeUninit<u8>] =
            &mut self.payload[..usize::from(self.n_chunk_payload_bytes)];
        // Due to the promises of MaybeUninit, it is sound to pointer-transmute here
        unsafe { &mut *(slice as *mut [MaybeUninit<u8>] as *mut [u8]) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compact_log::log_tests::*;
    use crate::compact_log::LogEvent;
    use crate::id::*;
    use crate::wire::chunked_report::*;
    use crate::*;
    use core::convert::TryInto;
    use proptest::prelude::*;
    use proptest::std_facade::*;

    const MAX_CHUNK_BYTES: usize = WireChunkedReport::<&[u8]>::MAX_CHUNK_BYTES;

    #[test]
    fn chunked_report_happy_path_single_chunk() {
        let probe_id = 1u32.try_into().expect("Invalid probe id");
        let mut report_transmission_buffer = [0u8; MAX_CHUNK_BYTES];
        let mut storage = [0u8; 4096];
        let mut eko = ModalityProbe::new_with_storage(&mut storage, probe_id)
            .expect("Could not initialize Modality probe");
        let token = eko
            .start_chunked_report()
            .expect("Could not start chunked report");
        let n_report_bytes = eko
            .write_next_report_chunk(&token, &mut report_transmission_buffer)
            .expect("Could not write chunk");
        // For now, we expect just a single logical clock (the local one) to be written in the log since no events were recorded
        // and no other logical histories merged in.
        let expected_size_bytes = WireChunkedReport::<&[u8]>::buffer_len(size_of::<LogicalClock>());
        assert_eq!(expected_size_bytes, n_report_bytes);
        let n_report_bytes = eko
            .write_next_report_chunk(&token, &mut report_transmission_buffer)
            .expect("Could not write chunk");
        assert_eq!(0, n_report_bytes);
        eko.finish_chunked_report(token)
            .expect("Could not finish chunked report")
    }

    #[test]
    fn chunked_report_happy_path_multi_chunk() {
        let probe_id = 1u32.try_into().expect("Invalid probe id");
        let mut report_transmission_buffer = [0u8; MAX_CHUNK_BYTES];
        let mut storage = [0u8; 4096];
        let mut eko = ModalityProbe::new_with_storage(&mut storage, probe_id)
            .expect("Could not initialize Modality probe");
        for i in 1..=MAX_PAYLOAD_COMPACT_LOG_ITEMS_PER_CHUNK {
            eko.record_event(EventId::new(i as u32).unwrap());
        }
        let token = eko
            .start_chunked_report()
            .expect("Could not start chunked report");
        let n_report_bytes = eko
            .write_next_report_chunk(&token, &mut report_transmission_buffer)
            .expect("Could not write chunk");
        // For now, we expect a single logical clock (the local one) to be written plus most of the events above
        // completely filling the chunk
        assert_eq!(MAX_CHUNK_BYTES, n_report_bytes);
        let n_report_bytes = eko
            .write_next_report_chunk(&token, &mut report_transmission_buffer)
            .expect("Could not write chunk");
        // Two events shouldn't have been able to fit in the prior report
        let expected_size_bytes =
            WireChunkedReport::<&[u8]>::buffer_len(2 * size_of::<CompactLogItem>());
        assert_eq!(expected_size_bytes, n_report_bytes);
        let n_report_bytes = eko
            .write_next_report_chunk(&token, &mut report_transmission_buffer)
            .expect("Could not write chunk");
        // Should signal done-ness with a 0-payload-written return
        assert_eq!(0, n_report_bytes);
        eko.finish_chunked_report(token)
            .expect("Could not finish chunked report")
    }

    #[test]
    fn chunked_report_prevents_produce_and_merge_and_bulk_report_while_in_progress() {
        let mut other_transmission_buffer = [0u8; 1024];

        let probe_id_foo = 1u32.try_into().expect("Invalid probe id");
        let mut report_transmission_buffer = [0u8; MAX_CHUNK_BYTES];
        let mut storage_foo = [0u8; 4096];
        let mut eko_foo = ModalityProbe::new_with_storage(&mut storage_foo, probe_id_foo)
            .expect("Could not initialize Modality probe");

        let probe_id_bar = 1u32.try_into().expect("Invalid probe id");
        let mut storage_bar = [0u8; 4096];
        let mut eko_bar = ModalityProbe::new_with_storage(&mut storage_bar, probe_id_bar)
            .expect("Could not initialize Modality probe");
        let bar_snapshot_len = eko_bar
            .produce_snapshot_bytes(&mut other_transmission_buffer)
            .unwrap();
        let bar_fixed_snapshot = eko_bar.produce_snapshot().unwrap();

        let token = eko_foo
            .start_chunked_report()
            .expect("Could not start chunked report");

        assert_eq!(
            MergeError::ReportLockConflict,
            eko_foo
                .merge_snapshot_bytes(&other_transmission_buffer[..bar_snapshot_len])
                .unwrap_err()
        );
        assert_eq!(
            MergeError::ReportLockConflict,
            eko_foo.merge_snapshot(&bar_fixed_snapshot).unwrap_err()
        );

        assert_eq!(
            ProduceError::ReportLockConflict,
            eko_foo.produce_snapshot().unwrap_err()
        );

        assert_eq!(
            ProduceError::ReportLockConflict,
            eko_foo
                .produce_snapshot_bytes(&mut other_transmission_buffer)
                .unwrap_err()
        );

        assert_eq!(
            ReportError::ReportLockConflict,
            eko_foo.report(&mut other_transmission_buffer).unwrap_err()
        );
        assert_eq!(
            ReportError::ReportLockConflict,
            eko_foo
                .report_with_extension(&mut other_transmission_buffer, ExtensionBytes(&[]))
                .unwrap_err()
        );

        // Recorded events are silently dropped
        eko_foo.record_event(EventId::new(271).unwrap());

        let n_report_bytes = eko_foo
            .write_next_report_chunk(&token, &mut report_transmission_buffer)
            .expect("Could not write chunk");
        // For now, we expect just a single logical clock (the local one) to be written in the log since no events were recorded
        // and no other logical histories merged in.
        let expected_size_bytes = WireChunkedReport::<&[u8]>::buffer_len(size_of::<LogicalClock>());
        assert_eq!(expected_size_bytes, n_report_bytes);
        let n_report_bytes = eko_foo
            .write_next_report_chunk(&token, &mut report_transmission_buffer)
            .expect("Could not write chunk");
        assert_eq!(0, n_report_bytes);
        eko_foo
            .finish_chunked_report(token)
            .expect("Could not finish chunked report");

        // Everything works again after the reporting is done
        assert_eq!(
            Ok(()),
            eko_foo.merge_snapshot_bytes(&other_transmission_buffer[..bar_snapshot_len])
        );
        assert_eq!(Ok(()), eko_foo.merge_snapshot(&bar_fixed_snapshot));

        assert!(eko_foo.produce_snapshot().is_ok());
        assert!(eko_foo
            .produce_snapshot_bytes(&mut other_transmission_buffer)
            .is_ok());

        assert!(eko_foo.report(&mut other_transmission_buffer).is_ok());
        assert!(eko_foo
            .report_with_extension(&mut other_transmission_buffer, ExtensionBytes(&[]))
            .is_ok());
    }

    #[test]
    fn chunked_no_report_in_progress_causes_error() {
        let probe_id = 1u32.try_into().expect("Invalid probe id");
        let mut report_transmission_buffer = [0u8; MAX_CHUNK_BYTES];
        let mut storage = [0u8; 4096];
        let mut eko = ModalityProbe::new_with_storage(&mut storage, probe_id)
            .expect("Could not initialize Modality probe");
        let made_up_token = ChunkedReportToken { group_id: 0 };
        assert_eq!(
            ChunkedReportError::NoChunkedReportInProgress,
            eko.write_next_report_chunk(&made_up_token, &mut report_transmission_buffer)
                .unwrap_err()
        );
        assert_eq!(
            ChunkedReportError::NoChunkedReportInProgress,
            eko.finish_chunked_report(made_up_token).unwrap_err()
        );
    }

    #[test]
    fn chunked_report_invalid_destination_buffer_size() {
        let probe_id = 1u32.try_into().expect("Invalid probe id");
        let mut report_transmission_buffer = [0u8; MAX_CHUNK_BYTES];
        let mut storage = [0u8; 4096];
        let mut eko = ModalityProbe::new_with_storage(&mut storage, probe_id)
            .expect("Could not initialize Modality probe");
        let token = eko
            .start_chunked_report()
            .expect("Could not start chunked report");
        assert_eq!(
            ChunkedReportError::ReportError(ReportError::InsufficientDestinationSize),
            eko.write_next_report_chunk(&token, &mut report_transmission_buffer[0..0])
                .unwrap_err()
        );
        // Starts to work when you give it a minimally-sized buffer
        let expected_size_bytes = WireChunkedReport::<&[u8]>::buffer_len(size_of::<LogicalClock>());
        assert_eq!(
            expected_size_bytes,
            eko.write_next_report_chunk(
                &token,
                &mut report_transmission_buffer[..expected_size_bytes]
            )
            .unwrap()
        );
        // No buffer size is required when there's no remaining report chunk to write
        assert_eq!(
            0,
            eko.write_next_report_chunk(&token, &mut report_transmission_buffer[0..0])
                .unwrap()
        );
        eko.finish_chunked_report(token)
            .expect("Could not finish chunked report");
    }

    #[test]
    fn chunked_report_attempt_multiple_starts_causes_error() {
        let probe_id = 1u32.try_into().expect("Invalid probe id");
        let mut storage = [0u8; 4096];
        let mut eko = ModalityProbe::new_with_storage(&mut storage, probe_id)
            .expect("Could not initialize Modality probe");
        let _token = eko
            .start_chunked_report()
            .expect("Could not start chunked report");

        assert_eq!(
            ChunkedReportError::ReportError(ReportError::ReportLockConflict),
            eko.start_chunked_report().unwrap_err()
        );
    }

    #[test]
    fn chunked_report_attempt_multiple_finishes_causes_error() {
        let probe_id = 1u32.try_into().expect("Invalid probe id");
        let mut report_transmission_buffer = [0u8; MAX_CHUNK_BYTES];
        let mut storage = [0u8; 4096];
        let mut eko = ModalityProbe::new_with_storage(&mut storage, probe_id)
            .expect("Could not initialize Modality probe");
        let token = eko
            .start_chunked_report()
            .expect("Could not start chunked report");
        let token_clone = ChunkedReportToken {
            group_id: token.group_id,
        };
        let unrelated_token = ChunkedReportToken {
            group_id: token.group_id + 20,
        };
        let _n_report_bytes = eko
            .write_next_report_chunk(&token, &mut report_transmission_buffer)
            .expect("Could not write chunk");
        let n_report_bytes = eko
            .write_next_report_chunk(&token, &mut report_transmission_buffer)
            .expect("Could not write chunk");
        assert_eq!(0, n_report_bytes);
        eko.finish_chunked_report(token)
            .expect("Could not finish chunked report");
        assert_eq!(
            ChunkedReportError::NoChunkedReportInProgress,
            eko.finish_chunked_report(token_clone).unwrap_err()
        );
        assert_eq!(
            ChunkedReportError::NoChunkedReportInProgress,
            eko.finish_chunked_report(unrelated_token).unwrap_err()
        );
    }

    prop_compose! {
        fn gen_lists_of_events(max_lists: usize, max_events: usize)(vec in proptest::collection::vec(gen_events(max_events), 0..max_lists)) -> Vec<Vec<LogEvent>> {
            vec
        }
    }

    proptest! {
        #[test]
        fn arbitrary_events_chunked_reporting_happy_path(
                raw_probe_id in id_tests::gen_raw_probe_id(),
                event_lists in gen_lists_of_events(10, 513)) {
            let probe_id = raw_probe_id.try_into().expect("Invalid probe id");
            let mut report_transmission_buffer: Vec<u8> = Vec::new();
            report_transmission_buffer.resize(MAX_CHUNK_BYTES, 0u8);
            let mut storage = [0u8; 4096];
            let mut eko = ModalityProbe::new_with_storage(
                &mut storage, probe_id).expect("Could not initialize Modality probe");
            let mut seen_group_ids = HashSet::new();

            for (produced_report_index, input_events) in event_lists.iter().enumerate() {
                // Each iteration of this loop is expected to represent a distinct report
                // consisting of logical clocks and a history of recorded events,
                // split over possibly multiple chunks.
                let mut has_checked_group_id = false;
                let mut gathered_log_items: Vec<CompactLogItem> = Vec::new();

                for e in input_events.iter() {
                    match e {
                        LogEvent::Event(event) => {
                            eko.record_event(*event);
                        }
                        LogEvent::EventWithPayload(event, payload) => {
                            eko.record_event_with_payload(*event, *payload);
                        }
                    }
                }

                let mut last_seen_chunk_index = None;
                let token = eko.start_chunked_report().expect("Could not start chunked report");
                loop {
                    let n_report_bytes = eko.write_next_report_chunk(&token, &mut report_transmission_buffer).expect("Could not write chunk");
                    if n_report_bytes == 0 {
                        break;
                    }
                    let buffer_slice: &[u8] = &report_transmission_buffer[..n_report_bytes];
                    let native_chunk = NativeChunk::from_wire_bytes(buffer_slice).unwrap();
                    assert_eq!(probe_id, native_chunk.header().probe_id);
                    let curr_group_id = native_chunk.header().chunk_group_id;
                    if !has_checked_group_id {
                        if !seen_group_ids.insert(curr_group_id) && seen_group_ids.len() < (core::u16::MAX - 1) as usize {
                            panic!("Duplicate group id detected before group id {} re-use was strictly necessary. Prev Ids {:?}", curr_group_id, seen_group_ids);
                        }
                        has_checked_group_id = true;
                    }
                    let curr_index = native_chunk.header().chunk_index;
                    if let Some(previous_chunk_index) = last_seen_chunk_index {
                        assert!(curr_index == previous_chunk_index + 1, "Not-monotonically-increasing chunk index");
                    }
                    last_seen_chunk_index = Some(curr_index);
                    if let NativeChunk::Log { header: _, contents} = native_chunk {
                        assert!(contents.log_slice().len() > 0, "Chunks where the payload bytes > 0 should contain at least one log item");
                        gathered_log_items.extend_from_slice(contents.log_slice());
                    } else {
                        panic!("Unexpected extension type chunk detected");
                    }
                }
                eko.finish_chunked_report(token).expect("Could not finish chunked report");

                // Assert that the collected log matches expectations
                let log_segments: Vec<crate::compact_log::LogSegment> = crate::compact_log::LogSegmentIterator::new(probe_id, &gathered_log_items).collect();
                assert_eq!(1, log_segments.len(), "expect a single log segment of clocks followed by many events. Log items looked like: {:?}", &gathered_log_items);
                let segment = &log_segments[0];
                let logical_clocks: Vec<crate::LogicalClock> = segment.iter_clocks().map(|r| r.unwrap()).collect();
                assert_eq!(1, logical_clocks.len());
                assert_eq!(probe_id, logical_clocks[0].id);
                assert_eq!(produced_report_index, logical_clocks[0].ticks as usize);
                let found_events: Vec<LogEvent> = segment.iter_events().map(|r| r.expect("Should be able to interpret event")).collect();
                let mut expected_events = Vec::new();
                if produced_report_index > 0 {
                    expected_events.push(LogEvent::Event(EventId::EVENT_PRODUCED_EXTERNAL_REPORT));
                }
                expected_events.extend(input_events);
                assert_eq!(found_events, expected_events);
            }
        }
    }
}
