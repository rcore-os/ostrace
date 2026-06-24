# ostrace 设计说明

`ostrace` 是一个面向 Rust OS kernel 的 `no_std` trace record 核心。它的目标是在 OS 特定 tracing 框架之下提供 record 写入层，角色类似 Linux ftrace 的底层部分，但保持独立于任何特定 tracepoint 框架、allocator、文件系统、串口驱动、QEMU 工作流或 eBPF runtime。

当前实现聚焦于一条较小但可运行的 tracing 路径：

```text
sched/task event -> compact binary record -> per-CPU ring buffer
```

对于 OS 集成，推荐的存储模型是由调用者拥有的一块 trace image RAM 区域：

```text
TraceImageHeader
CpuDescriptor[cpu_count]
PerCpuRingBuffer[cpu_count]
```

image 是一个 live in-memory container。`ostrace` 会初始化它、向其中追加 record、原地更新 cursor/drop 状态，并且后续可以从 RAM dump 中解析它。库本身不会在 kernel 路径上分配内存。

## 文档同步

`DESIGN.md` 和 `DESIGN_CN.md` 分别用英文和中文描述同一份设计。只要项目设计、public API、trace image layout、record format、exporter 行为或开发策略发生变化，两份文件必须在同一次修改中同步更新。如果某一处有意不一致，原因必须同时记录在两份文件中。

## 当前范围

当前已经实现：

- `#![no_std]` library core。
- 严格的 public API rustdoc lint。
- 只使用调用者提供的内存；trace 热路径上没有库自有 heap allocation。
- 推荐的 single-RAM-region trace image API。
- 用于测试和实验的旧 split-storage session API。
- Per-CPU ring buffer。
- 带 `per_cpu_seq` 的 per-CPU append 顺序。
- Per-CPU reentrant append 检测。
- `Overwrite` 和 `StopOnFull` buffer mode。
- `sched_switch` 风格的 context switch record。
- 同步 `tracing_mark_write` begin/end record。
- 对 live image 或完整 RAM dump 的 snapshot 解析。
- `std` feature 下的 bytrace exporter。
- `trace2bytrace` host tool。

尚未实现：

- 通用 `instant`、`counter`、async span 或结构化 metadata event。
- `task_new`/`task_exit` 等 task lifecycle metadata。
- Trace mark custom metadata 或 `custom_args` 输出。
- 全局 cross-CPU ring buffer。
- Lock-free multi-writer 同步。
- Perfetto protobuf 或 Chrome Trace JSON export。
- eBPF verifier、VM 或 JIT。

## 目标

- 支持 `#![no_std]`。
- 可用于教学 OS、实验性 OS 和偏生产向的 Rust kernel。
- 可在 allocator 或文件系统存在之前工作。
- 将调用者提供的内存作为固定 trace storage 使用。
- 热路径只追加紧凑的二进制 record，保持开销较低。
- 当前支持 host-side 转换到 SmartPerf/bytrace，后续支持 ftrace text、Chrome Trace JSON、Perfetto 以及相关格式。
- 避免强制依赖文件系统、块设备、串口、QEMU、eBPF 或某个特定 tracepoint 机制。

## 非目标

- 不实现 eBPF verifier。
- 不实现 eBPF bytecode VM 或 JIT。
- 不在 hook 点执行用户程序。
- 不把 core record format 直接绑定到 Linux ftrace ABI。
- 不在 kernel 热路径生成文本格式。
- 不要求 OS 已经具备文件系统或 allocator。

## 源码布局

当前 MVP 将核心实现保存在一个 library 文件中：

```text
src/
  lib.rs
  bin/
    trace2bytrace.rs
```

随着项目增长，实现应按如下边界拆分：

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

## 核心存储模型

推荐的运行时集成方式是 trace image API。OS 提供一个 mutable byte slice，`ostrace` 在其中格式化 image：

```rust
pub struct TraceImageConfig<'a, P> {
    pub bytes: &'a mut [u8],
    pub cpu_count: usize,
    pub per_cpu_buffer_size: usize,
    pub mode: BufferMode,
    pub platform: P,
}
```

`bytes` 是这条路径需要的唯一持久存储。编码 record 时会使用少量栈上缓冲区，但调用者不需要为 trace image operation 提供独立的 global state、session state 或 per-CPU buffer 数组。

旧的 split-storage API 仍然保留：

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

这套 API 适合聚焦测试和实验。新的 OS 集成应优先使用 `TraceImage`，因为 host tool 可以直接从 RAM 中解析它，而不需要重建隐藏的 Rust 状态。

## Trace Image Layout

当前格式中所有整数字段都是 little-endian。

Header 常量：

