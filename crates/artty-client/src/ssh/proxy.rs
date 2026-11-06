//! `ProxyCommand`s, done here: a phone has no shell to run them in, but the
//! ones people write mostly do a few things this client can do itself. Each
//! is read for what it does — through another ssh (`ssh -W %h:%p jump`, `ssh
//! jump nc %h %p`), through a SOCKS or HTTP proxy (netcat's `-X`/`-x`,
//! ncat's `--proxy`, `connect -S`/`-H`, corkscrew, socat's `SOCKS4A:` and
//! `PROXY:`), straight there (`nc %h %p`, `socat - TCP:%h:%p`), or through a
//! Cloudflare Tunnel (`cloudflared access ssh`) — and anything else is
//! refused, saying so.

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::cloudflare::Tunnel;
use crate::ssh_config::{Local, Resolved};

/// What a `ProxyCommand` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proxy {
    /// Through another machine's ssh (`ssh -W`): `destination` as the
    /// config resolves it (with `identity` first, from `-i`), on to `to`.
    Jump {
        destination: String,
        identity: Option<String>,
        to: (String, u16),
    },
    /// Through a SOCKS proxy at `at` (version 4, as 4a, or 5) to `to`.
    Socks {
        version: u8,
        at: (String, u16),
        to: (String, u16),
        login: Option<(String, String)>,
    },
    /// Through an HTTP proxy at `at` (`CONNECT`) to `to`.
    Http {
        at: (String, u16),
        to: (String, u16),
        login: Option<(String, String)>,
    },
    /// Straight to `to`.
    Direct {
        to: (String, u16),
    },
    Cloudflare(Tunnel),
}

impl Proxy {
    /// For the user: what it goes through.
    pub fn describe(&self) -> String {
        let at = |(host, port): &(String, u16)| format!("{host}:{port}");
        match self {
            Proxy::Jump { destination, to, .. } => {
                format!("through {destination} (ssh -W), to {}", at(to))
            },
            Proxy::Socks { version, at: proxy, to, .. } => {
                format!("through the SOCKS{version} proxy at {}, to {}", at(proxy), at(to))
            },
            Proxy::Http { at: proxy, to, .. } => {
                format!("through the HTTP proxy at {}, to {}", at(proxy), at(to))
            },
            Proxy::Direct { to } => format!("straight to {}", at(to)),
            Proxy::Cloudflare(tunnel) => {
                let access = if tunnel.service_token.is_some() {
                    ", with an Access service token"
                } else {
                    ""
                };
                format!("through Cloudflare's tunnel at {}{access}", tunnel.hostname)
            },
        }
    }
}

/// What `resolved`'s `ProxyCommand` does: `None` without one, an error for
/// one this client can't do.
pub fn proxy(resolved: &Resolved) -> Result<Option<Proxy>, String> {
    let Some(command) = resolved.proxy_command.as_deref() else { return Ok(None) };
    let refused = |why: &str| {
        Err(format!(
            "{}: ProxyCommand {command}: {why}; a ProxyJump can do the same",
            resolved.alias
        ))
    };
    let Some(mut words) = words(command) else {
        return refused("its shell syntax (pipes, `;`, `$(…)`) can't run here");
    };
    // `exec nc …`: the same.
    if words.first().map(String::as_str) == Some("exec") {
        words.remove(0);
    }
    let local = Local::default();
    let words: Vec<String> = words.iter().map(|w| resolved.expand(w, &local)).collect();
    let Some((program, args)) = words.split_first() else { return refused("it's empty") };
    let name = program.rsplit('/').next().unwrap_or(program);
    let read = match name {
        "ssh" => ssh(args),
        "nc" | "netcat" => netcat(args),
        "ncat" => ncat(args),
        "connect" | "connect-proxy" => connect(args),
        "corkscrew" => corkscrew(args),
        "socat" => socat(args),
        "cloudflared" => cloudflared(args),
        _ => Err(format!("{name} can't run here")),
    };
    match read {
        Ok(proxy) => Ok(Some(proxy)),
        Err(why) => refused(&why),
    }
}

