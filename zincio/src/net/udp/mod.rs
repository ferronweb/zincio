//! UDP socket types for async I/O.
//!
//! This module provides:
//! - [`UdpSocket`]: An async UDP socket that can use either completion-based or poll-based I/O.
//!
//! # Implementation details
//!
//! - On Linux with `io_uring` support, UDP operations use native async syscalls via the async driver.
//! - When `io_uring` completion is available, operations complete directly.
//! - For platforms without native async support, operations fall back to synchronous `std::net` calls.
//! - The runtime must be active when calling these types' methods; otherwise they will panic.

use std::cell::RefCell;
use std::future::poll_fn;
use std::io;
use std::mem::ManuallyDrop;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket as StdUdpSocket};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use std::time::Duration;

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::SOCKADDR;

use crate::driver::RegistrationMode;
use crate::fd_inner::InnerRawHandle;
use crate::io::{IoBuf, IoBufMut};
use crate::net::socket_addr_to_raw;
use crate::op::{ConnectOp, RecvOp, RecvfromOp, SendOp, SendtoOp};

mod poll;
pub use poll::PollUdpSocket;

#[cfg(unix)]
#[inline]
async fn connect_one(handle: &InnerRawHandle, address: SocketAddr) -> Result<(), io::Error> {
    let (_, raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let raw_addr_ptr = (&raw const raw_addr).cast::<libc::sockaddr>();
    let mut op = ConnectOp::new(handle, raw_addr_ptr, raw_addr_len);
    poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
}

#[cfg(windows)]
#[inline]
async fn connect_one(
    handle: &mut InnerRawHandle,
    socket: &mut StdUdpSocket,
    address: SocketAddr,
) -> Result<(), io::Error> {
    // Since ConnectEx (used by `ConnectOp`) requires the socket to be connection-oriented,
    // we use poll-based I/O instead of completion-based I/O on Windows.
    let old_registration_mode = handle.mode();
    handle.rebind_mode(RegistrationMode::Poll)?;
    socket.set_nonblocking(true)?;
    let (_, raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let raw_addr_ptr = (&raw const raw_addr).cast::<SOCKADDR>();
    let mut op = ConnectOp::new(handle, raw_addr_ptr, raw_addr_len);
    let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
    drop(op);
    handle.rebind_mode(old_registration_mode)?;
    socket.set_nonblocking(!handle.uses_completion())?;
    result
}

/// An async UDP socket that can use either completion-based or poll-based I/O.
///
/// This is the async version of [`std::net::UdpSocket`].
///
/// # Implementation details
///
/// - On Linux with `io_uring` support, UDP operations use native async syscalls via the async driver.
/// - When `io_uring` completion is available, operations complete directly.
/// - For platforms without native async support, operations fall back to synchronous `std::net` calls.
/// - The runtime must be active when calling these methods; otherwise they will panic.
///
/// # Examples
///
/// ```ignore
/// use zincio::net::UdpSocket;
///
/// let socket = UdpSocket::bind("127.0.0.1:0").await?;
/// socket.connect("127.0.0.1:9000").await?;
/// socket.send(b"hello").await?;
/// ```
pub struct UdpSocket {
    pub(crate) inner: StdUdpSocket,
    pub(crate) handle: ManuallyDrop<InnerRawHandle>,
}

impl UdpSocket {
    /// Creates a new `UdpSocket` which will be bound to the specified address.
    ///
    /// This is the async version of [`std::net::UdpSocket::bind`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - DNS resolution fails
    /// - The address is already in use
    /// - The process lacks permissions to bind to the address
    /// - The runtime is not active
    #[inline]
    pub fn bind(address: impl ToSocketAddrs) -> Result<Self, io::Error> {
        let inner = StdUdpSocket::bind(address)?;
        Self::from_std(inner)
    }

    /// Creates a new `UdpSocket` from a standard library `UdpSocket`.
    ///
    /// # Errors
    ///
    /// This function will return an error if registration with the async driver fails.
    #[inline]
    pub fn from_std(inner: StdUdpSocket) -> Result<Self, io::Error> {
        Self::from_std_with_mode(inner, RegistrationMode::Completion)
    }

    /// Creates a new `UdpSocket` from a standard library `UdpSocket` with a specific registration mode.
    #[inline]
    pub(crate) fn from_std_with_mode(
        inner: StdUdpSocket,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        #[cfg(unix)]
        let handle = ManuallyDrop::new(InnerRawHandle::new_with_mode(
            inner.as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?);
        #[cfg(windows)]
        let handle = ManuallyDrop::new(InnerRawHandle::new_with_mode(
            crate::fd_inner::RawOsHandle::Socket(inner.as_raw_socket()),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?);

        inner.set_nonblocking(!handle.uses_completion())?;
        Ok(Self { inner, handle })
    }

    /// Converts this socket into a poll-only variant.
    ///
    /// The returned `PollUdpSocket` will always use readiness-based I/O.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O operation fails.
    #[inline]
    pub fn into_poll(self) -> Result<PollUdpSocket, io::Error> {
        let mut socket = self;
        socket.handle.rebind_mode(RegistrationMode::Poll)?;
        socket
            .inner
            .set_nonblocking(!socket.handle.uses_completion())?;
        Ok(PollUdpSocket {
            socket,
            read_ready: RefCell::new(false),
            // Writable until the kernel says otherwise, as a new socket is.
            write_ready: RefCell::new(true),
        })
    }

    /// Converts this `UdpSocket` into the standard library `UdpSocket`.
    #[inline]
    #[must_use]
    pub fn into_std(self) -> StdUdpSocket {
        let mut this = ManuallyDrop::new(self);

        // Safety: `this` will not be dropped, so we must drop the registration
        // handle manually and move out the inner socket.
        unsafe {
            ManuallyDrop::drop(&mut this.handle);
            std::ptr::read(&raw const this.inner)
        }
    }

    /// Returns the local address of this socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not bound.
    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.local_addr()
    }

    /// Returns the remote address of this socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.peer_addr()
    }

    /// Connects this UDP socket to a remote address.
    ///
    /// This is the async version of [`std::net::UdpSocket::connect`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - DNS resolution fails
    /// - Connection fails
    /// - The runtime is not active
    #[inline]
    pub async fn connect(&mut self, address: impl ToSocketAddrs) -> Result<(), io::Error> {
        let addresses = address.to_socket_addrs()?;
        let mut last_error = None;
        for address in addresses {
            #[cfg(unix)]
            let connect_one_result = connect_one(&self.handle, address).await;
            #[cfg(windows)]
            let connect_one_result = connect_one(&mut self.handle, &mut self.inner, address).await;
            match connect_one_result {
                Ok(()) => return Ok(()),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses")))
    }

    /// Receives a single datagram message.
    ///
    /// This is the async version of [`std::net::UdpSocket::recv`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The socket is not bound
    /// - The runtime is not active
    #[inline]
    pub async fn recv<B: IoBufMut>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    /// Receives a single datagram message, returning the sender's address.
    ///
    /// This is the async version of [`std::net::UdpSocket::recv_from`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The socket is not bound
    /// - The runtime is not active
    #[inline]
    pub async fn recv_from<B: IoBufMut>(
        &self,
        buf: B,
    ) -> (Result<(usize, SocketAddr), io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvfromOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    /// Sends data on a connected socket.
    ///
    /// This is the async version of [`std::net::UdpSocket::send`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The socket is not connected
    /// - The runtime is not active
    #[inline]
    pub async fn send<B: IoBuf>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = SendOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    /// Sends data to the specified address.
    ///
    /// This is the async version of [`std::net::UdpSocket::send_to`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - DNS resolution fails
    /// - The send operation fails
    /// - The runtime is not active
    #[inline]
    pub async fn send_to<B: IoBuf>(
        &self,
        mut buf: B,
        address: impl ToSocketAddrs,
    ) -> (Result<usize, io::Error>, B) {
        let addresses = match address.to_socket_addrs() {
            Ok(addresses) => addresses,
            Err(err) => return (Err(err), buf),
        };
        let mut last_error = None;

        for address in addresses {
            let handle = &self.handle;
            let mut op = SendtoOp::new(handle, buf, address);
            match poll_fn(|cx| handle.poll_op(cx, &mut op)).await {
                Ok(sent) => return (Ok(sent), op.take_bufs()),
                Err(err) => {
                    buf = op.take_bufs();
                    last_error = Some(err);
                }
            }
        }

        (
            Err(last_error
                .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses"))),
            buf,
        )
    }

    /// Receives data without removing it from the socket's receive queue.
    ///
    /// This is the async version of [`std::net::UdpSocket::peek`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The socket is not bound
    /// - The runtime is not active
    #[inline]
    pub async fn peek<B: IoBufMut>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvOp::new_peek(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    /// Receives data without removing it from the socket's receive queue,
    /// returning the sender's address.
    ///
    /// This is the async version of [`std::net::UdpSocket::peek_from`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The socket is not bound
    /// - The runtime is not active
    #[inline]
    pub async fn peek_from<B: IoBufMut>(
        &self,
        buf: B,
    ) -> (Result<(usize, SocketAddr), io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvfromOp::new_peek(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    /// Returns a new `UdpSocket` that shares the same underlying file descriptor.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be cloned.
    #[inline]
    pub fn try_clone(&self) -> Result<Self, io::Error> {
        Self::from_std(self.inner.try_clone()?)
    }

    /// Sets the broadcast flag.
    ///
    /// When set, the socket can send broadcast packets.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_broadcast(&self, broadcast: bool) -> Result<(), io::Error> {
        self.inner.set_broadcast(broadcast)
    }

    /// Returns the current value of the broadcast flag.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn broadcast(&self) -> Result<bool, io::Error> {
        self.inner.broadcast()
    }

    /// Sets the time-to-live (TTL) value.
    ///
    /// This controls how many hops a packet can traverse before being discarded.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_ttl(&self, ttl: u32) -> Result<(), io::Error> {
        self.inner.set_ttl(ttl)
    }

    /// Returns the current TTL value.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn ttl(&self) -> Result<u32, io::Error> {
        self.inner.ttl()
    }

    /// Sets the multicast loop flag for IPv4.
    ///
    /// When set, multicast packets are looped back to the local socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_multicast_loop_v4(&self, multicast_loop_v4: bool) -> Result<(), io::Error> {
        self.inner.set_multicast_loop_v4(multicast_loop_v4)
    }

    /// Returns the current IPv4 multicast loop flag.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn multicast_loop_v4(&self) -> Result<bool, io::Error> {
        self.inner.multicast_loop_v4()
    }

    /// Sets the multicast TTL for IPv4.
    ///
    /// This controls how many hops multicast packets can traverse.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_multicast_ttl_v4(&self, multicast_ttl_v4: u32) -> Result<(), io::Error> {
        self.inner.set_multicast_ttl_v4(multicast_ttl_v4)
    }

    /// Returns the current IPv4 multicast TTL.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn multicast_ttl_v4(&self) -> Result<u32, io::Error> {
        self.inner.multicast_ttl_v4()
    }

    /// Sets the multicast loop flag for IPv6.
    ///
    /// When set, multicast packets are looped back to the local socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_multicast_loop_v6(&self, multicast_loop_v6: bool) -> Result<(), io::Error> {
        self.inner.set_multicast_loop_v6(multicast_loop_v6)
    }

    /// Returns the current IPv6 multicast loop flag.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn multicast_loop_v6(&self) -> Result<bool, io::Error> {
        self.inner.multicast_loop_v6()
    }

    /// Joins a multicast group for IPv4.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn join_multicast_v4(
        &self,
        multiaddr: &Ipv4Addr,
        interface: &Ipv4Addr,
    ) -> Result<(), io::Error> {
        self.inner.join_multicast_v4(multiaddr, interface)
    }

    /// Joins a multicast group for IPv6.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> Result<(), io::Error> {
        self.inner.join_multicast_v6(multiaddr, interface)
    }

    /// Leaves a multicast group for IPv4.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn leave_multicast_v4(
        &self,
        multiaddr: &Ipv4Addr,
        interface: &Ipv4Addr,
    ) -> Result<(), io::Error> {
        self.inner.leave_multicast_v4(multiaddr, interface)
    }

    /// Leaves a multicast group for IPv6.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn leave_multicast_v6(
        &self,
        multiaddr: &Ipv6Addr,
        interface: u32,
    ) -> Result<(), io::Error> {
        self.inner.leave_multicast_v6(multiaddr, interface)
    }

    /// Takes the pending error from the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn take_error(&self) -> Result<Option<io::Error>, io::Error> {
        self.inner.take_error()
    }

    /// Sets the read timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> Result<(), io::Error> {
        self.inner.set_read_timeout(dur)
    }

    /// Sets the write timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> Result<(), io::Error> {
        self.inner.set_write_timeout(dur)
    }

    /// Returns the read timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn read_timeout(&self) -> Result<Option<Duration>, io::Error> {
        self.inner.read_timeout()
    }

    /// Returns the write timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn write_timeout(&self) -> Result<Option<Duration>, io::Error> {
        self.inner.write_timeout()
    }
}

