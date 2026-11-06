//! Each window's status line: the `[status]` templates (see
//! `artty_core::status`) filled in from the window's focused pane. What's
//! slow to find out runs off the main loop and is kept: commands'
//! output, on their intervals, and whether a repository has changes;
//! the branch is read from the repository's files.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use artty_core::layout::PaneId;
use artty_core::protocol::{StatusLine, WindowId};
use artty_core::status::{Filled, LocalTime, Template};
use artty_core::{StatusCommand, StatusConfig, Theme};

/// How long a directory's branch is taken as known.
const HEAD_FRESH: Duration = Duration::from_secs(1);
/// How often a repository is checked for changes, and how long that may take.
const DIRTY_EVERY: Duration = Duration::from_secs(5);
const DIRTY_TIMEOUT: Duration = Duration::from_secs(2);
/// A command that failed or took too long shows this, dimmed.
pub(crate) const WARNING: &str = "⚠";

/// What runs off the main loop, by what it's for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Job {
    /// `{cmd:NAME}` for a window.
    Command(String, WindowId),
    /// Whether the repository at this root has changes.
    Dirty(PathBuf),
}

/// Where a job's result goes (to the main loop, which hands it to
/// [`Status::finished`]).
pub(crate) type Done = Arc<dyn Fn(Job, Result<String, String>) + Send + Sync>;

/// What a status line shows of a window's focused pane.
#[derive(Clone, Debug, Default)]
pub(crate) struct Facts {
    pub pane: PaneId,
    pub cwd: Option<PathBuf>,
    /// The command running.
    pub command: Option<String>,
    pub exit: Option<i32>,
    /// How long the last command took.
    pub duration: Option<Duration>,
    /// Its agent and what it's doing (`claude working`).
    pub agent: Option<String>,
    /// Panes of the window whose agent waits for the user.
    pub waiting: usize,
    pub progress: Option<u8>,
    /// What the window's panes listen on.
    pub ports: Vec<u16>,
}

/// A job's last result, and whether it's running.
#[derive(Default)]
struct Kept {
    output: Option<Result<String, String>>,
    ran: Option<Instant>,
    running: bool,
    /// The pane and directory it last ran for.
    context: Option<(PaneId, Option<PathBuf>)>,
}

pub(crate) struct Status {
    left: Template,
    right: Template,
    values: Values,
}

struct Values {
    commands: BTreeMap<String, StatusCommand>,
    kept: HashMap<Job, Kept>,
    /// Each directory's repository root and branch, and when read.
    heads: HashMap<PathBuf, (Instant, Option<(PathBuf, String)>)>,
    /// Names not known, logged once each.
    warned: HashSet<String>,
    host: String,
    user: String,
    home: Option<PathBuf>,
    done: Done,
}

impl Status {
    pub(crate) fn new(config: &StatusConfig, done: Done) -> Status {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut status = Status {
            left: Template::default(),
            right: Template::default(),
            values: Values {
                commands: BTreeMap::new(),
                kept: HashMap::new(),
                heads: HashMap::new(),
                warned: HashSet::new(),
                host: host_name(),
                user: user_name(),
                home,
                done,
            },
        };
        status.configure(config);
        status
    }

    /// The config's templates and commands (again, after it changed).
    pub(crate) fn configure(&mut self, config: &StatusConfig) {
        self.left = Template::parse(&config.left);
        self.right = Template::parse(&config.right);
        if self.values.commands != config.commands {
            self.values.commands = config.commands.clone();
            self.values.kept.retain(|job, _| matches!(job, Job::Dirty(_)));
        }
        self.values.warned.clear();
    }

    /// Nothing to show, ever.
    pub(crate) fn is_empty(&self) -> bool {
        self.left.is_empty() && self.right.is_empty()
    }

