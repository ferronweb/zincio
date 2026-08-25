#[cfg(windows)]
use std::ffi::c_void;
use std::io;
#[cfg(unix)]
use std::mem::{self, MaybeUninit};
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(windows)]
use std::ptr;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Networking::WinSock::{
        self as WinSock, AF_INET, AF_INET6, INVALID_SOCKET, IPPROTO_TCP, SOCKADDR, SOCKADDR_IN,
        SOCKADDR_IN6, SOCKADDR_STORAGE, SOCKET, SOCK_STREAM, SOL_SOCKET, SO_UPDATE_ACCEPT_CONTEXT,
        WSAID_ACCEPTEX, WSAID_GETACCEPTEXSOCKADDRS, WSA_FLAG_OVERLAPPED, WSA_IO_PENDING,
    },
    System::IO::OVERLAPPED,
};

use crate::driver::CompletionIoResult;
use crate::fd_inner::{InnerRawHandle, RawOsHandle};
use crate::op::Op;
use crate::{driver::AnyDriver, try_current_driver};

#[cfg(unix)]
fn set_cloexec(fd: RawFd) -> Result<(), io::Error> {
    // set FD_CLOEXEC on file descriptor flags
    let fdflags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if fdflags == -1 {
        return Err(io::Error::last_os_error());
    }
    if fdflags & libc::FD_CLOEXEC == 0 {
        let result = unsafe { libc::fcntl(fd, libc::F_SETFD, fdflags | libc::FD_CLOEXEC) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}

#[cfg(unix)]
fn sockaddr_storage_to_socketaddr(
    storage: &libc::sockaddr_storage,
) -> Result<SocketAddr, io::Error> {
    // Determine family. ss_family field is platform-dependent type; cast to c_uchar then to c_int for comparison.
    let family = libc::c_int::from(storage.ss_family);

    if family == libc::AF_INET {
        let addr_in: &libc::sockaddr_in =
            unsafe { &*std::ptr::from_ref(storage).cast::<libc::sockaddr_in>() };
        let port = u16::from_be(addr_in.sin_port);
        // s_addr is in network byte order
        let ip_u32 = u32::from_be(addr_in.sin_addr.s_addr);
        let ip = std::net::Ipv4Addr::from(ip_u32);
        Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
    } else if family == libc::AF_INET6 {
        let addr_in6: &libc::sockaddr_in6 =
            unsafe { &*std::ptr::from_ref(storage).cast::<libc::sockaddr_in6>() };
        let port = u16::from_be(addr_in6.sin6_port);
        let ip = std::net::Ipv6Addr::from(addr_in6.sin6_addr.s6_addr);
        Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
            ip,
            port,
            addr_in6.sin6_flowinfo,
            addr_in6.sin6_scope_id,
        )))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported socket family",
        ))
    }
}

#[cfg(windows)]
fn sockaddr_storage_to_socketaddr(storage: &SOCKADDR_STORAGE) -> Result<SocketAddr, io::Error> {
    // Determine family. ss_family field is platform-dependent type; cast to c_uchar then to c_int for comparison.
    let family = storage.ss_family;

    if family == AF_INET {
        let addr_in: &SOCKADDR_IN = unsafe { &*std::ptr::from_ref(storage).cast::<SOCKADDR_IN>() };
        let port = u16::from_be(addr_in.sin_port);
        // s_addr is in network byte order
        let ip_u32 = u32::from_be(unsafe { addr_in.sin_addr.S_un.S_addr });
        let ip = std::net::Ipv4Addr::from(ip_u32);
        Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
    } else if family == AF_INET6 {
        let addr_in6: &SOCKADDR_IN6 = unsafe { &*std::ptr::from_ref(storage).cast::<SOCKADDR_IN6>() };
        let port = u16::from_be(addr_in6.sin6_port);
        let ip = std::net::Ipv6Addr::from(unsafe { addr_in6.sin6_addr.u.Byte });
        Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
            ip,
            port,
            addr_in6.sin6_flowinfo,
            unsafe { addr_in6.Anonymous.sin6_scope_id },
        )))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported socket family",
        ))
    }
}

