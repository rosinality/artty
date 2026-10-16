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
        let name = process_name(pgrp).map(|name| script_name(pgrp, &name).unwrap_or(name));
        ProcessInfo { foreground: name, at_shell: pgrp == self.shell_pid }
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

/// Interpreters whose processes are better named after the script they run
/// (Gemini CLI is `node .../bin/gemini`).
const INTERPRETERS: &[&str] = &["node", "bun", "deno", "python", "python3"];

/// For an interpreter, the name of the script it runs.
fn script_name(pid: i32, name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    if !INTERPRETERS.contains(&lower.as_str()) && !lower.starts_with("python") {
        return None;
    }
    let args = process_args(pid)?;
    let script = args.iter().skip(1).find(|arg| !arg.starts_with('-'))?;
    let file = std::path::Path::new(script).file_stem()?.to_str()?;
    // `node /usr/lib/node_modules/x/dist/index.js`: the package is the name.
    if matches!(file, "index" | "main" | "cli") {
        let parts: Vec<&str> = script.split('/').collect();
        let package = parts.iter().rposition(|p| *p == "node_modules").and_then(|i| parts.get(i + 1))?;
        return Some(package.trim_start_matches('@').rsplit('/').next()?.to_owned());
    }
    Some(file.to_owned())
}

/// A process's arguments (macOS: `KERN_PROCARGS2`).
#[cfg(target_os = "macos")]
fn process_args(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: a size query with a null buffer.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` is valid for `size` bytes.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    buf.truncate(size);
    // Layout: argc (i32), executable path, NUL padding, then argc arguments.
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let mut fields = buf[4..].split(|&b| b == 0).filter(|field| !field.is_empty());
    fields.next()?; // The executable path.
    Some(fields.take(argc).map(|field| String::from_utf8_lossy(field).into_owned()).collect())
}

#[cfg(target_os = "linux")]
fn process_args(pid: i32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(raw.split(|&b| b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_args(_pid: i32) -> Option<Vec<String>> {
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

    #[test]
    fn interpreter_scripts_are_named() {
        // A real interpreter process: python running a script file.
        let dir = std::env::temp_dir().join(format!("artty-script-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("my-agent.py");
        std::fs::write(&script, "import time\ntime.sleep(5)\n").unwrap();
        let Ok(mut child) = std::process::Command::new("python3").arg(&script).spawn() else { return };
        std::thread::sleep(std::time::Duration::from_millis(300));
        let pid = child.id() as i32;
        let name = process_name(pid).unwrap_or_default();
        let resolved = script_name(pid, if name.starts_with("python") { "python3" } else { &name });
        let _ = child.kill();
        assert_eq!(resolved.as_deref(), Some("my-agent"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn inspects_own_process() {
        let pid = std::process::id() as i32;
        assert!(process_name(pid).is_some());
        assert_eq!(process_cwd(pid), std::env::current_dir().ok());
    }
}
