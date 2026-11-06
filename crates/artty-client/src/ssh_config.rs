//! OpenSSH's client config (`ssh_config(5)`), read as `ssh` reads it: what
//! a destination (an alias, or `user@host`) resolves to — its real host name,
//! user and port, keys, jump hosts, forwards and the rest. For an app with
//! no `ssh` to ask (a phone's), keeping its own config file.
//!
//! As OpenSSH: options before any `Host` apply to all; `Host` patterns (`*`,
//! `?`, `!` negation) match the destination as given, `Match host` the host
//! name so far; the first value of an option wins, but keys and forwards
//! add up. `Include` reads other files (as the caller finds them). What a
//! phone can't do (`ProxyCommand`, `Match exec`) is reported, not done.

use std::collections::HashMap;

/// A config, parsed.
#[derive(Clone, Debug, Default)]
pub struct Config {
    entries: Vec<Entry>,
    /// Lines that couldn't be read: (line, why).
    pub errors: Vec<(usize, String)>,
}

#[derive(Clone, Debug)]
enum Entry {
    Host(Vec<String>),
    Match(Vec<Criterion>),
    Option {
        keyword: String,
        args: Vec<String>,
    },
    /// What an `Include` read, in the context it was included in.
    Include(Vec<Entry>),
}

#[derive(Clone, Debug)]
enum Criterion {
    All,
    /// `host`, `originalhost`, `user`, `localuser`: patterns, negated with `!` before the keyword.
    Patterns {
        what: String,
        patterns: String,
        negated: bool,
    },
    /// What can't be told here (`exec`, `canonical`, `final`, `localnetwork`…): never matches.
    Unknown(String),
}

/// Who's connecting, for `%` tokens and `localuser`.
#[derive(Clone, Debug, Default)]
pub struct Local {
    /// `~` and `%d`.
    pub home: String,
    /// `%u`, and `Match localuser`.
    pub user: String,
    /// `%L` and `%l`.
    pub host: String,
}

impl Local {
    /// This machine's: `$HOME`, `$USER`, its host name.
    pub fn here() -> Local {
        let var = |name| std::env::var(name).unwrap_or_default();
        let host = std::process::Command::new("hostname")
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .unwrap_or_default();
        Local { home: var("HOME"), user: var("USER"), host }
    }
}

/// What a destination resolves to.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    /// The destination's host as given (an alias, maybe): `%n`.
    pub alias: String,
    /// Where to connect: `HostName`, else the alias.
    pub host_name: String,
    /// `None`: the caller's default.
    pub user: Option<String>,
    pub port: u16,
    /// As written (`~`, `%` tokens): see [`Resolved::expand`].
    pub identity_files: Vec<String>,
    pub identities_only: bool,
    /// Hops to go through, in order.
    pub proxy_jump: Vec<Hop>,
    /// Set: what can't run here (no shell on a phone).
    pub proxy_command: Option<String>,
    pub local_forwards: Vec<Forward>,
    pub remote_forwards: Vec<Forward>,
    pub dynamic_forwards: Vec<Forward>,
    /// Seconds; 0 is off.
    pub server_alive_interval: u32,
    pub server_alive_count_max: u32,
    /// Seconds.
    pub connect_timeout: Option<u32>,
    pub compression: bool,
    /// As written: a list, or one starting with `+`, `-` or `^` (changing the defaults).
    pub ciphers: Option<String>,
    pub kex_algorithms: Option<String>,
    pub host_key_algorithms: Option<String>,
    pub macs: Option<String>,
    /// `publickey`, `keyboard-interactive`, `password`, in the order to try.
    pub preferred_authentications: Vec<String>,
    pub pubkey_authentication: bool,
    pub password_authentication: bool,
    pub kbd_interactive_authentication: bool,
    pub strict_host_key_checking: StrictHostKeys,
    /// The name its key is recorded under, in place of the host's.
    pub host_key_alias: Option<String>,
    pub set_env: Vec<(String, String)>,
    pub remote_command: Option<String>,
    pub request_tty: RequestTty,
    pub forward_agent: bool,
    /// Options read but not done here, by keyword (for telling the user).
    pub ignored: Vec<String>,
}

/// A jump host: `[user@]host[:port]`, resolved itself when connecting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hop {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

