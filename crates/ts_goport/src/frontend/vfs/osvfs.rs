//! Go: internal/vfs/osvfs/os.go. The realpath and symlink helpers it calls
//! are in `frontend::nativepath`. Off Linux: the Windows branches of os.go
//! and the Windows `path/filepath` pieces (`FromSlash`, `Abs`, `Clean`) are
//! ported; the other targets use the unix ones.
//!
//! The Go standard library pieces that osvfs reaches (`os.DirFS`,
//! `os.RemoveAll`, `filepath.Abs`, `filepath.Clean`) are ported here too.

use crate::frontend::prelude::*;
use std::borrow::Cow;
use std::cell::OnceCell;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write as _};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::{Path as OsPath, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

// PORT: a Go path is a Go string, so it can hold any bytes, and Go passes
// those bytes to the OS unchanged. A port string is the port form of a Go
// string (see `scanner_util::GO_STRING_MARKER`). `os_path` gives the OS the
// Go bytes, and `go_string_from_os` turns OS bytes (file names, link
// targets, the working directory, arguments) into the port form. Every OS
// call in the port goes through them.

/// The OS path of the port form path `path`: its Go bytes.
#[cfg(unix)]
pub fn os_path(path: &str) -> Cow<'_, OsPath> {
    match crate::scanner_util::go_string_bytes(path) {
        Cow::Borrowed(bytes) => Cow::Borrowed(OsPath::new(OsStr::from_bytes(bytes))),
        Cow::Owned(bytes) => Cow::Owned(PathBuf::from(OsString::from_vec(bytes))),
    }
}

/// The value form of the OS string `s` (see `os_path` and
/// `scanner_util::go_value_from_bytes`).
#[cfg(unix)]
pub fn go_string_from_os(s: impl Into<OsString>) -> String {
    match String::from_utf8(s.into().into_vec()) {
        Ok(text) => crate::scanner_util::go_string_from_utf8(text),
        Err(err) => crate::scanner_util::go_value_from_bytes(err.as_bytes()).into_owned(),
    }
}

// PORT divergence: off unix an OS path is text (UTF-16 on Windows), and the
// port converts it lossily. Go on Windows uses WTF-8 (go1.26
// syscall/wtf8_windows.go): `UTF16ToString` keeps an unpaired surrogate as
// its 3-byte WTF-8 form, and `UTF16FromString` turns those 3 bytes back into
// the surrogate, so a name read from the OS goes back to the OS unchanged.
// The port makes an unpaired surrogate U+FFFD. For other bytes that are not
// UTF-8, Go makes each byte U+FFFD, and `from_utf8_lossy` makes each bad
// sequence U+FFFD. A port of Go's form would use `encode_wide` and
// `from_wide` (`std::os::windows::ffi`). Not run on such a target.
#[cfg(not(unix))]
pub fn os_path(path: &str) -> Cow<'_, OsPath> {
    match crate::scanner_util::go_string_bytes(path) {
        Cow::Borrowed(_) => Cow::Borrowed(OsPath::new(path)),
        Cow::Owned(bytes) => {
            Cow::Owned(PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
        }
    }
}

#[cfg(not(unix))]
pub fn go_string_from_os(s: impl Into<OsString>) -> String {
    crate::scanner_util::go_string_from_utf8(s.into().to_string_lossy().into_owned())
}

/// The process arguments after the program name, in the port form (Go
/// `os.Args[1:]`, see `os_path`).
pub fn os_args() -> Vec<String> {
    std::env::args_os().skip(1).map(go_string_from_os).collect()
}

/// The current directory in the port form (Go `os.Getwd`, see `os_path`).
/// With an OS override installed, the override's directory.
/// `getwd_error_text` gives the Go text of an error.
// Go: os/getwd.go:26 Getwd (go1.26.4)
// PORT: on unix, `$PWD` when it is absolute and names the same file as "."
// (Go `SameFile`: the same device and inode), so a directory reached
// through a symlink keeps the link path. Else `syscall.Getwd` is
// `std::env::current_dir` (getcwd). Go's own walk up the parents, for a
// getcwd that fails with ENAMETOOLONG, is not ported: glibc's getcwd makes
// the same walk. On Windows Go calls `syscall.Getwd` only.
pub fn os_current_dir() -> io::Result<String> {
    if let Some(o) = OS_OVERRIDE.get() {
        return Ok(o.current_directory.clone());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Clumsy but widespread kludge:
        // if $PWD is set and matches ".", use it.
        if let Some(dir) = std::env::var_os("PWD")
            && dir.as_bytes().first() == Some(&b'/')
        {
            // Go returns the `*PathError` of this stat as it is.
            let dot = std::fs::metadata(".").map_err(|err| {
                let text = format!("stat .: {}", crate::fswatch::syscall::io_error_text(&err));
                io::Error::new(err.kind(), text)
            })?;
            if let Ok(d) = std::fs::metadata(&dir)
                && d.dev() == dot.dev()
                && d.ino() == dot.ino()
            {
                return Ok(go_string_from_os(dir));
            }
        }
    }
    std::env::current_dir().map(go_string_from_os)
}

