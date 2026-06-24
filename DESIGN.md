# ostrace Design Notes

`ostrace` is a `no_std` trace record core for Rust-based OS kernels. It is
intended to provide the record-writing layer below an OS-specific tracing
framework, in a role similar to the lower part of Linux ftrace, while staying
independent from any specific tracepoint framework, allocator, file system,
serial driver, QEMU workflow, or eBPF runtime.

The current implementation focuses on a small, runnable tracing path:

```text
sched/task event -> compact binary record -> per-CPU ring buffer
```

For OS integration, the preferred storage model is a single caller-owned trace
image RAM region:

```text
TraceImageHeader
CpuDescriptor[cpu_count]
PerCpuRingBuffer[cpu_count]
```

The image is a live in-memory container. `ostrace` initializes it, appends
records into it, updates cursor/drop state in place, and can later parse it from
a RAM dump. The library does not allocate memory on the kernel path.

## Documentation Sync

`DESIGN.md` and `DESIGN_CN.md` describe the same design in English and Chinese.
Whenever project design, public API, trace image layout, record format, exporter
behavior, or development policy changes, both files must be updated in the same
change. If one file intentionally differs, the reason must be documented in both
files.

## Current Scope

Implemented now:

- `#![no_std]` library core.
- Strict public API rustdoc lints.
- Caller-provided memory only; no library-owned heap allocation on the trace
  hot path.
- Preferred single-RAM-region trace image API.
- Older split-storage session API for tests and experiments.
- Per-CPU ring buffers.
- Per-CPU append order with `per_cpu_seq`.
- Reentrant append detection per CPU.
- `Overwrite` and `StopOnFull` buffer modes.
- `sched_switch`-style context switch records.
- Synchronous `tracing_mark_write` begin/end records.
- Snapshot parsing for a live image or a full RAM dump.
- `std`-gated bytrace exporter.
- `trace2bytrace` host tool.

Not implemented yet:

- Generic `instant`, `counter`, async span, or structured metadata events.
- Task lifecycle metadata such as `task_new`/`task_exit`.
- Trace mark custom metadata or `custom_args` emission.
- Global cross-CPU ring buffer.
- Lock-free multi-writer synchronization.
- Perfetto protobuf or Chrome Trace JSON export.
- eBPF verifier, VM, or JIT.

## Goals

- Support `#![no_std]`.
- Work in teaching OSes, experimental OSes, and production-oriented Rust
  kernels.
- Work before an allocator or file system exists.
- Use caller-provided memory as fixed trace storage.
- Keep the hot path cheap by appending compact binary records only.
- Support host-side conversion to SmartPerf/bytrace now, and later to ftrace
  text, Chrome Trace JSON, Perfetto, and related formats.
- Avoid mandatory dependencies on file systems, block devices, serial ports,
  QEMU, eBPF, or a specific tracepoint mechanism.

## Non-Goals

- Do not implement an eBPF verifier.
- Do not implement an eBPF bytecode VM or JIT.
- Do not execute user programs at hook points.
- Do not bind the core record format directly to the Linux ftrace ABI.
- Do not generate text formats in the kernel hot path.
- Do not require the OS to have a file system or allocator.

## Source Layout

The current MVP keeps the core implementation in one library file:

```text
src/
  lib.rs
  bin/
    trace2bytrace.rs
```

As the project grows, the implementation should be split along these boundaries:

```text
src/
  lib.rs
  image.rs        // trace image container layout and parser
  record.rs       // record header, kind, payload encoding
  ringbuf.rs      // fixed memory ring buffer
  session.rs      // split-storage session API
  platform.rs     // OS-injected traits
  task.rs         // task references and scheduler states
  snapshot.rs     // snapshot/drain views
  export/
    bytrace.rs
    ftrace_text.rs
    chrome_json.rs
    perfetto.rs
```

## Core Storage Model

