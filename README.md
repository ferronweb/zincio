# zincio

A high-performance, cross-platform asynchronous runtime for Rust.

`zincio` provides an efficient I/O event loop that leverages the best available driver for each operating system:

- **Linux** - uses `io_uring` for true asynchronous I/O.
- **Windows** - uses I/O Completion Ports (IOCP) for scalable I/O.
- **macOS / BSD / Others** - uses `kqueue` or `epoll` via `mio` for event notification.

## Core features

- **Networking** - asynchronous TCP, UDP, and Unix Domain Sockets.
- **File system** - asynchronous file operations.
- **Timers** - efficient timer and sleep functionality.
- **Signals** - handling of OS signals.
- **Process management** - spawning and managing child processes.
- **Blocking tasks** - offload CPU-intensive or blocking operations to a thread pool.

## Concurrency model: thread-per-core

`zincio` is designed as a **single-threaded** runtime. To utilize multiple cores, you should employ a **thread-per-core** architecture, where a separate `Runtime` is pinned to each processor core. This approach minimizes synchronization overhead and maximizes cache locality.

Shared state can be communicated between runtimes using message passing (e.g., channels) or shared atomic structures, but I/O resources are typically owned by the thread that created them.

## But why not Tokio?

Tokio is a popular asynchronous runtime for Rust, but it uses a **work-stealing** model and may introduce additional synchronization overhead when using it as a thread-per-core runtime. `zincio` is more specialized for thread-per-core architectures that are optimized for low overhead and cache locality.

## Getting started

Add `zincio` to your `Cargo.toml`:

```toml
[dependencies]
zincio = "0.2"
```

### Example: TCP echo server

```rust
use zincio::RuntimeBuilder;
use zincio::net::TcpListener;

fn main() -> std::io::Result<()> {
    // 1. Build the runtime
    let runtime = RuntimeBuilder::new()
        .enable_timer(true)
        .build()?;

    // 2. Run the main future
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:8080")?;
        println!("Listening on 127.0.0.1:8080");

        loop {
            let (mut stream, _) = listener.accept().await?;

            zincio::spawn(async move {
                let (mut reader, mut writer) = zincio::io::split(stream);
                if let Err(e) = zincio::io::copy(&mut reader, &mut writer).await {
                    eprintln!("Echo failed: {}", e);
                }
            });
        }
    })
}
```

## Feature flags

The following features are available (most are enabled by default):

- `fs` - enables asynchronous file system operations.
- `time` - enables time and timer functionality.
- `signal` - enables signal handling.
- `process` - enables child process management.
- `pipe` - enables pipe support.
- `stdio` - enables standard I/O support.
- `splice` - enables splice support (Linux).
- `blocking-default` - enables the default blocking thread pool.

## Miri (unsafe checking)

`zincio` contains `unsafe` code in the I/O drivers (`io_uring`, `mio`, `IOCP`, `libc`).
[Miri](https://github.com/rust-lang/miri) can validate the core memory safety
without exercising kernel I/O. Only the `MockDriver` plus timer/executor
logic is fully Miri-compatible; real drivers need kernel syscalls that Miri
does not emulate.

Setup (nightly required):

```sh
rustup toolchain install nightly --component miri
cargo miri setup
```

Run the Miri-compatible subset (35 tests, 25 `#[cfg_attr(miri, ignore)]`):

```sh
MIRIFLAGS="-Zmiri-disable-isolation" cargo miri test -p zincio --lib --verbose
```

Notes:

- `-Zmiri-disable-isolation` is required for `TCP`/`mio` socket syscalls
  (`socket`, `epoll_wait`). Without it those tests fail with
  “`socket` not available when isolation is enabled”.
- `io_uring` (`io_uring_setup` syscall 425), `AF_UNIX`/`SOCK_DGRAM`,
  `fs` blocking-pool, `process`, `signal` (`sigemptyset`), and
  `spawn_blocking` (`rusty_pool` park) are unsupported and marked
  `#[cfg_attr(miri, ignore)]`. The suite still covers the timing wheel,
  executor, `MockDriver`, `MioDriver` (interrupt/wake), and TCP
  readiness paths — the parts that exercise the crate's `unsafe`.
- A CI job (`.github/workflows/miri.yml`) runs this command on `nightly`.

## License

[MIT](./LICENSE)
