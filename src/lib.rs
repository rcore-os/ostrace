//! `ostrace` is a `no_std` trace record core for Rust OS kernels.
//!
//! The crate does not allocate memory. The embedding OS provides library-level
//! storage, session-level CPU state, and one ring buffer per CPU. A trace session
//! appends compact binary records to per-CPU buffers and can later snapshot them
//! for export.

#![no_std]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::bare_urls)]

#[cfg(any(test, feature = "std"))]
extern crate std;

const RECORD_ALIGN: usize = 8;
const MIN_RING_BUFFER_SIZE: usize = RECORD_HEADER_SIZE * 2;
const EVENT_CONTEXT_SWITCH: u32 = 1;
const EVENT_TRACE_MARK_BEGIN: u32 = 2;
const EVENT_TRACE_MARK_END: u32 = 3;
const TRACE_IMAGE_HEADER_SIZE: usize = 96;
const TRACE_IMAGE_CPU_DESC_SIZE: usize = 80;
const TRACE_IMAGE_ENDIAN_LITTLE: u8 = 1;
const TRACE_IMAGE_FLAG_ACTIVE: u64 = 1;
const TRACE_IMAGE_CPU_FLAG_WRITER_ACTIVE: u64 = 1;
const TRACE_IMAGE_MAGIC_OFFSET: usize = 0;
const TRACE_IMAGE_CONTAINER_MAJOR_OFFSET: usize = 8;
const TRACE_IMAGE_CONTAINER_MINOR_OFFSET: usize = 10;
const TRACE_IMAGE_HEADER_SIZE_OFFSET: usize = 12;
const TRACE_IMAGE_ENDIAN_OFFSET: usize = 14;
const TRACE_IMAGE_RECORD_MAJOR_OFFSET: usize = 16;
const TRACE_IMAGE_RECORD_MINOR_OFFSET: usize = 18;
const TRACE_IMAGE_RECORD_HEADER_SIZE_OFFSET: usize = 20;
const TRACE_IMAGE_RECORD_ALIGN_OFFSET: usize = 22;
const TRACE_IMAGE_CPU_COUNT_OFFSET: usize = 24;
const TRACE_IMAGE_CPU_DESC_OFFSET_OFFSET: usize = 32;
const TRACE_IMAGE_CPU_DESC_SIZE_OFFSET: usize = 40;
const TRACE_IMAGE_RING_OFFSET_OFFSET: usize = 48;
const TRACE_IMAGE_IMAGE_SIZE_OFFSET: usize = 56;
const TRACE_IMAGE_FLAGS_OFFSET: usize = 64;
const TRACE_IMAGE_BUFFER_MODE_OFFSET: usize = 72;
const CPU_DESC_CPU_ID_OFFSET: usize = 0;
const CPU_DESC_BUFFER_OFFSET_OFFSET: usize = 8;
const CPU_DESC_BUFFER_SIZE_OFFSET: usize = 16;
const CPU_DESC_WRITE_POS_OFFSET: usize = 24;
const CPU_DESC_READ_POS_OFFSET: usize = 32;
const CPU_DESC_USED_OFFSET: usize = 40;
const CPU_DESC_NEXT_SEQ_OFFSET: usize = 48;
const CPU_DESC_DROPPED_OFFSET: usize = 56;
const CPU_DESC_REENTRANT_DROPPED_OFFSET: usize = 64;
const CPU_DESC_FLAGS_OFFSET: usize = 72;

/// Magic value used to identify an `ostrace` trace image in a RAM dump.
pub const TRACE_IMAGE_MAGIC: u64 = 0x0045_4341_5254_534f;

/// Major version of the trace image container layout.
pub const TRACE_IMAGE_CONTAINER_VERSION_MAJOR: u16 = 1;

/// Minor version of the trace image container layout.
pub const TRACE_IMAGE_CONTAINER_VERSION_MINOR: u16 = 0;

/// Major version of the binary record payload format.
pub const TRACE_RECORD_FORMAT_VERSION_MAJOR: u16 = 1;

/// Minor version of the binary record payload format.
pub const TRACE_RECORD_FORMAT_VERSION_MINOR: u16 = 0;

/// Identifies the binary record kind stored in a trace ring buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RecordKind {
    /// Point-in-time event.
    Instant = 1,
    /// Synchronous duration begin event.
    DurationBegin = 2,
    /// Synchronous duration end event.
    DurationEnd = 3,
    /// Numeric counter sample.
    Counter = 4,
    /// Metadata record used to describe tasks, names, or exporter context.
    Metadata = 5,
    /// CPU context switch event.
    ContextSwitch = 6,
    /// Padding record used to wrap from the end of a ring buffer to the start.
    Padding = 255,
}

/// Fixed binary header that prefixes every record in a ring buffer.
///
/// The header is encoded in little-endian byte order when written to a buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct RecordHeader {
    /// Total aligned record length in bytes, including this header and padding.
    pub len: u16,
    /// Semantic kind of the record payload.
    pub kind: RecordKind,
    /// Record flags reserved for future format extensions.
    pub flags: u8,
    /// Stable event identifier used by decoders and exporters.
    pub event_id: u32,
    /// Event timestamp in nanoseconds.
    pub timestamp: u64,
}

/// Size in bytes of [`RecordHeader`].
pub const RECORD_HEADER_SIZE: usize = core::mem::size_of::<RecordHeader>();

/// Controls what happens when a per-CPU ring buffer has insufficient free space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferMode {
    /// Drop oldest records until the new record fits.
    Overwrite,
    /// Reject the new record when the buffer is full.
    StopOnFull,
}

/// Errors returned while creating a recorder or starting a trace session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraceError {
    /// `cpu_count` was zero.
    InvalidCpuCount,
    /// Runtime `cpu_count` exceeds the session storage compile-time capacity.
    TooManyCpus,
    /// The number of supplied per-CPU buffers does not match `cpu_count`.
    CpuBufferCountMismatch,
    /// At least one per-CPU buffer is too small to hold records safely.
    BufferTooSmall,
    /// Another trace session is already active for this recorder storage.
    SessionAlreadyActive,
}

/// Result of an append attempt on the trace hot path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppendStatus {
    /// The record was appended to the selected per-CPU buffer.
    Written,
    /// The record was not written; the contained reason explains why.
    Dropped(DropReason),
}

/// Reason why a record append was dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DropReason {
    /// [`TracePlatform::cpu_id`] returned a CPU outside the configured range.
    InvalidCpu,
    /// The target CPU was already appending a record.
    Reentrant,
    /// The target buffer was full and the session uses [`BufferMode::StopOnFull`].
    BufferFull,
    /// The encoded record is larger than the temporary payload space or buffer.
    RecordTooLarge,
}

/// OS-provided platform hooks used by trace sessions.
pub trait TracePlatform {
    /// Returns the current timestamp in nanoseconds.
    ///
    /// # Returns
    ///
    /// Returns a timestamp in the platform's trace clock domain.
    ///
    /// # Panics
    ///
    /// Implementations should not panic.
    ///
    /// # Side Effects
    ///
    /// Implementations should avoid side effects because this method is called
    /// on the trace hot path.
    fn now_ns(&self) -> u64;

    /// Returns the current CPU identifier.
    ///
    /// # Returns
    ///
    /// Returns the zero-based CPU id used to select the per-CPU ring buffer.
    ///
    /// # Panics
    ///
    /// Implementations should not panic.
    ///
    /// # Side Effects
    ///
    /// Implementations should avoid side effects because this method is called
    /// on the trace hot path.
    fn cpu_id(&self) -> u32;
}

/// Errors returned while initializing or parsing a trace image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraceImageError {
    /// The supplied image buffer is too small for the requested layout.
    ImageTooSmall,
    /// `cpu_count` was zero.
    InvalidCpuCount,
    /// The requested per-CPU ring buffer size is too small.
    BufferTooSmall,
    /// The image magic value does not match [`TRACE_IMAGE_MAGIC`].
    BadMagic {
        /// Magic value found in the image.
        found: u64,
    },
    /// The image endian marker is not supported by this parser.
    UnsupportedEndian {
        /// Endian marker found in the image.
        found: u8,
    },
    /// The container version is not supported by this parser.
    UnsupportedContainerVersion {
        /// Major version found in the image.
        found_major: u16,
        /// Minor version found in the image.
        found_minor: u16,
    },
    /// The record format version is not supported by this parser.
    UnsupportedRecordVersion {
        /// Major version found in the image.
        found_major: u16,
        /// Minor version found in the image.
        found_minor: u16,
    },
    /// The image header size is smaller than the known v1 header prefix.
    InvalidHeaderSize,
    /// The CPU descriptor size is smaller than the known v1 descriptor prefix.
    InvalidCpuDescriptorSize,
    /// A descriptor points outside the trace image.
    BufferOutOfRange,
    /// A RAM dump address calculation does not fit inside the supplied RAM bytes.
    RamRangeOutOfBounds,
    /// The requested trace base is below the RAM base.
    TraceBaseBeforeRamBase,
}

