use crate::fdentry::{Descriptor, FdEntry};
use crate::sys::fdentry_impl::OsHandle;
use crate::virtfs::{VirtualDir, VirtualDirEntry};
use crate::wasi::{self, WasiError, WasiResult};
use std::borrow::Borrow;
use std::collections::HashMap;
use std::ffi::{self, CString, OsString};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::{env, io, string};

/// Possible errors when `WasiCtxBuilder` fails building
/// `WasiCtx`.
#[derive(Debug, thiserror::Error)]
pub enum WasiCtxBuilderError {
    /// General I/O error was encountered.
    #[error("general I/O error encountered: {0}")]
    Io(#[from] io::Error),
    /// Provided sequence of bytes was not a valid UTF-8.
    #[error("provided sequence is not valid UTF-8: {0}")]
    InvalidUtf8(#[from] string::FromUtf8Error),
    /// Provided sequence of bytes was not a valid UTF-16.
    ///
    /// This error is expected to only occur on Windows hosts.
    #[error("provided sequence is not valid UTF-16: {0}")]
    InvalidUtf16(#[from] string::FromUtf16Error),
    /// Provided sequence of bytes contained an unexpected NUL byte.
    #[error("provided sequence contained an unexpected NUL byte")]
    UnexpectedNul(#[from] ffi::NulError),
    /// Provided `File` is not a directory.
    #[error("preopened directory path {} is not a directory", .0.display())]
    NotADirectory(PathBuf),
    /// `WasiCtx` has too many opened files.
    #[error("context object has too many opened files")]
    TooManyFilesOpen,
}

type WasiCtxBuilderResult<T> = std::result::Result<T, WasiCtxBuilderError>;

enum PendingFdEntry {
    Thunk(fn() -> io::Result<FdEntry>),
    File(File),
}

impl std::fmt::Debug for PendingFdEntry {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Thunk(f) => write!(
                fmt,
                "PendingFdEntry::Thunk({:p})",
                f as *const fn() -> io::Result<FdEntry>
            ),
            Self::File(f) => write!(fmt, "PendingFdEntry::File({:?})", f),
        }
    }
}

#[derive(Debug, Eq, Hash, PartialEq)]
enum PendingCString {
    Bytes(Vec<u8>),
    OsString(OsString),
}

impl From<Vec<u8>> for PendingCString {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Bytes(bytes)
    }
}

impl From<OsString> for PendingCString {
    fn from(s: OsString) -> Self {
        Self::OsString(s)
    }
}

impl PendingCString {
    fn into_string(self) -> WasiCtxBuilderResult<String> {
        let res = match self {
            Self::Bytes(v) => String::from_utf8(v)?,
            #[cfg(unix)]
            Self::OsString(s) => {
                use std::os::unix::ffi::OsStringExt;
                String::from_utf8(s.into_vec())?
            }
            #[cfg(windows)]
            Self::OsString(s) => {
                use std::os::windows::ffi::OsStrExt;
                let bytes: Vec<u16> = s.encode_wide().collect();
                String::from_utf16(&bytes)?
            }
        };
        Ok(res)
    }

    /// Create a `CString` containing valid UTF-8.
    fn into_utf8_cstring(self) -> WasiCtxBuilderResult<CString> {
        let s = self.into_string()?;
        let s = CString::new(s)?;
        Ok(s)
    }
}

/// A builder allowing customizable construction of `WasiCtx` instances.
pub struct WasiCtxBuilder {
    fds: Option<HashMap<wasi::__wasi_fd_t, PendingFdEntry>>,
    preopens: Option<Vec<(PathBuf, Descriptor)>>,
    args: Option<Vec<PendingCString>>,
    env: Option<HashMap<PendingCString, PendingCString>>,
}

impl WasiCtxBuilder {
    /// Builder for a new `WasiCtx`.
    pub fn new() -> Self {
        let mut fds = HashMap::new();

        fds.insert(0, PendingFdEntry::Thunk(FdEntry::null));
        fds.insert(1, PendingFdEntry::Thunk(FdEntry::null));
        fds.insert(2, PendingFdEntry::Thunk(FdEntry::null));

        Self {
            fds: Some(fds),
            preopens: Some(Vec::new()),
            args: Some(Vec::new()),
            env: Some(HashMap::new()),
        }
    }