/// Go `err.Error()` of an `os_current_dir` error: "getwd: <errno text>"
/// (`*os.SyscallError`) when getcwd failed, or "stat .: <errno text>"
/// (`*os.PathError`) when the stat of "." failed.
pub fn getwd_error_text(err: &io::Error) -> String {
    match err.raw_os_error() {
        Some(_) => format!("getwd: {}", crate::fswatch::syscall::io_error_text(err)),
        None => err.to_string(),
    }
}

// PORT: Go reads files and the current directory through `sys.FS()` and
// `sys.GetCurrentDirectory()`, so a Go test swaps the whole OS for its
// `TestSys`. The port's file system is `Rc`, so the parse, checker and emit
// threads cannot share `sys.FS()`: they call `osvfs_fs()` and
// `os_current_dir()` directly. A test process installs an `OsOverride` once
// at start, and those calls then reach the test file system and directory.
// Only test processes install it; a real run never does and keeps the OS
// behavior below unchanged.

/// The file system and current directory that replace the OS in a test
/// process (see `install_os_override`).
pub struct OsOverride {
    /// Makes the file system for one thread. `osvfs_fs` calls it once per
    /// thread. Each value must share one state (for example a map behind
    /// an `Arc<Mutex>`), so that all threads see the same files. It must
    /// not call `osvfs_fs` itself.
    pub fs: Arc<dyn Fn() -> Rc<dyn Fs> + Send + Sync>,
    /// The value of `os_current_dir`.
    pub current_directory: String,
}

static OS_OVERRIDE: OnceLock<OsOverride> = OnceLock::new();

/// Replaces the OS file system and current directory for the rest of the
/// process. Install it before the first `osvfs_fs` or `os_current_dir`
/// call. Panics when an override is already installed.
pub fn install_os_override(o: OsOverride) {
    assert!(
        OS_OVERRIDE.set(o).is_ok(),
        "osvfs: an OS override is already installed"
    );
}

/// True when `install_os_override` has run in this process.
pub fn os_override_installed() -> bool {
    OS_OVERRIDE.get().is_some()
}

// PORT: the Go semaphores `blockingOpSema`, `readSema` and `writeSema`
// (os.go:20) limit concurrent syscalls. The port is single-threaded, so they
// are not ported.

// Go: os.go:30 FS
// FS creates a new FS from the OS file system.
// PORT: the Go package function `osvfs.FS` is `osvfs_fs`. Go returns one
// package-level value; this returns a clone of one per-thread value. With
// an OS override installed (a test process), the per-thread value is the
// one that the override's `fs` makes on the first call on that thread.
pub fn osvfs_fs() -> Rc<dyn Fs> {
    thread_local! {
        // Go: os.go:34 osVFS
        static OS_VFS: Rc<dyn Fs> = Rc::new(OsFs {
            common: Common {
                root_for: os_dir_fs,
                is_reparse_point: Some(is_reparse_point),
            },
        });
        static OVERRIDE_FS: OnceCell<Rc<dyn Fs>> = const { OnceCell::new() };
    }
    if let Some(o) = OS_OVERRIDE.get() {
        return OVERRIDE_FS.with(|cell| Rc::clone(cell.get_or_init(|| (o.fs)())));
    }
    OS_VFS.with(Rc::clone)
}

// Go: os.go:174 isReparsePoint
fn is_reparse_point(path: &str) -> bool {
    crate::frontend::nativepath::is_symlink_or_reparse_point(&filepath_from_slash(path))
}

// Go: os.go:41 osFS
pub struct OsFs {
    common: Common,
}

// Go: os.go:46 isFileSystemCaseSensitive
// We do this right at startup to minimize the chance that executable gets moved or deleted.
// PORT: Go computes this in package init. The port computes it on first use.
// The wasm branch does not apply to the port.
fn is_file_system_case_sensitive() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| {
        // win32/win64 are case insensitive platforms
        if cfg!(windows) {
            return false;
        }

        // As a proxy for case-insensitivity, we check if the current executable exists under a different case.
        // This is not entirely correct, since different OSs can have differing case sensitivity in different paths,
        // but this is largely good enough for our purposes (and what sys.ts used to do with __filename).
        let exe = match crate::frontend::osutil::executable() {
            Ok(exe) => exe,
            Err(err) => panic!("vfs: failed to get executable path: {err}"),
        };

        // If the current executable exists under a different case, we must be case-insensitive.
        let swapped = swap_case(&exe);
        if let Err(err) = std::fs::metadata(os_path(&swapped)) {
            if err.kind() == io::ErrorKind::NotFound {
                return true;
            }
            panic!("vfs: failed to stat {swapped:?}: {err}");
        }
        false
    })
}

