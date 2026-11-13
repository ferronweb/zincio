//! Standard I/O utilities for stdin, stdout, and stderr.
//!
//! This module provides async-aware wrappers around standard I/O streams:
//! - `Stdin`: async stdin reader.
//! - `Stdout`: async stdout writer.
//! - `Stderr`: async stderr writer.
//!
//! These types use a blocking thread pool for I/O operations when inside
//! a runtime context. Outside a runtime, they fall back to synchronous I/O.
//!
//! # Examples
//!
//! ```ignore
//! use zincio::io::{AsyncRead, AsyncWrite, stdin, stdout};
//!
//! async fn echo() {
//!     let mut stdin = stdin();
//!     let mut stdout = stdout();
//!     let mut buf = vec![0u8; 1024];
//!     loop {
//!         let (read, buf) = stdin.read(buf).await;
//!         let read = read?;
//!         if read == 0 {
//!             break;
//!         }
//!         let (written, buf) = stdout.write(buf).await;
//!         written?;
//!     }
//! }
//! ```

use std::io::{self, Read, Write};

use crate::executor::current_driver;
use crate::io::{iobuf_to_slice, iobufmut_to_slice, AsyncRead, AsyncWrite, IoBuf, IoBufMut};

/// Async-aware stdin reader.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stdin {
    _private: (),
}

/// Async-aware stdout writer.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stdout {
    _private: (),
}

/// Async-aware stderr writer.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stderr {
    _private: (),
}

/// Get an async-aware stdin reader.
#[inline]
#[must_use]
pub fn stdin() -> Stdin {
    Stdin { _private: () }
}

/// Get an async-aware stdout writer.
#[inline]
#[must_use]
pub fn stdout() -> Stdout {
    Stdout { _private: () }
}

/// Get an async-aware stderr writer.
#[inline]
#[must_use]
pub fn stderr() -> Stderr {
    Stderr { _private: () }
}

impl AsyncRead for Stdin {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        if current_driver().is_some() {
            let (result, buf) = crate::io::run_blocking_with_state(buf, |buf| {
                let slice = iobufmut_to_slice(buf);
                std::io::stdin().read(slice)
            })
            .await;
            (
                result
                    .map_err(|_| io::Error::other("can't spawn blocking task for stdio I/O"))
                    .flatten(),
                buf,
            )
        } else {
            let slice = iobufmut_to_slice(&mut buf);
            (std::io::stdin().read(slice), buf)
        }
    }
}

impl AsyncWrite for Stdout {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        if current_driver().is_some() {
            let (result, buf) = crate::io::run_blocking_with_state(buf, |buf| {
                let slice = iobuf_to_slice(buf);
                std::io::stdout().write(slice)
            })
            .await;
            (
                result
                    .map_err(|_| io::Error::other("can't spawn blocking task for stdio I/O"))
                    .flatten(),
                buf,
            )
        } else {
            let slice = iobuf_to_slice(&buf);
            (std::io::stdout().write(slice), buf)
        }
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        if current_driver().is_some() {
            crate::spawn_blocking(|| std::io::stdout().flush())
                .await
                .map_err(|_| io::Error::other("can't spawn blocking task for stdio I/O"))?
        } else {
            std::io::stdout().flush()
        }
    }
}

impl AsyncWrite for Stderr {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        if current_driver().is_some() {
            let (result, buf) = crate::io::run_blocking_with_state(buf, |buf| {
                let slice = iobuf_to_slice(buf);
                std::io::stderr().write(slice)
            })
            .await;
            (
                result
                    .map_err(|_| io::Error::other("can't spawn blocking task for stdio I/O"))
                    .flatten(),
                buf,
            )
        } else {
            let slice = iobuf_to_slice(&buf);
            (std::io::stderr().write(slice), buf)
        }
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        if current_driver().is_some() {
            crate::spawn_blocking(|| std::io::stderr().flush())
                .await
                .map_err(|_| io::Error::other("can't spawn blocking task for stdio I/O"))?
        } else {
            std::io::stderr().flush()
        }
    }
}