    /// Add arguments to the command-line arguments list.
    ///
    /// Arguments must be valid UTF-8 with no NUL bytes, or else `WasiCtxBuilder::build()` will fail.
    pub fn args<S: AsRef<[u8]>>(&mut self, args: impl IntoIterator<Item = S>) -> &mut Self {
        self.args
            .as_mut()
            .unwrap()
            .extend(args.into_iter().map(|a| a.as_ref().to_vec().into()));
        self
    }

    /// Add an argument to the command-line arguments list.
    ///
    /// Arguments must be valid UTF-8 with no NUL bytes, or else `WasiCtxBuilder::build()` will fail.
    pub fn arg<S: AsRef<[u8]>>(&mut self, arg: S) -> &mut Self {
        self.args
            .as_mut()
            .unwrap()
            .push(arg.as_ref().to_vec().into());
        self
    }

    /// Inherit the command-line arguments from the host process.
    ///
    /// If any arguments from the host process contain invalid UTF-8, `WasiCtxBuilder::build()` will
    /// fail.
    pub fn inherit_args(&mut self) -> &mut Self {
        let args = self.args.as_mut().unwrap();
        args.clear();
        args.extend(env::args_os().map(PendingCString::OsString));
        self
    }

    /// Inherit stdin from the host process.
    pub fn inherit_stdin(&mut self) -> &mut Self {
        self.fds
            .as_mut()
            .unwrap()
            .insert(0, PendingFdEntry::Thunk(FdEntry::duplicate_stdin));
        self
    }

    /// Inherit stdout from the host process.
    pub fn inherit_stdout(&mut self) -> &mut Self {
        self.fds
            .as_mut()
            .unwrap()
            .insert(1, PendingFdEntry::Thunk(FdEntry::duplicate_stdout));
        self
    }

    /// Inherit stdout from the host process.
    pub fn inherit_stderr(&mut self) -> &mut Self {
        self.fds
            .as_mut()
            .unwrap()
            .insert(2, PendingFdEntry::Thunk(FdEntry::duplicate_stderr));
        self
    }

    /// Inherit the stdin, stdout, and stderr streams from the host process.
    pub fn inherit_stdio(&mut self) -> &mut Self {
        let fds = self.fds.as_mut().unwrap();
        fds.insert(0, PendingFdEntry::Thunk(FdEntry::duplicate_stdin));
        fds.insert(1, PendingFdEntry::Thunk(FdEntry::duplicate_stdout));
        fds.insert(2, PendingFdEntry::Thunk(FdEntry::duplicate_stderr));
        self
    }