/// A forward: listening at `bind` (else the loopback) on `port`; to
/// `to` (none for a dynamic one: SOCKS says where).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forward {
    pub bind: Option<String>,
    pub port: u16,
    pub to: Option<(String, u16)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrictHostKeys {
    /// A new machine's key is asked about; a changed one refused.
    Ask,
    /// Only known keys.
    Yes,
    /// A new key is taken (and remembered); a changed one refused.
    AcceptNew,
    /// Anything is taken (a changed key too); new ones remembered.
    No,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestTty {
    Auto,
    Yes,
    No,
    Force,
}

/// Options that add up rather than the first value winning.
/// (`SetEnv`'s first line wins, with all its variables.)
const ADDING: [&str; 6] = [
    "identityfile",
    "certificatefile",
    "localforward",
    "remoteforward",
    "dynamicforward",
    "sendenv",
];

/// Options read here that this client does nothing with, quietly (they're
/// about the desktop: the agent, keychains, multiplexing, logging…).
const QUIET: [&str; 17] = [
    "addkeystoagent",
    "usekeychain",
    "ignoreunknown",
    "loglevel",
    "controlmaster",
    "controlpath",
    "controlpersist",
    "visualhostkey",
    "hashknownhosts",
    "updatehostkeys",
    "userknownhostsfile",
    "globalknownhostsfile",
    "checkhostip",
    "identityagent",
    "tcpkeepalive",
    "escapechar",
    "addressfamily",
];

/// Options this client does something with.
const DONE: [&str; 30] = [
    "hostname",
    "user",
    "port",
    "identityfile",
    "identitiesonly",
    "proxyjump",
    "proxycommand",
    "localforward",
    "remoteforward",
    "dynamicforward",
    "serveraliveinterval",
    "serveralivecountmax",
    "connecttimeout",
    "compression",
    "ciphers",
    "kexalgorithms",
    "hostkeyalgorithms",
    "macs",
    "preferredauthentications",
    "pubkeyauthentication",
    "passwordauthentication",
    "kbdinteractiveauthentication",
    "challengeresponseauthentication",
    "stricthostkeychecking",
    "hostkeyalias",
    "setenv",
    "remotecommand",
    "requesttty",
    "forwardagent",
    "sendenv",
];

impl Config {
    /// Read the file at `path`, its `Include`s from disk: relative names
    /// in `home`'s `.ssh`, `~` being `home`, `*` and `?` in file names.
    pub fn read(path: &std::path::Path, home: &str) -> Result<Config, String> {
        let text =
            std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
        Ok(Config::parse_in(&text, home))
    }

    /// Read `text`, its `Include`s from disk, as [`Config::read`] does.
    pub fn parse_in(text: &str, home: &str) -> Config {
        let ssh = std::path::Path::new(home).join(".ssh");
        let include = |pattern: &str| -> Vec<String> {
            let pattern = match pattern.strip_prefix("~/") {
                Some(rest) => std::path::Path::new(home).join(rest),
                None => ssh.join(pattern),
            };
            let (Some(dir), Some(name)) = (pattern.parent(), pattern.file_name()) else {
                return Vec::new();
            };
            let name = name.to_string_lossy();
            let mut files: Vec<_> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| wildcard(&name, &entry.file_name().to_string_lossy()))
                .map(|entry| entry.path())
                .collect();
            files.sort();
            files.iter().filter_map(|file| std::fs::read_to_string(file).ok()).collect()
        };
        Config::parse(text, &include)
    }

    /// Read `text`; `include` gives the texts of the files an `Include`'s
    /// pattern names (relative names are `~/.ssh`'s, as OpenSSH reads them).
    pub fn parse(text: &str, include: &dyn Fn(&str) -> Vec<String>) -> Config {
        let mut config = Config::default();
        config.entries = parse_entries(text, include, &mut config.errors, 0);
        config
    }

    /// The non-wildcard `Host` names, in order, for offering as servers.
    pub fn hosts(&self) -> Vec<String> {
        fn collect(entries: &[Entry], into: &mut Vec<String>) {
            for entry in entries {
                match entry {
                    Entry::Host(patterns) => {
                        for pattern in patterns {
                            let plain = !pattern.contains(['*', '?', '!']);
                            if plain && !into.contains(pattern) {
                                into.push(pattern.clone());
                            }
                        }
                    },
                    Entry::Include(entries) => collect(entries, into),
                    _ => {},
                }
            }
        }
        let mut hosts = Vec::new();
        collect(&self.entries, &mut hosts);
        hosts
    }

    /// What `destination` (`host`, `user@host`, `user@host:port`, or
    /// `ssh://user@host:port`) resolves to. What the destination itself
    /// says wins over the config, as on `ssh`'s command line.
    pub fn resolve(&self, destination: &str, local: &Local) -> Resolved {
        let (user, alias, port) = split_destination(destination);
        let mut found = Found::default();
        if let Some(user) = &user {
            found.set("user", vec![user.clone()]);
        }
        if let Some(port) = port {
            found.set("port", vec![port.to_string()]);
        }
        let mut active = true;
        self.apply(&self.entries, &alias, local, &mut found, &mut active);
        found.resolved(alias, local)
    }

    fn apply(
        &self,
        entries: &[Entry],
        alias: &str,
        local: &Local,
        found: &mut Found,
        active: &mut bool,
    ) {
        for entry in entries {
            match entry {
                Entry::Host(patterns) => *active = host_matches(patterns, alias),
                Entry::Match(criteria) => {
                    for criterion in criteria {
                        if let Criterion::Unknown(what) = criterion {
                            found.unknown_match(what);
                        }
                    }
                    *active = criteria.iter().all(|c| criterion_matches(c, alias, local, found))
                },
                Entry::Option { keyword, args } if *active => found.set(keyword, args.clone()),
                Entry::Option { .. } => {},
                Entry::Include(entries) => {
                    // Its Host lines are its own: after it, this block carries on.
                    let mut inner = *active;
                    if inner {
                        self.apply(entries, alias, local, found, &mut inner);
                    }
                },
            }
        }
    }
}