/// A command's words, as a shell splits them (quotes, backslashes); `None`
/// if it does more than run one program.
fn words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    w.push(c);
                }
            },
            '"' => {
                let w = word.get_or_insert_with(String::new);
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => w.extend(chars.next()),
                        '$' | '`' => return None,
                        c => w.push(c),
                    }
                }
            },
            '\\' => word.get_or_insert_with(String::new).extend(chars.next()),
            c if c.is_whitespace() => words.extend(word.take()),
            '|' | ';' | '&' | '<' | '>' | '$' | '`' | '(' | ')' => return None,
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    Some(words)
}

/// `host` and `port` (as words), as a target.
fn target(host: Option<&String>, port: Option<&String>) -> Result<(String, u16), String> {
    let host = host.ok_or("no host to go to")?;
    let port = port.ok_or("no port to go to")?.parse().map_err(|_| "a bad port")?;
    Ok((host.trim_matches(['[', ']']).to_owned(), port))
}

/// `host[:port]` (or `[v6]:port`), the port `default` if not given.
fn host_port(text: &str, default: u16) -> Result<(String, u16), String> {
    if let Some(inner) = text.strip_prefix('[')
        && let Some((host, rest)) = inner.split_once(']')
    {
        let port =
            rest.strip_prefix(':').map_or(Ok(default), |p| p.parse().map_err(|_| "a bad port"))?;
        return Ok((host.to_owned(), port));
    }
    match text.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            Ok((host.to_owned(), port.parse().map_err(|_| "a bad port")?))
        },
        _ => Ok((text.to_owned(), default)),
    }
}

/// Options read: each letter, and its value if it takes one.
type Flags = Vec<(char, Option<String>)>;

/// Options and what's left, as getopt reads them: `valued` letters take a
/// value (attached, or the next word).
fn getopt(args: &[String], valued: &str) -> Result<(Flags, Vec<String>), String> {
    let (mut flags, mut rest) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            rest.extend(args[i + 1..].iter().cloned());
            break;
        }
        // The first word that isn't an option ends them (as POSIX getopt).
        if !arg.starts_with('-') || arg == "-" {
            rest.extend(args[i..].iter().cloned());
            break;
        }
        for (at, letter) in arg[1..].char_indices() {
            if valued.contains(letter) {
                let attached = &arg[1 + at + letter.len_utf8()..];
                let value = if attached.is_empty() {
                    i += 1;
                    args.get(i).cloned().ok_or(format!("-{letter} without its value"))?
                } else {
                    attached.to_owned()
                };
                flags.push((letter, Some(value)));
                break;
            }
            flags.push((letter, None));
        }
        i += 1;
    }
    Ok((flags, rest))
}

/// `ssh [options] jump -W host:port`, or `ssh [options] jump nc host port`.
fn ssh(args: &[String]) -> Result<Proxy, String> {
    // Options, the destination, more options (as ssh takes them), the command.
    const VALUED: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let (mut flags, rest) = getopt(args, VALUED)?;
    let (destination, after) = rest.split_first().ok_or("no machine to go through")?;
    let (more, command) = getopt(after, VALUED)?;
    flags.extend(more);
    let (mut user, mut port, mut to, mut identity) = (None, None, None, None);
    for (flag, value) in flags {
        match (flag, value) {
            ('l', Some(v)) => user = Some(v),
            ('p', Some(v)) => port = Some(v),
            ('i', Some(v)) => identity = Some(v),
            ('W', Some(v)) => to = Some(host_port(&v, 22)?),
            ('J', _) => return Err("its own jump hosts (-J)".into()),
            _ => {},
        }
    }
    // No -W: a command that connects on (`nc host port`), the same.
    if to.is_none() {
        let mut command = command.clone();
        if command.first().is_some_and(|w| w == "exec") {
            command.remove(0);
        }
        let program = command.first().map(|w| w.rsplit('/').next().unwrap_or(w).to_owned());
        if !matches!(program.as_deref(), Some("nc" | "netcat" | "ncat")) {
            return Err("an ssh without -W, or nc there".into());
        }
        let (_, positional) = getopt(&command[1..], "wqi")?;
        to = Some(target(positional.first(), positional.get(1))?);
    } else if !command.is_empty() {
        return Err("an ssh with both -W and a command".into());
    }
    let mut destination = destination.to_string();
    if let Some(user) = user
        && !destination.contains('@')
    {
        destination = format!("{user}@{destination}");
    }
    if let Some(port) = port {
        destination = format!("{destination}:{port}");
    }
    Ok(Proxy::Jump { destination, identity, to: to.expect("set above") })
}