/// Configuration used to initialize one self-describing trace image.
pub struct TraceImageConfig<'a, P> {
    /// Caller-owned RAM region that will contain the full trace image.
    pub bytes: &'a mut [u8],
    /// Number of CPUs that may write records.
    pub cpu_count: usize,
    /// Size in bytes of each per-CPU ring buffer inside `bytes`.
    pub per_cpu_buffer_size: usize,
    /// Ring buffer overflow policy.
    pub mode: BufferMode,
    /// Platform hooks used to obtain timestamps and CPU ids.
    pub platform: P,
}

/// Active trace image backed by one caller-provided RAM region.
///
/// The image owns no memory. Header fields, CPU descriptors, and per-CPU ring
/// buffers are all stored inside the supplied byte slice.
pub struct TraceImage<'a, P> {
    /// Full trace image bytes.
    bytes: &'a mut [u8],
    /// Runtime CPU count enabled for tracing.
    cpu_count: usize,
    /// Platform hooks used by append operations.
    platform: P,
    /// Ring buffer overflow policy.
    mode: BufferMode,
}

impl<'a, P: TracePlatform> TraceImage<'a, P> {
    /// Initializes a trace image inside one caller-provided RAM region.
    ///
    /// # Parameters
    ///
    /// - `config`: Image backing bytes, CPU count, per-CPU buffer size, buffer
    ///   mode, and platform hooks.
    ///
    /// # Returns
    ///
    /// Returns an active [`TraceImage`] on success.
    /// Returns [`TraceImageError::InvalidCpuCount`] if `cpu_count == 0`.
    /// Returns [`TraceImageError::BufferTooSmall`] if the per-CPU buffer size is
    /// too small for ring records.
    /// Returns [`TraceImageError::ImageTooSmall`] if `bytes` cannot hold the
    /// image header, CPU descriptors, and all per-CPU buffers.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// Clears the supplied RAM region, writes the image header and CPU
    /// descriptors, marks the image active, and reserves per-CPU ring buffers
    /// inside the same RAM region.
    pub fn init(config: TraceImageConfig<'a, P>) -> Result<Self, TraceImageError> {
        if config.cpu_count == 0 {
            return Err(TraceImageError::InvalidCpuCount);
        }
        if config.per_cpu_buffer_size < MIN_RING_BUFFER_SIZE {
            return Err(TraceImageError::BufferTooSmall);
        }
        let desc_offset = TRACE_IMAGE_HEADER_SIZE;
        let ring_offset = align_up(
            desc_offset + config.cpu_count * TRACE_IMAGE_CPU_DESC_SIZE,
            RECORD_ALIGN,
        );
        let image_size = ring_offset + config.cpu_count * config.per_cpu_buffer_size;
        if image_size > config.bytes.len() {
            return Err(TraceImageError::ImageTooSmall);
        }

        config.bytes[..image_size].fill(0);
        write_image_header(
            config.bytes,
            config.cpu_count,
            desc_offset,
            ring_offset,
            image_size,
            config.mode,
            true,
        );
        for cpu in 0..config.cpu_count {
            let buffer_offset = ring_offset + cpu * config.per_cpu_buffer_size;
            let desc = CpuImageDescriptor {
                cpu_id: cpu as u32,
                buffer_offset,
                buffer_size: config.per_cpu_buffer_size,
                state: CpuState::new(),
            };
            write_cpu_descriptor(config.bytes, desc_offset, cpu, &desc);
        }

        Ok(Self {
            bytes: config.bytes,
            cpu_count: config.cpu_count,
            platform: config.platform,
            mode: config.mode,
        })
    }

    /// Appends a CPU context switch record to this image.
    ///
    /// # Parameters
    ///
    /// - `prev`: Task leaving the CPU.
    /// - `prev_state`: Off-CPU state entered by `prev`.
    /// - `next`: Task selected to run on the CPU.
    ///
    /// # Returns
    ///
    /// Returns [`AppendStatus::Written`] if the record is appended.
    /// Returns [`AppendStatus::Dropped`] with [`DropReason::InvalidCpu`],
    /// [`DropReason::Reentrant`], [`DropReason::BufferFull`], or
    /// [`DropReason::RecordTooLarge`] when the append cannot be completed.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// Reads timestamp and CPU id from the platform, mutates the selected
    /// in-image ring buffer and descriptor, increments the per-CPU sequence
    /// number, and may increment drop counters.
    pub fn context_switch(
        &mut self,
        prev: TaskRef<'_>,
        prev_state: OffCpuState,
        next: TaskRef<'_>,
    ) -> AppendStatus {
        let cpu_id = self.platform.cpu_id();
        let timestamp = self.platform.now_ns();
        self.append_for_current_cpu(
            cpu_id,
            |cpu_id, seq, out| encode_context_switch(out, cpu_id, seq, prev, prev_state, next),
            RecordKind::ContextSwitch,
            EVENT_CONTEXT_SWITCH,
            timestamp,
        )
    }

    /// Appends a synchronous trace mark begin record to this image.
    ///
    /// # Parameters
    ///
    /// - `task`: Task associated with the trace mark.
    /// - `name`: Slice name. The name is copied into the binary record.
    ///
    /// # Returns
    ///
    /// Returns [`AppendStatus::Written`] if the record is appended.
    /// Returns [`AppendStatus::Dropped`] with the same drop reasons documented
    /// by [`TraceImage::context_switch`].
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// Reads timestamp and CPU id from the platform, mutates the selected
    /// in-image ring buffer and descriptor, increments the per-CPU sequence
    /// number, and may increment drop counters.
    pub fn trace_mark_begin(&mut self, task: TaskRef<'_>, name: &str) -> AppendStatus {
        let cpu_id = self.platform.cpu_id();
        let timestamp = self.platform.now_ns();
        self.append_for_current_cpu(
            cpu_id,
            |cpu_id, seq, out| encode_trace_mark_begin(out, cpu_id, seq, task, name),
            RecordKind::DurationBegin,
            EVENT_TRACE_MARK_BEGIN,
            timestamp,
        )
    }

    /// Appends a synchronous trace mark end record to this image.
    ///
    /// # Parameters
    ///
    /// - `task`: Task associated with the ending trace mark.
    ///
    /// # Returns
    ///
    /// Returns [`AppendStatus::Written`] if the record is appended.
    /// Returns [`AppendStatus::Dropped`] with the same drop reasons documented
    /// by [`TraceImage::context_switch`].
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// Reads timestamp and CPU id from the platform, mutates the selected
    /// in-image ring buffer and descriptor, increments the per-CPU sequence
    /// number, and may increment drop counters.
    pub fn trace_mark_end(&mut self, task: TaskRef<'_>) -> AppendStatus {
        let cpu_id = self.platform.cpu_id();
        let timestamp = self.platform.now_ns();
        self.append_for_current_cpu(
            cpu_id,
            |cpu_id, seq, out| encode_trace_mark_end(out, cpu_id, seq, task),
            RecordKind::DurationEnd,
            EVENT_TRACE_MARK_END,
            timestamp,
        )
    }

    /// Marks this image as no longer actively written.
    ///
    /// # Returns
    ///
    /// This function does not return a value.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// Clears the active flag in the image header. Existing records and
    /// descriptors remain in place for host-side parsing.
    pub fn finish(&mut self) {
        let flags = read_u64_at(self.bytes, TRACE_IMAGE_FLAGS_OFFSET) & !TRACE_IMAGE_FLAG_ACTIVE;
        write_u64_at(self.bytes, TRACE_IMAGE_FLAGS_OFFSET, flags);
    }

    /// Creates a read-only snapshot view of this image.
    ///
    /// # Returns
    ///
    /// Returns a [`TraceImageSnapshot`] borrowing this image's RAM bytes.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects. It does not copy records or stop the
    /// image.
    pub fn snapshot(&self) -> TraceImageSnapshot<'_> {
        TraceImageSnapshot {
            bytes: self.bytes,
            cpu_count: self.cpu_count,
            desc_offset: TRACE_IMAGE_HEADER_SIZE,
            desc_size: TRACE_IMAGE_CPU_DESC_SIZE,
        }
    }

    fn append_for_current_cpu<F>(
        &mut self,
        cpu_id: u32,
        encode_payload: F,
        kind: RecordKind,
        event_id: u32,
        timestamp: u64,
    ) -> AppendStatus
    where
        F: FnOnce(u32, u64, &mut PayloadWriter<'_>) -> Result<(), DropReason>,
    {
        let cpu_index = cpu_id as usize;
        if cpu_index >= self.cpu_count {
            return AppendStatus::Dropped(DropReason::InvalidCpu);
        }

        let desc_offset = read_u64_at(self.bytes, TRACE_IMAGE_CPU_DESC_OFFSET_OFFSET) as usize;
        let mut desc = match read_cpu_descriptor(self.bytes, desc_offset, cpu_index) {
            Some(desc) => desc,
            None => return AppendStatus::Dropped(DropReason::RecordTooLarge),
        };
        if desc.state.writer_active {
            desc.state.dropped = desc.state.dropped.saturating_add(1);
            desc.state.reentrant_dropped = desc.state.reentrant_dropped.saturating_add(1);
            write_cpu_descriptor(self.bytes, desc_offset, cpu_index, &desc);
            return AppendStatus::Dropped(DropReason::Reentrant);
        }

        desc.state.writer_active = true;
        write_cpu_descriptor(self.bytes, desc_offset, cpu_index, &desc);

        let seq = desc.state.per_cpu_seq;
        desc.state.per_cpu_seq = desc.state.per_cpu_seq.saturating_add(1);

        let mut payload = [0u8; 512];
        let mut writer = PayloadWriter::new(&mut payload);
        let result = encode_payload(cpu_id, seq, &mut writer).and_then(|()| {
            let start = desc.buffer_offset;
            let end = desc.buffer_offset + desc.buffer_size;
            append_encoded_record(
                &mut desc.state,
                &mut self.bytes[start..end],
                self.mode,
                kind,
                event_id,
                timestamp,
                writer.written(),
            )
        });

        desc.state.writer_active = false;
        let status = match result {
            Ok(()) => AppendStatus::Written,
            Err(reason) => {
                desc.state.dropped = desc.state.dropped.saturating_add(1);
                AppendStatus::Dropped(reason)
            }
        };
        write_cpu_descriptor(self.bytes, desc_offset, cpu_index, &desc);
        status
    }
}

