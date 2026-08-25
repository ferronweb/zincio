use std::future::poll_fn;
use std::io::{self, ErrorKind};
use std::path::Path;

use crate::executor::current_driver;
use crate::op::Op;

#[cfg(target_os = "linux")]
use crate::op::OpenOp;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

#[cfg(windows)]
use crate::fd_inner::RawOsHandle;

use crate::fs::file::File;

const OPEN_READ: u8 = 1 << 0;
const OPEN_WRITE: u8 = 1 << 1;
const OPEN_APPEND: u8 = 1 << 2;
const OPEN_TRUNCATE: u8 = 1 << 3;
const OPEN_CREATE: u8 = 1 << 4;
const OPEN_CREATE_NEW: u8 = 1 << 5;

/// Internal grouping of the boolean open flags stored as a bitfield so the
/// public `OpenOptions` struct does not carry more than three separate `bool`
/// fields.
#[derive(Clone, Debug, Default)]
pub struct OpenOptionsFlags {
    bits: u8,
}

impl OpenOptionsFlags {
    #[inline]
    fn read(&self) -> bool {
        self.bits & OPEN_READ != 0
    }
    #[inline]
    fn write(&self) -> bool {
        self.bits & OPEN_WRITE != 0
    }
    #[inline]
    fn append(&self) -> bool {
        self.bits & OPEN_APPEND != 0
    }
    #[inline]
    fn truncate(&self) -> bool {
        self.bits & OPEN_TRUNCATE != 0
    }
    #[inline]
    fn create(&self) -> bool {
        self.bits & OPEN_CREATE != 0
    }
    #[inline]
    fn create_new(&self) -> bool {
        self.bits & OPEN_CREATE_NEW != 0
    }

    #[inline]
    fn set(&mut self, flag: u8, value: bool) {
        if value {
            self.bits |= flag;
        } else {
            self.bits &= !flag;
        }
    }

    #[inline]
    fn set_read(&mut self, value: bool) {
        self.set(OPEN_READ, value);
    }
    #[inline]
    fn set_write(&mut self, value: bool) {
        self.set(OPEN_WRITE, value);
    }
    #[inline]
    fn set_append(&mut self, value: bool) {
        self.set(OPEN_APPEND, value);
    }
    #[inline]
    fn set_truncate(&mut self, value: bool) {
        self.set(OPEN_TRUNCATE, value);
    }
    #[inline]
    fn set_create(&mut self, value: bool) {
        self.set(OPEN_CREATE, value);
    }
    #[inline]
    fn set_create_new(&mut self, value: bool) {
        self.set(OPEN_CREATE_NEW, value);
    }
}

/// Options and flags for opening files.
///
/// This struct provides a builder-style interface for configuring how a file
/// should be opened, similar to [`std::fs::OpenOptions`]. It supports both
/// `io_uring` completion-based opening on Linux and blocking thread pool fallback
/// for other platforms.
///
/// # Examples
///
/// ```ignore
/// use zincio::fs::OpenOptions;
///
/// // Open a file for reading
/// let file = OpenOptions::new()
///     .read(true)
///     .open("hello.txt")
///     .await?;
///
/// // Create a new file for writing (truncate if exists)
/// let file = OpenOptions::new()
///     .write(true)
///     .create(true)
///     .truncate(true)
///     .open("output.txt")
///     .await?;
///
/// ```
#[derive(Clone, Debug)]
pub struct OpenOptions {
    flags: OpenOptionsFlags,
}