// Go: os.go:77 swapCase
// Convert all lowercase chars to uppercase, and vice-versa
fn swap_case(str: &str) -> String {
    str.chars()
        .map(|r| {
            let upper = simple_to_upper(r);
            if upper == r {
                simple_to_lower(r)
            } else {
                upper
            }
        })
        .collect()
}

// PORT: Go `unicode.ToUpper` uses the simple one-rune case mapping. Rust
// only has the full mapping; a mapping to more than one char is treated as
// no simple mapping.
fn simple_to_upper(r: char) -> char {
    let mut it = r.to_uppercase();
    match (it.next(), it.next()) {
        (Some(c), None) => c,
        _ => r,
    }
}

// PORT: Go `unicode.ToLower`; see `simple_to_upper`.
fn simple_to_lower(r: char) -> char {
    let mut it = r.to_lowercase();
    match (it.next(), it.next()) {
        (Some(c), None) => c,
        _ => r,
    }
}

impl Fs for OsFs {
    // Go: os.go:88 UseCaseSensitiveFileNames
    fn use_case_sensitive_file_names(&self) -> bool {
        is_file_system_case_sensitive()
    }

    // Go: os.go:92 ReadFile
    fn read_file(&self, path: &str) -> (String, bool) {
        self.common.read_file(path)
    }

    // Go: os.go:97 DirectoryExists
    fn directory_exists(&self, path: &str) -> bool {
        self.common.directory_exists(path)
    }

    // Go: os.go:102 FileExists
    fn file_exists(&self, path: &str) -> bool {
        self.common.file_exists(path)
    }

    // Go: os.go:107 GetAccessibleEntries
    fn get_accessible_entries(&self, path: &str) -> Entries {
        self.common.get_accessible_entries(path)
    }

    // Go: os.go:112 Stat
    fn stat(&self, path: &str) -> Option<FileInfo> {
        self.common.stat(path)
    }

    // Go: os.go:152 Realpath
    fn realpath(&self, path: &str) -> String {
        os_fs_realpath(path)
    }

    // Go: os.go:174 WriteFile
    fn write_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.write_file_ensuring_dir(path, content, WriteFlag::Truncate)
    }

    // Go: os.go:178 AppendFile
    fn append_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.write_file_ensuring_dir(path, content, WriteFlag::Append)
    }

    // Go: os.go:213 Remove
    fn remove(&self, path: &str) -> Result<(), FsError> {
        // todo: #701 add retry mechanism?
        os_remove_all(path)
    }

    // Go: os.go:219 Chtimes
    // PORT: Go `os.Chtimes` is utimensat(AT_FDCWD, path, times, 0) on unix,
    // so it works on a file without read permission; a zero Go time
    // (`None` here) is UTIME_OMIT. Off unix, Rust std sets times through an
    // open file (opened read-only here).
    #[cfg(unix)]
    fn chtimes(
        &self,
        path: &str,
        a_time: Option<SystemTime>,
        m_time: Option<SystemTime>,
    ) -> Result<(), FsError> {
        use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, UTIME_OMIT};
        // Go: syscall.NsecToTimespec(t.UnixNano())
        let timespec = |time: Option<SystemTime>| match time {
            None => Timespec {
                tv_sec: 0,
                tv_nsec: UTIME_OMIT,
            },
            Some(time) => {
                let (sec, nsec) = match time.duration_since(SystemTime::UNIX_EPOCH) {
                    Ok(d) => (d.as_secs() as i64, i64::from(d.subsec_nanos())),
                    Err(err) => {
                        let d = err.duration();
                        let (sec, nsec) = (-(d.as_secs() as i64), -i64::from(d.subsec_nanos()));
                        if nsec < 0 {
                            (sec - 1, nsec + 1_000_000_000)
                        } else {
                            (sec, nsec)
                        }
                    }
                };
                Timespec {
                    tv_sec: sec,
                    tv_nsec: nsec as _,
                }
            }
        };
        let times = Timestamps {
            last_access: timespec(a_time),
            last_modification: timespec(m_time),
        };
        rustix::fs::utimensat(CWD, &*os_path(path), &times, AtFlags::empty())
            .map_err(|err| FsError::path("chtimes", path, io::Error::from(err)))
    }

    #[cfg(not(unix))]
    fn chtimes(
        &self,
        path: &str,
        a_time: Option<SystemTime>,
        m_time: Option<SystemTime>,
    ) -> Result<(), FsError> {
        let file = std::fs::File::open(os_path(path))
            .map_err(|err| FsError::path("chtimes", path, err))?;
        let mut times = std::fs::FileTimes::new();
        if let Some(a_time) = a_time {
            times = times.set_accessed(a_time);
        }
        if let Some(m_time) = m_time {
            times = times.set_modified(m_time);
        }
        file.set_times(times)
            .map_err(|err| FsError::path("chtimes", path, err))
    }
}

