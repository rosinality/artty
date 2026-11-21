//! User configuration, read from `~/.config/artty/config.toml`.
//!
//! Every key is optional; see `config.example.toml` at the repository root.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use artty_vt::ansi::Rgb;
use serde::{Deserialize, Deserializer};

use crate::theme::Theme;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// Program to run in new tabs, e.g. `["/opt/homebrew/bin/fish", "-l"]`.
    /// Empty runs the account's login shell.
    pub shell: Vec<String>,
    /// Font family names, tried in order.
    pub font_family: Vec<String>,
    pub font_size: f32,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
    /// Opacity of the terminal background over the blurred desktop, 0–1.
    pub background_opacity: f32,
    pub background_blur: bool,
    pub scrollback: usize,
    /// Treat Option as Meta (⌥B sends ESC b). Turn off to type characters
    /// like `å` with Option.
    pub option_as_meta: bool,
    pub cursor_blink: bool,
    /// Cursor shape; programs can still change it while they run (vim).
    pub cursor_style: CursorStyle,
    /// Foreground process names treated as coding agents in the sidebar.
    pub agents: Vec<String>,
    /// macOS notifications when a tab needs attention while you're elsewhere.
    pub notifications: bool,
    /// Set up zsh and bash to report prompts and commands (fish does natively).
    pub shell_integration: bool,
    /// Flash the pane on the bell.
    pub visual_bell: bool,
    /// Play the system alert sound on the bell.
    pub bell_sound: bool,
    /// Copy text to the clipboard as soon as it's selected.
    pub copy_on_select: bool,
    /// In the sidebar, a tab with split panes lists them, a line each; off,
    /// it's one line, for its focused pane.
    pub list_panes: bool,
    /// How the sidebar shows workspaces: all of them, only the current one
    /// open (accordion), or only the current one with a switcher.
    pub sidebar_style: SidebarStyle,
    /// The sidebar's width in points (dragging its edge sets it).
    pub sidebar_width: u32,
    /// Tabs' status icons (working, needs you, done) in the sidebar; off,
    /// their titles start at the edge (a tab that needs you is still bold).
    pub sidebar_icons: bool,
    /// The sidebar lists agents (each with those it started), then the
    /// tabs with none, instead of tabs by workspace.
    pub sidebar_by_agent: bool,
    /// In the sidebar by workspace: a workspace's agents' tabs first (each
    /// with those it started under it), then its other tabs.
    pub sidebar_agents_first: bool,
    /// The servers whose past conversations are open in the sidebar (by
    /// name; `""` for this Mac's): folded otherwise.
    pub sidebar_past_open: Vec<String>,
    /// Ask before pasting several lines into a program that would run them
    /// one by one (one that hasn't turned on bracketed paste).
    pub confirm_multiline_paste: bool,
    /// Keep other apps from reading what's typed while artty is in front
    /// (macOS's secure keyboard entry), as for passwords.
    pub secure_keyboard_entry: bool,
    /// Show typing at once on other machines' panes, before their echo
    /// comes back (as mosh does), where the round trip is noticeable.
    pub local_echo: bool,
    /// Forward other machines' ports here as their programs start
    /// listening (else from the sidebar or Activity, on request).
    pub auto_forward_ports: bool,
    /// Reopen the previous tabs on launch.
    pub restore_sessions: bool,
    /// Lines of each tab's output to save and show again; 0 saves none.
    pub restore_scrollback: usize,
    /// Resume coding agents that were running in restored tabs.
    pub resume_agents: bool,
    /// How to resume each agent, by process name.
    pub resume: BTreeMap<String, ResumeCommand>,
    /// Agents started with a task (`artty new --task`, `/new`) run without
    /// their terminal UI, when they have an adapter (`acp`): ^T in the tab
    /// takes one over.
    pub headless_tasks: bool,
    /// Agents run without their terminal UI (`artty acp AGENT`): the
    /// command of each one's Agent Client Protocol adapter.
    pub acp: BTreeMap<String, String>,
    /// How many agents deep agents may start agents (`artty new --task`
    /// from an agent's pane); 0: no limit.
    pub max_agent_depth: usize,
    pub colors: Colors,
    /// Key bindings over the defaults: keystrokes (as in `cmd-shift-t`, or
    /// several separated by spaces; Ghostty's `cmd+e>1` too) to what they do.
    pub keys: BTreeMap<String, KeyAction>,
    /// How long a key sequence (`cmd-e 1`) waits for its next key, in
    /// seconds; 0 waits as long as it takes.
    pub key_sequence_timeout: f32,
    /// Other machines' artty servers, shown in the sidebar beside this one's.
    pub servers: Vec<ServerConfig>,
    /// A status line for each window, worked out by the server.
    pub status: StatusConfig,
}