#[cfg(windows)]
fn load_accept_ex(socket: SOCKET) -> Result<WinSock::LPFN_ACCEPTEX, io::Error> {
    let mut bytes_returned: u32 = 0;
    let mut accept_ex: WinSock::LPFN_ACCEPTEX = None;
    let mut guid = WSAID_ACCEPTEX;

    let ioctl_result = unsafe {
        WinSock::WSAIoctl(
            socket,
            WinSock::SIO_GET_EXTENSION_FUNCTION_POINTER,
            (&raw mut guid).cast::<c_void>(),
            std::mem::size_of_val(&guid) as u32,
            (&raw mut accept_ex).cast::<c_void>(),
            std::mem::size_of_val(&accept_ex) as u32,
            &raw mut bytes_returned,
            ptr::null_mut(),
            None,
        )
    };

    if ioctl_result == WinSock::SOCKET_ERROR {
        let err_code = unsafe { WinSock::WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(err_code));
    }

    if accept_ex.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "AcceptEx extension function is unavailable",
        ));
    }

    Ok(accept_ex)
}

#[cfg(windows)]
fn load_get_accept_ex_sockaddrs(
    socket: SOCKET,
) -> Result<WinSock::LPFN_GETACCEPTEXSOCKADDRS, io::Error> {
    let mut bytes_returned: u32 = 0;
    let mut get_accept_ex_sockaddrs: WinSock::LPFN_GETACCEPTEXSOCKADDRS = None;
    let mut guid = WSAID_GETACCEPTEXSOCKADDRS;

    let ioctl_result = unsafe {
        WinSock::WSAIoctl(
            socket,
            WinSock::SIO_GET_EXTENSION_FUNCTION_POINTER,
            (&raw mut guid).cast::<c_void>(),
            std::mem::size_of_val(&guid) as u32,
            (&raw mut get_accept_ex_sockaddrs).cast::<c_void>(),
            std::mem::size_of_val(&get_accept_ex_sockaddrs) as u32,
            &raw mut bytes_returned,
            ptr::null_mut(),
            None,
        )
    };

    if ioctl_result == WinSock::SOCKET_ERROR {
        let err_code = unsafe { WinSock::WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(err_code));
    }

    if get_accept_ex_sockaddrs.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "GetAcceptExSockaddrs extension function is unavailable",
        ));
    }

    Ok(get_accept_ex_sockaddrs)
}

#[cfg(windows)]
fn listener_socket_family(listener_socket: SOCKET) -> Result<i32, io::Error> {
    let mut addr = SOCKADDR_IN6::default();
    let mut addr_len = std::mem::size_of::<SOCKADDR_IN6>() as i32;
    let result = unsafe {
        WinSock::getsockname(
            listener_socket,
            (&raw mut addr).cast::<SOCKADDR>(),
            &raw mut addr_len,
        )
    };

    if result == WinSock::SOCKET_ERROR {
        let err_code = unsafe { WinSock::WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(err_code));
    }

    Ok(i32::from(addr.sin6_family))
}

#[cfg(windows)]
fn create_accept_socket(listener_socket: SOCKET) -> Result<SOCKET, io::Error> {
    let family = listener_socket_family(listener_socket)?;
    if family != i32::from(AF_INET) && family != i32::from(AF_INET6) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported listening socket family for AcceptEx",
        ));
    }

    let accept_socket = unsafe {
        WinSock::WSASocketW(
            family,
            SOCK_STREAM,
            IPPROTO_TCP,
            ptr::null_mut(),
            0,
            WSA_FLAG_OVERLAPPED,
        )
    };
    if accept_socket == INVALID_SOCKET {
        let err_code = unsafe { WinSock::WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(err_code));
    }

    Ok(accept_socket)
}

#[cfg(windows)]
fn set_accept_context(listener_socket: SOCKET, accepted_socket: SOCKET) -> Result<(), io::Error> {
    let result = unsafe {
        WinSock::setsockopt(
            accepted_socket,
            SOL_SOCKET,
            SO_UPDATE_ACCEPT_CONTEXT,
            (&raw const listener_socket).cast(),
            std::mem::size_of::<SOCKET>() as i32,
        )
    };
    if result == WinSock::SOCKET_ERROR {
        let err_code = unsafe { WinSock::WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(err_code));
    }
    Ok(())
}

