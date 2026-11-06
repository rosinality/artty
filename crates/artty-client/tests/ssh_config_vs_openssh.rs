//! The config read as OpenSSH reads it: each case resolved here and by
//! `ssh -G` (which resolves without connecting), compared. Skipped without
//! `ssh`.

use std::process::Command;

use artty_client::ssh_config::{Config, Local, RequestTty, StrictHostKeys};

const CASES: &[(&str, &[&str])] = &[
    (
        "Host a\n  HostName real-%h.example\n  User u1\n  Port 2200\n  IdentityFile ~/.ssh/k1\n\
         Host *\n  User other\n  ServerAliveInterval 30\n  IdentityFile ~/.ssh/id_x\n",
        &["a", "b", "root@a", "DevBox"],
    ),
    (
        "Host *.corp !bastion.corp\n  User corp\nHost web?\n  Port 8022\n",
        &["db.corp", "bastion.corp", "web1", "web12", "DB.CORP"],
    ),
    (
        "Host short\n  HostName long.example.org\nMatch host *.example.org\n  User ex\n\
         Match originalhost short user ex\n  Port 7\nMatch all\n  Compression yes\n",
        &["short", "other", "long.example.org"],
    ),
    (
        "Host t\n  ProxyJump j1,me@j2:2222\n  LocalForward 8080 localhost:80\n  LocalForward 127.0.0.1:5432 db:5432\n\
         DynamicForward 1080\n  SetEnv A=1 B=2\n  SetEnv C=3\n  RequestTTY force\n  StrictHostKeyChecking accept-new\n\
         ForwardAgent yes\nHost j1\n  ProxyJump none\n  StrictHostKeyChecking no\n  RequestTTY no\n",
        &["t", "j1"],
    ),
    (
        "User=all\nHost x y\n  Port 3\n  Compression=yes\nHost *\n  Port 4\n  ServerAliveCountMax 9\n",
        &["x", "y", "z"],
    ),
];

/// `ssh -G`'s answer: `keyword value` lines.
fn openssh(config: &std::path::Path, destination: &str) -> Option<Vec<(String, String)>> {
    let out = Command::new("ssh").arg("-G").arg("-F").arg(config).arg(destination).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    Some(
        text.lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
    )
}

#[test]
fn as_openssh_resolves() {
    let dir = std::env::temp_dir().join(format!("artty-ssh-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let local = Local {
        home: std::env::var("HOME").unwrap_or_default(),
        user: std::env::var("USER").unwrap_or_default(),
        host: String::new(),
    };
    for (n, (text, destinations)) in CASES.iter().enumerate() {
        let path = dir.join(format!("config{n}"));
        std::fs::write(&path, text).unwrap();
        let config = Config::parse(text, &|_| Vec::new());
        for destination in *destinations {
            let Some(theirs) = openssh(&path, destination) else {
                eprintln!("no ssh -G here: skipped");
                return;
            };
            let all = |key: &str| -> Vec<String> {
                theirs.iter().filter(|(k, _)| k == key).map(|(_, v)| v.clone()).collect()
            };
            let one = |key: &str| all(key).into_iter().next().unwrap_or_default();
            let ours = config.resolve(destination, &local);
            let case = format!("case {n}, {destination}");
            assert_eq!(ours.host_name, one("hostname"), "{case}: hostname");
            assert_eq!(
                ours.user.clone().unwrap_or(local.user.clone()),
                one("user"),
                "{case}: user"
            );
            assert_eq!(ours.port.to_string(), one("port"), "{case}: port");
            // With none configured, ssh lists its defaults (which this leaves to the caller).
            if !ours.identity_files.is_empty() {
                assert_eq!(ours.identity_files, all("identityfile"), "{case}: identityfile");
            }
            let jumps: Vec<String> = ours
                .proxy_jump
                .iter()
                .map(|h| {
                    let user = h.user.as_ref().map(|u| format!("{u}@")).unwrap_or_default();
                    let port = h.port.map(|p| format!(":{p}")).unwrap_or_default();
                    format!("{user}{}{port}", h.host)
                })
                .collect();
            let theirs_jump = one("proxyjump");
            assert_eq!(
                jumps.join(","),
                if theirs_jump == "none" { String::new() } else { theirs_jump },
                "{case}: proxyjump"
            );
            let forwards: Vec<String> = ours
                .local_forwards
                .iter()
                .map(|f| {
                    let (host, port) = f.to.clone().unwrap();
                    match &f.bind {
                        Some(bind) => format!("[{bind}]:{} [{host}]:{port}", f.port),
                        None => format!("{} [{host}]:{port}", f.port),
                    }
                })
                .collect();
            assert_eq!(forwards, all("localforward"), "{case}: localforward");
            let dynamic: Vec<String> =
                ours.dynamic_forwards.iter().map(|f| f.port.to_string()).collect();
            assert_eq!(dynamic, all("dynamicforward"), "{case}: dynamicforward");
            let env: Vec<String> = ours.set_env.iter().map(|(k, v)| format!("{k}={v}")).collect();
            assert_eq!(env, all("setenv"), "{case}: setenv");
            assert_eq!(
                ours.server_alive_interval.to_string(),
                one("serveraliveinterval"),
                "{case}"
            );
            assert_eq!(
                ours.server_alive_count_max.to_string(),
                one("serveralivecountmax"),
                "{case}"
            );
            let yes = |b: bool| if b { "yes" } else { "no" };
            assert_eq!(yes(ours.compression), one("compression"), "{case}: compression");
            assert_eq!(yes(ours.forward_agent), one("forwardagent"), "{case}: forwardagent");
            let strict = match ours.strict_host_key_checking {
                StrictHostKeys::Ask => "ask",
                StrictHostKeys::Yes => "true",
                StrictHostKeys::No => "false",
                StrictHostKeys::AcceptNew => "accept-new",
            };
            assert_eq!(strict, one("stricthostkeychecking"), "{case}: stricthostkeychecking");
            let tty = match ours.request_tty {
                RequestTty::Auto => "auto",
                RequestTty::Yes => "true",
                RequestTty::No => "false",
                RequestTty::Force => "force",
            };
            assert_eq!(tty, one("requesttty"), "{case}: requesttty");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
