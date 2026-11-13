macro_rules! impl_handle_traits {
    ($t:ty) => {
        #[cfg(unix)]
        impl std::os::fd::AsRawFd for $t {
            #[inline]
            fn as_raw_fd(&self) -> std::os::fd::RawFd {
                self.inner.as_raw_fd()
            }
        }

        #[cfg(unix)]
        impl std::os::fd::AsFd for $t {
            #[inline]
            fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
                self.inner.as_fd()
            }
        }

        #[cfg(unix)]
        impl std::os::fd::IntoRawFd for $t {
            #[inline]
            fn into_raw_fd(self) -> std::os::fd::RawFd {
                let mut this = std::mem::ManuallyDrop::new(self);

                // Safety: `this` will not be dropped, so we must drop the registration handle manually.
                // We then move out the inner std stream and transfer its fd ownership to the caller.
                unsafe {
                    std::mem::ManuallyDrop::drop(&mut this.handle);
                    std::ptr::read(&raw const this.inner).into_raw_fd()
                }
            }
        }

        impl Drop for $t {
            #[inline]
            fn drop(&mut self) {
                // Safety: The struct is dropped after the handle is dropped.
                unsafe {
                    std::mem::ManuallyDrop::drop(&mut self.handle);
                }
            }
        }
    };
}

macro_rules! impl_handle_windows_traits {
    ($t:ty) => {
        #[cfg(windows)]
        impl std::os::windows::io::AsRawSocket for $t {
            #[inline]
            fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
                self.inner.as_raw_socket()
            }
        }

        #[cfg(windows)]
        impl std::os::windows::io::AsSocket for $t {
            #[inline]
            fn as_socket(&self) -> std::os::windows::io::BorrowedSocket<'_> {
                self.inner.as_socket()
            }
        }

        #[cfg(windows)]
        impl std::os::windows::io::IntoRawSocket for $t {
            #[inline]
            fn into_raw_socket(self) -> std::os::windows::io::RawSocket {
                let mut this = std::mem::ManuallyDrop::new(self);

                // Safety: `this` will not be dropped, so we must drop the registration handle manually.
                // We then move out the inner std stream and transfer its socket ownership to the caller.
                unsafe {
                    std::mem::ManuallyDrop::drop(&mut this.handle);
                    std::ptr::read(&raw const this.inner).into_raw_socket()
                }
            }
        }
    };
}

macro_rules! impl_poll_handle_traits {
    ($t:ty, $inner:ident) => {
        impl $t {
            /// Tries to perform an I/O operation on the socket, returning an error if it is not ready.
            ///
            /// # Errors
            ///
            /// Returns an error if the underlying I/O operation fails.
            #[inline]
            pub fn try_io_readable<Io, IoR>(&self, io: Io) -> std::io::Result<IoR>
            where
                Io: FnOnce() -> std::io::Result<IoR>,
            {
                if *self.read_ready.borrow() {
                    let result = io();
                    if result.is_err() {
                        *self.read_ready.borrow_mut() = false;
                    }
                    result
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "read not ready",
                    ))
                }
            }

            /// Tries to perform an I/O operation on the socket, returning an error if it is not ready.
            ///
            /// # Errors
            ///
            /// Returns an error if the underlying I/O operation fails.
            #[inline]
            pub fn try_io_writable<Io, IoR>(&self, io: Io) -> std::io::Result<IoR>
            where
                Io: FnOnce() -> std::io::Result<IoR>,
            {
                if *self.write_ready.borrow() {
                    let result = io();
                    if result.is_err() {
                        *self.write_ready.borrow_mut() = false;
                    }
                    result
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "write not ready",
                    ))
                }
            }
        }

        #[cfg(unix)]
        impl std::os::fd::AsRawFd for $t {
            #[inline]
            fn as_raw_fd(&self) -> std::os::fd::RawFd {
                self.$inner.as_raw_fd()
            }
        }

        #[cfg(unix)]
        impl std::os::fd::AsFd for $t {
            #[inline]
            fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
                self.$inner.as_fd()
            }
        }

        #[cfg(unix)]
        impl std::os::fd::IntoRawFd for $t {
            #[inline]
            fn into_raw_fd(self) -> std::os::fd::RawFd {
                self.$inner.into_raw_fd()
            }
        }

        impl<'a> crate::io::AsInnerRawHandle<'a> for $t {
            #[inline]
            fn as_inner_raw_handle(&'a self) -> &'a crate::fd_inner::InnerRawHandle {
                self.$inner.as_inner_raw_handle()
            }
        }

        impl crate::io::AsyncReadPoll for $t {
            #[inline]
            fn poll_readable(
                &self,
                cx: &mut std::task::Context,
            ) -> std::task::Poll<std::io::Result<()>> {
                if *self.read_ready.borrow() {
                    return std::task::Poll::Ready(Ok(()));
                }
                let poll = self.$inner.handle.poll_op_poll(
                    cx,
                    &mut crate::op::ReadinessOp::new_readable(&self.$inner.handle),
                )?;
                *self.read_ready.borrow_mut() = true;
                poll.map(Ok)
            }
        }

        impl crate::io::AsyncWritePoll for $t {
            #[inline]
            fn poll_writable(
                &self,
                cx: &mut std::task::Context,
            ) -> std::task::Poll<std::io::Result<()>> {
                if *self.write_ready.borrow() {
                    return std::task::Poll::Ready(Ok(()));
                }
                let poll = self.$inner.handle.poll_op_poll(
                    cx,
                    &mut crate::op::ReadinessOp::new_writable(&self.$inner.handle),
                )?;
                *self.write_ready.borrow_mut() = true;
                poll.map(Ok)
            }
        }
    };
}