/// OpenBSD's netcat: `nc [-X 4|5|connect] [-x proxy[:port]] host port`.
fn netcat(args: &[String]) -> Result<Proxy, String> {
    let (flags, rest) = getopt(args, "IiMmOPpsTVwXx")?;
    let (mut kind, mut proxy) = (None, None);
    for (flag, value) in flags {
        match (flag, value) {
            ('X', Some(v)) => kind = Some(v.to_lowercase()),
            ('x', Some(v)) => proxy = Some(v),
            ('P', _) => return Err("a proxy login that asks for a password (-P)".into()),
            ('l' | 'u' | 'U', _) => return Err("not a TCP connection".into()),
            _ => {},
        }
    }
    let to = target(rest.first(), rest.get(1))?;
    let Some(proxy) = proxy else { return Ok(Proxy::Direct { to }) };
    match kind.as_deref().unwrap_or("5") {
        "5" => Ok(Proxy::Socks { version: 5, at: host_port(&proxy, 1080)?, to, login: None }),
        "4" => Ok(Proxy::Socks { version: 4, at: host_port(&proxy, 1080)?, to, login: None }),
        "connect" => Ok(Proxy::Http { at: host_port(&proxy, 3128)?, to, login: None }),
        other => Err(format!("a proxy of kind {other}")),
    }
}

/// nmap's ncat: `ncat [--proxy host:port [--proxy-type socks4|socks5|http]
/// [--proxy-auth user:password]] host port`.
fn ncat(args: &[String]) -> Result<Proxy, String> {
    let (mut proxy, mut kind, mut login, mut positional) = (None, None, None, Vec::new());
    let mut words = args.iter();
    while let Some(word) = words.next() {
        let (name, attached) = match word.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (word.as_str(), None),
        };
        let mut value = || {
            attached
                .clone()
                .or_else(|| words.next().cloned())
                .ok_or(format!("{name} without its value"))
        };
        match name {
            "--proxy" => proxy = Some(value()?),
            "--proxy-type" => kind = Some(value()?.to_lowercase()),
            "--proxy-auth" => login = Some(value()?),
            "--proxy-dns" | "-w" | "--wait" | "-i" | "--idle-timeout" => drop(value()?),
            "-l" | "--listen" | "-u" | "--udp" => return Err("not a TCP connection".into()),
            n if n.starts_with('-') => {},
            _ => positional.push(word),
        }
    }
    let to = target(positional.first().copied(), positional.get(1).copied())?;
    let login = login.map(|l| match l.split_once(':') {
        Some((user, password)) => (user.to_owned(), password.to_owned()),
        None => (l.clone(), String::new()),
    });
    let Some(proxy) = proxy else { return Ok(Proxy::Direct { to }) };
    match kind.as_deref().unwrap_or("http") {
        "socks5" => Ok(Proxy::Socks { version: 5, at: host_port(&proxy, 1080)?, to, login }),
        "socks4" => Ok(Proxy::Socks { version: 4, at: host_port(&proxy, 1080)?, to, login }),
        "http" => Ok(Proxy::Http { at: host_port(&proxy, 3128)?, to, login }),
        other => Err(format!("a proxy of kind {other}")),
    }
}