/// `[status]`: what the status line shows, left and right (templates, see
/// [`crate::status`]), and the commands whose output it can show.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct StatusConfig {
    pub left: String,
    pub right: String,
    /// `{cmd:NAME}`'s commands, by name.
    pub commands: BTreeMap<String, StatusCommand>,
}

/// A command whose output's first line a status line shows.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct StatusCommand {
    /// Run with `sh -c`, in the focused pane's directory.
    pub run: String,
    /// How often (`5s`, `500ms`, `2m`); 10 seconds if not said.
    #[serde(default = "every", deserialize_with = "duration")]
    pub every: std::time::Duration,
    /// How long it may take; 2 seconds if not said.
    #[serde(default = "timeout", deserialize_with = "duration")]
    pub timeout: std::time::Duration,
}

fn every() -> std::time::Duration {
    std::time::Duration::from_secs(10)
}

fn timeout() -> std::time::Duration {
    std::time::Duration::from_secs(2)
}

/// `500ms`, `5s`, `2m`, `1h`; a bare number is seconds.
pub fn parse_duration(text: &str) -> Option<std::time::Duration> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let seconds = match unit.trim() {
        "ms" => number / 1000.,
        "" | "s" => number,
        "m" => number * 60.,
        "h" => number * 3600.,
        _ => return None,
    };
    std::time::Duration::try_from_secs_f64(seconds).ok()
}

fn duration<'de, D: Deserializer<'de>>(d: D) -> Result<std::time::Duration, D::Error> {
    let text = String::deserialize(d)?;
    parse_duration(&text).ok_or_else(|| {
        serde::de::Error::custom(format!("invalid duration {text:?}, expected e.g. \"5s\""))
    })
}

/// Another machine's artty server: reached over ssh, or by any command
/// that carries the connection on its stdin and stdout.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct ServerConfig {
    /// Shown in the sidebar.
    pub name: String,
    /// An ssh destination (`host`, `user@host`, a `Host` from ~/.ssh/config).
    pub ssh: Option<String>,
    /// artty on that machine, if it isn't `artty` on the login shell's PATH.
    pub artty: Option<String>,
    /// Instead of ssh: a command whose stdin and stdout reach the server
    /// (it runs `artty server --stdio` somewhere).
    pub command: Vec<String>,
    /// Keep artty there up to date with this app's build (over ssh, only
    /// ever to a later one; its server moves to it with its programs
    /// running). On unless `false`.
    pub auto_update: Option<bool>,
}

impl ServerConfig {
    /// Whether artty there is kept up to date (reached by ssh, and not
    /// turned off).
    pub fn updates(&self) -> bool {
        self.ssh.is_some() && self.command.is_empty() && self.auto_update != Some(false)
    }

    /// The command that reaches the server.
    pub fn connect_command(&self) -> Option<Vec<String>> {
        if !self.command.is_empty() {
            return Some(self.command.clone());
        }
        let artty = self.artty.as_deref().unwrap_or("artty");
        // Through a login shell, for the PATH set up in its profile.
        self.ssh_command(&format!("exec \"$SHELL\" -lc 'exec {artty} server --stdio'"))
    }

    /// ssh running `remote` there (for one reached by ssh), sharing the
    /// connection to it (see [`ServerConfig::shared`]).
    pub fn ssh_command(&self, remote: &str) -> Option<Vec<String>> {
        let destination = self.ssh.as_ref()?;
        let mut command = vec!["ssh".to_owned(), "-T".to_owned()];
        command.extend(shared_options());
        command.extend(
            // Screens are text and colors: they shrink well.
            ["-o", "Compression=yes", "-o", "ConnectTimeout=10"].map(str::to_owned),
        );
        command.extend([destination.clone(), remote.to_owned()]);
        Some(command)
    }

