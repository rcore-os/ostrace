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

#[cfg(feature = "std")]
/// Exporters that require the Rust standard library.
pub mod export {
    /// bytrace text exporter.
    pub mod bytrace {
        use crate::{DecodedRecord, TraceSnapshot};
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
mod tests {
    use super::*;
    #[cfg(feature = "std")]
    use std::string::String;
    use std::vec::Vec;

    #[test]
    fn record_header_size_is_stable() {
        assert_eq!(RECORD_HEADER_SIZE, 16);
    }

    #[test]
    fn single_cpu_append_order_is_preserved() {
        let mut storage = TraceStorage::new();
        let platform = TestPlatform::new(&[0, 0], &[10, 20]);
        let mut recorder = TraceRecorder::new(TraceConfig {
            storage: &mut storage,
            cpu_count: 1,
            platform,
        })
        .unwrap();
        let mut session_storage = TraceSessionStorage::<1>::new();
        let mut buf0 = [0u8; 512];
        let mut cpu_buffers = [CpuBuffer { bytes: &mut buf0 }];
        let mut session = recorder
            .start_session(SessionConfig {
                state: &mut session_storage,
                cpu_buffers: &mut cpu_buffers,
                mode: BufferMode::Overwrite,
            })
            .unwrap();
        assert_eq!(
            session.trace_mark_begin(task("A", 1, 1), "outer"),
            AppendStatus::Written
        );
        assert_eq!(
            session.trace_mark_end(task("A", 1, 1)),
            AppendStatus::Written
        );

        let records = decoded(session.snapshot());
        assert!(matches!(
            records[0],
            DecodedRecord::TraceMarkBegin { name: "outer", .. }
        ));
        assert!(matches!(records[1], DecodedRecord::TraceMarkEnd { .. }));
    }

    #[test]
    fn multi_cpu_append_is_independent() {
        let mut storage = TraceStorage::new();
        let platform = TestPlatform::new(&[0, 1], &[10, 20]);
        let mut recorder = TraceRecorder::new(TraceConfig {
            storage: &mut storage,
            cpu_count: 2,
            platform,
        })
        .unwrap();
        let mut session_storage = TraceSessionStorage::<2>::new();
        let mut buf0 = [0u8; 512];
        let mut buf1 = [0u8; 512];
        let mut cpu_buffers = [
            CpuBuffer { bytes: &mut buf0 },
            CpuBuffer { bytes: &mut buf1 },
        ];
        let mut session = recorder
            .start_session(SessionConfig {
                state: &mut session_storage,
                cpu_buffers: &mut cpu_buffers,
                mode: BufferMode::Overwrite,
            })
            .unwrap();
        assert_eq!(
            session.trace_mark_begin(task("A", 1, 1), "cpu0"),
            AppendStatus::Written
        );
        assert_eq!(
            session.trace_mark_begin(task("B", 2, 2), "cpu1"),
            AppendStatus::Written
        );

        let snapshot = session.snapshot();
        let cpu0: Vec<_> = snapshot.cpu_stream(0).unwrap().records().collect();
        let cpu1: Vec<_> = snapshot.cpu_stream(1).unwrap().records().collect();
        assert_eq!(cpu0.len(), 1);
        assert_eq!(cpu1.len(), 1);
    }

    #[test]
    fn overwrite_wrap_drops_old_records() {
        let mut storage = TraceStorage::new();
        let platform = TestPlatform::new(&[0, 0, 0, 0, 0], &[10, 20, 30, 40, 50]);
        let mut recorder = TraceRecorder::new(TraceConfig {
            storage: &mut storage,
            cpu_count: 1,
            platform,
        })
        .unwrap();
        let mut session_storage = TraceSessionStorage::<1>::new();
        let mut buf0 = [0u8; 96];
        let mut cpu_buffers = [CpuBuffer { bytes: &mut buf0 }];
        let mut session = recorder
            .start_session(SessionConfig {
                state: &mut session_storage,
                cpu_buffers: &mut cpu_buffers,
                mode: BufferMode::Overwrite,
            })
            .unwrap();
        assert_eq!(
            session.trace_mark_begin(task("A", 1, 1), "first"),
            AppendStatus::Written
        );
        assert_eq!(
            session.trace_mark_begin(task("A", 1, 1), "second"),
            AppendStatus::Written
        );
        assert_eq!(
            session.trace_mark_begin(task("A", 1, 1), "third"),
            AppendStatus::Written
        );

        let records = decoded(session.snapshot());
        assert!(records.len() < 3);
        assert!(session.state.cpu_state(0).unwrap().dropped() > 0);
    }

    #[test]
    fn reentrant_append_is_dropped() {
        let mut storage = TraceStorage::new();
        storage.global.active_session = true;
        let platform = TestPlatform::new(&[0], &[10]);
        let mut session_storage = TraceSessionStorage::<1>::new();
        session_storage.cpus[0].set_writer_active_for_test(true);
        let mut buf0 = [0u8; 512];
        let mut cpu_buffers = [CpuBuffer { bytes: &mut buf0 }];
        let mut session = TraceSession {
            storage: &mut storage,
            cpu_count: 1,
            platform: &platform,
            state: &mut session_storage,
            cpu_buffers: &mut cpu_buffers,
            mode: BufferMode::Overwrite,
        };
        assert_eq!(
            session.trace_mark_begin(task("A", 1, 1), "dropped"),
            AppendStatus::Dropped(DropReason::Reentrant)
        );
        assert_eq!(session.state.cpu_state(0).unwrap().reentrant_dropped(), 1);
    }

