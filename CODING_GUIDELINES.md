# ostrace Coding Guidelines

## Rustdoc Standard

All public Rust APIs must use `///` rustdoc comments. Comments should describe
the API contract, not repeat the implementation.

The crate enforces this with:

```rust
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::bare_urls)]
```

Code that adds undocumented public APIs, broken rustdoc links, or bare URLs in
rustdoc must not compile.

Public structs, enums, traits, and public fields must document:

- What the item represents.
- Ownership and lifetime requirements when relevant.
- Units for numeric values.
- Whether values are caller-provided, session-local, or library-owned.

Public functions and trait methods must document these sections when applicable:

- Function behavior.
- `# Parameters`: every parameter and its meaning.
- `# Returns`: successful result and every meaningful error/status variant.
- `# Panics`: when the function can panic. Use "This function does not panic."
  when no panic is expected.
- `# Side Effects`: externally visible state changes, buffer mutations, counters,
  session flags, I/O, or none.

Prefer concise wording. Do not document private helper functions unless their
behavior is subtle enough that future maintainers need the context.

## Example

```rust
/// Starts a trace session over caller-provided per-CPU buffers.
///
/// # Parameters
///
/// - `config`: Session-local state, per-CPU buffers, and buffer mode.
///
/// # Returns
///
/// Returns an active [`TraceSession`] on success.
/// Returns [`TraceError::SessionAlreadyActive`] if another session is active.
///
/// # Panics
///
/// This function does not panic.
///
/// # Side Effects
///
/// Sets the global active-session flag, clears session CPU state, and zeroes the
/// supplied per-CPU buffers.
```