    /// ssh logging in to the machine in a terminal, where it can ask for
    /// anything a login needs: it becomes the shared connection (see
    /// [`ServerConfig::shared`]), which the app's then goes through. A
    /// shell there, once in.
    pub fn login_command(&self) -> Option<Vec<String>> {
        let destination = self.ssh.as_ref()?;
        let mut command = vec!["ssh".to_owned()];
        command.extend(
            shared_options()
                .chunks(2)
                .filter(|option| option[1] != "BatchMode=yes")
                .flatten()
                .cloned(),
        );
        command.push(destination.clone());
        Some(command)
    }

    /// One ssh connection to the machine, shared: the app's, forwards'
    /// and installs' sessions all go through it (OpenSSH's ControlMaster),
    /// so a login that needs a password or a code is made once. It
    /// outlives its last session by ten minutes.
    ///
    /// `operation`, given to it: `check` (whether it's up), `exit`, or
    /// `forward`/`cancel` with `forward` (`-L` arguments).
    pub fn shared(&self, operation: &str, forward: Option<&str>) -> Option<Vec<String>> {
        let destination = self.ssh.as_ref()?;
        let mut command = vec!["ssh".to_owned()];
        command.extend(shared_options());
        command.extend(["-O".to_owned(), operation.to_owned()]);
        if let Some(forward) = forward {
            command.extend(["-L".to_owned(), forward.to_owned()]);
        }
        command.push(destination.clone());
        Some(command)
    }
}

/// Options for every ssh artty runs, besides where the shared connection's
/// socket is.
const SHARED: [&str; 5] = [
    "BatchMode=yes",
    "ServerAliveInterval=15",
    "ServerAliveCountMax=3",
    "ControlMaster=auto",
    "ControlPersist=600",
];

fn shared_options() -> Vec<String> {
    let mut options = Vec::new();
    for option in SHARED {
        options.extend(["-o".to_owned(), option.to_owned()]);
    }
    options.extend(["-o".to_owned(), format!("ControlPath={}", control_path())]);
    options
}

/// Where shared connections' sockets go: `~/.ssh/artty`, made private (a
/// socket path must be short: macOS's temporary directory is too long).
fn control_path() -> String {
    let home = std::env::var_os("HOME").map_or_else(|| "/".into(), std::path::PathBuf::from);
    let dir = home.join(".ssh/artty");
    if let Err(err) = crate::control::make_private_dir(&dir) {
        log::warn!("{}: {err}", dir.display());
    }
    let path = format!("{}/%C", dir.display());
    // ssh reads the option like a config line: quote a path with spaces.
    if path.contains(char::is_whitespace) { format!("\"{path}\"") } else { path }
}

/// Commands that resume an agent's conversation.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct ResumeCommand {
    /// Resume a specific conversation; `{session}` is replaced by its id.
    pub session: Option<String>,
    /// Resume the latest conversation in the directory, when the id is unknown.
    pub latest: Option<String>,
}

impl ResumeCommand {
    /// The command line to run, if this agent can be resumed. Session ids are
    /// only used if they can't break out of the command.
    pub fn command(&self, session: Option<&str>) -> Option<String> {
        let safe = |id: &&str| {
            !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        };
        match (session.filter(safe), &self.session) {
            (Some(id), Some(template)) => Some(template.replace("{session}", id)),
            _ => self.latest.clone(),
        }
    }
}

/// How the sidebar shows workspaces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SidebarStyle {
    /// Every workspace with its tabs (each folds by hand).
    #[default]
    List,
    /// Every workspace, only the current one open; the rest one line each,
    /// saying what needs you there.
    Accordion,
    /// Only the current workspace's tabs, under a switcher to the others.
    Switcher,
}

impl SidebarStyle {
    pub const ALL: [SidebarStyle; 3] =
        [SidebarStyle::List, SidebarStyle::Accordion, SidebarStyle::Switcher];