// PORT: the Go `flag int` of `writeFileWithFlag`. Go passes
// `O_WRONLY|O_CREATE|O_TRUNC` or `O_WRONLY|O_CREATE|O_APPEND`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteFlag {
    Truncate,
    Append,
}

// Go: os.go:157 osFSRealpath
pub fn os_fs_realpath(path: &str) -> String {
    let _ = root_length(path); // Assert path is rooted

    let orig = path;
    let path = filepath_from_slash(path);
    let path = match crate::frontend::nativepath::realpath(&path) {
        Ok(path) => path,
        Err(_) => return orig.to_string(),
    };
    let path = match filepath_abs(&path) {
        Ok(path) => path,
        Err(_) => return orig.to_string(),
    };
    normalize_slashes(&path).into()
}

impl OsFs {
    // Go: os.go:173 writeFileWithFlag
    fn write_file_with_flag(
        &self,
        path: &str,
        content: &str,
        flag: WriteFlag,
    ) -> Result<(), FsError> {
        // PORT: Go's perm 0o666 is the std default create mode on unix.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true);
        match flag {
            WriteFlag::Truncate => options.truncate(true),
            WriteFlag::Append => options.append(true),
        };
        let mut file = options
            .open(os_path(path))
            .map_err(|err| FsError::path("open", path, err))?;

        // PORT: Go writes the string bytes unchanged. `content` is the port
        // form of the Go string (see `scanner_util::GO_STRING_MARKER`), so
        // write its Go bytes.
        file.write_all(&crate::scanner_util::go_string_bytes(content))
            .map_err(|err| FsError::path("write", path, err))?;

        Ok(())
    }

    // Go: os.go:189 ensureDirectoryExists
    fn ensure_directory_exists(&self, directory_path: &str) -> Result<(), FsError> {
        os_mkdir_all(directory_path, 0o777)
    }

    // Go: os.go:194 writeFileEnsuringDir
    fn write_file_ensuring_dir(
        &self,
        path: &str,
        content: &str,
        flag: WriteFlag,
    ) -> Result<(), FsError> {
        let _ = root_length(path); // Assert path is rooted
        if self.write_file_with_flag(path, content, flag).is_ok() {
            return Ok(());
        }
        let normalized: String = normalize_path(path).into();
        let directory: String = get_directory_path(&normalized).into();
        self.ensure_directory_exists(&directory)?;
        self.write_file_with_flag(path, content, flag)
    }
}

// Go: os.go:224 GetGlobalTypingsCacheLocation
// PORT: not ported. Only the language server (cmd/tsgo/lsp.go) calls it.

// Go: os/file.go DirFS
// PORT: Go standard library. `os.DirFS(dir)` with the `Stat`, `ReadDir`
// and `ReadFile` methods that `io/fs` helpers use.
pub fn os_dir_fs(dir: &str) -> Option<Box<dyn IoFs>> {
    Some(Box::new(DirFs {
        dir: dir.to_string(),
    }))
}

// Go: os/file.go dirFS
pub struct DirFs {
    dir: String,
}

impl DirFs {
    // Go: os/file.go dirFS.join
    // PORT: Go `filepathlite.Localize` on Unix is `fs.ValidPath` plus a
    // NUL byte check.
    fn join(&self, name: &str) -> Result<String, FsError> {
        if self.dir.is_empty() {
            return Err(FsError::Other("os: DirFS with empty root".to_string()));
        }
        if !io_fs_valid_path(name) || name.contains('\0') {
            return Err(FsError::Invalid);
        }
        if self.dir.ends_with('/') {
            return Ok(format!("{}{}", self.dir, name));
        }
        Ok(format!("{}/{}", self.dir, name))
    }
}

impl IoFs for DirFs {
    // Go: os/file.go dirFS.Stat
    fn stat(&self, name: &str) -> Result<FileInfo, FsError> {
        let fullname = self.join(name)?;
        // Go os.Stat follows symlinks.
        match std::fs::metadata(os_path(&fullname)) {
            Ok(md) => Ok(file_info_from_metadata(basename(&fullname), &md)),
            Err(err) => Err(FsError::path("stat", name, err)),
        }
    }

    // Go: os/file.go dirFS.ReadDir
    fn read_dir(&self, name: &str) -> Result<Vec<DirEntry>, FsError> {
        let fullname = self.join(name)?;
        os_read_dir(&fullname).map_err(|err| FsError::path("readdirent", name, err))
    }

    // Go: os/file.go dirFS.ReadFile
    fn read_file(&self, name: &str) -> Result<Vec<u8>, FsError> {
        let fullname = self.join(name)?;
        std::fs::read(os_path(&fullname)).map_err(|err| FsError::path("open", name, err))
    }
}