impl<'a> crate::io::AsInnerRawHandle<'a> for UdpSocket {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a crate::fd_inner::InnerRawHandle {
        &self.handle
    }
}

impl_handle_traits!(UdpSocket);
impl_handle_windows_traits!(UdpSocket);

#[cfg(test)]
mod tests {
    use std::io::{self as std_io};
    use std::net::SocketAddr;

    use crate::driver::AnyDriver;

    use super::{PollUdpSocket, UdpSocket};

    #[inline]
    fn try_bind_udp(address: SocketAddr) -> Option<UdpSocket> {
        match UdpSocket::bind(address) {
            Ok(socket) => Some(socket),
            Err(err) if err.kind() == std_io::ErrorKind::PermissionDenied => None,
            Err(err) => panic!("udp socket should bind: {err}"),
        }
    }

    #[inline]
    fn try_bind_poll_udp(address: SocketAddr) -> Option<PollUdpSocket> {
        match PollUdpSocket::bind(address) {
            Ok(socket) => Some(socket),
            Err(err) if err.kind() == std_io::ErrorKind::PermissionDenied => None,
            Err(err) => panic!("poll udp socket should bind: {err}"),
        }
    }

    /// A socket just made is writable: `try_io_writable` goes to the kernel without a
    /// readiness poll first. quinn sends its Retry packets, stateless resets and
    /// refusals that way, and they were dropped as "write not ready".
    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_new_poll_udp_socket_writes_before_any_poll() {
        let runtime = crate::executor::Runtime::new(
            #[cfg(unix)]
            AnyDriver::new_mio().expect("mio driver should initialize"),
            #[cfg(windows)]
            AnyDriver::new_iocp().expect("iocp driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(socket) = try_bind_poll_udp(address) else {
                return;
            };
            for socket in [
                socket.try_clone().expect("clone should succeed"),
                socket,
                try_bind_udp(address)
                    .expect("udp socket should bind")
                    .into_poll()
                    .expect("into_poll should succeed"),
            ] {
                let mut ran = false;
                socket
                    .try_io_writable(|| {
                        ran = true;
                        Ok(())
                    })
                    .expect("a new socket should be writable");
                assert!(ran);
                // The kernel's would-block is what makes it wait for readiness.
                let blocked = socket.try_io_writable(|| -> std_io::Result<()> {
                    Err(std_io::ErrorKind::WouldBlock.into())
                });
                assert!(blocked.is_err());
                assert!(socket.try_io_writable(|| Ok(())).is_err(), "now it waits");
            }
        });
    }

    /// `send_to` on `io_uring` goes through `SendMsg`, which reads the whole `msghdr`.
    #[cfg(target_os = "linux")]
    #[cfg_attr(miri, ignore)]
    #[test]
    fn udp_send_to_works_on_io_uring() {
        let Ok(driver) = AnyDriver::new_uring() else {
            return;
        };
        let runtime = crate::executor::Runtime::new(driver);
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(server) = try_bind_udp(address) else {
                return;
            };
            let Some(client) = try_bind_udp(address) else {
                return;
            };
            let server_addr = server.local_addr().expect("server local_addr should work");
            let client_addr = client.local_addr().expect("client local_addr should work");

            let sent = client
                .send_to(b"ping".to_vec(), server_addr)
                .await
                .0
                .expect("send_to should succeed");
            assert_eq!(sent, 4);

            let (data, buf) = server.recv_from(vec![0u8; 16]).await;
            let (read, from) = data.expect("recv_from should succeed");
            assert_eq!(&buf[..read], b"ping");
            assert_eq!(from, client_addr);
        });
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn udp_send_recv_and_peek_variants_work() {
        let runtime = crate::executor::Runtime::new(
            #[cfg(unix)]
            AnyDriver::new_mio().expect("mio driver should initialize"),
            #[cfg(windows)]
            AnyDriver::new_iocp().expect("iocp driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");

            let Some(mut server) = try_bind_udp(address) else {
                return;
            };
            let Some(mut client) = try_bind_udp(address) else {
                return;
            };

            let server_addr = server.local_addr().expect("server local_addr should work");
            let client_addr = client.local_addr().expect("client local_addr should work");

            let sent = client
                .send_to(b"ping".to_vec(), server_addr)
                .await
                .0
                .expect("send_to should succeed");
            assert_eq!(sent, 4);

            let peek_from_buf = vec![0u8; 16];
            let (data, peek_from_buf) = server.peek_from(peek_from_buf).await;
            let (peeked, from_peek) = data.expect("peek_from should succeed");
            assert_eq!(&peek_from_buf[..peeked], b"ping");
            assert_eq!(from_peek, client_addr);

            let recv_from_buf = vec![0u8; 16];
            let (data, recv_from_buf) = server.recv_from(recv_from_buf).await;
            let (read, from_read) = data.expect("recv_from should succeed");
            assert_eq!(&recv_from_buf[..read], b"ping");
            assert_eq!(from_read, client_addr);

            server
                .connect(client_addr)
                .await
                .expect("server connect should work");
            client
                .connect(server_addr)
                .await
                .expect("client connect should work");

            let sent = client
                .send(b"echo".to_vec())
                .await
                .0
                .expect("send should succeed");
            assert_eq!(sent, 4);

            let peek_buf = vec![0u8; 16];
            let (peeked, peek_buf) = server.peek(peek_buf).await;
            let peeked = peeked.expect("peek should succeed");
            assert_eq!(&peek_buf[..peeked], b"echo");

            let recv_buf = vec![0u8; 16];
            let (read, recv_buf) = server.recv(recv_buf).await;
            let read = read.expect("recv should succeed");
            assert_eq!(&recv_buf[..read], b"echo");
        });
    }
}
