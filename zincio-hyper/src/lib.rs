//! A compatibility layer for using [`hyper`] with the `zincio` async runtime.
//!
//! This crate provides the necessary adapters to run `hyper`-based HTTP servers
//! and clients on top of `zincio` instead of `tokio`. It implements the traits
//! required by `hyper`'s runtime abstraction:
//!
//! - [`ZincioExecutor`]: An executor that spawns tasks on the `zincio` runtime
//!   using `zincio::spawn`.
//! - [`ZincioTimer`]: A timer that uses `zincio::time::sleep` for delay operations.
//! - [`ZincioIo`]: A wrapper type that adapts `zincio`'s I/O types to implement
//!   `hyper`'s `Read` and `Write` traits, and also implements `tokio`'s
//!   `AsyncRead` and `AsyncWrite` traits for compatibility.
//!
//! # Overview
//!
//! This crate enables `hyper` to work with `zincio` by implementing `hyper`'s
//! runtime traits (`Executor`, `Timer`, `Read`, `Write`, `Sleep`) in terms of
//! `zincio` primitives.
//!
//! ## Executor
//!
//! The [`ZincioExecutor`] type implements `hyper::rt::Executor` by spawning
//! futures onto the `zincio` runtime via `zincio::spawn`.
//!
//! ## Timer
//!
//! The [`ZincioTimer`] type implements `hyper::rt::Timer` by converting sleep
//! requests into `zincio::time::sleep` futures wrapped in a compatible type.
//!
//! ## I/O Adapters
//!
//! The [`ZincioIo<T>`] wrapper adapts any type that implements `tokio::io::AsyncRead`
//! and `tokio::io::AsyncWrite` to work with `hyper`'s I/O traits. It also
//! implements the reverse conversion, allowing `hyper`'s I/O types to be used
//! with `tokio`-style async functions.
//!
//! # Implementation notes
//!
//! - The `ZincioIo` wrapper uses `Pin<Box<T>>` internally to support the
//!   trait implementations required by `hyper` and `tokio`.
//! - The `ZincioSleep` type (internal) implements both `hyper::rt::Sleep`
//!   and `std::future::Future` to bridge the two runtimes' sleep abstractions.
//! - Timer handles are properly cancelled when `ZincioSleep` is dropped to
//!   avoid resource leaks.

use std::{
    ops::{Deref, DerefMut},
    pin::Pin,
    task::{Context, Poll},
};

