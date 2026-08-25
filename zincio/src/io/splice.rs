//! Zero-copy I/O utilities using `splice` and `sendfile`.
//!
//! This module provides async-aware zero-copy I/O operations:
//! - `splice()`: transfer data between file descriptors without copying to userspace.
//! - `splice_exact()`: transfer exactly `len` bytes using `splice`.
//!
//! These operations are only available on Linux with the `splice` feature enabled.
//!
//! # Examples
//!
//! ```ignore
//! use zincio::io::{splice, AsyncRead, AsyncWrite, pipe};
//!
//! async fn zero_copy_example() {
//!     let (reader, writer) = pipe().unwrap();
//!     let mut file = std::fs::File::open("data.txt").unwrap();
//!
//!     // Transfer 1024 bytes from file to pipe
//!     let n = splice(&file, &writer, 1024).await.unwrap();
//!     println!("Spliced {} bytes", n);
//! }
//! ```

use std::os::fd::{AsRawFd, BorrowedFd};

use crate::{io::AsInnerRawHandle, op::SpliceOp};

/// Transfer data from one file descriptor to another using `splice`.
///
/// This function uses the kernel's `splice` system call to transfer data
/// between file descriptors without copying to userspace.
///
/// # Errors
/// Returns an error if the underlying `splice` operation fails.
pub async fn splice<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: usize,
) -> Result<usize, std::io::Error> {
    let from_handle = unsafe { BorrowedFd::borrow_raw(from.as_raw_fd()) };
    let to_handle = to.as_inner_raw_handle();

    let mut op = SpliceOp::new(from_handle, to_handle, len);
    let result = std::future::poll_fn(move |cx| to_handle.poll_op(cx, &mut op)).await;
    result
}

/// Transfer exactly `len` bytes from one file descriptor to another using `splice`.
///
/// This function calls `splice()` repeatedly until `len` bytes have been transferred
/// or EOF is reached.
///
/// # Errors
/// Returns an error if the underlying `splice` operation fails.
///
/// # Panics
///
/// Panics if the remaining number of bytes to transfer cannot be represented
/// as `usize` on the target platform (only possible with an enormous `len`).
pub async fn splice_exact<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: u64,
) -> Result<u64, std::io::Error> {
    let mut total = 0;
    while total < len {
        let n = splice(
            from,
            to,
            usize::try_from((len - total).min(usize::MAX as u64)).unwrap(),
        )
        .await?;
        if n == 0 {
            break;
        }
        total += n as u64;
    }

    Ok(total)
}
