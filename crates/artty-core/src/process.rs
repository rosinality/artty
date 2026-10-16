//! What is running in a terminal: the foreground process and the shell's
//! working directory.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

/// Handle for asking the OS about a session's processes.
pub(crate) struct ProcessProbe {
    /// Duplicate of the PTY controller, used for `tcgetpgrp`.
    pty: OwnedFd,
    shell_pid: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessInfo {
    /// Name of the foreground process (e.g. `fish`, `vim`, `claude`).
    pub foreground: Option<String>,
    /// The foreground process is the shell itself, i.e. nothing is running.
    pub at_shell: bool,
}

impl ProcessProbe {
    pub(crate) fn new(pty: RawFd, shell_pid: u32) -> Option<Self> {
        // SAFETY: `dup` returns a new descriptor we own, or -1.
        let fd = unsafe { libc::dup(pty) };
        (fd >= 0).then(|| Self {
            // SAFETY: `fd` is a fresh descriptor owned by nobody else.
            pty: unsafe { OwnedFd::from_raw_fd(fd) },
            shell_pid: shell_pid as i32,
        })
    }

    pub(crate) fn info(&self) -> ProcessInfo {
        // SAFETY: plain syscall on a descriptor we own.
        let pgrp = unsafe { libc::tcgetpgrp(self.pty.as_raw_fd()) };
        if pgrp <= 0 {
            return ProcessInfo::default();
        }
        ProcessInfo { foreground: process_name(pgrp), at_shell: pgrp == self.shell_pid }
    }

    /// The shell's current directory.
    pub(crate) fn shell_cwd(&self) -> Option<PathBuf> {
        process_cwd(self.shell_pid)
    }
}

#[cfg(target_os = "macos")]
fn process_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for `buf.len()` bytes.
    let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (len > 0).then(|| String::from_utf8_lossy(&buf[..len as usize]).into_owned())
}

#[cfg(target_os = "macos")]
fn process_cwd(pid: i32) -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::mem::{MaybeUninit, size_of};

    let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = size_of::<libc::proc_vnodepathinfo>() as i32;
    // SAFETY: `info` is valid for `size` bytes and zero-initialized.
    let written = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, info.as_mut_ptr().cast(), size)
    };
    if written != size {
        return None;
    }
    // SAFETY: the kernel filled the struct; the path is NUL-terminated.
    let info = unsafe { info.assume_init() };
    let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    let path = PathBuf::from(path.to_str().ok()?);
    (!path.as_os_str().is_empty()).then_some(path)
}

#[cfg(target_os = "linux")]
fn process_name(pid: i32) -> Option<String> {
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(name.trim_end().to_owned())
}

#[cfg(target_os = "linux")]
fn process_cwd(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_name(_pid: i32) -> Option<String> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_cwd(_pid: i32) -> Option<PathBuf> {
    None
}

/// The account's login shell, from the user database.
pub(crate) fn login_shell() -> Option<String> {
    use std::ffi::CStr;
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut pw = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the sizes given.
    let status = unsafe {
        libc::getpwuid_r(libc::getuid(), pw.as_mut_ptr(), buf.as_mut_ptr(), buf.len(), &mut result)
    };
    if status != 0 || result.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r filled `pw`, and pw_shell points into `buf`.
    let shell = unsafe { CStr::from_ptr(pw.assume_init().pw_shell) };
    shell.to_str().ok().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Decode an OSC 7 `file://host/path` URL into a path.
pub(crate) fn path_from_file_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let mut bytes = Vec::with_capacity(path.len());
    let mut iter = path.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hex = [iter.next()?, iter.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls() {
        assert_eq!(path_from_file_url("file://host/Users/me/a%20b"), Some(PathBuf::from("/Users/me/a b")));
        assert_eq!(path_from_file_url("file:///tmp"), Some(PathBuf::from("/tmp")));
        assert_eq!(path_from_file_url("http://x/y"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn inspects_own_process() {
        let pid = std::process::id() as i32;
        assert!(process_name(pid).is_some());
        assert_eq!(process_cwd(pid), std::env::current_dir().ok());
    }
}