// Go: os/dir.go ReadDir
// PORT: Go standard library. Returns the entries sorted by file name. The
// entry type comes from the directory entry (d_type) and does not follow
// symlinks. Go skips an entry that is removed before its `lstat`; so does
// the port. Go returns the entries read before an error; the port returns
// only the error (callers here drop the entries on error).
fn os_read_dir(dirname: &str) -> io::Result<Vec<DirEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(os_path(dirname))? {
        let entry = entry?;
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        let typ = if file_type.is_dir() {
            FileMode::DIR
        } else if file_type.is_symlink() {
            FileMode::SYMLINK
        } else if file_type.is_file() {
            FileMode(0)
        } else {
            special_file_mode(file_type)
        };
        let name = go_string_from_os(entry.file_name());
        let full_path = format!("{}/{}", dirname, name);
        entries.push(DirEntry {
            name,
            typ,
            info: DirEntryInfo::Lstat(full_path),
        });
    }
    // Go sorts by the name bytes.
    // Go: os/dir.go:122 ReadDir: slices.SortFunc(dirs, bytealg.CompareString on the names)
    crate::gostd::slices::sort_func(&mut entries, |a, b| {
        crate::scanner_util::go_string_bytes(&a.name)
            .cmp(&crate::scanner_util::go_string_bytes(&b.name)) as i32
    });
    Ok(entries)
}

// PORT: the `os_read_dir` mode of an entry that is not a directory, a link
// or a regular file.
#[cfg(unix)]
fn special_file_mode(file_type: std::fs::FileType) -> FileMode {
    if file_type.is_block_device() {
        FileMode::DEVICE
    } else if file_type.is_char_device() {
        FileMode::DEVICE | FileMode::CHAR_DEVICE
    } else if file_type.is_fifo() {
        FileMode::NAMED_PIPE
    } else if file_type.is_socket() {
        FileMode::SOCKET
    } else {
        FileMode::IRREGULAR
    }
}

// PORT: off unix, std names no other file type. Not run on such a target.
#[cfg(not(unix))]
fn special_file_mode(_: std::fs::FileType) -> FileMode {
    FileMode::IRREGULAR
}

// Go: os/removeall_at.go RemoveAll
// PORT: Go standard library. Removes path and any children. A missing
// path is not an error. Rust `remove_dir_all` does not follow symlinks,
// like Go.
fn os_remove_all(path: &str) -> Result<(), FsError> {
    if path.is_empty() {
        // fail silently to retain compatibility with previous behavior
        // of RemoveAll. See issue 28830.
        return Ok(());
    }

    // The rmdir system call does not permit removing ".",
    // so we don't permit it either.
    if ends_with_dot(path) {
        return Err(FsError::path(
            "RemoveAll",
            path,
            io::Error::from(io::ErrorKind::InvalidInput),
        ));
    }

    let os = os_path(path);
    let result = match std::fs::symlink_metadata(&os) {
        Err(err) => Err(err),
        Ok(md) if md.is_dir() => std::fs::remove_dir_all(&os),
        Ok(_) => std::fs::remove_file(&os),
    };
    match result {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(FsError::path("unlinkat", path, err)),
        Ok(()) => Ok(()),
    }
}

// Go: os/path.go:19 MkdirAll (go1.26.4)
// MkdirAll creates a directory named path,
// along with any necessary parents, and returns nil,
// or else returns an error.
// PORT: Go standard library. Go `Stat`, `Mkdir` and `Lstat` are
// `std::fs::metadata`, `DirBuilder::create` and `std::fs::symlink_metadata`.
// Off unix `perm` does not apply, as in Go. pprof.rs uses it too.
pub fn os_mkdir_all(path: &str, perm: u32) -> Result<(), FsError> {
    // Fast path: if we can tell whether path is a directory or file, stop with success or error.
    if let Ok(dir) = std::fs::metadata(os_path(path)) {
        if dir.is_dir() {
            return Ok(());
        }
        return Err(FsError::path(
            "mkdir",
            path,
            io::Error::from_raw_os_error(ENOTDIR),
        ));
    }

    // Slow path: make sure parent exists and then call Mkdir for path.

    // Extract the parent folder from path by first removing any trailing
    // path separator and then scanning backward until finding a path
    // separator or reaching the beginning of the string.
    let p = path.as_bytes();
    let mut i = p.len() as isize - 1;
    while i >= 0 && is_path_separator(p[i as usize]) {
        i -= 1;
    }
    while i >= 0 && !is_path_separator(p[i as usize]) {
        i -= 1;
    }
    if i < 0 {
        i = 0;
    }

    // If there is a parent directory, and it is not the volume name,
    // recurse to ensure parent directory exists.
    let parent = &path[..i as usize];
    if parent.len() > volume_name_len(path) {
        os_mkdir_all(parent, perm)?;
    }

    // Parent now exists; invoke Mkdir and use its result.
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, perm);
    if let Err(err) = builder.create(os_path(path)) {
        // Handle arguments like "foo/." by
        // double-checking that directory doesn't exist.
        if std::fs::symlink_metadata(os_path(path)).is_ok_and(|dir| dir.is_dir()) {
            return Ok(());
        }
        return Err(FsError::path("mkdir", path, err));
    }
    Ok(())
}