```text
TRACE_IMAGE_MAGIC = 0x0045_4341_5254_534f
container version = 1.0
record format version = 1.0
header size = 96 bytes
CPU descriptor size = 80 bytes
record header size = 16 bytes
record alignment = 8 bytes
```

当前 header v1 prefix 存储：

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

每个 CPU descriptor v1 prefix 当前存储：

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

image 不是 finish 时才生成的 artifact。追加 record 会修改选中的 per-CPU ring buffer，并把更新后的 CPU descriptor 写回同一块 RAM 区域。因此，即使在 clean shutdown 之前抓取 RAM dump，也仍然可以导出已经提交的 record。

`TraceImage::finish()` 和 `Drop` 会清除 image active flag。已有 record 和 descriptor 保持可解析。

## 兼容性规则

parser 会在 export 前校验 image：

- magic 错误表示这块内存不是 `ostrace` image。
- 不支持的 endian 会被拒绝。
- container major version 不匹配会被拒绝。
- record major version 不匹配会被拒绝。
- 非预期的 record header size 或 alignment 会被拒绝。
- header 和 CPU descriptor size 小于 v1 prefix 会被拒绝。
- 当已知的 v1 prefix 存在时，允许更大的 header 或 descriptor size，因此新版本可以追加字段。
- 指向 declared image 之外的 CPU descriptor 会被拒绝。
- 当前 exporter 会跳过未知或 malformed record payload。

container version 和 record format version 有意分开。container layout 变化不一定意味着已有 record payload 发生变化，反之亦然。

## Platform Hooks

OS 通过 `TracePlatform` 注入 timestamp 和 CPU id：

```rust
pub trait TracePlatform {
    fn now_ns(&self) -> u64;
    fn cpu_id(&self) -> u32;
}
```

`now_ns()` 返回写入 record header 的 timestamp。`cpu_id()` 选择 per-CPU ring buffer。如果 CPU id 超出配置的 CPU 数量，append 会返回 `AppendStatus::Dropped(DropReason::InvalidCpu)`。

当前代码没有定义 synchronization trait。same-CPU reentrancy 通过 `writer_active` 检测；reentrant append 会被丢弃并计数，而不是破坏正在写入的 record。OS 集成仍然需要负责选择合适的外层同步策略，例如禁止抢占、使用 per-CPU guard，或者在 append 调用期间防止迁移。

## Record Format

每条二进制 record 都以 16 字节 header 开始：

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

Header 字段编码如下：

```text
offset  size  field
0       2     total aligned record length
2       1     record kind
3       1     flags, currently zero
4       4     event_id
8       8     timestamp in nanoseconds
```

record 按 8 字节对齐：

```text
[RecordHeader][payload][zero padding]
```

如果 per-CPU buffer 末尾剩余字节无法容纳下一条 record，writer 会在可能的情况下写入一条 `Padding` record，wrap 到 offset zero，然后在那里写入 event record。

当前 record kind 和 event ID：

```text
RecordKind::ContextSwitch = 6, event_id = 1
RecordKind::DurationBegin = 2, event_id = 2
RecordKind::DurationEnd   = 3, event_id = 3
RecordKind::Padding       = 255
```

payload 当前使用如下编码：

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

对 context switch 来说，总 record size 是：

```text
align8(16 + 42 + prev.comm.len + next.comm.len)
```

这里 42 字节 payload base 由 `cpu_id + seq + prev task fixed fields + prev_comm_len + prev_state + next task fixed fields + next_comm_len` 组成。

## Append 语义

每个 CPU 都有独立 ring buffer 和 cursor state：

```text
write_pos
read_pos
used
next_per_cpu_seq
dropped
reentrant_dropped
writer_active
```

append 行为：

- writer 通过 `platform.cpu_id()` 获取目标 CPU。
- 超出 `cpu_count` 的 CPU id 返回 `Dropped(InvalidCpu)`。
- same-CPU reentrant append 返回 `Dropped(Reentrant)` 并更新 drop counter。
- payload encoding 使用固定的 512 字节栈上 scratch buffer；更大的 payload 返回 `Dropped(RecordTooLarge)`。
- 在 `StopOnFull` 模式下，空间不足返回 `Dropped(BufferFull)`。
- 在 `Overwrite` 模式下，writer 会从该 CPU 丢弃最旧 record，直到新 record 可以放入。
- per-CPU sequence number 在 payload encoding 前分配，并用于稳定 export 排序。

write 层只保证 per-CPU 顺序。跨 CPU 排序在 snapshot/export 代码解码所有 CPU stream 后完成。

## Event API

当前 public write API 有意保持很窄：

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

