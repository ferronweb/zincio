//! TCP stream types for async I/O.
//!
//! This module provides:
//! - [`TcpStream`]: An async TCP stream that can use either completion-based or poll-based I/O.
//! - [`PollTcpStream`]: A poll-only variant that always uses readiness-based operations.
//!
//! # Implementation details
//!
//! - On Linux with `io_uring` support, TCP operations use native async syscalls via the async driver.
//! - When `io_uring` completion is available, operations complete directly.
//! - For platforms without native async support, operations fall back to synchronous `std::net` calls.
//! - The runtime must be active when calling these types' methods; otherwise they will panic.

use std::cell::RefCell;
use std::future::poll_fn;
use std::io;
use std::mem::ManuallyDrop;
use std::net::{Shutdown, SocketAddr, ToSocketAddrs};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket};

use mio::Interest;

#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{
    self, SOCKADDR, SOCKADDR_STORAGE, SOCK_STREAM, WSADATA,
};

use crate::driver::RegistrationMode;
use crate::io::{IoBufMut, IoBufTemporaryPoll};
use crate::net::socket_addr_to_raw;
use crate::op::{ConnectOp, RecvOp};

#[cfg(unix)]
fn new_socket(
    address: SocketAddr,
) -> Result<(std::net::TcpStream, libc::sockaddr_storage, libc::socklen_t), io::Error> {
    let (domain, raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let socket_fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
    if socket_fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let stream =
        unsafe { std::net::TcpStream::from_raw_fd(std::os::fd::IntoRawFd::into_raw_fd(socket_fd)) };
    Ok((stream, raw_addr, raw_addr_len))
}

#[cfg(windows)]
fn new_socket(
    address: SocketAddr,
) -> Result<(std::net::TcpStream, SOCKADDR_STORAGE, i32), io::Error> {
    // 0x202 = MAKEWORD(2, 2)
    let mut wsadata = WSADATA::default();
    if unsafe { WinSock::WSAStartup(0x202, &raw mut wsadata) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (domain, raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let socket = unsafe { WinSock::socket(domain, SOCK_STREAM, 0) };
    if socket == WinSock::INVALID_SOCKET {
        let err = io::Error::last_os_error();
        let _ = unsafe { WinSock::WSACleanup() };
        return Err(err);
    }
    let stream = unsafe { std::net::TcpStream::from_raw_socket(socket as _) };
    Ok((stream, raw_addr, raw_addr_len))
}

/// An async TCP stream that can use either completion-based or poll-based I/O.
///
/// This is the async version of [`std::net::TcpStream`].
///
/// # Implementation details
///
/// - On Linux with `io_uring` support, TCP operations use native async syscalls via the async driver.
/// - When `io_uring` completion is available, operations complete directly.
/// - For platforms without native async support, operations fall back to synchronous `std::net` calls.
/// - The runtime must be active when calling these methods; otherwise they will panic.
///
/// # Examples
///
/// ```ignore
/// use zincio::net::TcpStream;
///
/// let mut stream = TcpStream::connect("127.0.0.1:8080").await?;
/// stream.write(b"hello").await.0?;
/// let mut buf = [0u8; 1024];
/// let (read, buf) = stream.read(buf).await;
/// let read = read?;
/// ```
pub struct TcpStream {
    inner: std::net::TcpStream,
    handle: ManuallyDrop<crate::fd_inner::InnerRawHandle>,
}

/// A poll-only variant that always uses readiness-based operations.
///
/// This type is useful when you want to ensure readiness-based I/O is used,
/// for example when integrating with other readiness-based systems.
///
/// # Implementation details
///
/// - Always uses readiness-based I/O via `mio`.
/// - Can be converted to [`TcpStream`] with adaptive or completion mode.
pub struct PollTcpStream {
    stream: TcpStream,
    write_ready: RefCell<bool>,
    read_ready: RefCell<bool>,
}

impl TcpStream {
    /// Connects to the specified address.
    ///
    /// This is the async version of [`std::net::TcpStream::connect`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - DNS resolution fails
    /// - Connection refused
    /// - Network unreachable
    /// - The runtime is not active
    #[inline]
    pub async fn connect(address: impl ToSocketAddrs) -> Result<Self, io::Error> {
        let addresses = address.to_socket_addrs()?;
        let mut last_error = None;
        for address in addresses {
            match Self::connect_one(address).await {
                Ok(stream) => return Ok(stream),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses")))
    }

    #[inline]
    async fn connect_one(address: SocketAddr) -> Result<Self, io::Error> {
        let (inner, raw_addr, raw_addr_len) = new_socket(address)?;
        let stream = Self::from_std(inner)?;

        #[cfg(unix)]
        let raw_addr_ptr = (&raw const raw_addr).cast::<libc::sockaddr>();
        #[cfg(windows)]
        let raw_addr_ptr = (&raw const raw_addr).cast::<SOCKADDR>();
        let handle = &stream.handle;
        let mut op = ConnectOp::new(handle, raw_addr_ptr, raw_addr_len);
        poll_fn(move |cx| handle.poll_op(cx, &mut op)).await?;

        Ok(stream)
    }

    /// Returns the local address of this connection.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.local_addr()
    }

    /// Returns the remote address of this connection.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.peer_addr()
    }

    /// Returns the current state of the `TCP_NODELAY` option for this socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub fn nodelay(&self) -> Result<bool, io::Error> {
        self.inner.nodelay()
    }

    /// Sets the value of the `TCP_NODELAY` option for this socket.
    ///
    /// When set, this disables the Nagle algorithm, which means that small
    /// packets are sent immediately rather than being buffered.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub fn set_nodelay(&self, nodelay: bool) -> Result<(), io::Error> {
        self.inner.set_nodelay(nodelay)
    }

    /// Shuts down the connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn shutdown(&self, how: Shutdown) -> Result<(), io::Error> {
        match self.inner.shutdown(how) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotConnected => Ok(()), // macOS-specific behavior
            Err(e) => Err(e),
        }
    }

    /// Peeks at data from the socket without removing it from the buffer.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub async fn peek<B: IoBufMut>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvOp::new_peek(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    /// Creates a new `TcpStream` from a standard library `TcpStream`.
    ///
    /// # Errors
    ///
    /// This function will return an error if registration with the async driver fails.
    #[inline]
    pub fn from_std(inner: std::net::TcpStream) -> Result<Self, io::Error> {
        Self::from_std_with_mode(inner, RegistrationMode::Completion)
    }

    /// Creates a new `TcpStream` from a standard library `TcpStream` with a specific registration mode.
    #[inline]
    pub(crate) fn from_std_with_mode(
        inner: std::net::TcpStream,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        #[cfg(unix)]
        let handle = ManuallyDrop::new(crate::fd_inner::InnerRawHandle::new_with_mode(
            inner.as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?);
        #[cfg(windows)]
        let handle = ManuallyDrop::new(crate::fd_inner::InnerRawHandle::new_with_mode(
            crate::fd_inner::RawOsHandle::Socket(inner.as_raw_socket()),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?);
        inner.set_nonblocking(!handle.uses_completion())?;
        Ok(Self { inner, handle })
    }

    /// Converts this stream into a poll-only variant.
    ///
    /// The returned `PollTcpStream` will always use readiness-based I/O.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn into_poll(self) -> Result<PollTcpStream, io::Error> {
        let mut stream = self;
        stream.handle.rebind_mode(RegistrationMode::Poll)?;
        stream
            .inner
            .set_nonblocking(!stream.handle.uses_completion())?;
        Ok(PollTcpStream {
            stream,
            write_ready: RefCell::new(false),
            read_ready: RefCell::new(false),
        })
    }
}