fn parse_entries(
    text: &str,
    include: &dyn Fn(&str) -> Vec<String>,
    errors: &mut Vec<(usize, String)>,
    depth: usize,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let words = match split_line(line) {
            Ok(words) => words,
            Err(why) => {
                errors.push((number, why));
                continue;
            },
        };
        let Some((keyword, args)) = words.split_first() else { continue };
        let keyword = keyword.to_lowercase();
        let args = args.to_vec();
        match keyword.as_str() {
            "host" => {
                if args.is_empty() {
                    errors.push((number, "Host without a pattern".into()));
                }
                entries.push(Entry::Host(args));
            },
            "match" => match parse_match(&args) {
                Ok(criteria) => entries.push(Entry::Match(criteria)),
                Err(why) => {
                    errors.push((number, why));
                    // Nothing below it applies, as a Match that never matches.
                    entries.push(Entry::Match(vec![Criterion::Unknown("invalid".into())]));
                },
            },
            "include" => {
                if depth >= 16 {
                    errors.push((number, "Include nested too deep".into()));
                    continue;
                }
                let mut read = Vec::new();
                for pattern in &args {
                    for text in include(pattern) {
                        read.extend(parse_entries(&text, include, &mut Vec::new(), depth + 1));
                    }
                }
                entries.push(Entry::Include(read));
            },
            _ if args.is_empty() => errors.push((number, format!("{keyword} without a value"))),
            _ => entries.push(Entry::Option { keyword, args }),
        }
    }
    entries
}

/// A line's words: `keyword args…` or `keyword=args…`, `"quoted words"`,
/// comments dropped.
fn split_line(line: &str) -> Result<Vec<String>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(Vec::new());
    }
    // `Keyword=value` (or `Keyword = value`).
    let (keyword, rest) = match line.find(|c: char| c.is_whitespace() || c == '=') {
        Some(at) => {
            let rest = line[at..].trim_start();
            let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
            (&line[..at], rest)
        },
        None => (line, ""),
    };
    let mut words = vec![keyword.to_owned()];
    let mut chars = rest.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        if first == '#' {
            break;
        }
        let mut word = String::new();
        if first == '"' {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some(c) => word.push(c),
                    None => return Err("a quote not closed".into()),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                word.push(c);
                chars.next();
            }
        }
        words.push(word);
    }
    Ok(words)
}