The preferred runtime integration is the trace image API. The OS supplies one
mutable byte slice and `ostrace` formats the image inside it:

```rust
pub struct TraceImageConfig<'a, P> {
    pub bytes: &'a mut [u8],
    pub cpu_count: usize,
    pub per_cpu_buffer_size: usize,
    pub mode: BufferMode,
    pub platform: P,
}
```

`bytes` is the only persistent storage required by this path. Small stack
buffers are used while encoding a record, but callers do not provide separate
global state, session state, or per-CPU buffer arrays for trace image operation.

The older split-storage API remains available:

```rust
pub struct TraceConfig<'a, P> {
    pub storage: &'a mut TraceStorage,
    pub cpu_count: usize,
    pub platform: P,
}

pub struct SessionConfig<'a, const MAX_CPUS: usize> {
    pub state: &'a mut TraceSessionStorage<MAX_CPUS>,
    pub cpu_buffers: &'a mut [CpuBuffer<'a>],
    pub mode: BufferMode,
}
```

This API is useful for focused tests and experiments. New OS integrations should
prefer `TraceImage`, because a host tool can parse it directly from RAM without
reconstructing hidden Rust state.

## Trace Image Layout

All integer fields are little-endian in the current format.

Header constants:

```text
TRACE_IMAGE_MAGIC = 0x0045_4341_5254_534f
container version = 1.0
record format version = 1.0
header size = 96 bytes
CPU descriptor size = 80 bytes
record header size = 16 bytes
record alignment = 8 bytes
```

The header v1 prefix currently stores:

```text
offset  size  field
0       8     magic
8       2     container_major
10      2     container_minor
12      2     header_size
14      1     endian marker, 1 means little-endian
16      2     record_major
18      2     record_minor
20      2     record_header_size
22      2     record_align
24      4     cpu_count
32      8     cpu_descriptor_offset
40      2     cpu_descriptor_size
48      8     ring_buffer_region_offset
56      8     image_size
64      8     flags, bit 0 means active
72      4     buffer_mode, 0 means Overwrite and 1 means StopOnFull
```

Each CPU descriptor v1 prefix currently stores:

```text
offset  size  field
0       4     cpu_id
8       8     buffer_offset
16      8     buffer_size
24      8     write_pos
32      8     read_pos
40      8     used
48      8     next_per_cpu_seq
56      8     dropped
64      8     reentrant_dropped
72      8     flags, bit 0 means writer_active
```

The image is not a finish-time artifact. Appending a record mutates the selected
per-CPU ring buffer and writes the updated CPU descriptor back into the same RAM
region. A RAM dump taken before clean shutdown can still export committed
records.

`TraceImage::finish()` and `Drop` clear the image active flag. Existing records
and descriptors remain parseable.

## Compatibility Rules

Parsers validate the image before export:

- Bad magic means the memory is not an `ostrace` image.
- Unsupported endian is rejected.
- Container major version mismatch is rejected.
- Record major version mismatch is rejected.
- Unexpected record header size or alignment is rejected.
- Header and CPU descriptor sizes smaller than the v1 prefix are rejected.
- Larger header or descriptor sizes are allowed when the known v1 prefix is
  present, so newer versions can append fields.
- CPU descriptors that point outside the declared image are rejected.
- Unknown or malformed record payloads are skipped by current exporters.

The container version and record format version are intentionally separate. A
container layout change does not necessarily imply that existing record payloads
changed, and vice versa.

## Platform Hooks

The OS injects the timestamp and CPU id through `TracePlatform`:

```rust
pub trait TracePlatform {
    fn now_ns(&self) -> u64;
    fn cpu_id(&self) -> u32;
}
```

`now_ns()` returns the timestamp written into record headers. `cpu_id()` selects
the per-CPU ring buffer. If the CPU id is outside the configured CPU count, the
append returns `AppendStatus::Dropped(DropReason::InvalidCpu)`.