impl<P> Drop for TraceImage<'_, P> {
    fn drop(&mut self) {
        let flags = read_u64_at(self.bytes, TRACE_IMAGE_FLAGS_OFFSET) & !TRACE_IMAGE_FLAG_ACTIVE;
        write_u64_at(self.bytes, TRACE_IMAGE_FLAGS_OFFSET, flags);
    }
}

/// Read-only snapshot of a self-describing trace image.
pub struct TraceImageSnapshot<'a> {
    /// Full trace image bytes.
    bytes: &'a [u8],
    /// Number of active CPU descriptors.
    cpu_count: usize,
    /// Offset of the CPU descriptor table.
    desc_offset: usize,
    /// Size of each CPU descriptor in bytes.
    desc_size: usize,
}

impl<'a> TraceImageSnapshot<'a> {
    /// Parses a trace image from a byte slice.
    ///
    /// # Parameters
    ///
    /// - `bytes`: Byte slice that starts at a trace image header.
    ///
    /// # Returns
    ///
    /// Returns a [`TraceImageSnapshot`] if the header and descriptors are
    /// supported. Returns [`TraceImageError`] when the image is not recognized or
    /// is not compatible with this parser.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, TraceImageError> {
        validate_image_header(bytes)?;
        let cpu_count = read_u32_at(bytes, TRACE_IMAGE_CPU_COUNT_OFFSET) as usize;
        let desc_offset = read_u64_at(bytes, TRACE_IMAGE_CPU_DESC_OFFSET_OFFSET) as usize;
        let desc_size = read_u16_at(bytes, TRACE_IMAGE_CPU_DESC_SIZE_OFFSET) as usize;
        let image_size = read_u64_at(bytes, TRACE_IMAGE_IMAGE_SIZE_OFFSET) as usize;
        if image_size > bytes.len() {
            return Err(TraceImageError::ImageTooSmall);
        }
        for cpu in 0..cpu_count {
            let desc = read_cpu_descriptor(bytes, desc_offset, cpu)
                .ok_or(TraceImageError::InvalidCpuDescriptorSize)?;
            let end = desc
                .buffer_offset
                .checked_add(desc.buffer_size)
                .ok_or(TraceImageError::BufferOutOfRange)?;
            if end > image_size {
                return Err(TraceImageError::BufferOutOfRange);
            }
        }
        Ok(Self {
            bytes: &bytes[..image_size],
            cpu_count,
            desc_offset,
            desc_size,
        })
    }

    /// Parses a trace image embedded in a RAM dump.
    ///
    /// # Parameters
    ///
    /// - `ram`: Full RAM dump bytes.
    /// - `ram_base`: Physical address represented by `ram[0]`.
    /// - `trace_base`: Physical address of the trace image.
    ///
    /// # Returns
    ///
    /// Returns a [`TraceImageSnapshot`] if the address range contains a supported
    /// image. Returns [`TraceImageError::TraceBaseBeforeRamBase`] if
    /// `trace_base < ram_base`, or [`TraceImageError::RamRangeOutOfBounds`] if
    /// the image header or declared image size is outside `ram`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn parse_from_ram(
        ram: &'a [u8],
        ram_base: u64,
        trace_base: u64,
    ) -> Result<Self, TraceImageError> {
        let trace_offset = trace_base
            .checked_sub(ram_base)
            .ok_or(TraceImageError::TraceBaseBeforeRamBase)? as usize;
        if trace_offset + TRACE_IMAGE_HEADER_SIZE > ram.len() {
            return Err(TraceImageError::RamRangeOutOfBounds);
        }
        let bytes = &ram[trace_offset..];
        validate_image_header(bytes)?;
        let image_size = read_u64_at(bytes, TRACE_IMAGE_IMAGE_SIZE_OFFSET) as usize;
        if image_size > bytes.len() {
            return Err(TraceImageError::RamRangeOutOfBounds);
        }
        Self::parse(&bytes[..image_size])
    }

    /// Returns the number of active CPU streams.
    ///
    /// # Returns
    ///
    /// Returns the CPU count declared by the image header.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn cpu_count(&self) -> usize {
        self.cpu_count
    }

    /// Returns a read-only stream for one CPU.
    ///
    /// # Parameters
    ///
    /// - `cpu`: Zero-based CPU index.
    ///
    /// # Returns
    ///
    /// Returns `Some(TraceImageCpuStream)` if `cpu` is within the image CPU
    /// count; otherwise returns `None`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn cpu_stream(&self, cpu: usize) -> Option<TraceImageCpuStream<'a>> {
        if cpu >= self.cpu_count {
            return None;
        }
        let desc = read_cpu_descriptor(self.bytes, self.desc_offset, cpu)?;
        let bytes = &self.bytes[desc.buffer_offset..desc.buffer_offset + desc.buffer_size];
        Some(TraceImageCpuStream {
            state: desc.state,
            bytes,
        })
    }

    /// Returns the size of CPU descriptors in this image.
    ///
    /// # Returns
    ///
    /// Returns the descriptor size declared by the image header.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn cpu_descriptor_size(&self) -> usize {
        self.desc_size
    }
}

/// Read-only stream for one CPU inside a trace image.
pub struct TraceImageCpuStream<'a> {
    /// Per-CPU state copied from the image descriptor.
    state: CpuState,
    /// Per-CPU ring buffer bytes.
    bytes: &'a [u8],
}

impl<'a> TraceImageCpuStream<'a> {
    /// Returns an iterator over readable non-padding records.
    ///
    /// # Returns
    ///
    /// Returns a [`RecordIter`] starting at this CPU's oldest readable record.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn records(&self) -> RecordIter<'a> {
        RecordIter {
            bytes: self.bytes,
            pos: self.state.read_pos,
            remaining: self.state.used,
        }
    }
}

/// Describes a task referenced by a trace event.
///
/// This type deliberately uses neutral task terminology instead of ftrace field
/// names. Exporters map it to target formats such as bytrace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskRef<'a> {
    /// Display name of the task or thread.
    pub comm: &'a str,
    /// OS-visible thread id.
    pub tid: u32,
    /// OS-visible thread group or process id.
    pub tgid: u32,
    /// Scheduler priority associated with the task at event time.
    pub prio: i32,
}

/// State entered by the previous task after it leaves a CPU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OffCpuState {
    /// Runnable or running state.
    Running,
    /// Interruptible sleep.
    Sleeping,
    /// Uninterruptible sleep, typically disk wait.
    DiskSleep,
    /// Stopped state.
    Stopped,
    /// Tracing stop state.
    TracingStop,
    /// Zombie state.
    Zombie,
    /// Dead state.
    Dead,
    /// Wake-kill state.
    WakeKill,
    /// Waking state.
    Waking,
    /// Parked state.
    Parked,
    /// Idle task state.
    Idle,
    /// OS-specific state not modeled by the built-in variants.
    Unknown(u32),
}

impl OffCpuState {
    fn encode(self) -> u32 {
        match self {
            Self::Running => 0,
            Self::Sleeping => 1,
            Self::DiskSleep => 2,
            Self::Stopped => 3,
            Self::TracingStop => 4,
            Self::Zombie => 5,
            Self::Dead => 6,
            Self::WakeKill => 7,
            Self::Waking => 8,
            Self::Parked => 9,
            Self::Idle => 10,
            Self::Unknown(value) => value,
        }
    }

    fn decode(value: u32) -> Self {
        match value {
            0 => Self::Running,
            1 => Self::Sleeping,
            2 => Self::DiskSleep,
            3 => Self::Stopped,
            4 => Self::TracingStop,
            5 => Self::Zombie,
            6 => Self::Dead,
            7 => Self::WakeKill,
            8 => Self::Waking,
            9 => Self::Parked,
            10 => Self::Idle,
            value => Self::Unknown(value),
        }
    }

    #[cfg(feature = "std")]
    fn bytrace(self) -> &'static str {
        match self {
            Self::Running => "R",
            Self::Sleeping => "S",
            Self::DiskSleep => "D",
            Self::Stopped => "T",
            Self::TracingStop => "t",
            Self::Zombie => "Z",
            Self::Dead => "X",
            Self::WakeKill => "K",
            Self::Waking => "W",
            Self::Parked => "P",
            Self::Idle => "I",
            Self::Unknown(_) => "R",
        }
    }
}