fn parse_match(args: &[String]) -> Result<Vec<Criterion>, String> {
    let mut criteria = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let lower = arg.to_lowercase();
        let (negated, what) = match lower.strip_prefix('!') {
            Some(what) => (true, what.to_owned()),
            None => (false, lower),
        };
        match what.as_str() {
            "all" => criteria.push(Criterion::All),
            "host" | "originalhost" | "user" | "localuser" => {
                let patterns = args.next().ok_or(format!("Match {what} without patterns"))?;
                criteria.push(Criterion::Patterns { what, patterns: patterns.clone(), negated });
            },
            "canonical" | "final" => criteria.push(Criterion::Unknown(what)),
            _ => {
                // exec, localnetwork, tagged, version…: with an argument.
                args.next();
                criteria.push(Criterion::Unknown(what));
            },
        }
    }
    if criteria.is_empty() {
        return Err("Match without criteria".into());
    }
    Ok(criteria)
}

/// `[ssh://][user@]host[:port]`: user, host, port.
fn split_destination(destination: &str) -> (Option<String>, String, Option<u16>) {
    let rest = destination.trim();
    let uri = rest.strip_prefix("ssh://");
    let rest = uri.unwrap_or(rest).trim_end_matches('/');
    let (user, rest) = match rest.rsplit_once('@') {
        Some((user, rest)) if !user.is_empty() => (Some(user.to_owned()), rest),
        _ => (None, rest),
    };
    // `[v6]:port`, `host:port`; a bare v6 address has more than one colon.
    if let Some(inner) = rest.strip_prefix('[')
        && let Some((host, after)) = inner.split_once(']')
    {
        let port = after.strip_prefix(':').and_then(|p| p.parse().ok());
        return (user, host.to_owned(), port);
    }
    match rest.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => match port.parse() {
            Ok(port) => (user, host.to_owned(), Some(port)),
            Err(_) => (user, rest.to_owned(), None),
        },
        _ => (user, rest.to_owned(), None),
    }
}

/// `Host` patterns: any positive one matching, and no negated one.
fn host_matches(patterns: &[String], host: &str) -> bool {
    let mut matched = false;
    for pattern in patterns {
        match pattern.strip_prefix('!') {
            Some(negated) if wildcard(negated, host) => return false,
            Some(_) => {},
            None => matched |= wildcard(pattern, host),
        }
    }
    matched
}

/// A comma-separated pattern list, as `Match` takes: as [`host_matches`].
fn list_matches(list: &str, text: &str) -> bool {
    let patterns: Vec<String> = list.split(',').map(str::to_owned).collect();
    host_matches(&patterns, text)
}

fn criterion_matches(criterion: &Criterion, alias: &str, local: &Local, found: &Found) -> bool {
    match criterion {
        Criterion::All => true,
        Criterion::Unknown(_) => false,
        Criterion::Patterns { what, patterns, negated } => {
            let text = match what.as_str() {
                "host" => {
                    found.first("hostname").map_or(alias.to_owned(), |h| expand_host(h, alias))
                },
                "originalhost" => alias.to_owned(),
                "user" => found.first("user").unwrap_or(&local.user).to_owned(),
                _ => local.user.clone(),
            };
            list_matches(patterns, &text) != *negated
        },
    }
}