#[cfg(windows)]
const ACCEPTEX_ADDR_LEN: usize = std::mem::size_of::<SOCKADDR_STORAGE>() + 16;
#[cfg(windows)]
const ACCEPTEX_OUTPUT_BUFFER_LEN: usize = ACCEPTEX_ADDR_LEN;

pub struct AcceptOp<'a> {
    handle: &'a InnerRawHandle,
    #[cfg(windows)]
    accept_ex: Option<WinSock::LPFN_ACCEPTEX>,
    #[cfg(windows)]
    get_accept_ex_sockaddrs: Option<WinSock::LPFN_GETACCEPTEXSOCKADDRS>,
    #[cfg(windows)]
    accept_socket: Option<SOCKET>,
    #[cfg(windows)]
    bytes_received: u32,
    #[cfg(windows)]
    accept_output_buffer: Option<Box<[u8]>>,
    completion_token: Option<usize>,
}

impl<'a> AcceptOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle) -> Self {
        Self {
            handle,
            #[cfg(windows)]
            accept_ex: None,
            #[cfg(windows)]
            get_accept_ex_sockaddrs: None,
            #[cfg(windows)]
            accept_socket: None,
            #[cfg(windows)]
            bytes_received: 0,
            #[cfg(windows)]
            accept_output_buffer: None,
            completion_token: None,
        }
    }
}

#[cfg(unix)]
fn accept_poll_unix(
    fd: RawFd,
    handle: &InnerRawHandle,
    driver: &AnyDriver,
    cx: &mut Context<'_>,
) -> Poll<io::Result<(RawOsHandle, SocketAddr)>> {
    #[cfg(syscall_accept4)]
    let accepted_fd = unsafe {
        libc::accept4(
            fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
        )
    };
    #[cfg(not(syscall_accept4))]
    let accepted_fd = unsafe { libc::accept(fd, std::ptr::null_mut(), std::ptr::null_mut()) };
    if accepted_fd == -1 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            if let Err(err) = driver.submit_poll(handle, cx.waker().clone(), Interest::READABLE) {
                return Poll::Ready(Err(err));
            }
            return Poll::Pending;
        }
        return Poll::Ready(Err(error));
    }

    let fd = accepted_fd as RawFd;

    // On non-Linux Unix, set close-on-exec + non-blocking manually.
    // Linux accept4() above already set these atomically.
    #[cfg(not(syscall_accept4))]
    if let Err(err) = set_cloexec(fd) {
        return Poll::Ready(Err(err));
    }

    // Obtain peer address via getpeername into a sockaddr_storage
    let mut peer = MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut peer_len = u32::try_from(mem::size_of::<libc::sockaddr_storage>()).unwrap();
    let getpeername_result = unsafe {
        libc::getpeername(
            fd,
            peer.as_mut_ptr().cast::<libc::sockaddr>(),
            &raw mut peer_len,
        )
    };

    if getpeername_result == -1 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            if let Err(err) = driver.submit_poll(handle, cx.waker().clone(), Interest::READABLE) {
                return Poll::Ready(Err(err));
            }
            return Poll::Pending;
        }
        return Poll::Ready(Err(error));
    }

    let peer = unsafe { peer.assume_init() };
    let address = sockaddr_storage_to_socketaddr(&peer)?;
    Poll::Ready(Ok((fd as RawOsHandle, address)))
}

#[cfg(windows)]
fn accept_poll_windows(
    handle: &InnerRawHandle,
    driver: &AnyDriver,
    cx: &mut Context<'_>,
) -> Poll<io::Result<(RawOsHandle, SocketAddr)>> {
    let RawOsHandle::Socket(listener_socket) = handle.handle else {
        return Poll::Ready(Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid raw handle",
        )));
    };

    let accepted_socket =
        unsafe { WinSock::accept(listener_socket as SOCKET, ptr::null_mut(), ptr::null_mut()) };
    if accepted_socket == INVALID_SOCKET {
        let error = io::Error::from_raw_os_error(unsafe { WinSock::WSAGetLastError() });
        if error.kind() == io::ErrorKind::WouldBlock {
            if let Err(err) = driver.submit_poll(handle, cx.waker().clone(), Interest::READABLE) {
                return Poll::Ready(Err(err));
            }
            return Poll::Pending;
        }
        return Poll::Ready(Err(error));
    }

    let mut peer = SOCKADDR_STORAGE::default();
    let mut peer_len = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;
    let getpeername_result = unsafe {
        WinSock::getpeername(
            accepted_socket,
            (&raw mut peer).cast::<SOCKADDR>(),
            &raw mut peer_len,
        )
    };
    if getpeername_result == WinSock::SOCKET_ERROR {
        let error = io::Error::from_raw_os_error(unsafe { WinSock::WSAGetLastError() });
        unsafe { WinSock::closesocket(accepted_socket) };
        return Poll::Ready(Err(error));
    }

    let address = match sockaddr_storage_to_socketaddr(&peer) {
        Ok(address) => address,
        Err(err) => {
            unsafe { WinSock::closesocket(accepted_socket) };
            return Poll::Ready(Err(err));
        }
    };

    Poll::Ready(Ok((
        RawOsHandle::Socket(accepted_socket as std::os::windows::io::RawSocket),
        address,
    )))
}

