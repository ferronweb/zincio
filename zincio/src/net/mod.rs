//! A networking module for `zincio`.
//!
//! This module provides async versions of common networking operations:
//! - TCP: [`TcpListener`], [`TcpStream`], [`PollTcpStream`]
//! - UDP: [`UdpSocket`]
//! - Unix domain sockets: [`UnixListener`], [`UnixStream`], [`PollUnixStream`]
//!
//! Implementation notes:
//! - On Linux with `io_uring` support, some operations use native async syscalls (e.g. `accept4`, `sendto`)
//!   via the async driver. When `io_uring` completion is available, operations complete directly.
//! - For platforms without native async support, operations either offload to a blocking thread pool
//!   or fall back to synchronous `std::net` calls.
//! - The runtime must be active when calling these functions; otherwise they will panic.
//!
//! # Examples
//!
//! ## TCP Server
//!
//! ```ignore
//! use zincio::net::TcpListener;
//!
//! let listener = TcpListener::bind("127.0.0.1:8080").await?;
//! loop {
//!     let (stream, addr) = listener.accept().await?;
//!     println!("Connection from: {}", addr);
//! }
//! ```
//!
//! ## UDP Client
//!
//! ```ignore
//! use zincio::net::UdpSocket;
//!
//! let socket = UdpSocket::bind("127.0.0.1:0").await?;
//! socket.connect("127.0.0.1:9000").await?;
//! socket.send(b"hello").await?;
//! ```
//!
//! ## Unix Domain Socket
//!
//! ```ignore
//! use zincio::net::UnixStream;
//!
//! let stream = UnixStream::connect("/tmp/mysocket").await?;
//! ```

mod tcp;
mod udp;
#[cfg(unix)]
mod unix;

pub use tcp::*;
pub use udp::*;
#[cfg(unix)]
pub use unix::*;

use std::net::SocketAddr;

#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_STORAGE,
};

#[cfg(unix)]
fn socket_addr_to_raw(
    address: SocketAddr,
) -> (libc::c_int, libc::sockaddr_storage, libc::socklen_t) {
    match address {
        SocketAddr::V4(address) => {
            let sockaddr = libc::sockaddr_in {
                sin_family: libc::sa_family_t::try_from(libc::AF_INET)
                    .expect("AF_INET fits in sa_family_t"),
                sin_port: address.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(address.ip().octets()),
                },
                sin_zero: [0; 8],
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "haiku",
                    target_os = "aix",
                    target_os = "hurd",
                ))]
                sin_len: 0,
            };

            let mut storage = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
            unsafe {
                storage
                    .as_mut_ptr()
                    .cast::<libc::sockaddr_in>()
                    .write(sockaddr);
                (
                    libc::AF_INET,
                    storage.assume_init(),
                    u32::try_from(std::mem::size_of::<libc::sockaddr_in>())
                        .expect("sockaddr_in size fits in u32"),
                )
            }
        }
        SocketAddr::V6(address) => {
            let sockaddr = libc::sockaddr_in6 {
                sin6_family: libc::sa_family_t::try_from(libc::AF_INET6)
                    .expect("AF_INET6 fits in sa_family_t"),
                sin6_port: address.port().to_be(),
                sin6_flowinfo: address.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: address.ip().octets(),
                },
                sin6_scope_id: address.scope_id(),
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "haiku",
                    target_os = "aix",
                    target_os = "hurd",
                ))]
                sin6_len: 0,
            };

            let mut storage = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
            unsafe {
                storage
                    .as_mut_ptr()
                    .cast::<libc::sockaddr_in6>()
                    .write(sockaddr);
                (
                    libc::AF_INET6,
                    storage.assume_init(),
                    u32::try_from(std::mem::size_of::<libc::sockaddr_in6>())
                        .expect("sockaddr_in6 size fits in u32"),
                )
            }
        }
    }
}

#[cfg(windows)]
fn socket_addr_to_raw(address: SocketAddr) -> (i32, SOCKADDR_STORAGE, i32) {
    match address {
        SocketAddr::V4(address) => {
            let mut sockaddr = SOCKADDR_IN::default();
            sockaddr.sin_family = AF_INET;
            sockaddr.sin_port = address.port().to_be();
            sockaddr.sin_addr.S_un.S_addr = u32::from_ne_bytes(address.ip().octets());

            let mut storage = SOCKADDR_STORAGE::default();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (&raw const sockaddr).cast::<u8>(),
                    (&raw mut storage).cast::<u8>(),
                    std::mem::size_of::<SOCKADDR_IN>(),
                );
            }

            (
                i32::from(AF_INET),
                storage,
                std::mem::size_of::<SOCKADDR_IN>() as i32,
            )
        }
        SocketAddr::V6(address) => {
            let mut sockaddr = SOCKADDR_IN6::default();
            sockaddr.sin6_family = AF_INET6;
            sockaddr.sin6_port = address.port().to_be();
            sockaddr.sin6_flowinfo = address.flowinfo();
            sockaddr.sin6_addr.u.Byte = address.ip().octets();
            sockaddr.Anonymous.sin6_scope_id = address.scope_id();

            let mut storage = SOCKADDR_STORAGE::default();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (&raw const sockaddr).cast::<u8>(),
                    (&raw mut storage).cast::<u8>(),
                    std::mem::size_of::<SOCKADDR_IN6>(),
                );
            }

            (
                i32::from(AF_INET6),
                storage,
                std::mem::size_of::<SOCKADDR_IN6>() as i32,
            )
        }
    }
}
