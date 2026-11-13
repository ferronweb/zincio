//! Unix domain socket stream types for async I/O.
//!
//! This module provides:
//! - [`UnixStream`]: An async Unix domain socket stream that can use either completion-based or poll-based I/O.
//! - [`PollUnixStream`]: A poll-only variant that always uses readiness-based operations.
//!
//! # Implementation details
//!
//! - Unix domain sockets use native async syscalls via the async driver when available.
//! - When `io_uring` completion is available, operations complete directly.
//! - For platforms without native async support, operations fall back to synchronous `std::os::unix::net` calls.
//! - The runtime must be active when calling these types' methods; otherwise they will panic.

use std::cell::RefCell;
use std::future::poll_fn;
use std::io;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixStream as StdUnixStream};
use std::path::Path;

use mio::Interest;

use crate::op::ConnectOp;
use crate::{driver::RegistrationMode, fd_inner::InnerRawHandle};

#[inline]
fn socket_addr_to_raw(path: &Path) -> Result<(libc::sockaddr_un, libc::socklen_t), io::Error> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty socket path",
        ));
    }
    if bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path contains interior NUL byte",
        ));
    }

    let mut sockaddr = unsafe { MaybeUninit::<libc::sockaddr_un>::zeroed().assume_init() };
    sockaddr.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "integer conversion out of range",
        )
    })?;

    let max_path_len = sockaddr.sun_path.len();
    if bytes.len() >= max_path_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is too long",
        ));
    }

    for (index, byte) in bytes.iter().copied().enumerate() {
        sockaddr.sun_path[index] = libc::c_char::try_from(byte).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "integer conversion out of range",
            )
        })?;
    }

    let addr_len =
        u32::try_from(std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "integer conversion out of range",
                )
            })?;
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "haiku",
        target_os = "aix",
    ))]
    {
        sockaddr.sun_len = addr_len as libc::sa_family_t;
    }

    Ok((sockaddr, addr_len))
}

#[inline]
fn new_socket(
    path: &Path,
) -> Result<(StdUnixStream, libc::sockaddr_un, libc::socklen_t), io::Error> {
    let (raw_addr, raw_addr_len) = socket_addr_to_raw(path)?;
    let socket_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if socket_fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { StdUnixStream::from_raw_fd(socket_fd) };
    Ok((stream, raw_addr, raw_addr_len))
}

/// An async Unix domain socket stream that can use either completion-based or poll-based I/O.
///
/// This is the async version of [`std::os::unix::net::UnixStream`].
///
/// # Implementation details
///
/// - Unix domain sockets use native async syscalls via the async driver when available.
/// - When `io_uring` completion is available, operations complete directly.
/// - For platforms without native async support, operations fall back to synchronous `std::os::unix::net` calls.
/// - The runtime must be active when calling these methods; otherwise they will panic.
///
/// # Examples
///
/// ```ignore
/// use zincio::net::UnixStream;
///
/// let mut stream = UnixStream::connect("/tmp/mysocket").await?;
/// stream.write(b"hello").await.0?;
/// let mut buf = [0u8; 1024];
/// let (read, buf) = stream.read(buf).await;
/// let read = read?;
/// ```
pub struct UnixStream {
    inner: StdUnixStream,
    handle: ManuallyDrop<InnerRawHandle>,
}

/// A poll-only variant that always uses readiness-based operations.
///
/// This type is useful when you want to ensure readiness-based I/O is used,
/// for example when integrating with other readiness-based systems.
///
/// # Implementation details
///
/// - Always uses readiness-based I/O via `mio`.
/// - Can be converted to [`UnixStream`] with adaptive or completion mode.
pub struct PollUnixStream {
    stream: UnixStream,
    read_ready: RefCell<bool>,
    write_ready: RefCell<bool>,
}