    /// The name used in the config file.
    pub fn name(self) -> &'static str {
        match self {
            SidebarStyle::List => "list",
            SidebarStyle::Accordion => "accordion",
            SidebarStyle::Switcher => "switcher",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CursorStyle {
    #[default]
    Block,
    /// A thin vertical line.
    Bar,
    Underline,
    /// An outlined block.
    Hollow,
}

impl CursorStyle {
    pub const ALL: [CursorStyle; 4] =
        [CursorStyle::Block, CursorStyle::Bar, CursorStyle::Underline, CursorStyle::Hollow];

    /// The name used in the config file.
    pub fn name(self) -> &'static str {
        match self {
            CursorStyle::Block => "block",
            CursorStyle::Bar => "bar",
            CursorStyle::Underline => "underline",
            CursorStyle::Hollow => "hollow",
        }
    }

    pub fn shape(self) -> artty_vt::ansi::CursorShape {
        use artty_vt::ansi::CursorShape;
        match self {
            CursorStyle::Block => CursorShape::Block,
            CursorStyle::Bar => CursorShape::Beam,
            CursorStyle::Underline => CursorShape::Underline,
            CursorStyle::Hollow => CursorShape::HollowBlock,
        }
    }
}

/// Coding agents recognized by default.
pub const DEFAULT_AGENTS: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "aider",
    "opencode",
    "amp",
    "goose",
    "cursor-agent",
    "qwen",
    "crush",
    "droid",
];

impl Default for Config {
    fn default() -> Self {
        Self {
            shell: Vec::new(),
            font_family: vec!["SF Mono".into(), "Menlo".into()],
            font_size: 13.,
            line_height: 1.35,
            background_opacity: 0.72,
            background_blur: true,
            scrollback: 10_000,
            option_as_meta: true,
            cursor_blink: true,
            cursor_style: CursorStyle::Block,
            agents: DEFAULT_AGENTS.iter().map(|&s| s.to_owned()).collect(),
            notifications: true,
            shell_integration: true,
            visual_bell: true,
            bell_sound: false,
            copy_on_select: false,
            list_panes: true,
            sidebar_style: SidebarStyle::List,
            sidebar_width: 232,
            sidebar_icons: true,
            sidebar_by_agent: false,
            sidebar_agents_first: false,
            sidebar_past_open: Vec::new(),
            confirm_multiline_paste: true,
            secure_keyboard_entry: false,
            local_echo: true,
            auto_forward_ports: false,
            restore_sessions: true,
            restore_scrollback: 1000,
            resume_agents: true,
            max_agent_depth: 2,
            headless_tasks: true,
            acp: BTreeMap::from([(
                "claude".to_owned(),
                "npx -y @agentclientprotocol/claude-agent-acp@0.109.0".to_owned(),
            )]),
            resume: BTreeMap::from([
                (
                    "claude-acp".to_owned(),
                    ResumeCommand {
                        session: Some("artty acp claude --resume {session}".to_owned()),
                        latest: None,
                    },
                ),
                (
                    "claude".to_owned(),
                    ResumeCommand {
                        session: Some("claude --resume {session}".to_owned()),
                        latest: Some("claude --continue".to_owned()),
                    },
                ),
                (
                    "codex".to_owned(),
                    ResumeCommand {
                        session: Some("codex resume {session}".to_owned()),
                        latest: Some("codex resume --last".to_owned()),
                    },
                ),
                (
                    "gemini".to_owned(),
                    ResumeCommand {
                        session: Some("gemini --resume {session}".to_owned()),
                        latest: Some("gemini --resume latest".to_owned()),
                    },
                ),
            ]),
            colors: Colors::default(),
            keys: BTreeMap::new(),
            key_sequence_timeout: 0.,
            servers: Vec::new(),
            status: StatusConfig::default(),
        }
    }
}