// Go: syscall.ENOTDIR (go1.26.4 syscall/zerrors_linux_amd64.go; darwin and
// the BSDs have the same value). On Windows it is ERROR_PATH_NOT_FOUND
// (syscall/zerrors_windows.go).
#[cfg(not(windows))]
const ENOTDIR: i32 = 0x14;
#[cfg(windows)]
const ENOTDIR: i32 = 3;

// Go: os.IsPathSeparator ('/' and, on Windows, '\\')
fn is_path_separator(c: u8) -> bool {
    c == b'/' || (cfg!(windows) && c == b'\\')
}

// Go: len(filepathlite.VolumeName(path)). There is no volume name on unix.
#[cfg(not(windows))]
fn volume_name_len(_: &str) -> usize {
    0
}
#[cfg(windows)]
fn volume_name_len(path: &str) -> usize {
    filepath_volume_name_len(path.as_bytes())
}

// Go: os/path.go endsWithDot
fn ends_with_dot(path: &str) -> bool {
    if path == "." {
        return true;
    }
    let b = path.as_bytes();
    b.len() >= 2 && b[b.len() - 1] == b'.' && b[b.len() - 2] == b'/'
}

// Go: path/filepath/path.go FromSlash
/// FromSlash returns the result of replacing each slash ('/') character in
/// path with a separator character. Multiple slashes are replaced by
/// multiple separators. On unix it is the identity.
pub fn filepath_from_slash(path: &str) -> Cow<'_, str> {
    if cfg!(windows) && path.contains('/') {
        return Cow::Owned(path.replace('/', "\\"));
    }
    Cow::Borrowed(path)
}

// Go: path/filepath/path.go Abs (unix)
// PORT: Go standard library.
#[cfg(not(windows))]
fn filepath_abs(path: &str) -> Result<String, FsError> {
    if path.starts_with('/') {
        return Ok(filepath_clean(path));
    }
    let wd = os_current_dir().map_err(|err| FsError::path("getwd", path, err))?;
    // Go: filepath.Join(wd, path)
    if path.is_empty() {
        return Ok(filepath_clean(&wd));
    }
    Ok(filepath_clean(&format!("{wd}/{path}")))
}

// Go: path/filepath/path_windows.go abs
// PORT: Go standard library. Go `syscall.FullPath` is GetFullPathNameW, which
// `std::path::absolute` calls on Windows.
#[cfg(windows)]
fn filepath_abs(path: &str) -> Result<String, FsError> {
    // syscall.FullPath returns an error on empty path, because it's not a valid path.
    // To implement Abs behavior of returning working directory on empty string input,
    // special-case empty path by changing it to "." path. See golang.org/issue/24441.
    let path = if path.is_empty() { "." } else { path };
    let full_path = std::path::absolute(os_path(path))
        .map_err(|err| FsError::path("GetFullPathName", path, err))?;
    Ok(filepath_clean(&go_string_from_os(full_path)))
}

// Go: path/filepath/path.go Clean (unix)
// PORT: Go standard library. Lexical cleanup only.
#[cfg(not(windows))]
pub fn filepath_clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let p = path.as_bytes();
    let rooted = p[0] == b'/';
    let n = p.len();

    // Invariants:
    //	reading from path; r is index of next byte to process.
    //	writing to out; w is index of next byte to write.
    //	dotdot is index in out where .. must stop, either because
    //		it is the leading slash or it is a leading ../../.. prefix.
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let (mut r, mut dotdot) = (0usize, 0usize);
    if rooted {
        out.push(b'/');
        r = 1;
        dotdot = 1;
    }

    while r < n {
        if p[r] == b'/' {
            // empty path element
            r += 1;
        } else if p[r] == b'.' && (r + 1 == n || p[r + 1] == b'/') {
            // . element
            r += 1;
        } else if p[r] == b'.' && p[r + 1] == b'.' && (r + 2 == n || p[r + 2] == b'/') {
            // .. element: remove to last /
            r += 2;
            if out.len() > dotdot {
                // can backtrack
                let mut w = out.len() - 1;
                while w > dotdot && out[w] != b'/' {
                    w -= 1;
                }
                out.truncate(w);
            } else if !rooted {
                // cannot backtrack, but not rooted, so append .. element.
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(b"..");
                dotdot = out.len();
            }
        } else {
            // real path element.
            // add slash if needed
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            // copy element
            while r < n && p[r] != b'/' {
                out.push(p[r]);
                r += 1;
            }
        }
    }

    // Turn empty string into "."
    if out.is_empty() {
        return ".".to_string();
    }
    // The input is valid UTF-8 and the cuts are at ASCII '/' bytes.
    String::from_utf8(out)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