/// `*` and `?`. Case counts, as in OpenSSH's `Host` (the host name is
/// lowercased only after).
pub(crate) fn wildcard(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => (0..=t.len()).any(|skip| go(rest, &t[skip..])),
            Some(('?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some((c, rest)) => t.first().is_some_and(|t0| t0 == c) && go(rest, &t[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    go(&p, &t)
}

/// `HostName`'s tokens: `%h` (the alias) and `%%`.
fn expand_host(name: &str, alias: &str) -> String {
    name.replace("%%", "\u{0}").replace("%h", alias).replace('\u{0}', "%")
}

/// Options as found, the first value of each (all of the adding ones).
#[derive(Default)]
struct Found {
    values: HashMap<String, Vec<Vec<String>>>,
    /// `Match` criteria that can't be told here (taken as not matching).
    unknown: Vec<String>,
}

impl Found {
    fn unknown_match(&mut self, what: &str) {
        let what = format!("match {what}");
        if !self.unknown.contains(&what) {
            self.unknown.push(what);
        }
    }

    fn set(&mut self, keyword: &str, args: Vec<String>) {
        let keyword = match keyword {
            // The old name of the same thing.
            "challengeresponseauthentication" => "kbdinteractiveauthentication",
            k => k,
        };
        let values = self.values.entry(keyword.to_owned()).or_default();
        if values.is_empty() || ADDING.contains(&keyword) {
            values.push(args);
        }
    }

    fn first(&self, keyword: &str) -> Option<&str> {
        self.values.get(keyword)?.first()?.first().map(String::as_str)
    }

    fn all(&self, keyword: &str) -> impl Iterator<Item = &Vec<String>> {
        self.values.get(keyword).into_iter().flatten()
    }

    fn flag(&self, keyword: &str, default: bool) -> bool {
        match self.first(keyword).map(str::to_lowercase).as_deref() {
            Some("yes" | "true") => true,
            Some("no" | "false") => false,
            _ => default,
        }
    }

    fn number(&self, keyword: &str) -> Option<u32> {
        self.first(keyword)?.parse().ok()
    }

    /// The rest of the line, as one (RemoteCommand, ProxyCommand).
    fn line(&self, keyword: &str) -> Option<String> {
        let args = self.values.get(keyword)?.first()?;
        Some(args.join(" ")).filter(|line| !line.eq_ignore_ascii_case("none"))
    }

    fn resolved(&self, alias: String, _local: &Local) -> Resolved {
        let host_name =
            self.first("hostname").map_or(alias.to_lowercase(), |h| expand_host(h, &alias));
        let none = |s: &&str| !s.eq_ignore_ascii_case("none");
        let proxy_jump = self
            .first("proxyjump")
            .filter(none)
            .map(|jumps| {
                jumps
                    .split(',')
                    .filter(|j| !j.is_empty())
                    .map(|jump| {
                        let (user, host, port) = split_destination(jump);
                        Hop { user, host, port }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let forwards = |keyword: &str, dynamic: bool| -> Vec<Forward> {
            self.all(keyword).filter_map(|args| parse_forward(args, dynamic)).collect()
        };
        let set_env = self
            .all("setenv")
            .flatten()
            .filter_map(|pair| pair.split_once('='))
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        let mut ignored: Vec<String> = self
            .values
            .keys()
            .filter(|k| !DONE.contains(&k.as_str()) && !QUIET.contains(&k.as_str()))
            .cloned()
            .collect();
        ignored.sort();
        ignored.extend(self.unknown.iter().cloned());
        let preferred = self
            .first("preferredauthentications")
            .map(|list| list.split(',').map(str::to_owned).collect())
            .unwrap_or_else(|| {
                ["publickey", "keyboard-interactive", "password"].map(str::to_owned).to_vec()
            });
        Resolved {
            alias,
            host_name,
            user: self.first("user").map(str::to_owned),
            port: self.number("port").and_then(|p| u16::try_from(p).ok()).unwrap_or(22),
            identity_files: self
                .all("identityfile")
                .filter_map(|args| args.first().cloned())
                .filter(|f| !f.eq_ignore_ascii_case("none"))
                .collect(),
            identities_only: self.flag("identitiesonly", false),
            proxy_jump,
            proxy_command: self.line("proxycommand"),
            local_forwards: forwards("localforward", false),
            remote_forwards: forwards("remoteforward", false),
            dynamic_forwards: forwards("dynamicforward", true),
            server_alive_interval: self.number("serveraliveinterval").unwrap_or(0),
            server_alive_count_max: self.number("serveralivecountmax").unwrap_or(3),
            connect_timeout: self.number("connecttimeout"),
            compression: self.flag("compression", false),
            ciphers: self.first("ciphers").map(str::to_owned),
            kex_algorithms: self.first("kexalgorithms").map(str::to_owned),
            host_key_algorithms: self.first("hostkeyalgorithms").map(str::to_owned),
            macs: self.first("macs").map(str::to_owned),
            preferred_authentications: preferred,
            pubkey_authentication: self.flag("pubkeyauthentication", true),
            password_authentication: self.flag("passwordauthentication", true),
            kbd_interactive_authentication: self.flag("kbdinteractiveauthentication", true),
            strict_host_key_checking: match self
                .first("stricthostkeychecking")
                .map(str::to_lowercase)
                .as_deref()
            {
                Some("yes" | "true") => StrictHostKeys::Yes,
                Some("no" | "off" | "false") => StrictHostKeys::No,
                Some("accept-new") => StrictHostKeys::AcceptNew,
                _ => StrictHostKeys::Ask,
            },
            host_key_alias: self.first("hostkeyalias").map(str::to_owned),
            set_env,
            remote_command: self.line("remotecommand"),
            request_tty: match self.first("requesttty").map(str::to_lowercase).as_deref() {
                Some("yes" | "true") => RequestTty::Yes,
                Some("no" | "false") => RequestTty::No,
                Some("force") => RequestTty::Force,
                _ => RequestTty::Auto,
            },
            forward_agent: self.flag("forwardagent", false),
            ignored,
        }
    }
}

/// `[bind:]port [host:hostport]` (or the two joined by a colon, as `-L`
/// takes them); `None` for what isn't (a Unix socket's path, say).
fn parse_forward(args: &[String], dynamic: bool) -> Option<Forward> {
    // Fields split on colons outside brackets.
    fn fields(text: &str) -> Vec<String> {
        let (mut out, mut field, mut bracket) = (Vec::new(), String::new(), false);
        for c in text.chars() {
            match c {
                '[' => bracket = true,
                ']' => bracket = false,
                ':' if !bracket => out.push(std::mem::take(&mut field)),
                c => field.push(c),
            }
        }
        out.push(field);
        out
    }
    let mut all = Vec::new();
    for arg in args {
        if arg.contains('/') {
            return None;
        }
        all.extend(fields(arg));
    }
    let (listen, to) = if dynamic {
        (&all[..], None)
    } else {
        let n = all.len();
        if n < 3 {
            return None;
        }
        let port = all[n - 1].parse().ok()?;
        (&all[..n - 2], Some((all[n - 2].clone(), port)))
    };
    let (bind, port) = match listen {
        [port] => (None, port),
        [bind, port] => (Some(bind.clone()).filter(|b| !b.is_empty()), port),
        _ => return None,
    };
    Some(Forward { bind, port: port.parse().ok()?, to })
}

impl Resolved {
    /// `~` and `%` tokens in a path (`IdentityFile`'s): `%d` home, `%h`
    /// host name, `%n` alias, `%p` port, `%r` remote user, `%u` and `%L`/`%l`
    /// local user and host, `%%`.
    pub fn expand(&self, text: &str, local: &Local) -> String {
        let text = match text.strip_prefix("~/") {
            Some(rest) => format!("{}/{rest}", local.home),
            None => text.to_owned(),
        };
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => out.push('%'),
                Some('d') => out.push_str(&local.home),
                Some('h') => out.push_str(&self.host_name),
                Some('n') => out.push_str(&self.alias),
                Some('p') => out.push_str(&self.port.to_string()),
                Some('r') => out.push_str(self.user.as_deref().unwrap_or(&local.user)),
                Some('u') => out.push_str(&local.user),
                Some('L' | 'l') => out.push_str(&local.host),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                },
                None => out.push('%'),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        Config::parse(text, &|_| Vec::new())
    }

    fn local() -> Local {
        Local { home: "/home/me".into(), user: "me".into(), host: "phone".into() }
    }

    #[test]
    fn first_value_wins_and_keys_add_up() {
        let c = config(
            "Host a\n  HostName real-%h.example\n  User u1\n  Port 2200\n  IdentityFile ~/.ssh/k1\n\
             Host *\n  User other\n  ServerAliveInterval 30\n  IdentityFile ~/.ssh/id_x\n",
        );
        let r = c.resolve("a", &local());
        assert_eq!(r.host_name, "real-a.example");
        assert_eq!(r.user.as_deref(), Some("u1"));
        assert_eq!(r.port, 2200);
        assert_eq!(r.identity_files, ["~/.ssh/k1", "~/.ssh/id_x"]);
        assert_eq!(r.server_alive_interval, 30);
        assert_eq!(r.expand(&r.identity_files[0], &local()), "/home/me/.ssh/k1");
        // Another host: only `Host *`.
        let r = c.resolve("b", &local());
        assert_eq!((r.host_name.as_str(), r.user.as_deref(), r.port), ("b", Some("other"), 22));
        // The destination's own user and port win.
        let r = c.resolve("root@a:1", &local());
        assert_eq!((r.user.as_deref(), r.port), (Some("root"), 1));
    }

    #[test]
    fn patterns() {
        let c = config("Host *.corp !bastion.corp\n  User corp\nHost web?\n  Port 8022\n");
        assert_eq!(c.resolve("db.corp", &local()).user.as_deref(), Some("corp"));
        // As OpenSSH: patterns match the name as typed, case and all.
        let r = c.resolve("DB.CORP", &local());
        assert_eq!((r.user, r.host_name.as_str()), (None, "db.corp"));
        assert_eq!(c.resolve("bastion.corp", &local()).user, None);
        assert_eq!(c.resolve("web1", &local()).port, 8022);
        assert_eq!(c.resolve("web12", &local()).port, 22);
        assert_eq!(c.hosts(), Vec::<String>::new());
        let c = config("Host devbox pi\n  User x\nHost *\n  Port 1\n");
        assert_eq!(c.hosts(), ["devbox", "pi"]);
    }

    #[test]
    fn match_blocks() {
        let c = config(
            "Host short\n  HostName long.example.org\nMatch host *.example.org\n  User ex\n\
             Match originalhost short user ex\n  Port 7\nMatch exec \"true\"\n  Port 9\nMatch all\n  Compression yes\n",
        );
        let r = c.resolve("short", &local());
        assert_eq!((r.user.as_deref(), r.port, r.compression), (Some("ex"), 7, true));
        assert_eq!(c.resolve("other", &local()).port, 22);
        // Told: `exec` can't run here.
        assert_eq!(c.resolve("short", &local()).ignored, ["match exec"]);
    }

    #[test]
    fn jumps_forwards_and_the_rest() {
        let c = config(
            "Host t\n  ProxyJump j1,me@j2:2222\n  LocalForward 8080 localhost:80\n  LocalForward 127.0.0.1:5432:db:5432\n\
             RemoteForward [::1]:9000 [::1]:9001\n  DynamicForward 1080\n  LocalForward /tmp/s host:1\n\
             SetEnv A=1 B=2\n  SetEnv C=3\n  RemoteCommand tmux new -A\n  RequestTTY force\n\
             StrictHostKeyChecking accept-new\n  ForwardAgent yes\n  PreferredAuthentications password,publickey\n\
             Ciphers +aes128-cbc\n  ProxyCommand nc %h %p\n  ControlMaster auto\n  GSSAPIAuthentication yes\n",
        );
        let r = c.resolve("t", &local());
        assert_eq!(
            r.proxy_jump,
            [
                Hop { user: None, host: "j1".into(), port: None },
                Hop { user: Some("me".into()), host: "j2".into(), port: Some(2222) },
            ]
        );
        assert_eq!(
            r.local_forwards,
            [
                Forward { bind: None, port: 8080, to: Some(("localhost".into(), 80)) },
                Forward {
                    bind: Some("127.0.0.1".into()),
                    port: 5432,
                    to: Some(("db".into(), 5432))
                },
            ]
        );
        assert_eq!(
            r.remote_forwards,
            [Forward { bind: Some("::1".into()), port: 9000, to: Some(("::1".into(), 9001)) }]
        );
        assert_eq!(r.dynamic_forwards, [Forward { bind: None, port: 1080, to: None }]);
        assert_eq!(r.set_env, [("A".into(), "1".into()), ("B".into(), "2".into())]);
        assert_eq!(r.remote_command.as_deref(), Some("tmux new -A"));
        assert_eq!(r.request_tty, RequestTty::Force);
        assert_eq!(r.strict_host_key_checking, StrictHostKeys::AcceptNew);
        assert!(r.forward_agent);
        assert_eq!(r.preferred_authentications, ["password", "publickey"]);
        assert_eq!(r.ciphers.as_deref(), Some("+aes128-cbc"));
        assert_eq!(r.proxy_command.as_deref(), Some("nc %h %p"));
        // Told about, but not ControlMaster (a desktop's business).
        assert_eq!(r.ignored, ["gssapiauthentication"]);
    }

    #[test]
    fn lines_includes_and_destinations() {
        let c = Config::parse(
            "# a comment\nUser=all\nHost \"with space\" x\n  Include extra\n  Port 3\nHost y\n  Bad\n",
            &|pattern| {
                assert_eq!(pattern, "extra");
                vec!["Port 4\nHost nothing\n  Port 5\n".into()]
            },
        );
        assert_eq!(c.errors, [(7, "bad without a value".to_owned())]);
        let r = c.resolve("x", &local());
        // The include's Port first; its own Host block doesn't end x's.
        assert_eq!((r.user.as_deref(), r.port), (Some("all"), 4));
        assert_eq!(c.resolve("with space", &local()).port, 4);
        let r = c.resolve("ssh://u@[::1]:2", &local());
        assert_eq!((r.user.as_deref(), r.host_name.as_str(), r.port), (Some("u"), "::1", 2));
        let r = c.resolve("fe80::1", &local());
        assert_eq!((r.host_name.as_str(), r.port), ("fe80::1", 22));
    }
}