    /// `window`'s line, its focused pane's `facts` filled in (with what's
    /// kept; what's due is started).
    pub(crate) fn line(
        &mut self,
        window: WindowId,
        facts: Option<&Facts>,
        theme: &Theme,
    ) -> StatusLine {
        if self.is_empty() {
            return StatusLine::default();
        }
        let now = Instant::now();
        let mut time = None;
        artty_core::status::line(&self.left, &self.right, theme, |name| {
            self.values.value(name, window, facts, now, &mut time)
        })
    }

    /// A job's result arrived: whether what it shows changed.
    pub(crate) fn finished(&mut self, job: Job, result: Result<String, String>) -> bool {
        let Some(kept) = self.values.kept.get_mut(&job) else { return false };
        kept.running = false;
        if let (Err(why), Job::Command(name, _)) = (&result, &job)
            && kept.output.as_ref().is_none_or(|o| o.as_ref().err() != Some(why))
        {
            log::warn!("status command {name}: {why}");
        }
        let changed = kept.output.as_ref() != Some(&result);
        kept.output = Some(result);
        changed
    }

    /// Let go of what's kept for windows that are gone, and directories
    /// not looked at lately.
    pub(crate) fn retain(&mut self, windows: impl Fn(WindowId) -> bool) {
        self.values.kept.retain(|job, kept| match job {
            Job::Command(_, window) => windows(*window),
            Job::Dirty(_) => kept.ran.is_some_and(|r| r.elapsed() < DIRTY_EVERY * 4),
        });
        self.values.heads.retain(|_, (read, _)| read.elapsed() < HEAD_FRESH * 60);
    }
}

impl Values {
    fn value(
        &mut self,
        name: &str,
        window: WindowId,
        facts: Option<&Facts>,
        now: Instant,
        time: &mut Option<LocalTime>,
    ) -> Filled {
        let text = match name.split_once(':') {
            Some(("time", format)) => time.get_or_insert_with(LocalTime::now).format(format),
            Some(("cmd", command)) => return self.command(command, window, facts, now),
            _ => match name {
                "time" => time.get_or_insert_with(LocalTime::now).format("%H:%M"),
                "host" => self.host.clone(),
                "user" => self.user.clone(),
                "cwd" => facts
                    .and_then(|f| f.cwd.as_deref())
                    .map(|cwd| short_path(cwd, self.home.as_deref()))
                    .unwrap_or_default(),
                "git.branch" => self.head(facts, now).map(|(_, branch)| branch).unwrap_or_default(),
                "git.dirty" => match self.head(facts, now) {
                    Some((root, _)) => self.dirty(root, now),
                    None => String::new(),
                },
                "command" => facts.and_then(|f| f.command.clone()).unwrap_or_default(),
                "exit" => match facts.and_then(|f| f.exit) {
                    Some(exit) if exit != 0 => exit.to_string(),
                    _ => String::new(),
                },
                "duration" => facts.and_then(|f| f.duration).map(duration).unwrap_or_default(),
                "agent" => facts.and_then(|f| f.agent.clone()).unwrap_or_default(),
                "agents.waiting" => match facts.map_or(0, |f| f.waiting) {
                    0 => String::new(),
                    n => n.to_string(),
                },
                "progress" => {
                    facts.and_then(|f| f.progress).map(|p| format!("{p}%")).unwrap_or_default()
                },
                "ports" => facts
                    .map(|f| f.ports.iter().map(u16::to_string).collect::<Vec<_>>().join(","))
                    .unwrap_or_default(),
                _ => {
                    if self.warned.insert(name.to_owned()) {
                        log::warn!("status: no value {{{name}}}");
                    }
                    String::new()
                },
            },
        };
        Filled::from(text)
    }