use hyper::rt::Executor;
#[cfg(feature = "time")]
use hyper::rt::{Sleep, Timer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// An executor that spawns tasks onto the `zincio` runtime.
///
/// This type implements `hyper::rt::Executor` and uses `zincio::spawn` to
/// execute futures on the `zincio` runtime.
#[derive(Debug, Default, Clone, Copy)]
pub struct ZincioExecutor;

impl<Fut> Executor<Fut> for ZincioExecutor
where
    Fut: std::future::Future + 'static,
    Fut::Output: 'static,
{
    #[inline]
    fn execute(&self, fut: Fut) {
        zincio::spawn(fut);
    }
}

/// A timer that uses `zincio`'s time utilities for sleep operations.
///
/// This type implements `hyper::rt::Timer` and uses `zincio::time::sleep`
/// to implement delay operations.
#[cfg(feature = "time")]
#[derive(Debug, Default, Clone, Copy)]
pub struct ZincioTimer;

#[cfg(feature = "time")]
impl Timer for ZincioTimer {
    #[inline]
    fn sleep(&self, duration: std::time::Duration) -> Pin<Box<dyn Sleep>> {
        Box::pin(ZincioSleep {
            inner: Box::pin(zincio::time::sleep(duration)),
        })
    }

    #[inline]
    fn sleep_until(&self, deadline: std::time::Instant) -> Pin<Box<dyn Sleep>> {
        Box::pin(ZincioSleep {
            inner: Box::pin(zincio::time::sleep_until(deadline)),
        })
    }

    #[inline]
    fn reset(&self, sleep: &mut Pin<Box<dyn Sleep>>, new_deadline: std::time::Instant) {
        if let Some(mut sleep) = sleep.as_mut().downcast_mut_pin::<ZincioSleep>() {
            sleep.reset(new_deadline);
        }
    }
}

/// A sleep future that wraps `zincio::time::Sleep` and implements `hyper::rt::Sleep`.
#[cfg(feature = "time")]
struct ZincioSleep {
    inner: Pin<Box<zincio::time::Sleep>>,
}

#[cfg(feature = "time")]
impl std::future::Future for ZincioSleep {
    type Output = ();

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

#[cfg(feature = "time")]
impl Sleep for ZincioSleep {}

#[cfg(feature = "time")]
unsafe impl Send for ZincioSleep {}
#[cfg(feature = "time")]
unsafe impl Sync for ZincioSleep {}

#[cfg(feature = "time")]
impl ZincioSleep {
    #[inline]
    fn reset(&mut self, new_deadline: std::time::Instant) {
        self.inner.reset(new_deadline);
    }
}

/// A wrapper type that adapts I/O types for use with `hyper` and `tokio`.
///
/// `ZincioIo<T>` wraps any type `T` that implements `tokio::io::AsyncRead`
/// and `tokio::io::AsyncWrite` and provides implementations for:
///
/// - `hyper::rt::Read` and `hyper::rt::Write`
/// - `tokio::io::AsyncRead` and `tokio::io::AsyncWrite`
///
/// This allows seamless interoperability between `hyper`'s I/O traits and
/// `tokio`'s async I/O traits when using the `zincio` runtime.
#[derive(Debug)]
pub struct ZincioIo<T> {
    inner: Pin<Box<T>>,
}

impl<T> ZincioIo<T> {
    /// Creates a new `ZincioIo` wrapper around the given I/O type.
    #[inline]
    pub fn new(inner: T) -> Self {
        Self {
            inner: Box::pin(inner),
        }
    }
}

impl<T> Deref for ZincioIo<T> {
    type Target = Pin<Box<T>>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> DerefMut for ZincioIo<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

// Implement hyper::rt::Read for types that implement tokio::io::AsyncRead
impl<T> hyper::rt::Read for ZincioIo<T>
where
    T: AsyncRead,
{
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let n = {
            let mut tbuf = unsafe { ReadBuf::uninit(buf.as_mut()) };
            match self.inner.as_mut().poll_read(cx, &mut tbuf) {
                Poll::Ready(Ok(_)) => tbuf.filled().len(),
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        };

        unsafe { buf.advance(n) };
        Poll::Ready(Ok(()))
    }
}

// Implement hyper::rt::Write for types that implement tokio::io::AsyncWrite
impl<T> hyper::rt::Write for ZincioIo<T>
where
    T: AsyncWrite,
{
    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.inner.as_mut().poll_write(cx, buf)
    }

    #[inline]
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.inner.as_mut().poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.inner.as_mut().poll_shutdown(cx)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    #[inline]
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.inner.as_mut().poll_write_vectored(cx, bufs)
    }
}

// Implement tokio::io::AsyncRead for types that implement hyper::rt::Read
impl<T> AsyncRead for ZincioIo<T>
where
    T: hyper::rt::Read,
{
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        tbuf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let filled = tbuf.filled().len();
        let sub_filled = {
            let mut buf = unsafe { hyper::rt::ReadBuf::uninit(tbuf.unfilled_mut()) };
            match self.inner.as_mut().poll_read(cx, buf.unfilled()) {
                Poll::Ready(Ok(_)) => buf.filled().len(),
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        };

        unsafe {
            tbuf.assume_init(sub_filled);
            tbuf.set_filled(filled + sub_filled);
        };
        Poll::Ready(Ok(()))
    }
}

// Implement tokio::io::AsyncWrite for types that implement hyper::rt::Write
impl<T> AsyncWrite for ZincioIo<T>
where
    T: hyper::rt::Write,
{
    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.inner.as_mut().poll_write(cx, buf)
    }

    #[inline]
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.inner.as_mut().poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.inner.as_mut().poll_shutdown(cx)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    #[inline]
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.inner.as_mut().poll_write_vectored(cx, bufs)
    }
}