/// connect-proxy: `connect [-4|-5] -S [user@]proxy[:port] host port`, or
/// `connect -H [user@]proxy[:port] host port`.
fn connect(args: &[String]) -> Result<Proxy, String> {
    let (flags, rest) = getopt(args, "cHpRSTw")?;
    let to = target(rest.first(), rest.get(1))?;
    let mut version = 5;
    let mut proxy = None;
    for (flag, value) in flags {
        match (flag, value) {
            ('4', _) => version = 4,
            ('5', _) => version = 5,
            ('S', Some(v)) => proxy = Some(('S', v)),
            ('H', Some(v)) => proxy = Some(('H', v)),
            ('T', _) | ('c', _) => return Err("a proxy of that kind".into()),
            _ => {},
        }
    }
    let user_less = |v: &str| v.rsplit_once('@').map_or(v, |(_, at)| at).to_owned();
    match proxy {
        Some(('S', v)) => {
            Ok(Proxy::Socks { version, at: host_port(&user_less(&v), 1080)?, to, login: None })
        },
        Some((_, v)) => Ok(Proxy::Http { at: host_port(&user_less(&v), 80)?, to, login: None }),
        None => Ok(Proxy::Direct { to }),
    }
}

/// `corkscrew proxy proxyport host port` (HTTP CONNECT).
fn corkscrew(args: &[String]) -> Result<Proxy, String> {
    if args.len() > 4 {
        return Err("a login file (on the computer, not here)".into());
    }
    let at = target(args.first(), args.get(1))?;
    let to = target(args.get(2), args.get(3))?;
    Ok(Proxy::Http { at, to, login: None })
}

/// `socat - TCP:host:port`, `socat - SOCKS4A:proxy:host:port[,socksport=N]`,
/// `socat - PROXY:proxy:host:port[,proxyport=N][,proxyauth=user:password]`.
fn socat(args: &[String]) -> Result<Proxy, String> {
    let addresses: Vec<&String> =
        args.iter().filter(|a| !a.starts_with('-') || a.as_str() == "-").collect();
    let address = addresses
        .iter()
        .find(|a| {
            !matches!(
                a.to_uppercase().split([',', ':']).next(),
                Some("-" | "STDIO" | "STDIN" | "STDOUT")
            )
        })
        .ok_or("no address to go to")?;
    let (head, options) = address.split_once(',').map_or((address.as_str(), ""), |(h, o)| (h, o));
    let option = |name: &str| {
        options.split(',').find_map(|o| {
            o.split_once('=').filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v)
        })
    };
    let fields: Vec<&str> = head.split(':').collect();
    let kind = fields[0].to_uppercase();
    let word = |i: usize| fields.get(i).map(|s| (*s).to_owned());
    match kind.as_str() {
        "TCP" | "TCP4" | "TCP6" | "TCP-CONNECT" => {
            Ok(Proxy::Direct { to: target(word(1).as_ref(), word(2).as_ref())? })
        },
        "SOCKS4" | "SOCKS4A" => {
            let port =
                option("socksport").map_or(Ok(1080), |p| p.parse().map_err(|_| "a bad port"))?;
            let at = (word(1).ok_or("no proxy")?, port);
            Ok(Proxy::Socks {
                version: 4,
                at,
                to: target(word(2).as_ref(), word(3).as_ref())?,
                login: None,
            })
        },
        "PROXY" | "PROXY-CONNECT" => {
            let port =
                option("proxyport").map_or(Ok(8080), |p| p.parse().map_err(|_| "a bad port"))?;
            let at = (word(1).ok_or("no proxy")?, port);
            let login = option("proxyauth").map(|l| match l.split_once(':') {
                Some((u, p)) => (u.to_owned(), p.to_owned()),
                None => (l.to_owned(), String::new()),
            });
            Ok(Proxy::Http { at, to: target(word(2).as_ref(), word(3).as_ref())?, login })
        },
        other => Err(format!("socat's {other} address")),
    }
}

/// `cloudflared [global flags] access ssh --hostname H [--service-token-id
/// ID --service-token-secret SECRET]`, flags as Go reads them.
fn cloudflared(args: &[String]) -> Result<Proxy, String> {
    let at = args
        .windows(2)
        .position(|w| w[0] == "access" && w[1] == "ssh")
        .ok_or("cloudflared other than access ssh")?;
    let args = &args[at + 2..];
    let flag = |names: &[&str]| -> Option<String> {
        args.iter().enumerate().find_map(|(i, arg)| {
            if !arg.starts_with('-') {
                return None;
            }
            let bare = arg.trim_start_matches('-');
            names.iter().find_map(|name| match bare.split_once('=') {
                Some((n, value)) if n == *name => Some(value.to_owned()),
                None if bare == *name => args.get(i + 1).cloned(),
                _ => None,
            })
        })
    };
    let hostname = flag(&["hostname"]).ok_or("cloudflared without --hostname")?;
    let hostname = hostname
        .trim_start_matches("https://")
        .trim_start_matches("ssh://")
        .trim_end_matches('/')
        .to_owned();
    if hostname.is_empty() {
        return Err("cloudflared without --hostname".into());
    }
    let service_token =
        flag(&["service-token-id", "id"]).zip(flag(&["service-token-secret", "secret"]));
    Ok(Proxy::Cloudflare(Tunnel { hostname, service_token }))
}

