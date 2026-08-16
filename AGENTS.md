# Working in this repository

## Layout

```
crates/httpsys/    library: async server over the Windows http.sys API
tools/httpbench/   harness: both servers under test, load generator, charts
charts/            committed benchmark output, referenced by the README
```

Virtual workspace — there is no root package. Build with `cargo build
--workspace`. Windows only; every target here links `httpapi.dll`.

Inside `crates/httpsys`, the hot path runs
`server.rs` (request slots) → `queue.rs` (the four `Http*` calls) →
`overlapped.rs` (`Op`, the one place an operation is handed to the kernel) →
`iocp.rs` (completion threads). `buffer.rs` owns every pool. Anything that
claims to be a performance change almost certainly belongs in one of those.

## Commands

```ps
cargo fmt --all --check                  # rustfmt, edition 2024
cargo clippy --workspace --all-targets   # must be clean, warnings included
cargo test --workspace                   # unit tests, no http.sys needed
cargo build --release                    # required before any benchmarking

# Real http.sys, no elevation needed (uses the Temporary_Listen_Addresses ACL)
cargo test -p httpsys --test server --release -- --ignored --test-threads=1

# Loom model check of the completion handshake
$env:RUSTFLAGS = "--cfg loom"
cargo test -p httpsys --test loom --release -- --test-threads=1
Remove-Item Env:\RUSTFLAGS
```

## Benchmarking

Never quote numbers from a debug build; `bench` re-executes the harness as a
server subprocess, so `cargo build --release` must be current first.

Unelevated runs go through the URL reservation Windows grants to `\Everyone`:

```ps
.\target\release\httpbench.exe bench --ports 80,8081 `
    --path /Temporary_Listen_Addresses/bench/ --out charts
```

Run-to-run variance is roughly ±10% at short durations. Do not report a
difference under about 15% from a single run as a win — re-run it, or raise
`--duration`. The load generator shares the machine with the server; that is
accepted, and it costs both engines the same.

Any change to the comparison must keep the two servers doing identical work:
same path, same body handling, same response bytes, `TCP_NODELAY` on both.
Optimisations with no axum equivalent (the http.sys kernel response cache)
stay opt-in and out of the head-to-head.

## Conventions

`unsafe_op_in_unsafe_fn` is `deny` and `clippy::undocumented_unsafe_blocks` is
`warn` in `crates/httpsys`. Every `unsafe` block and every `unsafe impl` needs
a `// SAFETY:` comment stating the invariant that makes it sound, not what the
call does.

The invariant the whole crate rests on: an async operation transfers one `Arc`
strong reference to the kernel, and exactly one party reclaims it — a
completion thread, or the submitting task when the kernel completed inline or
refused the call. If you add an `Http*` call, route it through
`overlapped::Op` so that stays true, and remember that dropping the future has
to cancel and wait before the buffer can be reused.

Comments say why, not what. If a line of code explains itself, leave it alone.

## Things that will bite you

- `HttpAddUrlToUrlGroup` rejects any prefix that does not end in `/`.
  `init::normalize_prefix` appends one; do not remove that.
- `HttpReceiveHttpRequest` returning `ERROR_MORE_DATA` leaves the request
  *queued*. Reissue it with the id from the buffer, or the slot spins forever
  on the same request.
- Pass request id `0` for "next request". Passing a stale id from a previous
  request silently costs a syscall per request.
- `http.sys` has no `HTTP_VERB` for PATCH. It arrives through `pUnknownVerb`.
- `windows` 0.62: `HANDLE` wraps a pointer, so it is not `Send`. Wrap it before
  it crosses a thread boundary rather than making a future `!Send` by accident.
- Growing a `Vec` inside a body-read loop reallocates and re-copies everything
  received so far. Size from `Content-Length` up front.
- Receive buffers are one-per-slot on purpose. Sharing them across slots makes
  the adaptive resizing pointless, because a slot rarely gets its own back.
- Regenerating `charts/` also changes the numbers quoted in the README. Update
  both together or neither.