    /// `{cmd:NAME}`: its kept output, run again when due.
    fn command(
        &mut self,
        name: &str,
        window: WindowId,
        facts: Option<&Facts>,
        now: Instant,
    ) -> Filled {
        let Some(spec) = self.commands.get(name).cloned() else {
            if self.warned.insert(format!("cmd:{name}")) {
                log::warn!("status: no [status.commands.{name}]");
            }
            return Filled::default();
        };
        let context = (facts.map_or(0, |f| f.pane), facts.and_then(|f| f.cwd.clone()));
        let job = Job::Command(name.to_owned(), window);
        let kept = self.kept.entry(job.clone()).or_default();
        let due = kept.ran.is_none_or(|r| now.duration_since(r) >= spec.every)
            || kept.context.as_ref() != Some(&context);
        if due && !kept.running {
            kept.running = true;
            kept.ran = Some(now);
            kept.context = Some(context.clone());
            let cwd = context.1.clone().filter(|d| d.is_dir()).or_else(|| self.home.clone());
            let mut env = vec![("ARTTY_PANE".to_owned(), context.0.to_string())];
            if let Some(cwd) = &context.1 {
                env.push(("ARTTY_CWD".to_owned(), cwd.display().to_string()));
            }
            start(job, spec.run, cwd, env, spec.timeout, self.done.clone());
        }
        match &self.kept[&Job::Command(name.to_owned(), window)].output {
            Some(Ok(text)) => Filled::from(text.clone()),
            Some(Err(_)) => Filled { text: WARNING.to_owned(), dim: true },
            None => Filled::default(),
        }
    }

    /// The focused pane's repository root and branch.
    fn head(&mut self, facts: Option<&Facts>, now: Instant) -> Option<(PathBuf, String)> {
        let cwd = facts?.cwd.clone()?;
        let fresh = self.heads.get(&cwd).filter(|(read, _)| now.duration_since(*read) < HEAD_FRESH);
        if let Some((_, head)) = fresh {
            return head.clone();
        }
        let head = git_head(&cwd);
        self.heads.insert(cwd, (now, head.clone()));
        head
    }

    /// `*` if the repository has changes, as of its last check.
    fn dirty(&mut self, root: PathBuf, now: Instant) -> String {
        let job = Job::Dirty(root.clone());
        let kept = self.kept.entry(job.clone()).or_default();
        if !kept.running && kept.ran.is_none_or(|r| now.duration_since(r) >= DIRTY_EVERY) {
            kept.running = true;
            kept.ran = Some(now);
            let command = "git --no-optional-locks status --porcelain 2>/dev/null".to_owned();
            start(job, command, Some(root), Vec::new(), DIRTY_TIMEOUT, self.done.clone());
        }
        match &kept.output {
            Some(Ok(changes)) if !changes.is_empty() => "*".to_owned(),
            _ => String::new(),
        }
    }
}

/// Run `command` (with `sh -c`) on a thread of its own; `done` gets its
/// output's first line, trimmed, or why it failed.
fn start(
    job: Job,
    command: String,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
    timeout: Duration,
    done: Done,
) {
    let spawned = std::thread::Builder::new()
        .name("artty-status".into())
        .spawn(move || done(job, run(&command, cwd.as_deref(), env, timeout)));
    if let Err(err) = spawned {
        log::warn!("status: couldn't start a thread: {err}");
    }
}

/// `command`'s output's first line, trimmed; an error if it fails or
/// takes longer than `timeout` (it's killed then, with what it started).
pub(crate) fn run(
    command: &str,
    cwd: Option<&Path>,
    env: Vec<(String, String)>,
    timeout: Duration,
) -> Result<String, String> {
    let mut child = Command::new("sh");
    child
        .args(["-c", command])
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(cwd) = cwd {
        child.current_dir(cwd);
    }
    let mut child = child.spawn().map_err(|err| format!("couldn't run it: {err}"))?;
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut text);
            }
            String::from_utf8_lossy(&text).into_owned()
        })
    };
    let stdout = read(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = read(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                // SAFETY: a signal to the group it leads (made above).
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                return Err(format!("took longer than {}", duration(timeout)));
            },
            Err(err) => return Err(err.to_string()),
        }
    };
    let first = |text: String| text.lines().next().unwrap_or_default().trim().to_owned();
    let output = first(stdout.join().unwrap_or_default());
    if !status.success() {
        let why = first(stderr.join().unwrap_or_default());
        return Err(if why.is_empty() { status.to_string() } else { format!("{status}: {why}") });
    }
    Ok(output)
}