`TaskRef` 使用中性的 scheduler 术语：

```rust
pub struct TaskRef<'a> {
    pub comm: &'a str,
    pub tid: u32,
    pub tgid: u32,
    pub prio: i32,
}
```

API 显式接收 task ID，而不是要求 platform 提供 `current_pid`，因为 scheduler event 可能涉及不止当前 task。例如，一个正在运行的 task 可能 wake 另一个来自不同 process 的 task。

`OffCpuState` 描述 previous task 离开 CPU 后进入的状态。exporter 会将其映射到目标格式特定的写法，例如 ftrace/bytrace 的 `R`、`S`、`D`、`T` 等。next task 隐含为 context switch 选择运行的 task。

未来的通用 event API 应构建在同一个二进制 record core 之上，而不是替换它：

```rust
trace::instant!("sched", "wakeup", tid = next_tid);
trace::begin!("syscall", "read", fd = fd);
trace::end!("syscall", "read");
trace::counter!("mm", "free_pages", value = free_pages);
```

## PID/TID 身份

当前 MVP 在每个 `TaskRef` 中直接记录 OS 可见的 `tid` 和 `tgid`。这对于早期 bytrace 转换和 SmartPerf 查看已经足够。

长期来看，API 不应假设 OS 可见的 PID/TID 值全局唯一。后续 metadata 层应区分 raw ID 和生命周期唯一的 trace ID：

```text
raw_pid/raw_tid:
  OS-visible IDs, possibly reused

trace_pid/trace_tid:
  lifecycle-unique IDs used to bind UI tracks
```

这层未来能力推荐的 OS 集成方式是在进程或线程创建时分配单调递增的 `trace_pid` 和 `trace_tid`，并将它们保存在 PCB/TCB 中。metadata record 会将 trace ID 映射回 raw ID 和显示名称。

## Snapshot 和 Export

热路径只写入二进制 record。export 通过只读 snapshot 完成：

```rust
TraceImage::snapshot() -> TraceImageSnapshot<'_>
TraceImageSnapshot::parse(bytes) -> Result<TraceImageSnapshot<'_>, TraceImageError>
TraceImageSnapshot::parse_from_ram(ram, ram_base, trace_base)
TraceSession::snapshot() -> TraceSnapshot<'_>
```

`TraceImageSnapshot::parse_from_ram` 面向拿到完整 RAM dump 的 host tool。`ram_base` 是 `ram[0]` 对应的物理地址，`trace_base` 是 trace image 的起始物理地址。

在 `std` feature 下，bytrace exporter 会写出 SmartPerf-compatible text：

```rust
export::bytrace::write_image_bytrace(snapshot, writer)
export::bytrace::write_bytrace(snapshot, writer)
```

exporter 会：

- 从每个 CPU stream 解码 supported record；
- 忽略 malformed 或 unsupported record；
- 按 `(timestamp, cpu_id, per_cpu_seq)` 排序；
- 输出 bytrace text header；
- 将 context switch 映射为 `sched_switch`；
- 将 trace mark begin/end 映射为 `tracing_mark_write: B|tgid|name` 和 `tracing_mark_write: E|tgid`。

`trace2bytrace` binary 可在 `std` feature 下使用：

```text
cargo run --features std --bin trace2bytrace -- \
  --ram path/to/ram.bin \
  --ram-base 0x80000000 \
  --trace-base 0x87b00000 \
  --out path/to/trace.bytrace
```

## bytrace 和 SmartPerf 说明

当前输出面向早期 SmartPerf 查看所需的子集：

```text
sched_switch: prev_comm=... prev_pid=... prev_prio=... prev_state=... ==> next_comm=... next_pid=... next_prio=...
tracing_mark_write: B|tgid|name
tracing_mark_write: E|tgid
```

SmartPerf 可以显示 `tracing_mark_write` duration block。额外的 click-time metadata 后续应该编码在 `B` 行上，因为 `E` 行只负责闭合同步 slice。未来 API 可以增加 trace mark metadata，并导出类似：

```text
B|tgid|name|Ktag|key=value,key2=value2
```

该扩展当前尚未实现。

## 开发策略

所有 public Rust API 都必须有 rustdoc。crate 强制启用：

```rust
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::bare_urls)]
```

详细注释要求记录在 `CODING_GUIDELINES.md` 中。

## 总结

`ostrace` 当前为 Rust OS kernel 提供了一条 minimal but end-to-end trace path：OS 提供一块 RAM 区域、platform timestamp/CPU hooks 和 task reference；`ostrace` 将 per-CPU binary record 写入 self-describing trace image；host-side 代码解析 image，并将 supported record 转换为 SmartPerf-compatible bytrace text。
