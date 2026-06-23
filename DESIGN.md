# ostrace Design Notes

`ostrace` is a general-purpose `no_std` trace record core for Rust-based OS
kernels. It is intended to play a role similar to the record-writing core below
Linux ftrace, while staying independent from any specific OS tracepoint
framework, file system, allocator, serial driver, QEMU integration, or eBPF
runtime.

The core responsibility is intentionally narrow:

```text
structured event -> compact binary record -> caller-provided ring buffer
```

The embedding OS injects:

```text
timestamp, CPU ID, process/thread identity, synchronization, memory buffer,
and export sinks
```

Tracepoint frameworks may depend on `ostrace`, but `ostrace` does not depend on
tracepoints. Normal kernel code can directly emit instant events, duration
begin/end events, counters, spans, and metadata.

## Goals

- Support `#![no_std]`.
- Work in teaching OSes, experimental OSes, and production-oriented Rust
  kernels.
- Work before an allocator or file system exists.
- Treat caller-provided memory as a fixed ring buffer.
- Keep the hot path cheap by appending compact binary records only.
- Support later conversion to ftrace text, Chrome Trace JSON, Perfetto, and
  related formats.
- Avoid mandatory dependencies on file systems, block devices, serial ports,
  QEMU, eBPF, or a specific tracepoint mechanism.

## Non-Goals

- Do not implement an eBPF verifier.
- Do not implement an eBPF bytecode VM or JIT.
- Do not execute user programs at hook points.
- Do not bind directly to the Linux ftrace ABI.
- Do not generate text formats in the kernel hot path.
- Do not require the OS to have a file system or allocator.

## Suggested Module Layout

```text
src/
  lib.rs
  record.rs       // record header, kind, payload encoding
  ringbuf.rs      // fixed memory ring buffer
  writer.rs       // append API
  platform.rs     // OS-injected traits
  event.rs        // event id, category/name model
  field.rs        // compact field representation
  snapshot.rs     // snapshot/drain view
  filter.rs       // optional enable/disable fast path
  export/         // optional, likely feature-gated later
    binary.rs
    ftrace_text.rs
    chrome_json.rs
    perfetto.rs
```

## Core Configuration

The OS initializes `ostrace` with memory it owns:

```rust
pub struct TraceConfig<'a, P> {
    pub buffer: &'a mut [u8],
    pub platform: P,
    pub mode: BufferMode,
}

pub enum BufferMode {
    Overwrite,
    StopOnFull,
}
```

The OS provides platform-specific data through a trait:

```rust
pub trait TracePlatform {
    fn now(&self) -> u64;
    fn cpu_id(&self) -> u32;

    fn current_process_trace_id(&self) -> u64;
    fn current_thread_trace_id(&self) -> u64;

    fn current_raw_pid(&self) -> u64;
    fn current_raw_tid(&self) -> u64;
}
```

Synchronization should also be injected by the OS rather than assumed by
`ostrace`:

```rust
pub trait TraceGuard {
    type Guard;

    fn enter(&self) -> Self::Guard;
}
```

A single-core teaching OS can implement this by disabling interrupts. An SMP OS
can use preemption disabling, per-CPU buffer guards, or a kernel lock. Tests can
use a normal lock.

## Record Format

The internal stream is binary, not text. The first version should use a fixed
header and variable payload:

```rust
#[repr(u8)]
pub enum RecordKind {
    Instant = 1,
    DurationBegin = 2,
    DurationEnd = 3,
    Counter = 4,
    Metadata = 5,
    Padding = 255,
}

#[repr(C)]
pub struct RecordHeader {
    pub len: u16,        // total aligned record length
    pub kind: RecordKind,
    pub flags: u8,
    pub event_id: u32,
    pub timestamp: u64,
}
```

Each record is laid out as:

```text
[RecordHeader][payload][padding]
```

Records should be aligned to 8 bytes. If the remaining space at the end of the
ring buffer cannot fit a complete record, the writer emits a `Padding` record
and wraps to the beginning. There is no heap fragmentation because the stream is
append-only and does not allocate or free arbitrary regions.

## Append-Only Semantics

The record stream is append-only:

```text
append event record
append metadata record
append padding record
overwrite old records when ring wraps
```

Ring buffer control state is mutable:

```text
write_pos
oldest_pos/read_pos
dropped_count
flags
```

The accurate model is:

```text
record stream append-only
ring buffer control state mutable
derived metadata cache optional
```

The first version should avoid maintaining a strongly consistent index of the
PIDs or TIDs currently present in the window. Metadata changes should be written
as ordinary metadata records.

## PID/TID Reuse