The current code does not define a synchronization trait. Same-CPU reentrancy is
detected by `writer_active`; a reentrant append is dropped and counted instead
of corrupting the in-progress record. OS integrations are still responsible for
choosing the right outer synchronization policy, such as disabling preemption,
using per-CPU guards, or preventing migration around append calls.

## Record Format

Each binary record starts with a 16-byte header:

```rust
#[repr(C)]
pub struct RecordHeader {
    pub len: u16,
    pub kind: RecordKind,
    pub flags: u8,
    pub event_id: u32,
    pub timestamp: u64,
}
```

Header fields are encoded as:

```text
offset  size  field
0       2     total aligned record length
2       1     record kind
3       1     flags, currently zero
4       4     event_id
8       8     timestamp in nanoseconds
```

Records are aligned to 8 bytes:

```text
[RecordHeader][payload][zero padding]
```

If the remaining bytes at the end of a per-CPU buffer cannot fit the next
record, the writer emits a `Padding` record when possible, wraps to offset zero,
and writes the event record there.

Current record kinds and event IDs:

```text
RecordKind::ContextSwitch = 6, event_id = 1
RecordKind::DurationBegin = 2, event_id = 2
RecordKind::DurationEnd   = 3, event_id = 3
RecordKind::Padding       = 255
```

Payloads currently use this encoding:

```text
u16 string length + UTF-8 bytes

TaskRef:
  u32 tid
  u32 tgid
  i32 prio
  string comm

ContextSwitch:
  u32 cpu_id
  u64 per_cpu_seq
  TaskRef prev
  u32 prev_state
  TaskRef next

TraceMarkBegin:
  u32 cpu_id
  u64 per_cpu_seq
  TaskRef task
  string name

TraceMarkEnd:
  u32 cpu_id
  u64 per_cpu_seq
  TaskRef task
```

For a context switch, the total record size is:

```text
align8(16 + 42 + prev.comm.len + next.comm.len)
```

The 42-byte payload base is `cpu_id + seq + prev task fixed fields +
prev_comm_len + prev_state + next task fixed fields + next_comm_len`.

## Append Semantics

Each CPU has an independent ring buffer and cursor state:

```text
write_pos
read_pos
used
next_per_cpu_seq
dropped
reentrant_dropped
writer_active
```

Append behavior:

- The writer asks `platform.cpu_id()` for the target CPU.
- CPU ids outside `cpu_count` return `Dropped(InvalidCpu)`.
- Same-CPU reentrant appends return `Dropped(Reentrant)` and update drop
  counters.
- Payload encoding uses a fixed 512-byte stack scratch buffer; larger payloads
  return `Dropped(RecordTooLarge)`.
- In `StopOnFull` mode, insufficient space returns `Dropped(BufferFull)`.
- In `Overwrite` mode, the writer drops oldest records from that CPU until the
  new record fits.
- Per-CPU sequence numbers are assigned before payload encoding and used for
  stable export ordering.

The write layer guarantees per-CPU order only. Cross-CPU ordering is performed
when snapshot/export code decodes all CPU streams and sorts records.

## Event API

The current public write API is intentionally narrow:

```rust
pub fn context_switch(
    &mut self,
    prev: TaskRef<'_>,
    prev_state: OffCpuState,
    next: TaskRef<'_>,
) -> AppendStatus;

pub fn trace_mark_begin(&mut self, task: TaskRef<'_>, name: &str) -> AppendStatus;

pub fn trace_mark_end(&mut self, task: TaskRef<'_>) -> AppendStatus;
```

`TaskRef` uses neutral scheduler terminology:

```rust
pub struct TaskRef<'a> {
    pub comm: &'a str,
    pub tid: u32,
    pub tgid: u32,
    pub prio: i32,
}
```

The API takes explicit task IDs rather than asking the platform for
`current_pid`, because scheduler events can involve more than the current task.
For example, a running task may wake another task from a different process.

