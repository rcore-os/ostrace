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

#[test]
fn trace_image_writes_records_into_single_ram_region() {
    let mut image_bytes = [0u8; 4096];
    let platform = TestPlatform::new(&[0, 1], &[10, 20]);
    {
        let mut image = TraceImage::init(TraceImageConfig {
            bytes: &mut image_bytes,
            cpu_count: 2,
            per_cpu_buffer_size: 512,
            mode: BufferMode::Overwrite,
            platform,
        })
        .unwrap();
        assert_eq!(
            image.context_switch(task("idle", 0, 0), OffCpuState::Running, task("A", 1, 1)),
            AppendStatus::Written
        );
        assert_eq!(
            image.trace_mark_begin(task("B", 2, 2), "work"),
            AppendStatus::Written
        );
        image.finish();
    }

    let snapshot = TraceImageSnapshot::parse(&image_bytes).unwrap();
    assert_eq!(snapshot.cpu_count(), 2);
    assert_eq!(snapshot.cpu_descriptor_size(), 80);
    let cpu0: Vec<_> = snapshot.cpu_stream(0).unwrap().records().collect();
    let cpu1: Vec<_> = snapshot.cpu_stream(1).unwrap().records().collect();
    assert_eq!(cpu0.len(), 1);
    assert_eq!(cpu1.len(), 1);
}

#[test]
fn trace_image_can_be_parsed_from_ram_dump() {
    const RAM_BASE: u64 = 0x8000_0000;
    const TRACE_BASE: u64 = 0x8000_0800;
    let mut ram = [0u8; 8192];
    let trace_offset = (TRACE_BASE - RAM_BASE) as usize;
    let platform = TestPlatform::new(&[0], &[10]);
    {
        let mut image = TraceImage::init(TraceImageConfig {
            bytes: &mut ram[trace_offset..],
            cpu_count: 1,
            per_cpu_buffer_size: 512,
            mode: BufferMode::Overwrite,
            platform,
        })
        .unwrap();
        assert_eq!(image.trace_mark_end(task("A", 1, 1)), AppendStatus::Written);
    }

    let snapshot = TraceImageSnapshot::parse_from_ram(&ram, RAM_BASE, TRACE_BASE).unwrap();
    assert_eq!(snapshot.cpu_count(), 1);
    let records: Vec<_> = snapshot.cpu_stream(0).unwrap().records().collect();
    assert_eq!(records.len(), 1);
    assert!(matches!(
        records[0].decode(),
        Some(DecodedRecord::TraceMarkEnd { .. })
    ));
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

#[cfg(feature = "std")]
#[test]
fn bytrace_export_accepts_trace_image_snapshot() {
    let mut image_bytes = [0u8; 4096];
    let platform = TestPlatform::new(&[0], &[10]);
    {
        let mut image = TraceImage::init(TraceImageConfig {
            bytes: &mut image_bytes,
            cpu_count: 1,
            per_cpu_buffer_size: 1024,
            mode: BufferMode::Overwrite,
            platform,
        })
        .unwrap();
        assert_eq!(
            image.context_switch(task("idle", 0, 0), OffCpuState::Running, task("A", 1, 1)),
            AppendStatus::Written
        );
    }

    let snapshot = TraceImageSnapshot::parse(&image_bytes).unwrap();
    let mut out = Vec::new();
    crate::export::bytrace::write_image_bytrace(snapshot, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("# entries-in-buffer/entries-written: 1/1"));
    assert!(text.contains("sched_switch: prev_comm=idle prev_pid=0"));
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