impl PollTcpStream {
    /// Connects to the specified address using poll-based I/O.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub async fn connect(address: impl ToSocketAddrs) -> Result<Self, io::Error> {
        let addresses = address.to_socket_addrs()?;
        let mut last_error = None;
        for address in addresses {
            match Self::connect_one(address).await {
                Ok(stream) => return Ok(stream),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses")))
    }

    #[inline]
    async fn connect_one(address: SocketAddr) -> Result<Self, io::Error> {
        let (inner, raw_addr, raw_addr_len) = new_socket(address)?;
        let stream = Self::from_std(inner)?;

        #[cfg(unix)]
        let raw_addr_ptr = (&raw const raw_addr).cast::<libc::sockaddr>();
        #[cfg(windows)]
        let raw_addr_ptr = (&raw const raw_addr).cast::<SOCKADDR>();

        let handle = &stream.stream.handle;
        let mut op = ConnectOp::new(handle, raw_addr_ptr, raw_addr_len);
        poll_fn(move |cx| handle.poll_op(cx, &mut op)).await?;

        Ok(stream)
    }

    /// Creates a new `PollTcpStream` from a standard library `TcpStream`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn from_std(inner: std::net::TcpStream) -> Result<Self, io::Error> {
        Ok(Self {
            stream: TcpStream::from_std_with_mode(inner, RegistrationMode::Poll)?,
            write_ready: RefCell::new(false),
            read_ready: RefCell::new(false),
        })
    }

    /// Converts this poll stream into an adaptive `TcpStream`.
    #[inline]
    pub fn into_adaptive(self) -> TcpStream {
        self.stream
    }

    /// Converts this poll stream into a completion-based `TcpStream`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn into_completion(self) -> Result<TcpStream, io::Error> {
        let mut stream = self.stream;
        stream.handle.rebind_mode(RegistrationMode::Completion)?;
        stream
            .inner
            .set_nonblocking(!stream.handle.uses_completion())?;
        Ok(stream)
    }

    /// Returns the local address of this connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.stream.local_addr()
    }

    /// Returns the remote address of this connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.stream.peer_addr()
    }

    /// Returns the current state of the `TCP_NODELAY` option for this socket.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn nodelay(&self) -> Result<bool, io::Error> {
        self.stream.nodelay()
    }

    /// Sets the value of the `TCP_NODELAY` option for this socket.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn set_nodelay(&self, nodelay: bool) -> Result<(), io::Error> {
        self.stream.set_nodelay(nodelay)
    }

    /// Shuts down the connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn shutdown(&self, how: Shutdown) -> Result<(), io::Error> {
        self.stream.shutdown(how)
    }

    /// Peeks at data from the socket without removing it from the buffer.
    ///
    /// This method uses readiness-based I/O and is compatible with `tokio::io`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub async fn peek(&self, buf: &mut [u8]) -> Result<usize, io::Error> {
        let handle = &self.stream.handle;
        let buf = unsafe { IoBufTemporaryPoll::new(buf.as_mut_ptr(), buf.len()) };
        let mut op = RecvOp::new_peek(handle, buf);
        poll_fn(move |cx| handle.poll_op_poll(cx, &mut op)).await
    }
}

impl_handle_traits!(TcpStream);
impl_handle_windows_traits!(TcpStream);
impl_handle_io_traits!(TcpStream);
impl_poll_handle_traits!(PollTcpStream, stream);
impl_poll_handle_windows_traits!(PollTcpStream, stream);
impl_poll_handle_io_traits!(PollTcpStream);
