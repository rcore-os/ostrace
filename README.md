# ostrace
ostrace is a no_std tracing core for Rust OS kernels. It writes compact per-CPU binary trace records into caller-provided RAM and supports host-side conversion to SmartPerf/bytrace for early scheduler and trace marker analysis.