macro_rules! impl_poll_handle_windows_traits {
    ($t:ty, $inner:ident) => {
        #[cfg(windows)]
        impl std::os::windows::io::AsRawSocket for $t {
            #[inline]
            fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
                self.$inner.as_raw_socket()
            }
        }

        #[cfg(windows)]
        impl std::os::windows::io::AsSocket for $t {
            #[inline]
            fn as_socket(&self) -> std::os::windows::io::BorrowedSocket<'_> {
                self.$inner.as_socket()
            }
        }

        #[cfg(windows)]
        impl std::os::windows::io::IntoRawSocket for $t {
            #[inline]
            fn into_raw_socket(self) -> std::os::windows::io::RawSocket {
                self.$inner.into_raw_socket()
            }
        }
    };
}

macro_rules! impl_handle_io_traits {
    ($t:ty) => {
        impl<'a> crate::io::AsInnerRawHandle<'a> for $t {
            #[inline]
            fn as_inner_raw_handle(&'a self) -> &'a crate::fd_inner::InnerRawHandle {
                &self.handle
            }
        }

        impl crate::io::AsyncRead for $t {
            #[inline]
            async fn read<B: crate::io::IoBufMut>(
                &mut self,
                buf: B,
            ) -> (Result<usize, std::io::Error>, B) {
                let handle = &self.handle;
                let mut op = crate::op::ReadOp::new(handle, buf);
                let result = std::future::poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
                (result, op.take_bufs())
            }

            #[inline]
            async fn read_vectored<B: crate::io::IoVectoredBufMut>(
                &mut self,
                bufs: B,
            ) -> (Result<usize, std::io::Error>, B) {
                if bufs.is_empty() {
                    return (Ok(0), bufs);
                }
                let handle = &self.handle;
                let mut op = crate::op::ReadvOp::new(handle, bufs);
                let result = std::future::poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
                (result, op.take_bufs())
            }
        }

        impl crate::io::AsyncWrite for $t {
            #[inline]
            async fn write<B: crate::io::IoBuf>(
                &mut self,
                buf: B,
            ) -> (Result<usize, std::io::Error>, B) {
                let handle = &self.handle;
                let mut op = crate::op::WriteOp::new(handle, buf);
                let result = std::future::poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
                (result, op.take_bufs())
            }

            #[inline]
            async fn write_vectored<B: crate::io::IoVectoredBuf>(
                &mut self,
                bufs: B,
            ) -> (Result<usize, std::io::Error>, B) {
                if bufs.is_empty() {
                    return (Ok(0), bufs);
                }
                let handle = &self.handle;
                let mut op = crate::op::WritevOp::new(handle, bufs);
                let result = std::future::poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
                (result, op.take_bufs())
            }
        }
    };
}

macro_rules! impl_poll_handle_io_traits {
    ($t:ty) => {
        impl tokio::io::AsyncRead for $t {
            #[inline]
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<Result<(), std::io::Error>> {
                if buf.remaining() == 0 {
                    return std::task::Poll::Ready(Ok(()));
                }

                let this = self.get_mut();
                // Only a pointer reaches the kernel, which writes the bytes `assume_init`
                // then counts: no `&mut [u8]` over memory that is not initialized yet.
                let unfilled = unsafe { buf.unfilled_mut() };
                let buf_temp = unsafe {
                    crate::io::IoBufTemporaryPoll::new(
                        unfilled.as_mut_ptr().cast::<u8>(),
                        unfilled.len(),
                    )
                };
                let mut op = crate::op::ReadOp::new(&this.stream.handle, buf_temp);
                match this.stream.handle.poll_op_poll(cx, &mut op) {
                    std::task::Poll::Ready(Ok(read)) => {
                        unsafe {
                            buf.assume_init(read);
                        }
                        buf.advance(read);
                        std::task::Poll::Ready(Ok(()))
                    }
                    std::task::Poll::Ready(Err(err)) => std::task::Poll::Ready(Err(err)),
                    std::task::Poll::Pending => std::task::Poll::Pending,
                }
            }
        }

        impl tokio::io::AsyncWrite for $t {
            #[inline]
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                buf: &[u8],
            ) -> std::task::Poll<Result<usize, std::io::Error>> {
                let this = self.get_mut();
                let buf = unsafe {
                    crate::io::IoBufTemporaryPoll::new(buf.as_ptr().cast_mut(), buf.len())
                };
                let mut op = crate::op::WriteOp::new(&this.stream.handle, buf);
                this.stream.handle.poll_op_poll(cx, &mut op)
            }

            #[inline]
            fn poll_write_vectored(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                bufs: &[std::io::IoSlice<'_>],
            ) -> std::task::Poll<Result<usize, std::io::Error>> {
                if bufs.is_empty() {
                    return std::task::Poll::Ready(Ok(0));
                }
                let this = self.get_mut();
                let bufs = unsafe { crate::io::IoVectoredBufTemporaryPoll::new(bufs) };
                let mut op = crate::op::WritevOp::new(&this.stream.handle, bufs);
                this.stream.handle.poll_op_poll(cx, &mut op)
            }

            #[inline]
            fn is_write_vectored(&self) -> bool {
                true
            }

            #[inline]
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), std::io::Error>> {
                std::task::Poll::Ready(Ok(()))
            }

            #[inline]
            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), std::io::Error>> {
                std::task::Poll::Ready(self.get_mut().shutdown(Shutdown::Write))
            }
        }
    };
}