impl OpenOptions {
    /// Creates a new `OpenOptions` with default values.
    ///
    /// By default, all options are set to `false`.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            flags: OpenOptionsFlags::default(),
        }
    }

    /// Sets whether the file should be opened for reading.
    #[inline]
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.flags.set_read(read);
        self
    }

    /// Sets whether the file should be opened for writing.
    #[inline]
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.flags.set_write(write);
        self
    }

    /// Sets whether the file should be opened in append mode.
    #[inline]
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.flags.set_append(append);
        self
    }

    /// Sets whether the file should be truncated if it already exists.
    #[inline]
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.flags.set_truncate(truncate);
        self
    }

    /// Sets whether the file should be created if it does not exist.
    #[inline]
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.flags.set_create(create);
        self
    }

    /// Sets whether the file should be created exclusively (fails if it already exists).
    #[inline]
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.flags.set_create_new(create_new);
        self
    }

    /// Validates the open options.
    ///
    /// This is an internal method used to ensure the options are valid.
    #[inline]
    fn validate(&self) -> io::Result<()> {
        let writing = self.flags.write() || self.flags.append();

        if !self.flags.read() && !writing {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "must enable read, write, or append access",
            ));
        }
        if (self.flags.truncate() || self.flags.create() || self.flags.create_new()) && !writing {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "truncate/create options require write or append access",
            ));
        }

        Ok(())
    }

    /// Returns the initial cursor position for the file.
    ///
    /// If append mode is enabled, the cursor is set to the end of the file.
    #[inline]
    fn initial_cursor_for_append(&self, file: &std::fs::File) -> io::Result<u64> {
        if self.flags.append() {
            Ok(file.metadata()?.len())
        } else {
            Ok(0)
        }
    }

    /// Opens a file with the configured options.
    ///
    /// This is the async version of [`std::fs::OpenOptions::open`].
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with `io_uring` support, this uses the `openat` syscall directly.
    /// - On other platforms, this either offloads to a blocking thread pool or falls back
    ///   to [`std::fs::OpenOptions::open`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The file cannot be opened with the specified options
    /// - The process lacks permissions to open the file
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use zincio::fs::OpenOptions;
    ///
    /// let file = OpenOptions::new()
    ///     .read(true)
    ///     .open("hello.txt")
    ///     .await?;
    /// ```
    #[inline]
    pub async fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        self.validate()?;
        let path = path.as_ref();

        let std_file = if let Some(driver) = current_driver() {
            #[cfg(target_os = "linux")]
            {
                if driver.supports_completion() {
                    let mut op = self.build_open_op(path)?;
                    let raw = poll_fn(move |cx| op.poll(cx, &driver)).await?;
                    unsafe { std::fs::File::from_raw_fd(raw) }
                } else if crate::offload_fs() {
                    self.open_in_blocking_pool(path).await?
                } else {
                    self.open_blocking(path)?
                }
            }

            #[cfg(not(target_os = "linux"))]
            {
                let _ = driver;
                if crate::offload_fs() {
                    self.open_in_blocking_pool(path).await?
                } else {
                    self.open_blocking(path)?
                }
            }
        } else {
            self.open_blocking(path)?
        };

        let cursor = self.initial_cursor_for_append(&std_file)?;
        Ok(File::from_std_with_cursor(std_file, cursor))
    }

    /// Opens a file in the blocking thread pool.
    ///
    /// This is an internal method used when `io_uring` is not available.
    #[inline]
    async fn open_in_blocking_pool(&self, path: &Path) -> io::Result<std::fs::File> {
        let path = path.to_path_buf();
        let read = self.flags.read();
        let write = self.flags.write();
        let append = self.flags.append();
        let truncate = self.flags.truncate();
        let create = self.flags.create();
        let create_new = self.flags.create_new();

        crate::spawn_blocking(move || {
            #[cfg(windows)]
            use std::os::windows::fs::OpenOptionsExt;
            #[cfg(windows)]
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

            let mut options = std::fs::OpenOptions::new();
            options
                .read(read)
                .write(write)
                .append(append)
                .truncate(truncate)
                .create(create)
                .create_new(create_new);
            #[cfg(windows)]
            options.attributes(FILE_FLAG_OVERLAPPED);
            options.open(path)
        })
        .await
        .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    }

    /// Opens a file synchronously.
    ///
    /// This is an internal method used when `io_uring` is not available.
    #[inline]
    fn open_blocking(&self, path: &Path) -> io::Result<std::fs::File> {
        #[cfg(windows)]
        use std::os::windows::fs::OpenOptionsExt;
        #[cfg(windows)]
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

        let mut options = std::fs::OpenOptions::new();
        options
            .read(self.flags.read())
            .write(self.flags.write())
            .append(self.flags.append())
            .truncate(self.flags.truncate())
            .create(self.flags.create())
            .create_new(self.flags.create_new());
        #[cfg(windows)]
        options.attributes(FILE_FLAG_OVERLAPPED);
        options.open(path)
    }

    /// Builds an `OpenOp` for use with `io_uring`.
    ///
    /// This is an internal method used on Linux with `io_uring` support.
    #[cfg(target_os = "linux")]
    #[inline]
    fn build_open_op(&self, path: &Path) -> io::Result<OpenOp> {
        let writing = self.flags.write() || self.flags.append();
        let mut flags = match (self.flags.read(), writing) {
            (true, false) => libc::O_RDONLY,
            (false, true) => libc::O_WRONLY,
            (true, true) => libc::O_RDWR,
            (false, false) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "must enable read, write, or append access",
                ))
            }
        };

        if self.flags.append() {
            flags |= libc::O_APPEND;
        }
        if self.flags.truncate() {
            flags |= libc::O_TRUNC;
        }
        if self.flags.create_new() {
            flags |= libc::O_CREAT | libc::O_EXCL;
        } else if self.flags.create() {
            flags |= libc::O_CREAT;
        }
        flags |= libc::O_CLOEXEC;

        let path_bytes = path.as_os_str().as_bytes();
        if path_bytes.contains(&0) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "path contains interior NUL byte",
            ));
        }

        let path = CString::new(path_bytes).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, "path contains interior NUL byte")
        })?;

        Ok(OpenOp::new(path, flags, 0o666))
    }
}

impl Default for OpenOptions {
    /// Returns the default `OpenOptions`.
    ///
    /// This is equivalent to `OpenOptions::new()`.
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}