// Go: internal/filepathlite/path.go Clean (windows)
// PORT: Go standard library (go1.27 internal/filepathlite, path_windows.go
// for the volume name and postClean). Go's `lazybuf` is `out` plus
// `changed` (Go's `buf != nil`: the output is no longer a prefix of the
// input).
#[cfg(windows)]
pub fn filepath_clean(path: &str) -> String {
    const SEPARATOR: u8 = b'\\';
    let original_path = path;
    let vol_len = filepath_volume_name_len(path.as_bytes());
    let p = &path.as_bytes()[vol_len..];
    if p.is_empty() {
        let o = original_path.as_bytes();
        if vol_len > 1 && win_is_path_separator(o[0]) && win_is_path_separator(o[1]) {
            // should be UNC
            return original_path.replace('/', "\\");
        }
        return format!("{original_path}.");
    }
    let rooted = win_is_path_separator(p[0]);

    // Invariants:
    //	reading from path; r is index of next byte to process.
    //	writing to buf; w is index of next byte to write.
    //	dotdot is index in buf where .. must stop, either because
    //		it is the leading slash or it is a leading ../../.. prefix.
    let n = p.len();
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut changed = false;
    let append = |out: &mut Vec<u8>, changed: &mut bool, c: u8| {
        if !*changed && (out.len() >= n || p[out.len()] != c) {
            *changed = true;
        }
        out.push(c);
    };
    let (mut r, mut dotdot) = (0usize, 0usize);
    if rooted {
        append(&mut out, &mut changed, SEPARATOR);
        r = 1;
        dotdot = 1;
    }

    while r < n {
        if win_is_path_separator(p[r]) {
            // empty path element
            r += 1;
        } else if p[r] == b'.' && (r + 1 == n || win_is_path_separator(p[r + 1])) {
            // . element
            r += 1;
        } else if p[r] == b'.'
            && p[r + 1] == b'.'
            && (r + 2 == n || win_is_path_separator(p[r + 2]))
        {
            // .. element: remove to last separator
            r += 2;
            if out.len() > dotdot {
                // can backtrack
                let mut w = out.len() - 1;
                while w > dotdot && !win_is_path_separator(out[w]) {
                    w -= 1;
                }
                out.truncate(w);
            } else if !rooted {
                // cannot backtrack, but not rooted, so append .. element.
                if !out.is_empty() {
                    append(&mut out, &mut changed, SEPARATOR);
                }
                append(&mut out, &mut changed, b'.');
                append(&mut out, &mut changed, b'.');
                dotdot = out.len();
            }
        } else {
            // real path element.
            // add slash if needed
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                append(&mut out, &mut changed, SEPARATOR);
            }
            // copy element
            while r < n && !win_is_path_separator(p[r]) {
                append(&mut out, &mut changed, p[r]);
                r += 1;
            }
        }
    }

    // Turn empty string into "."
    if out.is_empty() {
        append(&mut out, &mut changed, b'.');
    }

    // postClean: avoid creating absolute paths on Windows
    if vol_len == 0 && changed {
        // If a ':' appears in the path element at the start of a path,
        // insert a .\ at the beginning to avoid converting relative paths
        // like a/../c: into c:.
        let first = out
            .iter()
            .position(|&c| win_is_path_separator(c))
            .unwrap_or(out.len());
        if out[..first].contains(&b':') {
            out.splice(0..0, [b'.', SEPARATOR]);
        } else if out.len() >= 3
            && win_is_path_separator(out[0])
            && out[1] == b'?'
            && out[2] == b'?'
        {
            // If a path begins with \??\, insert a \. at the beginning
            // to avoid converting paths like \a\..\??\c:\x into \??\c:\x
            // (equivalent to c:\x).
            out.splice(0..0, [SEPARATOR, b'.']);
        }
    }

    let mut result = original_path.as_bytes()[..vol_len].to_vec();
    result.extend_from_slice(&out);
    // FromSlash. The cuts are at ASCII bytes, so the bytes stay UTF-8.
    String::from_utf8(result)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
        .replace('/', "\\")
}

// Go: internal/filepathlite/path_windows.go IsPathSeparator
#[cfg(windows)]
pub fn win_is_path_separator(c: u8) -> bool {
    c == b'\\' || c == b'/'
}