/// Open a SOCKS or HTTP proxy's way to its target (the others aren't
/// opened here): the stream that is the machine's ssh.
pub(super) async fn open(proxy: &Proxy) -> Result<TcpStream, String> {
    let connect = |(host, port): &(String, u16)| {
        let (host, port) = (host.clone(), *port);
        async move {
            let tcp = TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|err| format!("{host}:{port}: {err}"))?;
            let _ = tcp.set_nodelay(true);
            Ok::<_, String>(tcp)
        }
    };
    match proxy {
        Proxy::Direct { to } => connect(to).await,
        Proxy::Socks { version, at, to, login } => {
            let mut tcp = connect(at).await?;
            let shaken = if *version == 4 {
                socks4(&mut tcp, to, login).await
            } else {
                socks5(&mut tcp, to, login).await
            };
            shaken.map_err(|err| format!("the SOCKS proxy at {}:{}: {err}", at.0, at.1))?;
            Ok(tcp)
        },
        Proxy::Http { at, to, login } => {
            let mut tcp = connect(at).await?;
            http_connect(&mut tcp, to, login)
                .await
                .map_err(|err| format!("the HTTP proxy at {}:{}: {err}", at.0, at.1))?;
            Ok(tcp)
        },
        Proxy::Jump { .. } | Proxy::Cloudflare(_) => Err("not a proxy to open here".into()),
    }
}

async fn socks5(
    tcp: &mut TcpStream,
    (host, port): &(String, u16),
    login: &Option<(String, String)>,
) -> Result<(), String> {
    let io = |err: std::io::Error| err.to_string();
    let methods: &[u8] = if login.is_some() { &[0, 2] } else { &[0] };
    let mut hello = vec![5, methods.len() as u8];
    hello.extend_from_slice(methods);
    tcp.write_all(&hello).await.map_err(io)?;
    let mut chosen = [0; 2];
    tcp.read_exact(&mut chosen).await.map_err(io)?;
    match (chosen, login) {
        ([5, 0], _) => {},
        ([5, 2], Some((user, password))) => {
            let mut auth = vec![1, user.len() as u8];
            auth.extend_from_slice(user.as_bytes());
            auth.push(password.len() as u8);
            auth.extend_from_slice(password.as_bytes());
            tcp.write_all(&auth).await.map_err(io)?;
            let mut answer = [0; 2];
            tcp.read_exact(&mut answer).await.map_err(io)?;
            if answer[1] != 0 {
                return Err("the login was refused".into());
            }
        },
        _ => return Err("it wants a login this command doesn't give".into()),
    }
    let mut request = vec![5, 1, 0];
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        },
        Ok(std::net::IpAddr::V6(ip)) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        },
        Err(_) => {
            request.extend_from_slice(&[3, host.len() as u8]);
            request.extend_from_slice(host.as_bytes());
        },
    }
    request.extend_from_slice(&port.to_be_bytes());
    tcp.write_all(&request).await.map_err(io)?;
    let mut head = [0; 4];
    tcp.read_exact(&mut head).await.map_err(io)?;
    if head[1] != 0 {
        let why = match head[1] {
            2 => "not allowed",
            3 => "the network is unreachable",
            4 => "the host is unreachable",
            5 => "the connection was refused",
            _ => "it failed",
        };
        return Err(format!("reaching {host}:{port}: {why}"));
    }
    // The address it's bound to, then its port: read past.
    let rest = match head[3] {
        1 => 4 + 2,
        4 => 16 + 2,
        3 => tcp.read_u8().await.map_err(io)? as usize + 2,
        _ => return Err("an answer it doesn't read".into()),
    };
    tcp.read_exact(&mut vec![0; rest]).await.map_err(io)?;
    Ok(())
}