impl UnixStream {
    /// Connects to the specified Unix domain socket path.
    ///
    /// This is the async version of [`std::os::unix::net::UnixStream::connect`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The path does not exist
    /// - The path is too long
    /// - Connection refused
    /// - The runtime is not active
    #[inline]
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        let (inner, raw_addr, raw_addr_len) = new_socket(path.as_ref())?;
        let stream = Self::from_std(inner)?;

        let raw_addr_ptr = (&raw const raw_addr).cast::<libc::sockaddr>();
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

    /// Creates a new `UnixStream` from a standard library `UnixStream`.
    ///
    /// # Errors
    ///
    /// This function will return an error if registration with the async driver fails.
    #[inline]
    pub fn from_std(inner: StdUnixStream) -> Result<Self, io::Error> {
        Self::from_std_with_mode(inner, RegistrationMode::Completion)
    }

    /// Creates a new `UnixStream` from a standard library `UnixStream` with a specific registration mode.
    #[inline]
    pub(crate) fn from_std_with_mode(
        inner: StdUnixStream,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        let handle = ManuallyDrop::new(InnerRawHandle::new_with_mode(
            inner.as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?);
        inner.set_nonblocking(!handle.uses_completion())?;
        Ok(Self { inner, handle })
    }

    /// Converts this stream into a poll-only variant.
    ///
    /// The returned `PollUnixStream` will always use readiness-based I/O.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn into_poll(self) -> Result<PollUnixStream, io::Error> {
        let mut stream = self;
        stream.handle.rebind_mode(RegistrationMode::Poll)?;
        stream
            .inner
            .set_nonblocking(!stream.handle.uses_completion())?;
        Ok(PollUnixStream {
            stream,
            read_ready: RefCell::new(false),
            write_ready: RefCell::new(false),
        })
    }

    /// Creates a new `UnixStream` that is ready to use with readiness-based I/O.
    ///
    /// This is useful when you want to create a Unix stream that uses poll-based I/O.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn from_std_poll(inner: StdUnixStream) -> Result<PollUnixStream, io::Error> {
        let handle = ManuallyDrop::new(InnerRawHandle::new(
            inner.as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
        )?);
        inner.set_nonblocking(!handle.uses_completion())?;
        Ok(PollUnixStream {
            stream: Self { inner, handle },
            read_ready: RefCell::new(false),
            write_ready: RefCell::new(false),
        })
    }
}

impl PollUnixStream {
    /// Connects to the specified Unix domain socket path using poll-based I/O.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        let (inner, raw_addr, raw_addr_len) = new_socket(path.as_ref())?;
        let stream = Self::from_std(inner)?;

        let raw_addr_ptr = (&raw const raw_addr).cast::<libc::sockaddr>();
        let handle = &stream.stream.handle;
        let mut op = ConnectOp::new(handle, raw_addr_ptr, raw_addr_len);
        poll_fn(move |cx| handle.poll_op(cx, &mut op)).await?;

        Ok(stream)
    }

    /// Creates a new `PollUnixStream` from a standard library `UnixStream`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn from_std(inner: StdUnixStream) -> Result<Self, io::Error> {
        Ok(Self {
            stream: UnixStream::from_std_with_mode(inner, RegistrationMode::Poll)?,
            read_ready: RefCell::new(false),
            write_ready: RefCell::new(false),
        })
    }

    /// Converts this poll stream into an adaptive `UnixStream`.
    #[inline]
    pub fn into_adaptive(self) -> UnixStream {
        self.stream
    }

    /// Converts this poll stream into a completion-based `UnixStream`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn into_completion(self) -> Result<UnixStream, io::Error> {
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

    /// Shuts down the connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn shutdown(&self, how: Shutdown) -> Result<(), io::Error> {
        self.stream.shutdown(how)
    }
}

impl_handle_traits!(UnixStream);
impl_handle_io_traits!(UnixStream);
impl_poll_handle_traits!(PollUnixStream, stream);
impl_poll_handle_io_traits!(PollUnixStream);