// Go: internal/filepathlite/path_windows.go volumeNameLen
#[cfg(windows)]
pub fn filepath_volume_name_len(path: &[u8]) -> usize {
    if path.len() >= 2 && path[1] == b':' {
        // Path starts with a drive letter.
        //
        // Not all Windows functions necessarily enforce the requirement that
        // drive letters be in the set A-Z, and we don't try to here.
        //
        // We don't handle the case of a path starting with a non-ASCII character,
        // in which case the "drive letter" might be multiple bytes long.
        return 2;
    }
    if path.is_empty() || !win_is_path_separator(path[0]) {
        // Path does not have a volume component.
        return 0;
    }
    if win_path_has_prefix_fold(path, br"\\.")
        || win_path_has_prefix_fold(path, br"\\?")
        || win_path_has_prefix_fold(path, br"\??")
    {
        // Path starts with a device prefix: \\.\ for Local Device paths,
        // or \\?\ or \??\ for Root Local Device paths.
        if path.len() == 3 {
            return 3; // exactly \\., \\?, or \??
        }
        if win_path_has_prefix_fold(&path[4..], b"UNC") {
            // We're going to treat the UNC host and share as part of the volume
            // prefix for historical reasons, but this isn't really principled;
            // Windows's own GetFullPathName will happily remove the first
            // component of the path in this space, converting
            // \\.\unc\a\b\..\c into \\.\unc\a\c.
            return win_valid_volume_name_len(path, win_unc_len(path, br"\\.\UNC\".len()));
        }
        // We treat the next component after the device prefix as
        // part of the volume name, which means Clean(`\\?\c:\`)
        // won't remove the trailing \. (See #64028.)
        return match win_cut_path(&path[4..]) {
            None => win_valid_volume_name_len(path, path.len()),
            Some((_, rest)) => win_valid_volume_name_len(path, path.len() - rest.len() - 1),
        };
    }
    if path.len() >= 2 && win_is_path_separator(path[1]) {
        // Path starts with \\, and is a UNC path.
        return win_valid_volume_name_len(path, win_unc_len(path, 2));
    }
    0
}

// Go: internal/filepathlite/path_windows.go validVolumeNameLen
#[cfg(windows)]
fn win_valid_volume_name_len(path: &[u8], n: usize) -> usize {
    let mut p = &path[..n];
    while !p.is_empty() {
        let (part, rest) = win_cut_path(p).unwrap_or((p, &[]));
        if part == b".." {
            return 0;
        }
        p = rest;
    }
    n
}

// Go: internal/filepathlite/path_windows.go pathHasPrefixFold
// pathHasPrefixFold tests whether the path s begins with prefix,
// ignoring case and treating all path separators as equivalent.
// If s is longer than prefix, then s[len(prefix)] must be a path separator.
#[cfg(windows)]
fn win_path_has_prefix_fold(s: &[u8], prefix: &[u8]) -> bool {
    if s.len() < prefix.len() {
        return false;
    }
    for i in 0..prefix.len() {
        if win_is_path_separator(prefix[i]) {
            if !win_is_path_separator(s[i]) {
                return false;
            }
        } else if prefix[i].to_ascii_uppercase() != s[i].to_ascii_uppercase() {
            return false;
        }
    }
    if s.len() > prefix.len() && !win_is_path_separator(s[prefix.len()]) {
        return false;
    }
    true
}

// Go: internal/filepathlite/path_windows.go uncLen
// uncLen returns the length of the volume prefix of a UNC path.
// prefixLen is the prefix prior to the start of the UNC host;
// for example, for "//host/share", the prefixLen is len("//")==2.
#[cfg(windows)]
fn win_unc_len(path: &[u8], prefix_len: usize) -> usize {
    let mut count = 0;
    for (i, &c) in path.iter().enumerate().skip(prefix_len) {
        if win_is_path_separator(c) {
            count += 1;
            if count == 2 {
                return i;
            }
        }
    }
    path.len()
}

// Go: internal/filepathlite/path_windows.go cutPath
// cutPath slices path around the first path separator.
// PORT: Go's `found == false` is `None`.
#[cfg(windows)]
fn win_cut_path(path: &[u8]) -> Option<(&[u8], &[u8])> {
    let i = path.iter().position(|&c| win_is_path_separator(c))?;
    Some((&path[..i], &path[i + 1..]))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    // PORT: not in Go. Go `os.Chtimes` (utimensat on the path) sets the
    // mtime of a file that its owner cannot read; so does `chtimes`. The
    // test skips when the file still opens (root, CAP_DAC_OVERRIDE): then a
    // `chtimes` that opens the file would pass too.
    #[test]
    fn chtimes_without_read_permission() {
        let dir = std::env::temp_dir().join(format!("ts_goport_chtimes_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out.js");
        std::fs::write(&file, "x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&file).is_ok() {
            let _ = std::fs::remove_dir_all(&dir);
            eprintln!("skipped: a file without read permission opens here");
            return;
        }
        let m_time = SystemTime::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 5);
        let result = osvfs_fs().chtimes(file.to_str().unwrap(), None, Some(m_time));
        let modified = std::fs::symlink_metadata(&file)
            .unwrap()
            .modified()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(result.is_ok());
        assert_eq!(modified, m_time);
    }
}