/// SOCKS 4a (4 for an IPv4 address): by name, the proxy finding it.
async fn socks4(
    tcp: &mut TcpStream,
    (host, port): &(String, u16),
    login: &Option<(String, String)>,
) -> Result<(), String> {
    let io = |err: std::io::Error| err.to_string();
    let mut request = vec![4, 1];
    request.extend_from_slice(&port.to_be_bytes());
    let ip = host.parse::<std::net::Ipv4Addr>().ok();
    request.extend_from_slice(&ip.map_or([0, 0, 0, 1], |ip| ip.octets()));
    if let Some((user, _)) = login {
        request.extend_from_slice(user.as_bytes());
    }
    request.push(0);
    if ip.is_none() {
        request.extend_from_slice(host.as_bytes());
        request.push(0);
    }
    tcp.write_all(&request).await.map_err(io)?;
    let mut answer = [0; 8];
    tcp.read_exact(&mut answer).await.map_err(io)?;
    if answer[1] != 0x5a {
        return Err(format!("reaching {host}:{port}: refused"));
    }
    Ok(())
}

async fn http_connect(
    tcp: &mut TcpStream,
    (host, port): &(String, u16),
    login: &Option<(String, String)>,
) -> Result<(), String> {
    let io = |err: std::io::Error| err.to_string();
    let authority =
        if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some((user, password)) = login {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("\r\n");
    tcp.write_all(request.as_bytes()).await.map_err(io)?;
    // Its answer's head, a byte at a time: what follows is ssh's.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 {
            return Err("an answer too long".into());
        }
        head.push(tcp.read_u8().await.map_err(io)?);
    }
    let head = String::from_utf8_lossy(&head);
    let status = head.lines().next().unwrap_or_default();
    let code = status.split_whitespace().nth(1).unwrap_or_default();
    if !code.starts_with('2') {
        return Err(format!("reaching {host}:{port}: {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_config::Config;

    fn proxy_of(command: &str) -> Result<Option<Proxy>, String> {
        let config = format!(
            "Host box\n  HostName box.example.com\n  Port 2222\n  User me\n  ProxyCommand {command}\n"
        );
        proxy(&Config::parse(&config, &|_| Vec::new()).resolve("box", &Local::default()))
    }

    fn to() -> (String, u16) {
        ("box.example.com".into(), 2222)
    }

    #[test]
    fn through_ssh() {
        let jump = |destination: &str, identity: Option<&str>, to: (String, u16)| {
            Ok(Some(Proxy::Jump {
                destination: destination.into(),
                identity: identity.map(Into::into),
                to,
            }))
        };
        assert_eq!(proxy_of("ssh -W %h:%p bastion"), jump("bastion", None, to()));
        assert_eq!(
            proxy_of("exec ssh -q -l admin -p 2200 -i ~/.ssh/work bastion -W %h:%p"),
            jump("admin@bastion:2200", Some("/.ssh/work"), to())
        );
        assert_eq!(proxy_of("ssh bastion nc %h %p"), jump("bastion", None, to()));
        assert_eq!(
            proxy_of("ssh -o StrictHostKeyChecking=no bastion exec nc -w 10 %h %p"),
            jump("bastion", None, to())
        );
        assert_eq!(
            proxy_of("ssh -W [fe80::1]:22 bastion"),
            jump("bastion", None, ("fe80::1".into(), 22))
        );
        assert!(proxy_of("ssh bastion").unwrap_err().contains("without -W"));
        assert!(proxy_of("ssh -J a bastion -W %h:%p").is_err());
    }

    #[test]
    fn through_proxies() {
        let socks = |version, at: (&str, u16), login: Option<(&str, &str)>| {
            Ok(Some(Proxy::Socks {
                version,
                at: (at.0.into(), at.1),
                to: to(),
                login: login.map(|(u, p)| (u.into(), p.into())),
            }))
        };
        let http = |at: (&str, u16), login: Option<(&str, &str)>| {
            Ok(Some(Proxy::Http {
                at: (at.0.into(), at.1),
                to: to(),
                login: login.map(|(u, p)| (u.into(), p.into())),
            }))
        };
        assert_eq!(
            proxy_of("nc -X 5 -x proxy.lan:1080 %h %p"),
            socks(5, ("proxy.lan", 1080), None)
        );
        assert_eq!(proxy_of("/usr/bin/nc -x127.0.0.1 %h %p"), socks(5, ("127.0.0.1", 1080), None));
        assert_eq!(
            proxy_of("nc -X 4 -x proxy.lan:9050 %h %p"),
            socks(4, ("proxy.lan", 9050), None)
        );
        assert_eq!(
            proxy_of("nc -X connect -x proxy.lan:8080 %h %p"),
            http(("proxy.lan", 8080), None)
        );
        assert!(proxy_of("nc -X connect -x proxy.lan:8080 -P me %h %p").is_err());
        assert_eq!(
            proxy_of("ncat --proxy proxy.lan:1080 --proxy-type socks5 --proxy-auth u:p %h %p"),
            socks(5, ("proxy.lan", 1080), Some(("u", "p")))
        );
        assert_eq!(proxy_of("ncat --proxy=proxy.lan:3128 %h %p"), http(("proxy.lan", 3128), None));
        assert_eq!(
            proxy_of("connect -S me@proxy.lan:1080 %h %p"),
            socks(5, ("proxy.lan", 1080), None)
        );
        assert_eq!(proxy_of("connect -4 -S proxy.lan %h %p"), socks(4, ("proxy.lan", 1080), None));
        assert_eq!(proxy_of("connect -H proxy.lan:3128 %h %p"), http(("proxy.lan", 3128), None));
        assert_eq!(proxy_of("corkscrew proxy.lan 3128 %h %p"), http(("proxy.lan", 3128), None));
        assert!(proxy_of("corkscrew proxy.lan 3128 %h %p ~/.corkscrew-auth").is_err());
        assert_eq!(
            proxy_of("socat - SOCKS4A:proxy.lan:%h:%p,socksport=9050"),
            socks(4, ("proxy.lan", 9050), None)
        );
        assert_eq!(
            proxy_of("socat STDIO PROXY:proxy.lan:%h:%p,proxyport=3128,proxyauth=u:p"),
            http(("proxy.lan", 3128), Some(("u", "p")))
        );
    }

    #[test]
    fn straight_cloudflare_and_refused() {
        assert_eq!(proxy_of("nc %h %p"), Ok(Some(Proxy::Direct { to: to() })));
        assert_eq!(proxy_of("socat - TCP:%h:%p"), Ok(Some(Proxy::Direct { to: to() })));
        let tunnel = proxy_of("cloudflared access ssh --hostname %h").unwrap();
        assert_eq!(
            tunnel,
            Some(Proxy::Cloudflare(Tunnel {
                hostname: "box.example.com".into(),
                service_token: None
            }))
        );
        let tunnel = proxy_of(
            "/opt/homebrew/bin/cloudflared --loglevel warn access ssh -hostname=https://ssh.x.io/ --service-token-id ID --service-token-secret=SECRET",
        );
        assert_eq!(
            tunnel,
            Ok(Some(Proxy::Cloudflare(Tunnel {
                hostname: "ssh.x.io".into(),
                service_token: Some(("ID".into(), "SECRET".into()))
            })))
        );
        assert!(proxy_of("cloudflared access tcp --hostname %h").is_err());
        assert!(
            proxy_of("aws ssm start-session --target %h")
                .unwrap_err()
                .contains("aws can't run here")
        );
        assert!(proxy_of("nc %h %p | tee log").unwrap_err().contains("shell syntax"));
        assert_eq!(proxy_of("'nc' \"%h\" %p"), Ok(Some(Proxy::Direct { to: to() })));
        assert_eq!(
            words("a 'b c' \"d\\\"e\" f\\ g"),
            Some(vec!["a".into(), "b c".into(), "d\"e".into(), "f g".into()])
        );
    }
}
