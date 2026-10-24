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
    /// The foreground process group.
    pub foreground_group: Option<i32>,
}

impl ProcessProbe {
    /// The duplicate of the PTY controller this probe holds.
    pub(crate) fn pty_fd(&self) -> RawFd {
        self.pty.as_raw_fd()
    }

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
        // The session's process is the shell, or `login` running the shell.
        let at_shell = pgrp == self.shell_pid || parent_pid(pgrp) == Some(self.shell_pid);
        ProcessInfo { foreground: name, at_shell, foreground_group: Some(pgrp) }
    }

    pub(crate) fn shell_pid(&self) -> i32 {
        self.shell_pid
    }

    /// The shell's current directory.
    pub(crate) fn shell_cwd(&self) -> Option<PathBuf> {
        process_cwd(self.shell_pid)
    }
}

#[cfg(target_os = "macos")]
fn parent_pid(pid: i32) -> Option<i32> {
    use std::mem::{MaybeUninit, size_of};
    let mut bsd = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: `bsd` is valid for `size` bytes.
    if unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, bsd.as_mut_ptr().cast(), size) }
        != size
    {
        return None;
    }
    // SAFETY: filled by the kernel.
    Some(unsafe { bsd.assume_init() }.pbi_ppid as i32)
}

#[cfg(not(target_os = "macos"))]
fn parent_pid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
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

/// One process, as sampled for `artty ps`.
#[derive(Clone, Debug)]
pub struct ProcessSample {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub name: String,
    /// CPU time used so far, in nanoseconds.
    pub cpu_ns: u64,
    /// Resident memory in bytes.
    pub memory: u64,
    /// Start time, in seconds since the Unix epoch.
    pub started: f64,
}

/// This user's processes.
#[cfg(target_os = "macos")]
// libc's Mach bindings are deprecated in favor of the `mach2` crate, but this
// one call is all that's needed.
#[allow(deprecated)]
pub fn processes() -> Vec<ProcessSample> {
    use std::mem::{MaybeUninit, size_of};

    // Task CPU times are in Mach time units (not nanoseconds on Apple Silicon).
    let mut timebase = libc::mach_timebase_info { numer: 1, denom: 1 };
    // SAFETY: fills the struct.
    unsafe { libc::mach_timebase_info(&mut timebase) };
    let to_ns = |ticks: u64| {
        (ticks as u128 * timebase.numer as u128 / timebase.denom.max(1) as u128) as u64
    };

    // SAFETY: a size query, then a buffer of that size.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0i32; count as usize + 64];
    let bytes = (pids.len() * size_of::<i32>()) as i32;
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(count.max(0) as usize);

    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    let mut samples = Vec::new();
    for pid in pids.into_iter().filter(|&pid| pid > 0) {
        let mut bsd = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: `bsd` is valid for `size` bytes.
        if unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, bsd.as_mut_ptr().cast(), size)
        } != size
        {
            continue;
        }
        // SAFETY: filled by the kernel.
        let bsd = unsafe { bsd.assume_init() };
        if bsd.pbi_uid != uid {
            continue;
        }
        let mut task = MaybeUninit::<libc::proc_taskinfo>::zeroed();
        let size = size_of::<libc::proc_taskinfo>() as i32;
        // SAFETY: `task` is valid for `size` bytes.
        let (cpu_ns, memory) = if unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, task.as_mut_ptr().cast(), size)
        } == size
        {
            // SAFETY: filled by the kernel.
            let task = unsafe { task.assume_init() };
            (to_ns(task.pti_total_user + task.pti_total_system), task.pti_resident_size)
        } else {
            (0, 0)
        };
        let name = process_name(pid).unwrap_or_default();
        let name = script_name(pid, &name).unwrap_or(name);
        samples.push(ProcessSample {
            pid,
            ppid: bsd.pbi_ppid as i32,
            pgid: bsd.pbi_pgid as i32,
            name,
            cpu_ns,
            memory,
            started: bsd.pbi_start_tvsec as f64 + bsd.pbi_start_tvusec as f64 / 1e6,
        });
    }
    samples
}

#[cfg(not(target_os = "macos"))]
pub fn processes() -> Vec<ProcessSample> {
    Vec::new()
}

/// `root` and its descendants. `root` itself may be missing from the samples
/// (a pane's `login` process runs as root); its descendants are still found.
pub fn process_tree(samples: &[ProcessSample], root: i32) -> Vec<&ProcessSample> {
    let mut tree: Vec<&ProcessSample> = samples.iter().filter(|p| p.pid == root).collect();
    let mut parents = vec![root];
    while let Some(parent) = parents.pop() {
        for child in samples.iter().filter(|p| p.ppid == parent && p.pid != parent) {
            tree.push(child);
            parents.push(child.pid);
        }
    }
    tree
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
        let package =
            parts.iter().rposition(|p| *p == "node_modules").and_then(|i| parts.get(i + 1))?;
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
    if unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0)
    } != 0
    {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` is valid for `size` bytes.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
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
    Some(
        raw.split(|&b| b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect(),
    )
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
        assert_eq!(
            path_from_file_url("file://host/Users/me/a%20b"),
            Some(PathBuf::from("/Users/me/a b"))
        );
        assert_eq!(path_from_file_url("file:///tmp"), Some(PathBuf::from("/tmp")));
        assert_eq!(path_from_file_url("http://x/y"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn samples_own_process_tree() {
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let me = std::process::id() as i32;
        let samples = processes();
        let tree = process_tree(&samples, me);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(tree.first().map(|p| p.pid), Some(me));
        assert!(tree.iter().any(|p| p.pid == child.id() as i32 && p.name == "sleep"));
        assert!(tree[0].memory > 0 && tree[0].cpu_ns > 0);
    }

    #[test]
    fn interpreter_scripts_are_named() {
        // A real interpreter process: python running a script file.
        let dir = std::env::temp_dir().join(format!("artty-script-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("my-agent.py");
        std::fs::write(&script, "import time\ntime.sleep(5)\n").unwrap();
        let Ok(mut child) = std::process::Command::new("python3").arg(&script).spawn() else {
            return;
        };
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