`OffCpuState` describes the state entered by the previous task. Exporters map it
to target-specific spellings such as ftrace/bytrace `R`, `S`, `D`, `T`, and so
on. The next task is implicitly the one selected to run by the context switch.

Future generic event APIs should be layered on top of the same binary record
core instead of replacing it:

```rust
trace::instant!("sched", "wakeup", tid = next_tid);
trace::begin!("syscall", "read", fd = fd);
trace::end!("syscall", "read");
trace::counter!("mm", "free_pages", value = free_pages);
```

## PID/TID Identity

The current MVP records OS-visible `tid` and `tgid` directly in each `TaskRef`.
This is sufficient for early bytrace conversion and SmartPerf inspection.

Longer term, the API should not assume that OS-visible PID/TID values are
globally unique. A later metadata layer should separate raw IDs from
lifecycle-unique trace IDs:

```text
raw_pid/raw_tid:
  OS-visible IDs, possibly reused

trace_pid/trace_tid:
  lifecycle-unique IDs used to bind UI tracks
```

The recommended OS integration for that future layer is to assign monotonically
increasing `trace_pid` and `trace_tid` values when a process or thread is
created and store them in the PCB/TCB. Metadata records would map trace IDs back
to raw IDs and display names.

## Snapshot And Export

The hot path writes binary records only. Export happens through read-only
snapshots:

```rust
TraceImage::snapshot() -> TraceImageSnapshot<'_>
TraceImageSnapshot::parse(bytes) -> Result<TraceImageSnapshot<'_>, TraceImageError>
TraceImageSnapshot::parse_from_ram(ram, ram_base, trace_base)
TraceSession::snapshot() -> TraceSnapshot<'_>
```

`TraceImageSnapshot::parse_from_ram` is intended for host tools that receive a
full RAM dump. `ram_base` is the physical address represented by `ram[0]`, and
`trace_base` is the physical address where the trace image starts.

Under the `std` feature, the bytrace exporter writes SmartPerf-compatible text:

```rust
export::bytrace::write_image_bytrace(snapshot, writer)
export::bytrace::write_bytrace(snapshot, writer)
```

The exporter:

- decodes supported records from every CPU stream;
- ignores malformed or unsupported records;
- sorts by `(timestamp, cpu_id, per_cpu_seq)`;
- emits a bytrace text header;
- maps context switches to `sched_switch`;
- maps trace mark begin/end to `tracing_mark_write: B|tgid|name` and
  `tracing_mark_write: E|tgid`.

The `trace2bytrace` binary is available with the `std` feature:

```text
cargo run --features std --bin trace2bytrace -- \
  --ram path/to/ram.bin \
  --ram-base 0x80000000 \
  --trace-base 0x87b00000 \
  --out path/to/trace.bytrace
```

## bytrace And SmartPerf Notes

Current output targets the subset needed for early SmartPerf inspection:

```text
sched_switch: prev_comm=... prev_pid=... prev_prio=... prev_state=... ==> next_comm=... next_pid=... next_prio=...
tracing_mark_write: B|tgid|name
tracing_mark_write: E|tgid
```

SmartPerf can display `tracing_mark_write` duration blocks. Additional
click-time metadata should eventually be encoded on the `B` line, because the
`E` line only closes the synchronous slice. A future API may add trace mark
metadata and export names such as:

```text
B|tgid|name|Ktag|key=value,key2=value2
```

That extension is not implemented yet.

## Development Policy

All public Rust APIs must have rustdoc. The crate enforces:

```rust
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::bare_urls)]
```

Detailed comment requirements are documented in `CODING_GUIDELINES.md`.

## Summary

`ostrace` currently provides a minimal but end-to-end trace path for Rust OS
kernels: the OS supplies one RAM region, platform timestamp/CPU hooks, and task
references; `ostrace` writes per-CPU binary records into a self-describing trace
image; host-side code parses the image and converts supported records to
SmartPerf-compatible bytrace text.