/// Color overrides on top of the built-in theme.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Colors {
    /// The theme these came from (shown in Settings), possibly edited.
    pub name: Option<String>,
    #[serde(deserialize_with = "color")]
    pub foreground: Option<Rgb>,
    #[serde(deserialize_with = "color")]
    pub background: Option<Rgb>,
    #[serde(deserialize_with = "color")]
    pub cursor: Option<Rgb>,
    #[serde(deserialize_with = "color")]
    pub selection: Option<Rgb>,
    #[serde(deserialize_with = "color")]
    pub search_match: Option<Rgb>,
    /// The 16 ANSI colors, normal then bright; may be shorter than 16.
    #[serde(deserialize_with = "palette")]
    pub palette: Vec<Rgb>,
}

fn parse_rgb<E: serde::de::Error>(s: &str) -> Result<Rgb, E> {
    Rgb::from_str(s).map_err(|()| E::custom(format!("invalid color {s:?}, expected \"#rrggbb\"")))
}

fn color<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Rgb>, D::Error> {
    parse_rgb(&String::deserialize(d)?).map(Some)
}

fn palette<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Rgb>, D::Error> {
    let colors = Vec::<String>::deserialize(d)?;
    if colors.len() > 16 {
        return Err(serde::de::Error::custom("palette has more than 16 colors"));
    }
    colors.iter().map(|s| parse_rgb(s)).collect()
}