/// Library-level trace state shared across trace sessions.
///
/// This state is caller-owned and borrowed by [`TraceRecorder`]. It does not
/// contain per-CPU buffers or session-local cursors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GlobalState {
    /// Whether a trace session is currently active.
    active_session: bool,
}

impl GlobalState {
    /// Creates an inactive global state value.
    ///
    /// # Returns
    ///
    /// Returns a state value with no active session.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub const fn new() -> Self {
        Self {
            active_session: false,
        }
    }
}

impl Default for GlobalState {
    fn default() -> Self {
        Self::new()
    }
}

/// Caller-owned storage for the trace subsystem.
///
/// A single [`TraceStorage`] can be reused across sessions, but only one session
/// may be active at a time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TraceStorage {
    /// Library-global state for the recorder.
    global: GlobalState,
}

impl TraceStorage {
    /// Creates trace subsystem storage with no active session.
    ///
    /// # Returns
    ///
    /// Returns initialized storage ready to pass to [`TraceRecorder::new`].
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub const fn new() -> Self {
        Self {
            global: GlobalState::new(),
        }
    }

    /// Reports whether this storage currently has an active session.
    ///
    /// # Returns
    ///
    /// Returns `true` while a [`TraceSession`] is alive and `false` otherwise.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn is_session_active(&self) -> bool {
        self.global.active_session
    }
}

impl Default for TraceStorage {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-CPU session-local ring buffer state.
///
/// `CpuState` tracks cursor positions, sequence numbers, and drop counters for
/// one CPU in one trace session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuState {
    /// Next byte offset to write in this CPU's ring buffer.
    write_pos: usize,
    /// Oldest readable byte offset in this CPU's ring buffer.
    read_pos: usize,
    /// Number of bytes currently occupied by readable records.
    used: usize,
    /// Monotonic per-CPU sequence number assigned to records.
    per_cpu_seq: u64,
    /// Number of records dropped for this CPU.
    dropped: u64,
    /// Number of records dropped because of same-CPU reentrant appends.
    reentrant_dropped: u64,
    /// Whether this CPU is currently appending a record.
    writer_active: bool,
}

impl CpuState {
    /// Creates empty per-CPU session state.
    ///
    /// # Returns
    ///
    /// Returns state with empty cursors and zeroed counters.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub const fn new() -> Self {
        Self {
            write_pos: 0,
            read_pos: 0,
            used: 0,
            per_cpu_seq: 0,
            dropped: 0,
            reentrant_dropped: 0,
            writer_active: false,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    /// Returns the total number of dropped records for this CPU.
    ///
    /// # Returns
    ///
    /// Returns the drop counter accumulated during the active session.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Returns the number of records dropped because of same-CPU reentrancy.
    ///
    /// # Returns
    ///
    /// Returns the reentrant drop counter accumulated during the active session.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn reentrant_dropped(&self) -> u64 {
        self.reentrant_dropped
    }

    #[cfg(test)]
    fn set_writer_active_for_test(&mut self, active: bool) {
        self.writer_active = active;
    }
}

impl Default for CpuState {
    fn default() -> Self {
        Self::new()
    }
}

/// Session-local storage for all per-CPU states.
///
/// The storage is caller-owned and borrowed for the lifetime of a
/// [`TraceSession`]. It can be reused after the session ends.
pub struct TraceSessionStorage<const MAX_CPUS: usize> {
    /// Per-CPU state array. Only `cpu_count` entries are active in a session.
    cpus: [CpuState; MAX_CPUS],
}

impl<const MAX_CPUS: usize> TraceSessionStorage<MAX_CPUS> {
    /// Creates session-local per-CPU state storage.
    ///
    /// # Returns
    ///
    /// Returns storage containing `MAX_CPUS` empty [`CpuState`] values.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub const fn new() -> Self {
        Self {
            cpus: [CpuState::new(); MAX_CPUS],
        }
    }

    /// Returns the state for one CPU.
    ///
    /// # Parameters
    ///
    /// - `cpu`: Zero-based CPU index.
    ///
    /// # Returns
    ///
    /// Returns `Some(&CpuState)` if `cpu < MAX_CPUS`; otherwise returns `None`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn cpu_state(&self, cpu: usize) -> Option<&CpuState> {
        self.cpus.get(cpu)
    }
}

impl<const MAX_CPUS: usize> Default for TraceSessionStorage<MAX_CPUS> {
    fn default() -> Self {
        Self::new()
    }
}

/// Caller-owned backing memory for one CPU's ring buffer in one session.
pub struct CpuBuffer<'buf> {
    /// Mutable byte slice used as this CPU's ring buffer.
    ///
    /// The slice is zeroed when a session starts and is returned to the caller
    /// when the session ends.
    pub bytes: &'buf mut [u8],
}

/// Configuration used to create a [`TraceRecorder`].
pub struct TraceConfig<'a, P> {
    /// Library-level storage borrowed by the recorder.
    pub storage: &'a mut TraceStorage,
    /// Number of CPUs that may write records.
    pub cpu_count: usize,
    /// Platform hooks used to obtain timestamps and CPU ids.
    pub platform: P,
}

/// Configuration used to start one trace session.
pub struct SessionConfig<'a, const MAX_CPUS: usize> {
    /// Session-local per-CPU state storage.
    pub state: &'a mut TraceSessionStorage<MAX_CPUS>,
    /// Per-CPU ring buffer backing memory.
    pub cpu_buffers: &'a mut [CpuBuffer<'a>],
    /// Ring buffer overflow policy.
    pub mode: BufferMode,
}

/// Trace subsystem handle that owns platform hooks and borrows global storage.
pub struct TraceRecorder<'a, P> {
    /// Borrowed library-level storage.
    storage: &'a mut TraceStorage,
    /// Runtime CPU count enabled for tracing.
    cpu_count: usize,
    /// Platform hooks used by active sessions.
    platform: P,
}

impl<'a, P: TracePlatform> TraceRecorder<'a, P> {
    /// Creates a trace recorder over caller-owned global storage.
    ///
    /// # Parameters
    ///
    /// - `config`: Global storage, runtime CPU count, and platform hooks.
    ///
    /// # Returns
    ///
    /// Returns a [`TraceRecorder`] on success.
    /// Returns [`TraceError::InvalidCpuCount`] when `config.cpu_count == 0`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function borrows `config.storage` for the recorder lifetime. It does
    /// not modify session state or buffers.
    pub fn new(config: TraceConfig<'a, P>) -> Result<Self, TraceError> {
        if config.cpu_count == 0 {
            return Err(TraceError::InvalidCpuCount);
        }
        Ok(Self {
            storage: config.storage,
            cpu_count: config.cpu_count,
            platform: config.platform,
        })
    }

    /// Starts a trace session over caller-provided per-CPU buffers.
    ///
    /// # Parameters
    ///
    /// - `config`: Session-local CPU state, per-CPU buffers, and buffer mode.
    ///
    /// # Returns
    ///
    /// Returns an active [`TraceSession`] on success.
    /// Returns [`TraceError::SessionAlreadyActive`] if another session is active.
    /// Returns [`TraceError::TooManyCpus`] if the recorder CPU count exceeds
    /// `MAX_CPUS`.
    /// Returns [`TraceError::CpuBufferCountMismatch`] if the number of supplied
    /// buffers differs from the recorder CPU count.
    /// Returns [`TraceError::BufferTooSmall`] if any buffer is smaller than the
    /// minimum ring buffer size.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// Sets the global active-session flag, resets active per-CPU session state,
    /// and zeroes all supplied per-CPU buffers. The active-session flag is
    /// cleared when the returned [`TraceSession`] is dropped.
    pub fn start_session<'session, const MAX_CPUS: usize>(
        &'session mut self,
        config: SessionConfig<'session, MAX_CPUS>,
    ) -> Result<TraceSession<'session, P, MAX_CPUS>, TraceError> {
        if self.storage.global.active_session {
            return Err(TraceError::SessionAlreadyActive);
        }
        if self.cpu_count > MAX_CPUS {
            return Err(TraceError::TooManyCpus);
        }
        if config.cpu_buffers.len() != self.cpu_count {
            return Err(TraceError::CpuBufferCountMismatch);
        }
        for buffer in config.cpu_buffers.iter() {
            if buffer.bytes.len() < MIN_RING_BUFFER_SIZE {
                return Err(TraceError::BufferTooSmall);
            }
        }

        for cpu in config.state.cpus[..self.cpu_count].iter_mut() {
            cpu.reset();
        }
        for buffer in config.cpu_buffers.iter_mut() {
            buffer.bytes.fill(0);
        }
        self.storage.global.active_session = true;

        Ok(TraceSession {
            storage: self.storage,
            cpu_count: self.cpu_count,
            platform: &self.platform,
            state: config.state,
            cpu_buffers: config.cpu_buffers,
            mode: config.mode,
        })
    }
}

/// Active trace session that appends records into per-CPU ring buffers.
///
/// Dropping a session ends it and clears the recorder's active-session flag.
pub struct TraceSession<'a, P, const MAX_CPUS: usize> {
    /// Borrowed library-level storage used to clear the active flag on drop.
    storage: &'a mut TraceStorage,
    /// Runtime CPU count enabled for this session.
    cpu_count: usize,
    /// Platform hooks shared with the recorder.
    platform: &'a P,
    /// Session-local per-CPU cursor and counter state.
    state: &'a mut TraceSessionStorage<MAX_CPUS>,
    /// Session-local per-CPU ring buffer backing memory.
    cpu_buffers: &'a mut [CpuBuffer<'a>],
    /// Ring buffer overflow policy.
    mode: BufferMode,
}