/// The repository `dir` is in, and its branch (or, detached, its commit,
/// shortened): read from its `.git`, as git would find it.
pub(crate) fn git_head(dir: &Path) -> Option<(PathBuf, String)> {
    let (root, git) = dir.ancestors().find_map(|root| {
        let git = root.join(".git");
        let meta = std::fs::metadata(&git).ok()?;
        if meta.is_dir() {
            return Some((root.to_owned(), git));
        }
        // A worktree's or submodule's: `gitdir: PATH`.
        let text = std::fs::read_to_string(&git).ok()?;
        let path = text.trim().strip_prefix("gitdir:")?.trim();
        Some((root.to_owned(), root.join(path)))
    })?;
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    let branch = match head.strip_prefix("ref:") {
        Some(reference) => {
            let reference = reference.trim();
            reference.strip_prefix("refs/heads/").unwrap_or(reference).to_owned()
        },
        None => head.chars().take(7).collect(),
    };
    Some((root, branch))
}

/// `~` for home, and only the last few folders of a long path.
pub(crate) fn short_path(path: &Path, home: Option<&Path>) -> String {
    const KEPT: usize = 3;
    let (prefix, rest) = match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => ("~", rest),
        None => ("", path.strip_prefix("/").unwrap_or(path)),
    };
    let parts: Vec<String> =
        rest.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if parts.len() > KEPT {
        return format!("…/{}", parts[parts.len() - KEPT..].join("/"));
    }
    match (prefix, parts.is_empty()) {
        ("~", true) => "~".to_owned(),
        ("~", false) => format!("~/{}", parts.join("/")),
        _ => format!("/{}", parts.join("/")),
    }
}

/// `42s`, `3m5s`, `1h2m`; nothing under a second.
pub(crate) fn duration(time: Duration) -> String {
    let seconds = time.as_secs();
    match seconds {
        0 if time.subsec_millis() > 0 => format!("{}ms", time.subsec_millis()),
        0 => String::new(),
        1..60 => format!("{seconds}s"),
        60..3600 => format!("{}m{}s", seconds / 60, seconds % 60),
        _ => format!("{}h{}m", seconds / 3600, seconds % 3600 / 60),
    }
}

fn host_name() -> String {
    let mut name = [0u8; 256];
    // SAFETY: the buffer is ours, its length given.
    let ok = unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } == 0;
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    let name = if ok { String::from_utf8_lossy(&name[..end]).into_owned() } else { String::new() };
    name.split('.').next().unwrap_or_default().to_owned()
}