    /// Inherit the environment variables from the host process.
    ///
    /// If any environment variables from the host process contain invalid Unicode (UTF-16 for
    /// Windows, UTF-8 for other platforms), `WasiCtxBuilder::build()` will fail.
    pub fn inherit_env(&mut self) -> &mut Self {
        let env = self.env.as_mut().unwrap();
        env.clear();
        env.extend(std::env::vars_os().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Add an entry to the environment.
    ///
    /// Environment variable keys and values must be valid UTF-8 with no NUL bytes, or else
    /// `WasiCtxBuilder::build()` will fail.
    pub fn env<S: AsRef<[u8]>>(&mut self, k: S, v: S) -> &mut Self {
        self.env
            .as_mut()
            .unwrap()
            .insert(k.as_ref().to_vec().into(), v.as_ref().to_vec().into());
        self
    }

    /// Add entries to the environment.
    ///
    /// Environment variable keys and values must be valid UTF-8 with no NUL bytes, or else
    /// `WasiCtxBuilder::build()` will fail.
    pub fn envs<S: AsRef<[u8]>, T: Borrow<(S, S)>>(
        &mut self,
        envs: impl IntoIterator<Item = T>,
    ) -> &mut Self {
        self.env.as_mut().unwrap().extend(envs.into_iter().map(|t| {
            let (k, v) = t.borrow();
            (k.as_ref().to_vec().into(), v.as_ref().to_vec().into())
        }));
        self
    }

    /// Provide a File to use as stdin
    pub fn stdin(&mut self, file: File) -> &mut Self {
        self.fds
            .as_mut()
            .unwrap()
            .insert(0, PendingFdEntry::File(file));
        self
    }

    /// Provide a File to use as stdout
    pub fn stdout(&mut self, file: File) -> &mut Self {
        self.fds
            .as_mut()
            .unwrap()
            .insert(1, PendingFdEntry::File(file));
        self
    }

    /// Provide a File to use as stderr
    pub fn stderr(&mut self, file: File) -> &mut Self {
        self.fds
            .as_mut()
            .unwrap()
            .insert(2, PendingFdEntry::File(file));
        self
    }

    /// Add a preopened directory.
    pub fn preopened_dir<P: AsRef<Path>>(&mut self, dir: File, guest_path: P) -> &mut Self {
        self.preopens.as_mut().unwrap().push((
            guest_path.as_ref().to_owned(),
            Descriptor::OsHandle(OsHandle::from(dir)),
        ));
        self
    }

    /// Add a preopened virtual directory.
    pub fn preopened_virt<P: AsRef<Path>>(
        &mut self,
        dir: VirtualDirEntry,
        guest_path: P,
    ) -> &mut Self {
        fn populate_directory(virtentry: HashMap<String, VirtualDirEntry>, dir: &mut VirtualDir) {
            for (path, entry) in virtentry.into_iter() {
                match entry {
                    VirtualDirEntry::Directory(dir_entries) => {
                        let mut subdir = VirtualDir::new(true);
                        populate_directory(dir_entries, &mut subdir);
                        dir.add_dir(subdir, path);
                    }
                    VirtualDirEntry::File(content) => {
                        dir.add_file(content, path);
                    }
                }
            }
        }

        let dir = if let VirtualDirEntry::Directory(entries) = dir {
            let mut dir = VirtualDir::new(true);
            populate_directory(entries, &mut dir);
            Box::new(dir)
        } else {
            panic!("the root of a VirtualDirEntry tree must be a VirtualDirEntry::Directory");
        };

        self.preopens
            .as_mut()
            .unwrap()
            .push((guest_path.as_ref().to_owned(), Descriptor::VirtualFile(dir)));
        self
    }

    /// Build a `WasiCtx`, consuming this `WasiCtxBuilder`.
    ///
    /// If any of the arguments or environment variables in this builder cannot be converted into
    /// `CString`s, either due to NUL bytes or Unicode conversions, this will fail.
    pub fn build(&mut self) -> WasiCtxBuilderResult<WasiCtx> {
        // Process arguments and environment variables into `CString`s, failing quickly if they
        // contain any NUL bytes, or if conversion from `OsString` fails.
        let args = self
            .args
            .take()
            .unwrap()
            .into_iter()
            .map(|arg| arg.into_utf8_cstring())
            .collect::<WasiCtxBuilderResult<Vec<CString>>>()?;

        let env = self
            .env
            .take()
            .unwrap()
            .into_iter()
            .map(|(k, v)| {
                k.into_string().and_then(|mut pair| {
                    v.into_string().and_then(|v| {
                        pair.push('=');
                        pair.push_str(v.as_str());
                        // We have valid UTF-8, but the keys and values have not yet been checked
                        // for NULs, so we do a final check here.
                        let s = CString::new(pair)?;
                        Ok(s)
                    })
                })
            })
            .collect::<WasiCtxBuilderResult<Vec<CString>>>()?;

        let mut fds: HashMap<wasi::__wasi_fd_t, FdEntry> = HashMap::new();
        // Populate the non-preopen fds.
        for (fd, pending) in self.fds.take().unwrap() {
            log::debug!("WasiCtx inserting ({:?}, {:?})", fd, pending);
            match pending {
                PendingFdEntry::Thunk(f) => {
                    fds.insert(fd, f()?);
                }
                PendingFdEntry::File(f) => {
                    fds.insert(fd, FdEntry::from(Descriptor::OsHandle(OsHandle::from(f)))?);
                }
            }
        }
        // Then add the preopen fds. Startup code in the guest starts looking at fd 3 for preopens,
        // so we start from there. This variable is initially 2, though, because the loop
        // immediately does the increment and check for overflow.
        let mut preopen_fd: wasi::__wasi_fd_t = 2;
        for (guest_path, dir) in self.preopens.take().unwrap() {
            // We do the increment at the beginning of the loop body, so that we don't overflow
            // unnecessarily if we have exactly the maximum number of file descriptors.
            preopen_fd = preopen_fd
                .checked_add(1)
                .ok_or(WasiCtxBuilderError::TooManyFilesOpen)?;

            match &dir {
                Descriptor::OsHandle(handle) => {
                    if !handle.metadata()?.is_dir() {
                        return Err(WasiCtxBuilderError::NotADirectory(guest_path));
                    }
                }
                Descriptor::VirtualFile(virt) => {
                    if virt.get_file_type() != wasi::__WASI_FILETYPE_DIRECTORY {
                        return Err(WasiCtxBuilderError::NotADirectory(guest_path));
                    }
                }
                Descriptor::Stdin | Descriptor::Stdout | Descriptor::Stderr => {
                    panic!("implementation error, stdin/stdout/stderr shouldn't be in the list of preopens");
                }
            }

            // We don't currently allow setting file descriptors other than 0-2, but this will avoid
            // collisions if we restore that functionality in the future.
            while fds.contains_key(&preopen_fd) {
                preopen_fd = preopen_fd
                    .checked_add(1)
                    .ok_or(WasiCtxBuilderError::TooManyFilesOpen)?;
            }
            let mut fe = FdEntry::from(dir)?;
            fe.preopen_path = Some(guest_path);
            log::debug!("WasiCtx inserting ({:?}, {:?})", preopen_fd, fe);
            fds.insert(preopen_fd, fe);
            log::debug!("WasiCtx fds = {:?}", fds);
        }

        Ok(WasiCtx { args, env, fds })
    }
}

#[derive(Debug)]
pub struct WasiCtx {
    fds: HashMap<wasi::__wasi_fd_t, FdEntry>,
    pub(crate) args: Vec<CString>,
    pub(crate) env: Vec<CString>,
}

impl WasiCtx {
    /// Make a new `WasiCtx` with some default settings.
    ///
    /// - File descriptors 0, 1, and 2 inherit stdin, stdout, and stderr from the host process.
    ///
    /// - Environment variables are inherited from the host process.
    ///
    /// To override these behaviors, use `WasiCtxBuilder`.
    pub fn new<S: AsRef<[u8]>>(args: impl IntoIterator<Item = S>) -> WasiCtxBuilderResult<Self> {
        WasiCtxBuilder::new()
            .args(args)
            .inherit_stdio()
            .inherit_env()
            .build()
    }

    /// Check if `WasiCtx` contains the specified raw WASI `fd`.
    pub(crate) unsafe fn contains_fd_entry(&self, fd: wasi::__wasi_fd_t) -> bool {
        self.fds.contains_key(&fd)
    }

    /// Get an immutable `FdEntry` corresponding to the specified raw WASI `fd`.
    pub(crate) unsafe fn get_fd_entry(&self, fd: wasi::__wasi_fd_t) -> WasiResult<&FdEntry> {
        self.fds.get(&fd).ok_or(WasiError::EBADF)
    }

    /// Get a mutable `FdEntry` corresponding to the specified raw WASI `fd`.
    pub(crate) unsafe fn get_fd_entry_mut(
        &mut self,
        fd: wasi::__wasi_fd_t,
    ) -> WasiResult<&mut FdEntry> {
        self.fds.get_mut(&fd).ok_or(WasiError::EBADF)
    }

    /// Insert the specified `FdEntry` into the `WasiCtx` object.
    ///
    /// The `FdEntry` will automatically get another free raw WASI `fd` assigned. Note that
    /// the two subsequent free raw WASI `fd`s do not have to be stored contiguously.
    pub(crate) fn insert_fd_entry(&mut self, fe: FdEntry) -> WasiResult<wasi::__wasi_fd_t> {
        // Never insert where stdio handles are expected to be.
        let mut fd = 3;
        while self.fds.contains_key(&fd) {
            if let Some(next_fd) = fd.checked_add(1) {
                fd = next_fd;
            } else {
                return Err(WasiError::EMFILE);
            }
        }
        self.fds.insert(fd, fe);
        Ok(fd)
    }

    /// Insert the specified `FdEntry` with the specified raw WASI `fd` key into the `WasiCtx`
    /// object.
    pub(crate) fn insert_fd_entry_at(
        &mut self,
        fd: wasi::__wasi_fd_t,
        fe: FdEntry,
    ) -> Option<FdEntry> {
        self.fds.insert(fd, fe)
    }

    /// Remove `FdEntry` corresponding to the specified raw WASI `fd` from the `WasiCtx` object.
    pub(crate) fn remove_fd_entry(&mut self, fd: wasi::__wasi_fd_t) -> WasiResult<FdEntry> {
        self.fds.remove(&fd).ok_or(WasiError::EBADF)
    }
}