impl<P, const MAX_CPUS: usize> Drop for TraceSession<'_, P, MAX_CPUS> {
    fn drop(&mut self) {
        self.storage.global.active_session = false;
    }
}

impl<'a, P: TracePlatform, const MAX_CPUS: usize> TraceSession<'a, P, MAX_CPUS> {
    /// Appends a CPU context switch record.
    ///
    /// # Parameters
    ///
    /// - `prev`: Task leaving the CPU.
    /// - `prev_state`: Off-CPU state entered by `prev`.
    /// - `next`: Task selected to run on the CPU.
    ///
    /// # Returns
    ///
    /// Returns [`AppendStatus::Written`] if the record is appended.
    /// Returns [`AppendStatus::Dropped`] with [`DropReason::InvalidCpu`] if the
    /// platform CPU id is outside the configured CPU range.
    /// Returns [`AppendStatus::Dropped`] with [`DropReason::Reentrant`] if this
    /// CPU is already appending a record.
    /// Returns [`AppendStatus::Dropped`] with [`DropReason::BufferFull`] when
    /// the buffer is full in [`BufferMode::StopOnFull`].
    /// Returns [`AppendStatus::Dropped`] with [`DropReason::RecordTooLarge`] if
    /// the encoded record does not fit.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// Reads timestamp and CPU id from the platform, mutates the selected
    /// per-CPU ring buffer and CPU state, increments the per-CPU sequence number,
    /// and may increment drop counters.
    pub fn context_switch(
        &mut self,
        prev: TaskRef<'_>,
        prev_state: OffCpuState,
        next: TaskRef<'_>,
    ) -> AppendStatus {
        let cpu_id = self.platform.cpu_id();
        let timestamp = self.platform.now_ns();
        self.append_for_current_cpu(
            cpu_id,
            |cpu_id, seq, out| encode_context_switch(out, cpu_id, seq, prev, prev_state, next),
            RecordKind::ContextSwitch,
            EVENT_CONTEXT_SWITCH,
            timestamp,
        )
    }

    /// Appends a synchronous trace mark begin record.
    ///
    /// # Parameters
    ///
    /// - `task`: Task associated with the trace mark.
    /// - `name`: Slice name. The name is copied into the binary record.
    ///
    /// # Returns
    ///
    /// Returns [`AppendStatus::Written`] if the record is appended.
    /// Returns [`AppendStatus::Dropped`] with the same drop reasons documented
    /// by [`TraceSession::context_switch`].
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// Reads timestamp and CPU id from the platform, mutates the selected
    /// per-CPU ring buffer and CPU state, increments the per-CPU sequence number,
    /// and may increment drop counters.
    pub fn trace_mark_begin(&mut self, task: TaskRef<'_>, name: &str) -> AppendStatus {
        let cpu_id = self.platform.cpu_id();
        let timestamp = self.platform.now_ns();
        self.append_for_current_cpu(
            cpu_id,
            |cpu_id, seq, out| encode_trace_mark_begin(out, cpu_id, seq, task, name),
            RecordKind::DurationBegin,
            EVENT_TRACE_MARK_BEGIN,
            timestamp,
        )
    }

    /// Appends a synchronous trace mark end record.
    ///
    /// # Parameters
    ///
    /// - `task`: Task associated with the ending trace mark.
    ///
    /// # Returns
    ///
    /// Returns [`AppendStatus::Written`] if the record is appended.
    /// Returns [`AppendStatus::Dropped`] with the same drop reasons documented
    /// by [`TraceSession::context_switch`].
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// Reads timestamp and CPU id from the platform, mutates the selected
    /// per-CPU ring buffer and CPU state, increments the per-CPU sequence number,
    /// and may increment drop counters.
    pub fn trace_mark_end(&mut self, task: TaskRef<'_>) -> AppendStatus {
        let cpu_id = self.platform.cpu_id();
        let timestamp = self.platform.now_ns();
        self.append_for_current_cpu(
            cpu_id,
            |cpu_id, seq, out| encode_trace_mark_end(out, cpu_id, seq, task),
            RecordKind::DurationEnd,
            EVENT_TRACE_MARK_END,
            timestamp,
        )
    }

    /// Creates a read-only snapshot view of the active session.
    ///
    /// # Returns
    ///
    /// Returns a [`TraceSnapshot`] borrowing this session's per-CPU state and
    /// buffers.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects. It does not stop the session or copy
    /// records.
    pub fn snapshot(&self) -> TraceSnapshot<'_> {
        TraceSnapshot {
            cpu_count: self.cpu_count,
            cpus: &self.state.cpus[..self.cpu_count],
            buffers: self.cpu_buffers,
        }
    }

    fn append_for_current_cpu<F>(
        &mut self,
        cpu_id: u32,
        encode_payload: F,
        kind: RecordKind,
        event_id: u32,
        timestamp: u64,
    ) -> AppendStatus
    where
        F: FnOnce(u32, u64, &mut PayloadWriter<'_>) -> Result<(), DropReason>,
    {
        let cpu_index = cpu_id as usize;
        if cpu_index >= self.cpu_count {
            return AppendStatus::Dropped(DropReason::InvalidCpu);
        }

        let state = &mut self.state.cpus[cpu_index];
        if state.writer_active {
            state.dropped = state.dropped.saturating_add(1);
            state.reentrant_dropped = state.reentrant_dropped.saturating_add(1);
            return AppendStatus::Dropped(DropReason::Reentrant);
        }

        state.writer_active = true;
        let seq = state.per_cpu_seq;
        state.per_cpu_seq = state.per_cpu_seq.saturating_add(1);

        let mut payload = [0u8; 512];
        let mut writer = PayloadWriter::new(&mut payload);
        let result = encode_payload(cpu_id, seq, &mut writer).and_then(|()| {
            append_encoded_record(
                state,
                self.cpu_buffers[cpu_index].bytes,
                self.mode,
                kind,
                event_id,
                timestamp,
                writer.written(),
            )
        });

        state.writer_active = false;

        match result {
            Ok(()) => AppendStatus::Written,
            Err(reason) => {
                state.dropped = state.dropped.saturating_add(1);
                AppendStatus::Dropped(reason)
            }
        }
    }
}

/// Read-only view of all per-CPU streams in a trace session.
pub struct TraceSnapshot<'a> {
    /// Number of active CPU streams in this snapshot.
    cpu_count: usize,
    /// Per-CPU state used to locate readable records.
    cpus: &'a [CpuState],
    /// Per-CPU backing buffers containing binary records.
    buffers: &'a [CpuBuffer<'a>],
}

impl<'a> TraceSnapshot<'a> {
    /// Returns the number of active CPU streams.
    ///
    /// # Returns
    ///
    /// Returns the runtime CPU count configured for the session.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn cpu_count(&self) -> usize {
        self.cpu_count
    }

    /// Returns a read-only stream for one CPU.
    ///
    /// # Parameters
    ///
    /// - `cpu`: Zero-based CPU index.
    ///
    /// # Returns
    ///
    /// Returns `Some(CpuStream)` if `cpu` is within the snapshot CPU count;
    /// otherwise returns `None`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn cpu_stream(&self, cpu: usize) -> Option<CpuStream<'a>> {
        if cpu >= self.cpu_count {
            return None;
        }
        Some(CpuStream {
            state: &self.cpus[cpu],
            bytes: self.buffers[cpu].bytes,
        })
    }
}

/// Read-only view of one CPU's records.
pub struct CpuStream<'a> {
    /// Per-CPU state used to create an iterator.
    state: &'a CpuState,
    /// Per-CPU backing buffer.
    bytes: &'a [u8],
}

impl<'a> CpuStream<'a> {
    /// Returns an iterator over readable non-padding records.
    ///
    /// # Returns
    ///
    /// Returns a [`RecordIter`] starting at this CPU's oldest readable record.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn records(&self) -> RecordIter<'a> {
        RecordIter {
            bytes: self.bytes,
            pos: self.state.read_pos,
            remaining: self.state.used,
        }
    }
}

/// Iterator over raw binary records in one CPU stream.
pub struct RecordIter<'a> {
    /// Backing buffer being scanned.
    bytes: &'a [u8],
    /// Current byte offset in the backing buffer.
    pos: usize,
    /// Remaining readable bytes.
    remaining: usize,
}

impl<'a> Iterator for RecordIter<'a> {
    type Item = RawRecord<'a>;

    /// Advances to the next non-padding record.
    ///
    /// # Returns
    ///
    /// Returns `Some(RawRecord)` while a well-formed record is available.
    /// Returns `None` when the stream is exhausted or malformed.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// Advances this iterator's internal cursor. It does not mutate the source
    /// trace buffer.
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.remaining < RECORD_HEADER_SIZE || self.bytes.is_empty() {
                return None;
            }
            let cap = self.bytes.len();
            if cap - self.pos < RECORD_HEADER_SIZE {
                self.remaining = self.remaining.saturating_sub(cap - self.pos);
                self.pos = 0;
                continue;
            }