fn user_name() -> String {
    if let Ok(user) = std::env::var("USER")
        && !user.is_empty()
    {
        return user;
    }
    // SAFETY: getpwuid's record is read at once, on this thread.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_name.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr((*entry).pw_name).to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("artty-status-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    type Results = mpsc::Receiver<(Job, Result<String, String>)>;

    /// A status whose jobs' results come on the returned channel.
    fn status(config: &str) -> (Status, Results) {
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let done: Done = Arc::new(move |job, result| {
            let _ = tx.lock().unwrap().send((job, result));
        });
        let config = artty_core::Config::parse(config).unwrap();
        (Status::new(&config.status, done), rx)
    }

    fn text(line: &StatusLine) -> (String, String) {
        let join = |s: &[artty_core::protocol::StatusSegment]| {
            s.iter().map(|s| s.text.as_str()).collect::<String>()
        };
        (join(&line.left), join(&line.right))
    }

    /// The next result, handed to the status.
    fn finish(status: &mut Status, rx: &Results) -> bool {
        let (job, result) = rx.recv_timeout(Duration::from_secs(10)).expect("no result");
        status.finished(job, result)
    }

    #[test]
    fn nothing_configured_is_nothing() {
        let (mut status, _rx) = status("");
        assert!(status.is_empty());
        assert!(status.line(1, Some(&Facts::default()), &Theme::default()).is_empty());
    }

    #[test]
    fn pane_values() {
        let (mut status, _rx) = status(
            "[status]\nleft = \"{user}@{host}\"\n\
             right = \"{command} ✘{exit} {duration} {agent} w{agents.waiting} {progress} :{ports} {nope}\"",
        );
        let facts = Facts {
            command: Some("make".into()),
            exit: Some(2),
            duration: Some(Duration::from_secs(65)),
            agent: Some("claude waiting".into()),
            waiting: 2,
            progress: Some(40),
            ports: vec![3000, 8080],
            ..Facts::default()
        };
        let line = status.line(1, Some(&facts), &Theme::default());
        let (left, right) = text(&line);
        assert!(left.contains('@') && !left.starts_with('@'), "{left}");
        assert_eq!(right, "make ✘2 1m5s claude waiting w2 40% :3000,8080");
        // Empty: gone, with their spaces; unknown names too.
        let facts = Facts { exit: Some(0), ..Facts::default() };
        assert_eq!(text(&status.line(1, Some(&facts), &Theme::default())).1, "");
        assert_eq!(status.values.warned.len(), 1);
    }

    #[test]
    fn paths_and_durations() {
        let home = Path::new("/home/me");
        assert_eq!(short_path(Path::new("/home/me"), Some(home)), "~");
        assert_eq!(short_path(Path::new("/home/me/src/artty"), Some(home)), "~/src/artty");
        assert_eq!(short_path(Path::new("/home/me/a/b/c/d"), Some(home)), "…/b/c/d");
        assert_eq!(short_path(Path::new("/etc"), Some(home)), "/etc");
        assert_eq!(short_path(Path::new("/"), None), "/");
        assert_eq!(duration(Duration::from_millis(250)), "250ms");
        assert_eq!(duration(Duration::ZERO), "");
        assert_eq!(duration(Duration::from_secs(42)), "42s");
        assert_eq!(duration(Duration::from_secs(3720)), "1h2m");
    }

    #[test]
    fn branches_from_the_repository_files() {
        let dir = scratch("git");
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(git_head(&repo.join("src/deep")), Some((repo.clone(), "feature/x".into())));
        std::fs::write(repo.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(git_head(&repo).unwrap().1, "0123456");
        // A worktree: `.git` names where its HEAD is.
        let tree = dir.join("tree");
        std::fs::create_dir_all(dir.join("repo/.git/worktrees/tree")).unwrap();
        std::fs::write(dir.join("repo/.git/worktrees/tree/HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(".git"), "gitdir: ../repo/.git/worktrees/tree\n").unwrap();
        assert_eq!(git_head(&tree).unwrap().1, "wt");
        assert_eq!(git_head(Path::new("/")), None);

        // Through the status line, read again after a moment.
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let (mut status, _rx) = status("[status]\nright = \"{git.branch} {cwd}\"");
        let facts = Facts { cwd: Some(repo.join("src")), ..Facts::default() };
        let line = status.line(1, Some(&facts), &Theme::default());
        assert!(text(&line).1.starts_with("main "), "{line:?}");
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/next\n").unwrap();
        let later = Instant::now() + HEAD_FRESH;
        assert_eq!(status.values.head(Some(&facts), later).unwrap().1, "next");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dirty_repositories() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = scratch("dirty");
        let git = |args: &[&str]| {
            Command::new("git").args(args).current_dir(&dir).output().unwrap();
        };
        git(&["init", "-q"]);
        let (mut status, rx) = status("[status]\nright = \"{git.branch}{git.dirty}\"");
        let facts = Facts { cwd: Some(dir.clone()), ..Facts::default() };
        let line = |status: &mut Status| text(&status.line(1, Some(&facts), &Theme::default())).1;
        let branch = line(&mut status);
        finish(&mut status, &rx);
        assert_eq!(line(&mut status), branch, "clean");
        std::fs::write(dir.join("new"), "x").unwrap();
        // Checked again once due.
        status.values.kept.values_mut().for_each(|k| k.ran = None);
        line(&mut status);
        assert!(finish(&mut status, &rx));
        assert_eq!(line(&mut status), format!("{branch}*"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn commands_run_on_their_interval_and_are_kept() {
        let dir = scratch("commands");
        let counter = dir.join("count");
        let config = format!(
            "[status]\nright = \"n{{cmd:count}} {{cmd:where}}\"\n\
             [status.commands.count]\nrun = \"echo x >> '{c}'; wc -l < '{c}'; echo second line\"\nevery = \"1h\"\n\
             [status.commands.where]\nrun = \"echo $ARTTY_PANE:$ARTTY_CWD:$(pwd)\"\n",
            c = counter.display()
        );
        let (mut status, rx) = status(&config);
        let facts = Facts { pane: 7, cwd: Some(dir.clone()), ..Facts::default() };
        let line = |status: &mut Status, facts: &Facts| {
            text(&status.line(1, Some(facts), &Theme::default())).1
        };
        // Nothing yet: the word goes.
        assert_eq!(line(&mut status, &facts), "");
        finish(&mut status, &rx);
        finish(&mut status, &rx);
        let real = dir.canonicalize().unwrap();
        let shown = line(&mut status, &facts);
        assert!(
            shown.starts_with("n1 7:") && shown.ends_with(&format!(":{}", real.display())),
            "{shown}"
        );
        // Kept: not run again before it's due (an hour).
        for _ in 0..5 {
            line(&mut status, &facts);
        }
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        // Another pane focused: run again for it at once.
        let other = Facts { pane: 8, ..facts.clone() };
        line(&mut status, &other);
        let mut results = vec![rx.recv_timeout(Duration::from_secs(10)).unwrap()];
        results.extend(rx.recv_timeout(Duration::from_secs(10)));
        for (job, result) in results {
            status.finished(job, result);
        }
        assert!(line(&mut status, &other).starts_with("n2 8:"));
        // Due again: run again.
        status.values.kept.values_mut().for_each(|k| k.ran = None);
        line(&mut status, &other);
        finish(&mut status, &rx);
        finish(&mut status, &rx);
        assert!(line(&mut status, &other).starts_with("n3 "));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failing_and_slow_commands_warn() {
        let (mut status, rx) = status(
            "[status]\nright = \"a{cmd:fails} b{cmd:slow} c{cmd:missing}\"\n\
             [status.commands.fails]\nrun = \"echo why >&2; exit 3\"\n\
             [status.commands.slow]\nrun = \"sleep 5; echo late\"\ntimeout = \"200ms\"\n",
        );
        let started = Instant::now();
        status.line(1, None, &Theme::default());
        finish(&mut status, &rx);
        finish(&mut status, &rx);
        assert!(started.elapsed() < Duration::from_secs(4), "waited for the slow one");
        let line = status.line(1, None, &Theme::default());
        assert_eq!(text(&line).1, format!("a{WARNING} b{WARNING}"));
        assert!(line.right.iter().any(|s| s.dim && s.text.contains(WARNING)));
        assert!(!line.right[0].dim);
        let fails = run("echo why >&2; exit 3", None, Vec::new(), Duration::from_secs(5));
        assert!(fails.unwrap_err().ends_with("why"));
        let slow = run("sleep 5", None, Vec::new(), Duration::from_millis(100));
        assert_eq!(slow.unwrap_err(), "took longer than 100ms");
    }
}