The API must not assume that OS-visible PID/TID values are globally unique.
Separate raw IDs from lifecycle-unique trace IDs:

```text
raw_pid/raw_tid:
  OS-visible IDs, possibly reused

trace_pid/trace_tid:
  lifecycle-unique IDs used to bind UI tracks
```

The recommended OS integration is to assign monotonically increasing
`trace_pid` and `trace_tid` values when a process or thread is created and store
them in the PCB/TCB.

Records should use trace IDs. Metadata records map trace IDs back to raw IDs and
display names:

```text
task_new(trace_pid=1001, trace_tid=2001, raw_pid=3, raw_tid=3, name="shell")
task_exit(trace_pid=1001, trace_tid=2001)
```

This prevents tools such as Perfetto or Chrome Trace from merging unrelated
process or thread lifetimes when a teaching OS later reuses raw PID values.

## Event API

Events do not need to originate from tracepoints. Kernel code should be able to
emit events directly:

```rust
trace::instant!("sched", "wakeup", tid = next_tid);
trace::begin!("syscall", "read", fd = fd);
trace::end!("syscall", "read");
trace::counter!("mm", "free_pages", value = free_pages);

let _span = trace::span!("fs", "open", path_hash = hash);
```

The lower-level API can start with ID-based functions:

```rust
pub fn instant(event_id: EventId, fields: &[Field]);
pub fn begin(event_id: EventId, fields: &[Field]);
pub fn end(event_id: EventId, fields: &[Field]);
pub fn counter(event_id: EventId, value: u64, fields: &[Field]);
pub fn metadata(kind: MetadataKind, fields: &[Field]);
```

For a teaching-oriented version, string category/name pairs are acceptable. A
production-oriented version should use statically registered `EventId` values to
reduce hot-path work.

## Tracepoint Integration

A tracepoint system should be an external framework that depends on `ostrace`:

```rust
pub trait Tracepoint {
    const EVENT: EventId;
    type Context;

    fn fields(ctx: &Self::Context, out: &mut FieldWriter);
}
```

At a hook site:

```rust
let ctx = SchedSwitchCtx { prev, next, prev_state };
ostrace::record_tracepoint::<SchedSwitch>(&ctx);
```

The `ostrace` core only needs to know the event ID, record kind, and fields. It
does not need to know whether the event came from a tracepoint or a direct
kernel call.

## Snapshot, Drain, and Export

The hot path writes binary records only. Export happens through snapshot or
drain operations:

```rust
pub trait TraceSink {
    fn write(&mut self, bytes: &[u8]) -> Result<(), TraceError>;
    fn flush(&mut self) -> Result<(), TraceError>;
}

pub fn drain_to<S: TraceSink>(&self, sink: &mut S) -> Result<(), TraceError>;
pub fn snapshot(&self) -> TraceSnapshot;
```

Possible OS-provided sinks:

```text
SerialSink
RawBlockSink
FsFileSink
VirtioConsoleSink
NetworkSink
HostMemoryDumpSink
```

Before a file system exists, practical capture options include:

```text
fixed memory ring buffer
+ QEMU file-backed RAM or monitor pmemsave
+ host-side trace.bin extraction from the memory dump
```

Once a file system exists, the OS can save binary traces to paths such as:

```text
/trace/kernel.trace
```

## Format Conversion

Preferred conversion path:

```text
ostrace binary
  -> ftrace text
  -> Chrome Trace JSON
  -> Perfetto protobuf / pftrace
```

The kernel hot path should not generate ftrace text. Text and protobuf export
should be done by OS-side drain code outside the hot path or by host-side
converter tools using binary records plus metadata.

## First MVP

Implement first:

- `#![no_std]`.
- Caller-provided `&'static mut [u8]`.
- Single ring buffer.
- Fixed header plus variable payload.
- 8-byte alignment.
- Padding record.
- Overwrite mode.
- `instant`, `begin`, `end`, `counter`, and `metadata`.
- `TracePlatform` trait.
- `snapshot` and `drain`.
- Focused tests for record layout and ring buffer behavior.

Defer:

- Per-CPU ring buffers.
- Lock-free concurrent writers.
- Allocator support.
- File system sinks.
- Perfetto protobuf output.
- eBPF verifier, VM, or JIT.
- Strong binding to any tracepoint framework.
- Strongly consistent PID/TID indexes for the current trace window.

## Summary

`ostrace` should be the common trace record core for Rust OS kernels. The OS
injects time, identity, synchronization, and memory. Kernel modules or
tracepoint hooks append structured events. `ostrace` writes compact binary
records into a ring buffer, then the OS or host-side tools drain and export them
to ftrace, Chrome Trace, Perfetto, or other formats.