            let header = decode_header(&self.bytes[self.pos..self.pos + RECORD_HEADER_SIZE])?;
            let len = header.len as usize;
            if len == 0 || len > self.remaining || len > cap - self.pos {
                return None;
            }

            let payload_start = self.pos + RECORD_HEADER_SIZE;
            let payload_end = self.pos + len;
            self.pos = (self.pos + len) % cap;
            self.remaining -= len;

            if header.kind == RecordKind::Padding {
                continue;
            }

            return Some(RawRecord {
                header,
                payload: &self.bytes[payload_start..payload_end],
            });
        }
    }
}

/// Raw record consisting of a decoded header and borrowed payload bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawRecord<'a> {
    /// Decoded record header.
    pub header: RecordHeader,
    /// Payload bytes, including any record-level padding after the logical
    /// payload.
    pub payload: &'a [u8],
}

/// Structured representation of records currently understood by `ostrace`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodedRecord<'a> {
    /// CPU context switch record.
    ContextSwitch {
        /// Event timestamp in nanoseconds.
        timestamp: u64,
        /// CPU id on which the record was written.
        cpu_id: u32,
        /// Per-CPU sequence number.
        seq: u64,
        /// Task leaving the CPU.
        prev: OwnedTaskRef<'a>,
        /// Off-CPU state entered by `prev`.
        prev_state: OffCpuState,
        /// Task selected to run on the CPU.
        next: OwnedTaskRef<'a>,
    },
    /// Synchronous trace mark begin record.
    TraceMarkBegin {
        /// Event timestamp in nanoseconds.
        timestamp: u64,
        /// CPU id on which the record was written.
        cpu_id: u32,
        /// Per-CPU sequence number.
        seq: u64,
        /// Task associated with the trace mark.
        task: OwnedTaskRef<'a>,
        /// Trace mark name borrowed from the record payload.
        name: &'a str,
    },
    /// Synchronous trace mark end record.
    TraceMarkEnd {
        /// Event timestamp in nanoseconds.
        timestamp: u64,
        /// CPU id on which the record was written.
        cpu_id: u32,
        /// Per-CPU sequence number.
        seq: u64,
        /// Task associated with the trace mark.
        task: OwnedTaskRef<'a>,
    },
}

/// Task reference decoded from a record payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnedTaskRef<'a> {
    /// Display name of the task or thread.
    pub comm: &'a str,
    /// OS-visible thread id.
    pub tid: u32,
    /// OS-visible thread group or process id.
    pub tgid: u32,
    /// Scheduler priority associated with the task at event time.
    pub prio: i32,
}