/// What a key does, in `[keys]`: an action by name (`"new-tab"`), `"none"`
/// to take a default binding away, or text to type (`{ send = "..." }`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum KeyAction {
    Named(String),
    Send { send: String },
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("artty").join("config.toml"))
    }

    /// Load the config file. A missing file gives the defaults; a broken one
    /// gives the defaults plus the error, so the UI can report it.
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = Self::path() else { return (Self::default(), None) };
        match std::fs::read_to_string(&path) {
            Ok(text) => match Self::parse(&text) {
                Ok(config) => (config, None),
                Err(err) => (Self::default(), Some(format!("{}: {err}", path.display()))),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(err) => (Self::default(), Some(format!("{}: {err}", path.display()))),
        }
    }

    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        let mut config: Self = toml::from_str(text)?;
        // Agents configured by the user add to the built-in ones.
        for (agent, command) in Self::default().resume {
            config.resume.entry(agent).or_insert(command);
        }
        Ok(config)
    }

    pub fn theme(&self) -> Theme {
        let mut theme = Theme::default();
        let colors = &self.colors;
        theme.foreground = colors.foreground.unwrap_or(theme.foreground);
        theme.background = colors.background.unwrap_or(theme.background);
        theme.cursor = colors.cursor.unwrap_or(theme.cursor);
        theme.selection = colors.selection.unwrap_or(theme.selection);
        theme.search_match = colors.search_match.unwrap_or(theme.search_match);
        theme.ansi[..colors.palette.len()].copy_from_slice(&colors.palette);
        theme
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_bindings() {
        let config = Config::parse(
            "[keys]\n\"cmd-shift-t\" = \"new-tab\"\n\"cmd-t\" = \"none\"\n\"shift-enter\" = { send = \"\\u001b[13;2u\" }\n",
        )
        .unwrap();
        assert_eq!(config.keys["cmd-shift-t"], KeyAction::Named("new-tab".into()));
        assert_eq!(config.keys["cmd-t"], KeyAction::Named("none".into()));
        assert_eq!(config.keys["shift-enter"], KeyAction::Send { send: "\x1b[13;2u".into() });
    }

    #[test]
    fn remote_servers() {
        let config = Config::parse(
            "[[servers]]\nname = \"devbox\"\nssh = \"me@devbox\"\n\n\
             [[servers]]\nname = \"test\"\ncommand = [\"artty\", \"server\", \"--stdio\"]\n",
        )
        .unwrap();
        let ssh = config.servers[0].connect_command().unwrap();
        assert_eq!(ssh[0], "ssh");
        assert!(ssh.contains(&"me@devbox".to_owned()));
        assert_eq!(ssh.last().unwrap(), "exec \"$SHELL\" -lc 'exec artty server --stdio'");
        assert_eq!(config.servers[1].connect_command().unwrap(), ["artty", "server", "--stdio"]);
        assert_eq!(ServerConfig::default().connect_command(), None);
    }

    #[test]
    fn ssh_shares_one_connection() {
        let server = ServerConfig { ssh: Some("me@devbox".into()), ..Default::default() };
        let has = |command: &[String], option: &str| command.iter().any(|o| o == option);
        let connect = server.connect_command().unwrap();
        assert!(has(&connect, "BatchMode=yes") && has(&connect, "ControlMaster=auto"));
        assert!(connect.iter().any(|o| o.starts_with("ControlPath=") && o.ends_with("/%C")));
        let forward = server.shared("forward", Some("127.0.0.1:1:localhost:2")).unwrap();
        let tail: Vec<&str> = forward.iter().rev().take(5).rev().map(String::as_str).collect();
        assert_eq!(tail, ["-O", "forward", "-L", "127.0.0.1:1:localhost:2", "me@devbox"]);
        // Logging in in a terminal: may ask, shares, and gets a shell there.
        let login = server.login_command().unwrap();
        assert!(!has(&login, "BatchMode=yes") && !has(&login, "-T"));
        assert!(has(&login, "ControlMaster=auto") && has(&login, "ControlPersist=600"));
        assert_eq!(login.last().map(String::as_str), Some("me@devbox"));
    }

    #[test]
    fn empty_config_is_default() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.font_size, 13.);
        assert!(config.shell.is_empty());
    }

    #[test]
    fn colors_override_theme() {
        let config = Config::parse(
            r##"
            font-size = 14
            [colors]
            background = "#000000"
            palette = ["#010203", "#ff0000"]
            "##,
        )
        .unwrap();
        let theme = config.theme();
        assert_eq!(theme.background, Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(theme.ansi[1], Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(theme.ansi[2], Theme::default().ansi[2]);
    }

    #[test]
    fn resume_commands() {
        let config = Config::default();
        let claude = &config.resume["claude"];
        assert_eq!(claude.command(Some("0f1e-2d")).as_deref(), Some("claude --resume 0f1e-2d"));
        assert_eq!(claude.command(None).as_deref(), Some("claude --continue"));
        // An id that could inject shell syntax falls back to the latest.
        assert_eq!(claude.command(Some("x; rm -rf ~")).as_deref(), Some("claude --continue"));

        let custom = Config::parse("[resume.codex]\nlatest = \"codex resume --last\"").unwrap();
        assert_eq!(
            custom.resume["codex"].command(Some("abc")).as_deref(),
            Some("codex resume --last")
        );
        assert!(custom.resume.contains_key("claude"));
    }

    #[test]
    fn cursor_styles() {
        assert_eq!(Config::parse("").unwrap().cursor_style, CursorStyle::Block);
        for style in CursorStyle::ALL {
            let config = Config::parse(&format!("cursor-style = \"{}\"", style.name())).unwrap();
            assert_eq!(config.cursor_style, style);
        }
        assert!(Config::parse("cursor-style = \"beam\"").is_err());
    }

    #[test]
    fn status_commands() {
        let config = Config::parse(
            "[status]\nright = \"{cmd:gpu}\"\n[status.commands.gpu]\nrun = \"nvidia-smi\"\nevery = \"5s\"\n\
             [status.commands.load]\nrun = \"uptime\"\ntimeout = \"500ms\"\n",
        )
        .unwrap();
        let gpu = &config.status.commands["gpu"];
        assert_eq!((gpu.every.as_secs(), gpu.timeout.as_secs()), (5, 2));
        let load = &config.status.commands["load"];
        assert_eq!((load.every.as_secs(), load.timeout.as_millis()), (10, 500));
        assert!(Config::parse("[status.commands.x]\nrun = \"x\"\nevery = \"often\"").is_err());
        assert_eq!(parse_duration("2m"), Some(std::time::Duration::from_secs(120)));
        assert_eq!(parse_duration("1.5"), Some(std::time::Duration::from_millis(1500)));
        assert_eq!(parse_duration("-1s"), None);
        assert!(Config::parse("").unwrap().status == StatusConfig::default());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Config::parse("[colors]\nforeground = \"red\"").is_err());
        assert!(Config::parse("font-sise = 12").is_err());
    }
}

#[cfg(test)]
mod example_tests {
    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../config.example.toml");
        let config = super::Config::parse(text).unwrap();
        assert_eq!(config.font_family, ["SF Mono", "Menlo"]);
    }
}