    #[test]
    fn invalid_cpu_is_dropped() {
        let mut storage = TraceStorage::new();
        let platform = TestPlatform::new(&[7], &[10]);
        let mut recorder = TraceRecorder::new(TraceConfig {
            storage: &mut storage,
            cpu_count: 1,
            platform,
        })
        .unwrap();
        let mut session_storage = TraceSessionStorage::<1>::new();
        let mut buf0 = [0u8; 512];
        let mut cpu_buffers = [CpuBuffer { bytes: &mut buf0 }];
        let mut session = recorder
            .start_session(SessionConfig {
                state: &mut session_storage,
                cpu_buffers: &mut cpu_buffers,
                mode: BufferMode::Overwrite,
            })
            .unwrap();
        assert_eq!(
            session.trace_mark_begin(task("A", 1, 1), "invalid"),
            AppendStatus::Dropped(DropReason::InvalidCpu)
        );
    }

    #[test]
    fn active_session_rejects_second_session() {
        let mut storage = TraceStorage::new();
        storage.global.active_session = true;
        let platform = TestPlatform::new(&[0], &[10]);
        let mut recorder = TraceRecorder::new(TraceConfig {
            storage: &mut storage,
            cpu_count: 1,
            platform,
        })
        .unwrap();
        let mut session_storage_a = TraceSessionStorage::<1>::new();
        let mut buf_a = [0u8; 256];
        let mut cpu_buffers_a = [CpuBuffer { bytes: &mut buf_a }];

        let result = recorder.start_session(SessionConfig {
            state: &mut session_storage_a,
            cpu_buffers: &mut cpu_buffers_a,
            mode: BufferMode::Overwrite,
        });
        assert!(matches!(result, Err(TraceError::SessionAlreadyActive)));
    }

    #[cfg(feature = "std")]
    #[test]
    fn bytrace_export_merges_by_timestamp() {
        let mut storage = TraceStorage::new();
        let platform = TestPlatform::new(&[1, 0, 1], &[30, 10, 20]);
        let mut recorder = TraceRecorder::new(TraceConfig {
            storage: &mut storage,
            cpu_count: 2,
            platform,
        })
        .unwrap();
        let mut session_storage = TraceSessionStorage::<2>::new();
        let mut buf0 = [0u8; 512];
        let mut buf1 = [0u8; 512];
        let mut cpu_buffers = [
            CpuBuffer { bytes: &mut buf0 },
            CpuBuffer { bytes: &mut buf1 },
        ];
        let mut session = recorder
            .start_session(SessionConfig {
                state: &mut session_storage,
                cpu_buffers: &mut cpu_buffers,
                mode: BufferMode::Overwrite,
            })
            .unwrap();
        assert_eq!(
            session.trace_mark_begin(task("B", 2, 2), "late"),
            AppendStatus::Written
        );
        assert_eq!(
            session.context_switch(task("idle", 0, 0), OffCpuState::Running, task("A", 1, 1)),
            AppendStatus::Written
        );
        assert_eq!(
            session.trace_mark_end(task("B", 2, 2)),
            AppendStatus::Written
        );

        let mut out = Vec::new();
        crate::export::bytrace::write_bytrace(session.snapshot(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("# entries-in-buffer/entries-written: 3/3"));
        assert!(text.contains("sched_switch: prev_comm=idle prev_pid=0"));
        assert!(text.contains("tracing_mark_write: B|2|late"));
        assert!(text.contains("tracing_mark_write: E|2"));

        let switch = text.find("sched_switch").unwrap();
        let end = text.find("tracing_mark_write: E|2").unwrap();
        let begin = text.find("tracing_mark_write: B|2|late").unwrap();
        assert!(switch < end);
        assert!(end < begin);
    }

    fn decoded<'a>(snapshot: TraceSnapshot<'a>) -> Vec<DecodedRecord<'a>> {
        let mut out = Vec::new();
        for cpu in 0..snapshot.cpu_count() {
            for record in snapshot.cpu_stream(cpu).unwrap().records() {
                out.push(record.decode().unwrap());
            }
        }
        out
    }

    fn task(comm: &str, tid: u32, tgid: u32) -> TaskRef<'_> {
        TaskRef {
            comm,
            tid,
            tgid,
            prio: 120,
        }
    }

    struct TestPlatform {
        cpus: Vec<u32>,
        times: Vec<u64>,
        index: core::cell::Cell<usize>,
    }

    impl TestPlatform {
        fn new(cpus: &[u32], times: &[u64]) -> Self {
            Self {
                cpus: cpus.to_vec(),
                times: times.to_vec(),
                index: core::cell::Cell::new(0),
            }
        }
    }

    impl TracePlatform for TestPlatform {
        fn now_ns(&self) -> u64 {
            let index = self.index.get().min(self.times.len() - 1);
            let time = self.times[index];
            self.index.set(self.index.get() + 1);
            time
        }

        fn cpu_id(&self) -> u32 {
            let index = self.index.get().min(self.cpus.len() - 1);
            self.cpus[index]
        }
    }
}