impl<'a> RawRecord<'a> {
    /// Decodes a raw record payload into a structured record.
    ///
    /// # Returns
    ///
    /// Returns `Some(DecodedRecord)` when the event id and payload are
    /// recognized and valid. Returns `None` for unsupported event ids,
    /// malformed payloads, or invalid UTF-8 strings.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    ///
    /// # Side Effects
    ///
    /// This function has no side effects.
    pub fn decode(self) -> Option<DecodedRecord<'a>> {
        let mut reader = PayloadReader::new(self.payload);
        let cpu_id = reader.u32()?;
        let seq = reader.u64()?;
        match self.header.event_id {
            EVENT_CONTEXT_SWITCH => {
                let prev = reader.task()?;
                let prev_state = OffCpuState::decode(reader.u32()?);
                let next = reader.task()?;
                Some(DecodedRecord::ContextSwitch {
                    timestamp: self.header.timestamp,
                    cpu_id,
                    seq,
                    prev,
                    prev_state,
                    next,
                })
            }
            EVENT_TRACE_MARK_BEGIN => {
                let task = reader.task()?;
                let name = reader.str()?;
                Some(DecodedRecord::TraceMarkBegin {
                    timestamp: self.header.timestamp,
                    cpu_id,
                    seq,
                    task,
                    name,
                })
            }
            EVENT_TRACE_MARK_END => {
                let task = reader.task()?;
                Some(DecodedRecord::TraceMarkEnd {
                    timestamp: self.header.timestamp,
                    cpu_id,
                    seq,
                    task,
                })
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CpuImageDescriptor {
    cpu_id: u32,
    buffer_offset: usize,
    buffer_size: usize,
    state: CpuState,
}

fn write_image_header(
    bytes: &mut [u8],
    cpu_count: usize,
    desc_offset: usize,
    ring_offset: usize,
    image_size: usize,
    mode: BufferMode,
    active: bool,
) {
    write_u64_at(bytes, TRACE_IMAGE_MAGIC_OFFSET, TRACE_IMAGE_MAGIC);
    write_u16_at(
        bytes,
        TRACE_IMAGE_CONTAINER_MAJOR_OFFSET,
        TRACE_IMAGE_CONTAINER_VERSION_MAJOR,
    );
    write_u16_at(
        bytes,
        TRACE_IMAGE_CONTAINER_MINOR_OFFSET,
        TRACE_IMAGE_CONTAINER_VERSION_MINOR,
    );
    write_u16_at(
        bytes,
        TRACE_IMAGE_HEADER_SIZE_OFFSET,
        TRACE_IMAGE_HEADER_SIZE as u16,
    );
    bytes[TRACE_IMAGE_ENDIAN_OFFSET] = TRACE_IMAGE_ENDIAN_LITTLE;
    write_u16_at(
        bytes,
        TRACE_IMAGE_RECORD_MAJOR_OFFSET,
        TRACE_RECORD_FORMAT_VERSION_MAJOR,
    );
    write_u16_at(
        bytes,
        TRACE_IMAGE_RECORD_MINOR_OFFSET,
        TRACE_RECORD_FORMAT_VERSION_MINOR,
    );
    write_u16_at(
        bytes,
        TRACE_IMAGE_RECORD_HEADER_SIZE_OFFSET,
        RECORD_HEADER_SIZE as u16,
    );
    write_u16_at(bytes, TRACE_IMAGE_RECORD_ALIGN_OFFSET, RECORD_ALIGN as u16);
    write_u32_at(bytes, TRACE_IMAGE_CPU_COUNT_OFFSET, cpu_count as u32);
    write_u64_at(
        bytes,
        TRACE_IMAGE_CPU_DESC_OFFSET_OFFSET,
        desc_offset as u64,
    );
    write_u16_at(
        bytes,
        TRACE_IMAGE_CPU_DESC_SIZE_OFFSET,
        TRACE_IMAGE_CPU_DESC_SIZE as u16,
    );
    write_u64_at(bytes, TRACE_IMAGE_RING_OFFSET_OFFSET, ring_offset as u64);
    write_u64_at(bytes, TRACE_IMAGE_IMAGE_SIZE_OFFSET, image_size as u64);
    write_u64_at(
        bytes,
        TRACE_IMAGE_FLAGS_OFFSET,
        if active { TRACE_IMAGE_FLAG_ACTIVE } else { 0 },
    );
    write_u32_at(
        bytes,
        TRACE_IMAGE_BUFFER_MODE_OFFSET,
        match mode {
            BufferMode::Overwrite => 0,
            BufferMode::StopOnFull => 1,
        },
    );
}

fn validate_image_header(bytes: &[u8]) -> Result<(), TraceImageError> {
    if bytes.len() < TRACE_IMAGE_HEADER_SIZE {
        return Err(TraceImageError::ImageTooSmall);
    }
    let magic = read_u64_at(bytes, TRACE_IMAGE_MAGIC_OFFSET);
    if magic != TRACE_IMAGE_MAGIC {
        return Err(TraceImageError::BadMagic { found: magic });
    }
    let endian = bytes[TRACE_IMAGE_ENDIAN_OFFSET];
    if endian != TRACE_IMAGE_ENDIAN_LITTLE {
        return Err(TraceImageError::UnsupportedEndian { found: endian });
    }
    let container_major = read_u16_at(bytes, TRACE_IMAGE_CONTAINER_MAJOR_OFFSET);
    let container_minor = read_u16_at(bytes, TRACE_IMAGE_CONTAINER_MINOR_OFFSET);
    if container_major != TRACE_IMAGE_CONTAINER_VERSION_MAJOR {
        return Err(TraceImageError::UnsupportedContainerVersion {
            found_major: container_major,
            found_minor: container_minor,
        });
    }
    let header_size = read_u16_at(bytes, TRACE_IMAGE_HEADER_SIZE_OFFSET) as usize;
    if header_size < TRACE_IMAGE_HEADER_SIZE {
        return Err(TraceImageError::InvalidHeaderSize);
    }
    let record_major = read_u16_at(bytes, TRACE_IMAGE_RECORD_MAJOR_OFFSET);
    let record_minor = read_u16_at(bytes, TRACE_IMAGE_RECORD_MINOR_OFFSET);
    if record_major != TRACE_RECORD_FORMAT_VERSION_MAJOR {
        return Err(TraceImageError::UnsupportedRecordVersion {
            found_major: record_major,
            found_minor: record_minor,
        });
    }
    if read_u16_at(bytes, TRACE_IMAGE_RECORD_HEADER_SIZE_OFFSET) as usize != RECORD_HEADER_SIZE {
        return Err(TraceImageError::UnsupportedRecordVersion {
            found_major: record_major,
            found_minor: record_minor,
        });
    }
    if read_u16_at(bytes, TRACE_IMAGE_RECORD_ALIGN_OFFSET) as usize != RECORD_ALIGN {
        return Err(TraceImageError::UnsupportedRecordVersion {
            found_major: record_major,
            found_minor: record_minor,
        });
    }
    let desc_size = read_u16_at(bytes, TRACE_IMAGE_CPU_DESC_SIZE_OFFSET) as usize;
    if desc_size < TRACE_IMAGE_CPU_DESC_SIZE {
        return Err(TraceImageError::InvalidCpuDescriptorSize);
    }
    Ok(())
}

fn write_cpu_descriptor(
    bytes: &mut [u8],
    desc_offset: usize,
    cpu: usize,
    desc: &CpuImageDescriptor,
) {
    let offset = desc_offset + cpu * TRACE_IMAGE_CPU_DESC_SIZE;
    write_u32_at(bytes, offset + CPU_DESC_CPU_ID_OFFSET, desc.cpu_id);
    write_u64_at(
        bytes,
        offset + CPU_DESC_BUFFER_OFFSET_OFFSET,
        desc.buffer_offset as u64,
    );
    write_u64_at(
        bytes,
        offset + CPU_DESC_BUFFER_SIZE_OFFSET,
        desc.buffer_size as u64,
    );
    write_u64_at(
        bytes,
        offset + CPU_DESC_WRITE_POS_OFFSET,
        desc.state.write_pos as u64,
    );
    write_u64_at(
        bytes,
        offset + CPU_DESC_READ_POS_OFFSET,
        desc.state.read_pos as u64,
    );
    write_u64_at(bytes, offset + CPU_DESC_USED_OFFSET, desc.state.used as u64);
    write_u64_at(
        bytes,
        offset + CPU_DESC_NEXT_SEQ_OFFSET,
        desc.state.per_cpu_seq,
    );
    write_u64_at(bytes, offset + CPU_DESC_DROPPED_OFFSET, desc.state.dropped);
    write_u64_at(
        bytes,
        offset + CPU_DESC_REENTRANT_DROPPED_OFFSET,
        desc.state.reentrant_dropped,
    );
    write_u64_at(
        bytes,
        offset + CPU_DESC_FLAGS_OFFSET,
        if desc.state.writer_active {
            TRACE_IMAGE_CPU_FLAG_WRITER_ACTIVE
        } else {
            0
        },
    );
}

fn read_cpu_descriptor(bytes: &[u8], desc_offset: usize, cpu: usize) -> Option<CpuImageDescriptor> {
    let desc_size = read_u16_at(bytes, TRACE_IMAGE_CPU_DESC_SIZE_OFFSET) as usize;
    if desc_size < TRACE_IMAGE_CPU_DESC_SIZE {
        return None;
    }
    let offset = desc_offset.checked_add(cpu.checked_mul(desc_size)?)?;
    if offset.checked_add(TRACE_IMAGE_CPU_DESC_SIZE)? > bytes.len() {
        return None;
    }
    let flags = read_u64_at(bytes, offset + CPU_DESC_FLAGS_OFFSET);
    Some(CpuImageDescriptor {
        cpu_id: read_u32_at(bytes, offset + CPU_DESC_CPU_ID_OFFSET),
        buffer_offset: read_u64_at(bytes, offset + CPU_DESC_BUFFER_OFFSET_OFFSET) as usize,
        buffer_size: read_u64_at(bytes, offset + CPU_DESC_BUFFER_SIZE_OFFSET) as usize,
        state: CpuState {
            write_pos: read_u64_at(bytes, offset + CPU_DESC_WRITE_POS_OFFSET) as usize,
            read_pos: read_u64_at(bytes, offset + CPU_DESC_READ_POS_OFFSET) as usize,
            used: read_u64_at(bytes, offset + CPU_DESC_USED_OFFSET) as usize,
            per_cpu_seq: read_u64_at(bytes, offset + CPU_DESC_NEXT_SEQ_OFFSET),
            dropped: read_u64_at(bytes, offset + CPU_DESC_DROPPED_OFFSET),
            reentrant_dropped: read_u64_at(bytes, offset + CPU_DESC_REENTRANT_DROPPED_OFFSET),
            writer_active: flags & TRACE_IMAGE_CPU_FLAG_WRITER_ACTIVE != 0,
        },
    })
}

fn append_encoded_record(
    state: &mut CpuState,
    buffer: &mut [u8],
    mode: BufferMode,
    kind: RecordKind,
    event_id: u32,
    timestamp: u64,
    payload: &[u8],
) -> Result<(), DropReason> {
    let total_len = align_up(RECORD_HEADER_SIZE + payload.len(), RECORD_ALIGN);
    if total_len > u16::MAX as usize || total_len > buffer.len() {
        return Err(DropReason::RecordTooLarge);
    }

    let remaining_to_end = buffer.len() - state.write_pos;
    let padding_len = if remaining_to_end < total_len {
        remaining_to_end
    } else {
        0
    };
    let needed = padding_len + total_len;
    ensure_space(state, buffer, mode, needed)?;

    if padding_len > 0 {
        write_padding(state, buffer, padding_len);
        state.write_pos = 0;
    }

    let start = state.write_pos;
    encode_header(
        &mut buffer[start..start + RECORD_HEADER_SIZE],
        total_len as u16,
        kind,
        event_id,
        timestamp,
    );
    buffer[start + RECORD_HEADER_SIZE..start + RECORD_HEADER_SIZE + payload.len()]
        .copy_from_slice(payload);
    buffer[start + RECORD_HEADER_SIZE + payload.len()..start + total_len].fill(0);
    state.write_pos = (state.write_pos + total_len) % buffer.len();
    state.used += total_len;
    Ok(())
}

fn ensure_space(
    state: &mut CpuState,
    buffer: &[u8],
    mode: BufferMode,
    needed: usize,
) -> Result<(), DropReason> {
    if needed > buffer.len() {
        return Err(DropReason::RecordTooLarge);
    }
    if buffer.len() - state.used >= needed {
        return Ok(());
    }
    if mode == BufferMode::StopOnFull {
        return Err(DropReason::BufferFull);
    }
    while buffer.len() - state.used < needed {
        drop_oldest_record(state, buffer);
    }
    Ok(())
}

fn drop_oldest_record(state: &mut CpuState, buffer: &[u8]) {
    if state.used == 0 {
        return;
    }
    let cap = buffer.len();
    if cap - state.read_pos < RECORD_HEADER_SIZE {
        let skipped = cap - state.read_pos;
        state.read_pos = 0;
        state.used = state.used.saturating_sub(skipped);
        return;
    }
    let len = decode_header(&buffer[state.read_pos..state.read_pos + RECORD_HEADER_SIZE])
        .map(|header| header.len as usize)
        .filter(|len| *len > 0 && *len <= state.used && *len <= cap - state.read_pos)
        .unwrap_or(RECORD_ALIGN.min(state.used));
    state.read_pos = (state.read_pos + len) % cap;
    state.used = state.used.saturating_sub(len);
    state.dropped = state.dropped.saturating_add(1);
}

fn write_padding(state: &mut CpuState, buffer: &mut [u8], padding_len: usize) {
    if padding_len >= RECORD_HEADER_SIZE {
        let start = state.write_pos;
        encode_header(
            &mut buffer[start..start + RECORD_HEADER_SIZE],
            padding_len as u16,
            RecordKind::Padding,
            0,
            0,
        );
        buffer[start + RECORD_HEADER_SIZE..start + padding_len].fill(0);
    } else {
        buffer[state.write_pos..state.write_pos + padding_len].fill(0);
    }
    state.used += padding_len;
}

fn encode_header(out: &mut [u8], len: u16, kind: RecordKind, event_id: u32, timestamp: u64) {
    out[0..2].copy_from_slice(&len.to_le_bytes());
    out[2] = kind as u8;
    out[3] = 0;
    out[4..8].copy_from_slice(&event_id.to_le_bytes());
    out[8..16].copy_from_slice(&timestamp.to_le_bytes());
}

fn decode_header(bytes: &[u8]) -> Option<RecordHeader> {
    if bytes.len() < RECORD_HEADER_SIZE {
        return None;
    }
    let len = u16::from_le_bytes([bytes[0], bytes[1]]);
    let kind = match bytes[2] {
        1 => RecordKind::Instant,
        2 => RecordKind::DurationBegin,
        3 => RecordKind::DurationEnd,
        4 => RecordKind::Counter,
        5 => RecordKind::Metadata,
        6 => RecordKind::ContextSwitch,
        255 => RecordKind::Padding,
        _ => return None,
    };
    let event_id = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let timestamp = u64::from_le_bytes([
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]);
    Some(RecordHeader {
        len,
        kind,
        flags: bytes[3],
        event_id,
        timestamp,
    })
}

fn encode_context_switch(
    out: &mut PayloadWriter<'_>,
    cpu_id: u32,
    seq: u64,
    prev: TaskRef<'_>,
    prev_state: OffCpuState,
    next: TaskRef<'_>,
) -> Result<(), DropReason> {
    out.u32(cpu_id)?;
    out.u64(seq)?;
    out.task(prev)?;
    out.u32(prev_state.encode())?;
    out.task(next)
}

fn encode_trace_mark_begin(
    out: &mut PayloadWriter<'_>,
    cpu_id: u32,
    seq: u64,
    task: TaskRef<'_>,
    name: &str,
) -> Result<(), DropReason> {
    out.u32(cpu_id)?;
    out.u64(seq)?;
    out.task(task)?;
    out.str(name)
}

fn encode_trace_mark_end(
    out: &mut PayloadWriter<'_>,
    cpu_id: u32,
    seq: u64,
    task: TaskRef<'_>,
) -> Result<(), DropReason> {
    out.u32(cpu_id)?;
    out.u64(seq)?;
    out.task(task)
}

struct PayloadWriter<'a> {
    bytes: &'a mut [u8],
    pos: usize,
}

impl<'a> PayloadWriter<'a> {
    fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn written(&self) -> &[u8] {
        &self.bytes[..self.pos]
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), DropReason> {
        if self.bytes.len() - self.pos < bytes.len() {
            return Err(DropReason::RecordTooLarge);
        }
        self.bytes[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        self.pos += bytes.len();
        Ok(())
    }

    fn u16(&mut self, value: u16) -> Result<(), DropReason> {
        self.put(&value.to_le_bytes())
    }

    fn u32(&mut self, value: u32) -> Result<(), DropReason> {
        self.put(&value.to_le_bytes())
    }

    fn i32(&mut self, value: i32) -> Result<(), DropReason> {
        self.put(&value.to_le_bytes())
    }

    fn u64(&mut self, value: u64) -> Result<(), DropReason> {
        self.put(&value.to_le_bytes())
    }

    fn str(&mut self, value: &str) -> Result<(), DropReason> {
        let len = value.len();
        if len > u16::MAX as usize {
            return Err(DropReason::RecordTooLarge);
        }
        self.u16(len as u16)?;
        self.put(value.as_bytes())
    }

    fn task(&mut self, task: TaskRef<'_>) -> Result<(), DropReason> {
        self.u32(task.tid)?;
        self.u32(task.tgid)?;
        self.i32(task.prio)?;
        self.str(task.comm)
    }
}

struct PayloadReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> PayloadReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn get(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.bytes.len() - self.pos < len {
            return None;
        }
        let out = &self.bytes[self.pos..self.pos + len];
        self.pos += len;
        Some(out)
    }

    fn u16(&mut self) -> Option<u16> {
        let bytes = self.get(2)?;
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes = self.get(4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn i32(&mut self) -> Option<i32> {
        let bytes = self.get(4)?;
        Some(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Option<u64> {
        let bytes = self.get(8)?;
        Some(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn str(&mut self) -> Option<&'a str> {
        let len = self.u16()? as usize;
        core::str::from_utf8(self.get(len)?).ok()
    }

    fn task(&mut self) -> Option<OwnedTaskRef<'a>> {
        let tid = self.u32()?;
        let tgid = self.u32()?;
        let prio = self.i32()?;
        let comm = self.str()?;
        Some(OwnedTaskRef {
            comm,
            tid,
            tgid,
            prio,
        })
    }
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

fn read_u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn write_u16_at(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32_at(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64_at(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(feature = "std")]
/// Exporters that require the Rust standard library.
pub mod export {
    /// bytrace text exporter.
    pub mod bytrace {
        use crate::{DecodedRecord, TraceImageSnapshot, TraceSnapshot};
        use std::format;
        use std::io::{Result, Write};
        use std::string::String;
        use std::vec::Vec;

        /// Writes a snapshot as SmartPerf-compatible bytrace text.
        ///
        /// # Parameters
        ///
        /// - `snapshot`: Snapshot containing one stream per CPU.
        /// - `out`: Destination writer that receives bytrace text.
        ///
        /// # Returns
        ///
        /// Returns `Ok(())` after all supported records are written.
        /// Returns any [`std::io::Error`] produced by `out`.
        ///
        /// # Panics
        ///
        /// This function does not panic.
        ///
        /// # Side Effects
        ///
        /// Writes a bytrace header and event lines to `out`. It allocates a
        /// temporary host-side vector, decodes supported records, and sorts them
        /// by `(timestamp, cpu_id, per_cpu_seq)`.
        pub fn write_bytrace<W: Write>(snapshot: TraceSnapshot<'_>, out: &mut W) -> Result<()> {
            let mut records = Vec::new();
            for cpu in 0..snapshot.cpu_count() {
                if let Some(stream) = snapshot.cpu_stream(cpu) {
                    for record in stream.records() {
                        if let Some(decoded) = record.decode() {
                            records.push(decoded);
                        }
                    }
                }
            }
            write_records(records, out)
        }

        /// Writes a trace image snapshot as SmartPerf-compatible bytrace text.
        ///
        /// # Parameters
        ///
        /// - `snapshot`: Trace image snapshot containing one stream per CPU.
        /// - `out`: Destination writer that receives bytrace text.
        ///
        /// # Returns
        ///
        /// Returns `Ok(())` after all supported records are written.
        /// Returns any [`std::io::Error`] produced by `out`.
        ///
        /// # Panics
        ///
        /// This function does not panic.
        ///
        /// # Side Effects
        ///
        /// Writes a bytrace header and event lines to `out`. It allocates a
        /// temporary host-side vector, decodes supported image records, and
        /// sorts them by `(timestamp, cpu_id, per_cpu_seq)`.
        pub fn write_image_bytrace<W: Write>(
            snapshot: TraceImageSnapshot<'_>,
            out: &mut W,
        ) -> Result<()> {
            let mut records = Vec::new();
            for cpu in 0..snapshot.cpu_count() {
                if let Some(stream) = snapshot.cpu_stream(cpu) {
                    for record in stream.records() {
                        if let Some(decoded) = record.decode() {
                            records.push(decoded);
                        }
                    }
                }
            }
            write_records(records, out)
        }

        fn write_records<W: Write>(mut records: Vec<DecodedRecord<'_>>, out: &mut W) -> Result<()> {
            records.sort_by_key(sort_key);
            writeln!(out, "# tracer: nop")?;
            writeln!(
                out,
                "# entries-in-buffer/entries-written: {0}/{0}",
                records.len()
            )?;
            writeln!(out, "#")?;
            writeln!(out, "#                              _-----=> irqs-off")?;
            writeln!(out, "#                             / _----=> need-resched")?;
            writeln!(
                out,
                "#                            | / _---=> hardirq/softirq"
            )?;
            writeln!(out, "#                            || / _--=> preempt-depth")?;
            writeln!(out, "#                            ||| /     delay")?;
            writeln!(
                out,
                "#           TASK-PID   TGID   CPU#  ||||    TIMESTAMP  FUNCTION"
            )?;
            writeln!(out)?;

            for record in records {
                write_record(out, record)?;
            }
            Ok(())
        }

        fn sort_key(record: &DecodedRecord<'_>) -> (u64, u32, u64) {
            match *record {
                DecodedRecord::ContextSwitch {
                    timestamp,
                    cpu_id,
                    seq,
                    ..
                }
                | DecodedRecord::TraceMarkBegin {
                    timestamp,
                    cpu_id,
                    seq,
                    ..
                }
                | DecodedRecord::TraceMarkEnd {
                    timestamp,
                    cpu_id,
                    seq,
                    ..
                } => (timestamp, cpu_id, seq),
            }
        }

        fn write_record<W: Write>(out: &mut W, record: DecodedRecord<'_>) -> Result<()> {
            match record {
                DecodedRecord::ContextSwitch {
                    timestamp,
                    cpu_id,
                    prev,
                    prev_state,
                    next,
                    ..
                } => writeln!(
                    out,
                    "{}: sched_switch: prev_comm={} prev_pid={} prev_prio={} prev_state={} ==> next_comm={} next_pid={} next_prio={}",
                    line_prefix(prev.comm, prev.tid, prev.tgid, cpu_id, timestamp),
                    prev.comm,
                    prev.tid,
                    prev.prio,
                    prev_state.bytrace(),
                    next.comm,
                    next.tid,
                    next.prio
                ),
                DecodedRecord::TraceMarkBegin {
                    timestamp,
                    cpu_id,
                    task,
                    name,
                    ..
                } => writeln!(
                    out,
                    "{}: tracing_mark_write: B|{}|{}",
                    line_prefix(task.comm, task.tid, task.tgid, cpu_id, timestamp),
                    task.tgid,
                    name
                ),
                DecodedRecord::TraceMarkEnd {
                    timestamp,
                    cpu_id,
                    task,
                    ..
                } => writeln!(
                    out,
                    "{}: tracing_mark_write: E|{}",
                    line_prefix(task.comm, task.tid, task.tgid, cpu_id, timestamp),
                    task.tgid
                ),
            }
        }

        fn line_prefix(comm: &str, tid: u32, tgid: u32, cpu_id: u32, timestamp_ns: u64) -> String {
            let seconds = timestamp_ns / 1_000_000_000;
            let micros = (timestamp_ns % 1_000_000_000) / 1_000;
            format!(
                "{:<12}-{} ({:>5}) [{:03}] .... {}.{:06}",
                comm, tid, tgid, cpu_id, seconds, micros
            )
        }
    }
}

#[cfg(test)]
mod tests;
