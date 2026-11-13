//! Link helpers.
//!
//! Async versions of `std::fs` link operations: [`hard_link`], [`symlink_dir`],
//! [`symlink_file`], and [`symlink`], plus Windows-specific symlink helpers.

#[cfg(target_os = "linux")]
use std::ffi::CString;

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::CreateSymbolicLinkW;

#[cfg(target_os = "linux")]
use crate::op::{HardLinkOp, Op, SymlinkOp};

/// Creates a hard link at the destination path pointing to the source.
///
/// This is the async version of [`std::fs::hard_link`].
///
/// # Platform-specific behavior
///
/// - On Linux with `io_uring` support, this uses the `linkat` syscall directly.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::hard_link`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist
/// - `dst` already exists
/// - The source and destination are on different filesystems
/// - The process lacks permissions
///
/// # Panics
/// Panics if the I/O driver is in an invalid state.
pub async fn hard_link(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();

    #[cfg(target_os = "linux")]
    {
        let driver = crate::executor::current_driver();
        if driver.as_ref().is_some_and(|d| d.supports_completion()) {
            let src_cstr = CString::new(src.as_os_str().as_encoded_bytes()).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Invalid path: {e}"),
                )
            })?;
            let dst_cstr = CString::new(dst.as_os_str().as_encoded_bytes()).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Invalid path: {e}"),
                )
            })?;

            let driver = driver.expect("invalid driver state");
            let mut op = HardLinkOp::new(src_cstr, dst_cstr);
            return std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await;
        }
    }

    if crate::executor::offload_fs() {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::spawn_blocking(move || std::fs::hard_link(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::hard_link(src, dst)
    }
}

/// Creates a symbolic link to a directory.
///
/// This is the async version of [`std::os::unix::fs::symlink`] (on Unix) or
/// [`std::os::windows::fs::symlink_dir`] (on Windows).
///
/// # Platform-specific behavior
///
/// - On Linux with `io_uring` support, this uses the `symlinkat` syscall directly.
/// - On Windows, this uses [`std::os::windows::fs::symlink_dir`].
/// - On other Unix platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::os::unix::fs::symlink`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a directory
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
///
/// # Panics
/// Panics if the I/O driver is in an invalid state.
pub async fn symlink_dir(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();

    #[cfg(target_os = "linux")]
    {
        let driver = crate::executor::current_driver();
        if driver.as_ref().is_some_and(|d| d.supports_completion()) {
            // On Linux with io_uring, use SymlinkOp
            let src_cstr = CString::new(src.as_os_str().as_encoded_bytes()).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Invalid path: {e}"),
                )
            })?;
            let dst_cstr = CString::new(dst.as_os_str().as_encoded_bytes()).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Invalid path: {e}"),
                )
            })?;
            let driver = driver.expect("invalid driver state");
            let mut op = SymlinkOp::new(src_cstr, dst_cstr);
            return std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await;
        }
    }

    if crate::executor::offload_fs() {
        let src = src.to_owned();
        let dst = dst.to_owned();
        #[cfg(not(windows))]
        let result = crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?;
        #[cfg(windows)]
        let result = crate::spawn_blocking(move || std::os::windows::fs::symlink_dir(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?;
        result
    } else {
        #[cfg(not(windows))]
        let result = std::os::unix::fs::symlink(src, dst);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_dir(src, dst);
        result
    }
}

/// Creates a symbolic link to a file.
///
/// This is the async version of [`std::os::unix::fs::symlink`] (on Unix) or
/// [`std::os::windows::fs::symlink_file`] (on Windows).
///
/// # Platform-specific behavior
///
/// - On Linux with `io_uring` support, this uses the `symlinkat` syscall directly.
/// - On Windows, this uses [`std::os::windows::fs::symlink_file`].
/// - On other Unix platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::os::unix::fs::symlink`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a file
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
///
/// # Panics
/// Panics if the I/O driver is in an invalid state.
pub async fn symlink_file(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();

    #[cfg(target_os = "linux")]
    {
        let driver = crate::executor::current_driver();
        if driver.as_ref().is_some_and(|d| d.supports_completion()) {
            // On Linux with io_uring, use SymlinkOp
            let src_cstr = CString::new(src.as_os_str().as_encoded_bytes()).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Invalid path: {e}"),
                )
            })?;
            let dst_cstr = CString::new(dst.as_os_str().as_encoded_bytes()).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Invalid path: {e}"),
                )
            })?;
            let driver = driver.expect("invalid driver state");
            let mut op = SymlinkOp::new(src_cstr, dst_cstr);
            return std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await;
        }
    }

    if crate::executor::offload_fs() {
        let src = src.to_owned();
        let dst = dst.to_owned();
        #[cfg(not(windows))]
        let result = crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?;
        #[cfg(windows)]
        let result = crate::spawn_blocking(move || std::os::windows::fs::symlink_file(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?;
        result
    } else {
        #[cfg(not(windows))]
        let result = std::os::unix::fs::symlink(src, dst);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_file(src, dst);
        result
    }
}

/// Creates a symbolic link.
///
/// This is a convenience function that calls [`symlink_file`] or [`symlink_dir`].
/// Use this when you don't know or don't care whether the source is a file or
/// directory.
///
/// For explicit symlink creation, use [`symlink_file`] or [`symlink_dir`] instead.
///
/// # Platform-specific behavior
///
/// See [`symlink_file`] or [`symlink_dir`] for platform-specific behavior details.
///
/// # Errors
///
/// See [`symlink_file`] for error conditions.
pub async fn symlink(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if super::metadata(&src).await.is_ok_and(|m| m.is_dir()) {
        symlink_dir(src, dst).await
    } else {
        symlink_file(src, dst).await
    }
}
