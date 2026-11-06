//! A small artty client in a terminal, for trying [`artty_client`] against
//! a server: list its windows, or show one here.
//!
//!     artty-client --ssh [user@]host[:port] windows    (as ~/.ssh/config says)
//!     artty-client --ssh user@host attach [WINDOW] [--share]
//!     artty-client --command artty server --stdio -- attach
//!
//! Attached, the window's focused pane fills this terminal; typing goes to
//! it, and Ctrl-] leaves.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use artty_client::ssh::{self, Credentials, HostKey, KnownHosts, Login, Target};
use artty_client::ssh_config::{Config, Local};
use artty_client::{Client, Event, Link};
use artty_core::TerminalSize;
use artty_core::layout::PaneId;
use artty_core::protocol::{WindowId, WindowLayout};

fn main() {
    if let Err(err) = run() {
        eprintln!("artty-client: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (link, rest) = match args.first().map(String::as_str) {
        Some("--ssh") => {
            let destination = args.get(1).ok_or("--ssh needs a destination")?;
            let local = Local::here();
            let config =
                Config::read(&std::path::Path::new(&local.home).join(".ssh/config"), &local.home)
                    .unwrap_or_default();
            let plan = ssh::plan(&config, &local, destination)?;
            for ignored in &plan.ignored {
                eprintln!("(not done here: {ignored})");
            }
            let (artty, rest) = match args.get(2).map(String::as_str) {
                Some("--artty") => (args.get(3).cloned(), &args[4.min(args.len())..]),
                _ => (None, &args[2..]),
            };
            let target = &plan.target;
            let through: Vec<&str> = plan.hops.iter().map(|h| h.alias.as_str()).collect();
            let user = target.user.as_deref().unwrap_or_default();
            eprintln!(
                "Logging in to {user}@{}:{} (through {through:?})…",
                target.host_name, target.port
            );
            let (link, _forwards) = ssh::connect(
                &plan,
                &ssh::server_command(artty.as_deref()),
                &Credentials::usual(),
                KnownHosts::usual(),
                Arc::new(TerminalLogin),
            )?;
            (link, rest.to_vec())
        },
        Some("--command") => {
            let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
            let link = Link::command(&args[1..end])?;
            (link, args[(end + 1).min(args.len())..].to_vec())
        },
        _ => return Err(usage()),
    };
    let (client, events) = Client::start(link, env!("CARGO_PKG_VERSION"))?;
    eprintln!("Connected (artty {}).", client.server_version);
    match rest.first().map(String::as_str) {
        Some("windows") => list(&client, &events),
        Some("attach") => {
            let window = rest.get(1).and_then(|w| w.parse().ok());
            let share = rest.iter().any(|a| a == "--share");
            attach(&client, &events, window, share)
        },
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "usage: artty-client (--ssh [user@]host[:port] [--artty PATH] | --command CMD... --) \
     (windows | attach [WINDOW] [--share])"
        .to_owned()
}

fn list(client: &Client, events: &std::sync::mpsc::Receiver<Event>) -> Result<(), String> {
    client.list_windows()?;
    loop {
        match events.recv_timeout(Duration::from_secs(10)).map_err(|_| "no answer")? {
            Event::Windows(windows) => {
                for w in windows {
                    let shown = match w.apps {
                        0 => "not shown".to_owned(),
                        1 => "shown in 1 app".to_owned(),
                        n => format!("shown in {n} apps"),
                    };
                    println!("{:>4}  {} tabs, {shown}: {}", w.window, w.tabs, w.titles.join(", "));
                }
                return Ok(());
            },
            Event::Closed(why) => return Err(why),
            _ => {},
        }
    }
}

fn attach(
    client: &Client,
    events: &std::sync::mpsc::Receiver<Event>,
    window: Option<WindowId>,
    share: bool,
) -> Result<(), String> {
    client.attach(window, share)?;
    let window = loop {
        match events.recv_timeout(Duration::from_secs(10)).map_err(|_| "not attached")? {
            Event::Attached { window } => break window,
            Event::Closed(why) => return Err(why),
            _ => {},
        }
    };
    let raw = RawMode::enter()?;
    // Typing, read on a thread; Ctrl-] leaves.
    let (keys, typed) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || keys.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut pane: Option<PaneId> = None;
    let mut size = terminal_size();
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\x1b[?1049h\x1b[2J");
    let result = 'shown: loop {
        while let Ok(bytes) = typed.try_recv() {
            if bytes.contains(&0x1d) {
                break 'shown Ok(());
            }
            if let Some(pane) = pane {
                client.input(pane, bytes)?;
            }
        }
        let now = terminal_size();
        if now != size {
            size = now;
            if let Some(pane) = pane {
                client.resize(pane, size)?;
            }
        }
        let mut redraw = false;
        loop {
            match events.recv_timeout(Duration::from_millis(16)) {
                Ok(Event::Layout { window: w, layout }) if w == window => {
                    let focused = focused_pane(&layout);
                    if focused != pane {
                        pane = focused;
                        if let Some(pane) = pane {
                            client.resize(pane, size)?;
                            client.focus(window, true)?;
                        }
                        redraw = true;
                    }
                },
                Ok(Event::Screen { pane: p }) if Some(p) == pane => redraw = true,
                Ok(Event::WindowClosed { window: w }) if w == window => break 'shown Ok(()),
                Ok(Event::Closed(why)) => break 'shown Err(why),
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        if redraw && let Some(screen) = pane.and_then(|p| client.screen(p)) {
            draw(&mut out, &screen.frame);
        }
    };
    let _ = write!(out, "\x1b[0m\x1b[?1049l");
    let _ = out.flush();
    drop(raw);
    let _ = client.detach(window);
    result
}

fn focused_pane(layout: &WindowLayout) -> Option<PaneId> {
    layout.active_tab().map(|tab| tab.focused)
}

/// The screen, line by line, in its colors (the terminal's cursor at its).
fn draw(out: &mut impl Write, frame: &artty_core::Frame) {
    let sgr = |rgb: artty_core::vt::ansi::Rgb, fg: bool| {
        format!("\x1b[{};2;{};{};{}m", if fg { 38 } else { 48 }, rgb.r, rgb.g, rgb.b)
    };
    let mut text = String::from("\x1b[?25l");
    for (row, line) in frame.lines.iter().enumerate() {
        // The line's background, then its text over it.
        let mut backgrounds = vec![frame.background; frame.columns];
        for rect in &line.backgrounds {
            for cell in backgrounds.iter_mut().skip(rect.column).take(rect.width) {
                *cell = rect.color;
            }
        }
        text.push_str(&format!("\x1b[{};1H{}\x1b[2K", row + 1, sgr(frame.background, false)));
        for (column, color) in backgrounds.iter().enumerate() {
            if *color != frame.background {
                text.push_str(&format!("\x1b[{};{}H{} ", row + 1, column + 1, sgr(*color, false)));
            }
        }
        for run in &line.runs {
            let background = backgrounds.get(run.column).copied().unwrap_or(frame.background);
            text.push_str(&format!(
                "\x1b[{};{}H\x1b[0m{}{}{}{}",
                row + 1,
                run.column + 1,
                if run.bold { "\x1b[1m" } else { "" },
                sgr(run.fg, true),
                sgr(background, false),
                run.text
            ));
        }
    }
    text.push_str("\x1b[0m");
    if let Some(cursor) = frame.cursor {
        text.push_str(&format!("\x1b[{};{}H\x1b[?25h", cursor.line + 1, cursor.column + 1));
    }
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

fn terminal_size() -> TerminalSize {
    // SAFETY: plain ioctl into a zeroed winsize.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) };
    let (columns, lines) = (size.ws_col.max(2), size.ws_row.max(1));
    TerminalSize {
        columns,
        lines,
        cell_width: if size.ws_xpixel > 0 { size.ws_xpixel / columns } else { 8 },
        cell_height: if size.ws_ypixel > 0 { size.ws_ypixel / lines } else { 16 },
    }
}

/// This terminal in raw mode, until dropped.
struct RawMode(libc::termios);

impl RawMode {
    fn enter() -> Result<RawMode, String> {
        // SAFETY: termios calls on stdin with valid pointers.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return Err("not a terminal".to_owned());
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(0, libc::TCSANOW, &raw);
            Ok(RawMode(saved))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restoring what was read.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}

/// Asking on this terminal.
struct TerminalLogin;

impl Login for TerminalLogin {
    fn host_key(&self, target: &Target, fingerprint: &str, known: HostKey) -> bool {
        if known == HostKey::Changed {
            eprintln!(
                "{}'s key isn't the one recorded ({fingerprint}): not logging in.",
                target.host
            );
            return false;
        }
        eprint!("{}'s key is {fingerprint}. Trust it? (yes/no) ", target.host);
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        answer.trim().eq_ignore_ascii_case("yes")
    }

    fn said(&self, text: &str) {
        eprintln!("{}", text.trim_end());
    }

    fn answer(&self, instructions: &str, prompts: &[(String, bool)]) -> Option<Vec<String>> {
        if !instructions.trim().is_empty() {
            eprintln!("{}", instructions.trim_end());
        }
        let mut answers = Vec::new();
        for (prompt, echo) in prompts {
            eprint!("{prompt} ");
            let _ = std::io::stderr().flush();
            // Unseen, unless it may show.
            let hidden = (!echo).then(|| {
                // SAFETY: termios calls on stdin with valid pointers.
                unsafe {
                    let mut saved: libc::termios = std::mem::zeroed();
                    libc::tcgetattr(0, &mut saved);
                    let mut quiet = saved;
                    quiet.c_lflag &= !libc::ECHO;
                    libc::tcsetattr(0, libc::TCSANOW, &quiet);
                    saved
                }
            });
            let mut answer = String::new();
            let read = std::io::stdin().read_line(&mut answer);
            if let Some(saved) = hidden {
                // SAFETY: restoring what was read.
                unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
                eprintln!();
            }
            if read.ok()? == 0 {
                return None;
            }
            answers.push(answer.trim_end_matches(['\r', '\n']).to_owned());
        }
        Some(answers)
    }
}