impl Op for AcceptOp<'_> {
    type Output = (RawOsHandle, SocketAddr);

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        #[cfg(unix)]
        {
            accept_poll_unix(self.handle.handle, self.handle, driver, cx)
        }

        #[cfg(windows)]
        {
            accept_poll_windows(self.handle, driver, cx)
        }
    }

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let result = if let Some(completion_token) = self.completion_token {
            if let Some(result) = driver.get_completion_result(completion_token) {
                self.completion_token = None;
                result
            } else {
                // The completion is not ready yet
                driver.set_completion_waker(completion_token, cx.waker().clone());
                return Poll::Pending;
            }
        } else {
            // Submit the op
            match driver.submit_completion(self, cx.waker().clone()) {
                CompletionIoResult::Ok(result) => result,
                CompletionIoResult::Retry(token) => {
                    self.completion_token = Some(token);
                    return Poll::Pending;
                }
                CompletionIoResult::SubmitErr(err) => return Poll::Ready(Err(err)),
            }
        };
        if result < 0 {
            #[cfg(windows)]
            if let Some(accept_socket) = self.accept_socket.take() {
                unsafe { WinSock::closesocket(accept_socket) };
            }
            return Poll::Ready(Err(io::Error::from_raw_os_error(-result)));
        }

        #[cfg(unix)]
        {
            poll_completion_unix(result)
        }

        #[cfg(windows)]
        {
            poll_completion_windows(self, result)
        }
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let RawOsHandle::Socket(listener_socket) = self.handle.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AcceptEx can be used only with listening sockets",
            ));
        };
        let listener_socket = listener_socket as SOCKET;

        if self.accept_ex.is_none() {
            self.accept_ex = Some(load_accept_ex(listener_socket)?);
        }
        if self.get_accept_ex_sockaddrs.is_none() {
            self.get_accept_ex_sockaddrs = Some(load_get_accept_ex_sockaddrs(listener_socket)?);
        }
        let accept_ex = self.accept_ex.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "AcceptEx extension function is unavailable",
            )
        })?;

        if self.accept_socket.is_none() {
            self.accept_socket = Some(create_accept_socket(listener_socket)?);
        }
        let accept_socket = self
            .accept_socket
            .expect("accept_socket must be initialized");
        if self.accept_output_buffer.is_none() {
            self.accept_output_buffer =
                Some(vec![0u8; ACCEPTEX_OUTPUT_BUFFER_LEN].into_boxed_slice());
        }
        let accept_output_buffer = self
            .accept_output_buffer
            .as_mut()
            .expect("accept_output_buffer must be initialized");

        let Some(accept_ex_fn) = accept_ex else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "AcceptEx extension function is unavailable",
            ));
        };

        let accept_result = unsafe {
            accept_ex_fn(
                listener_socket,
                accept_socket,
                accept_output_buffer.as_mut_ptr().cast::<c_void>(),
                0,
                0,
                ACCEPTEX_ADDR_LEN as u32,
                &raw mut self.bytes_received,
                overlapped,
            )
        };
        if accept_result != 0 {
            return Ok(());
        }

        let err_code = unsafe { WinSock::WSAGetLastError() };
        if err_code == WSA_IO_PENDING {
            Ok(())
        } else {
            if let Some(socket) = self.accept_socket.take() {
                unsafe { WinSock::closesocket(socket) };
            }
            Err(io::Error::from_raw_os_error(err_code))
        }
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let entry = opcode::Accept::new(
            types::Fd(self.handle.handle),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .flags(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

#[cfg(unix)]
#[inline]
fn poll_completion_unix(result: i32) -> Poll<io::Result<(RawOsHandle, SocketAddr)>> {
    let fd = result as RawFd;

    // Ensure close-on-exec for the accepted fd (io_uring may have already set them)
    if let Err(err) = set_cloexec(fd) {
        return Poll::Ready(Err(err));
    }

    let mut peer = MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut peer_len = u32::try_from(mem::size_of::<libc::sockaddr_storage>()).unwrap();
    let getpeername_result = unsafe {
        libc::getpeername(
            fd,
            peer.as_mut_ptr().cast::<libc::sockaddr>(),
            &raw mut peer_len,
        )
    };

    if getpeername_result == -1 {
        return Poll::Ready(Err(io::Error::last_os_error()));
    }

    let peer = unsafe { peer.assume_init() };
    let address = sockaddr_storage_to_socketaddr(&peer)?;
    Poll::Ready(Ok((fd as RawOsHandle, address)))
}

#[cfg(windows)]
#[inline]
fn poll_completion_windows(
    op: &mut AcceptOp<'_>,
    _result: i32,
) -> Poll<io::Result<(RawOsHandle, SocketAddr)>> {
    

    let RawOsHandle::Socket(listener_socket) = op.handle.handle else {
        return Poll::Ready(Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "AcceptEx can be used only with listening sockets",
        )));
    };

    let Some(accept_socket) = op.accept_socket.take() else {
        return Poll::Ready(Err(io::Error::other(
            "AcceptEx completion missing accepted socket",
        )));
    };

    if let Err(err) = set_accept_context(listener_socket as SOCKET, accept_socket) {
        unsafe { WinSock::closesocket(accept_socket) };
        return Poll::Ready(Err(err));
    }

    let peer = match op.accept_output_buffer.take() {
        Some(buf) => buf,
        None => {
            return Poll::Ready(Err(io::Error::other(
                "AcceptEx completion missing peer address",
            )));
        }
    };

    let mut local_sockaddr: *mut SOCKADDR_STORAGE = std::ptr::null_mut();
    let mut local_sockaddr_len: i32 = 0;
    let mut remote_sockaddr: *mut SOCKADDR_STORAGE = std::ptr::null_mut();
    let mut remote_sockaddr_len: i32 = 0;

    let get_accept_ex_sockaddrs = op.get_accept_ex_sockaddrs.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "GetAcceptExSockaddrs extension function is unavailable",
        )
    })?;
    let Some(get_accept_ex_sockaddrs_fn) = get_accept_ex_sockaddrs else {
        return Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "GetAcceptExSockaddrs extension function is unavailable",
        )));
    };

    let () = unsafe {
        get_accept_ex_sockaddrs_fn(
            peer.as_ptr().cast::<c_void>(),
            0,
            0,
            ACCEPTEX_ADDR_LEN as _,
            (&raw mut local_sockaddr).cast::<*mut SOCKADDR>(),
            &raw mut local_sockaddr_len,
            (&raw mut remote_sockaddr).cast::<*mut SOCKADDR>(),
            &raw mut remote_sockaddr_len,
        );
    };

    if remote_sockaddr.is_null() {
        return Poll::Ready(Err(io::Error::other(
            "can't obtain remote socket address",
        )));
    }

    let address = match sockaddr_storage_to_socketaddr(
        &(unsafe { std::ptr::read_unaligned(remote_sockaddr.cast_const()) }),
    ) {
        Ok(address) => address,
        Err(err) => {
            unsafe { WinSock::closesocket(accept_socket) };
            return Poll::Ready(Err(err));
        }
    };

    Poll::Ready(Ok((RawOsHandle::Socket(accept_socket as _), address)))
}

impl Drop for AcceptOp<'_> {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = try_current_driver() {
                driver.ignore_completion(completion_token, Box::new(()));
            }
        }
        #[cfg(windows)]
        if let Some(socket) = self.accept_socket.take() {
            unsafe { WinSock::closesocket(socket) };
        }
    }
}
