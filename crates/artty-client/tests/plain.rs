//! The stand-in for a server on a machine without artty (`plain`), with
//! shells that echo what's typed, through the client as any server's.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use artty_client::plain::{self, Shells};
use artty_client::{Client, Event, Key};
use artty_core::TerminalSize;
use artty_core::vt::tty::RemoteEnds;

/// Shells that say `$ ` and echo each line back; `exit` ends one.
#[derive(Clone, Default)]
struct Echo {
    closed: Arc<AtomicBool>,
    sizes: Arc<Mutex<Vec<TerminalSize>>>,
}

impl Shells for Echo {
    fn open(
        &self,
        size: TerminalSize,
        ends: RemoteEnds,
        mut sizes: tokio::sync::mpsc::UnboundedReceiver<TerminalSize>,
    ) {
        self.sizes.lock().unwrap().push(size);
        let seen = self.sizes.clone();
        std::thread::spawn(move || {
            while let Some(size) = sizes.blocking_recv() {
                seen.lock().unwrap().push(size);
            }
        });
        std::thread::spawn(move || {
            let RemoteEnds { mut stream, mut ended } = ends;
            let _ = stream.write_all(b"$ ");
            let mut line = Vec::new();
            let mut byte = [0];
            while let Ok(1) = stream.read(&mut byte) {
                if byte[0] == b'\r' {
                    let _ = stream.write_all(b"\r\n");
                    if line == b"exit" {
                        let _ = stream.write_all(b"bye\r\n");
                        let _ = ended.write_all(&[1]);
                        return;
                    }
                    line.clear();
                    let _ = stream.write_all(b"$ ");
                } else {
                    line.push(byte[0]);
                    let _ = stream.write_all(&byte);
                }
            }
        });
    }

    fn ping(&self, answered: Box<dyn FnOnce() + Send>) {
        answered();
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

fn until<T>(
    events: &std::sync::mpsc::Receiver<Event>,
    mut want: impl FnMut(&Event) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = events.recv_timeout(left) else { break };
        if let Some(found) = want(&event) {
            return found;
        }
    }
    panic!("not in time");
}

/// A pane's screen text, lines joined.
fn text(client: &Client, pane: u64) -> String {
    let Some(screen) = client.screen(pane) else { return String::new() };
    let lines = screen.frame.lines.iter().map(|line| {
        let mut text = String::new();
        for run in &line.runs {
            while text.chars().count() < run.column {
                text.push(' ');
            }
            text.push_str(&run.text);
        }
        text.trim_end().to_owned()
    });
    lines.collect::<Vec<_>>().join("\n")
}

fn wait_for(client: &Client, events: &std::sync::mpsc::Receiver<Event>, pane: u64, want: &str) {
    if text(client, pane).contains(want) {
        return;
    }
    until(events, |e| {
        matches!(e, Event::Screen { pane: p } if *p == pane && text(client, pane).contains(want))
            .then_some(())
    });
}

#[test]
fn tabs_of_shells() {
    let shells = Echo::default();
    let link = plain::serve(Box::new(shells.clone()), "me@box".into()).unwrap();
    let (client, events) = Client::start(link, "test").unwrap();
    assert_eq!(client.server_version, plain::SERVER_VERSION);

    // Its one window, there before it's shown.
    client.list_windows().unwrap();
    let windows = until(&events, |e| match e {
        Event::Windows(windows) => Some(windows.clone()),
        _ => None,
    });
    assert_eq!(windows.iter().map(|w| w.window).collect::<Vec<_>>(), [plain::WINDOW]);

    client.attach(Some(plain::WINDOW), true).unwrap();
    until(&events, |e| matches!(e, Event::Attached { .. }).then_some(()));
    let pane = until(&events, |e| match e {
        Event::Layout { layout, .. } => {
            assert_eq!(layout.workspaces[0].title, "me@box");
            layout.active_tab().map(|t| t.focused)
        },
        _ => None,
    });
    let size = TerminalSize { columns: 50, lines: 10, cell_width: 9, cell_height: 18 };
    client.resize(pane, size).unwrap();
    wait_for(&client, &events, pane, "$");

    client.input(pane, b"hello there".to_vec()).unwrap();
    client
        .key(pane, &Key { key: "enter", shift: false, alt: false, control: false, platform: false })
        .unwrap();
    wait_for(&client, &events, pane, "hello there\n$");
    assert!(shells.sizes.lock().unwrap().contains(&size), "the shell is told its size");

    // Finding, selecting, links: as on a server.
    let found = client.find(pane, "there", true, true).unwrap();
    assert!(found.found && found.total == 1);
    assert_eq!(client.selection_text(pane).unwrap().as_deref(), Some("there"));
    client.end_find(pane).unwrap();
    client.input(pane, b"see https://example.com/x\r".to_vec()).unwrap();
    wait_for(&client, &events, pane, "example.com/x\n$");
    let link = client.link_at(pane, 1, 10).unwrap();
    assert_eq!(link.map(|l| l.0).as_deref(), Some("https://example.com/x"));
    assert_eq!(client.search_output("hello").unwrap().len(), 1);
    assert!(client.list_dir(pane, None, false).is_err(), "no files without artty there");

    // A second tab, shown; then closed, the first is again.
    client.new_tab(plain::WINDOW, None).unwrap();
    let second = until(&events, |e| match e {
        Event::Layout { layout, .. } if layout.workspaces[0].tabs.len() == 2 => {
            assert_eq!(layout.workspaces[0].active, 1);
            layout.active_tab().map(|t| t.focused)
        },
        _ => None,
    });
    assert_ne!(second, pane);
    wait_for(&client, &events, second, "$");
    client.close_tab(1, 1).unwrap();
    until(&events, |e| match e {
        Event::Layout { layout, .. } => (layout.workspaces[0].tabs.len() == 1
            && layout.active_tab().unwrap().focused == pane)
            .then_some(()),
        _ => None,
    });

    // A shell ending at once is kept, for what it said.
    client.new_tab(plain::WINDOW, None).unwrap();
    let quick = until(&events, |e| match e {
        Event::Layout { layout, .. } if layout.workspaces[0].tabs.len() == 2 => {
            layout.active_tab().map(|t| t.focused)
        },
        _ => None,
    });
    client.input(quick, b"exit\r".to_vec()).unwrap();
    wait_for(&client, &events, quick, "bye");
    std::thread::sleep(Duration::from_millis(300));
    client.list_windows().unwrap();
    let tabs = until(&events, |e| match e {
        Event::Windows(windows) => Some(windows[0].tabs),
        _ => None,
    });
    assert_eq!(tabs, 2);

    // The last tab closed closes the window.
    client.close_tab(1, 1).unwrap();
    client.close_tab(1, 0).unwrap();
    until(&events, |e| matches!(e, Event::WindowClosed { .. }).then_some(()));

    // The connection gone: so is the stand-in.
    shells.closed.store(true, Ordering::Relaxed);
    until(&events, |e| matches!(e, Event::Closed(_)).then_some(()));
}

/// Against a real machine: `ARTTY_PLAIN_TARGET=[user@]host[:port]` (as
/// `ARTTY_PLAIN_CONFIG`, an ssh config, says, if given), logging
/// in with the key in `ARTTY_PLAIN_KEY` (else the agent and `~/.ssh`'s), its
/// key known in `ARTTY_PLAIN_KNOWN_HOSTS`.
#[test]
#[ignore]
fn over_ssh() {
    use artty_client::ssh::{self, Credentials, HostKey, Key, KnownHosts, Login, Target};
    use artty_client::ssh_config::{Config, Local};
    struct Known;
    impl Login for Known {
        fn host_key(&self, _: &Target, _: &str, _: HostKey) -> bool {
            false
        }
        fn said(&self, text: &str) {
            eprintln!("said: {text}");
        }
        fn answer(&self, _: &str, _: &[(String, bool)]) -> Option<Vec<String>> {
            None
        }
    }
    let destination = std::env::var("ARTTY_PLAIN_TARGET").unwrap();
    // A config, if given (`ARTTY_PLAIN_CONFIG`): its jump hosts and the rest.
    let config = std::env::var("ARTTY_PLAIN_CONFIG")
        .map(|path| Config::read(path.as_ref(), "/nowhere").unwrap())
        .unwrap_or_default();
    let plan = ssh::plan(&config, &Local::here(), &destination).unwrap();
    let known = KnownHosts(std::env::var("ARTTY_PLAIN_KNOWN_HOSTS").unwrap().into());
    let credentials = match std::env::var("ARTTY_PLAIN_KEY") {
        Ok(path) => Credentials {
            keys: vec![Key {
                name: "user".into(),
                text: ssh::read_key(path.as_ref()).unwrap(),
                passphrase: None,
            }],
            ..Default::default()
        },
        Err(_) => Credentials::usual(),
    };
    let (link, forwards) = ssh::shells(&plan, &credentials, known, Arc::new(Known)).unwrap();
    let (client, events) = Client::start(link, "test").unwrap();
    client.attach(Some(plain::WINDOW), true).unwrap();
    let pane = until(&events, |e| match e {
        Event::Layout { layout, .. } => layout.active_tab().map(|t| t.focused),
        _ => None,
    });
    let size = TerminalSize { columns: 70, lines: 12, cell_width: 9, cell_height: 18 };
    client.resize(pane, size).unwrap();
    client
        .input(pane, b"echo $TERM; stty size; echo v=$ARTTY_TEST_VAR; echo d''one\r".to_vec())
        .unwrap();
    wait_for(&client, &events, pane, "done");
    let shown = text(&client, pane);
    eprintln!("{shown}");
    assert!(shown.contains("xterm-256color") && shown.contains("12 70"));
    // The config's SetEnv, if the server takes it.
    if let Ok(value) = std::env::var("ARTTY_PLAIN_EXPECT_VAR") {
        assert!(shown.contains(&format!("v={value}")), "SetEnv");
    }
    // Pings are answered by the machine.
    std::thread::sleep(Duration::from_millis(500));
    client.ping().unwrap();
    let pinged = std::time::Instant::now();
    let deadline = pinged + Duration::from_secs(3);
    while client.silent_for() >= pinged.elapsed() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(client.silent_for() < pinged.elapsed(), "no answer");
    eprintln!("answered in {:?}", pinged.elapsed() - client.silent_for());
    if std::env::var_os("ARTTY_PLAIN_FORWARDS").is_some() {
        forwarding(&forwards);
    }
    client.input(pane, b"exit\r".to_vec()).unwrap();
}

/// Each kind of forward to an echo server here: `-L` and `-D` listen here,
/// `-R` there (the server is this machine, so its port is reached here too).
fn forwarding(forwards: &artty_client::ssh::Forwards) {
    use artty_client::ssh::ForwardKind;
    use artty_client::ssh_config::Forward;
    use std::net::{TcpListener, TcpStream};
    let echo = TcpListener::bind("127.0.0.1:0").unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in echo.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut reader = stream.try_clone().unwrap();
                let _ = std::io::copy(&mut reader, &mut stream);
            });
        }
    });
    let to = Some(("127.0.0.1".to_owned(), echo_port));
    let local =
        forwards.add(ForwardKind::Local, Forward { bind: None, port: 0, to: to.clone() }, false);
    let dynamic =
        forwards.add(ForwardKind::Dynamic, Forward { bind: None, port: 0, to: None }, false);
    let remote = forwards.add(ForwardKind::Remote, Forward { bind: None, port: 0, to }, false);
    let listening = |id: u64| {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = forwards.list().into_iter().find(|s| s.id == id).unwrap();
            assert_eq!(state.error, None, "{}", state.describe());
            if let Some(port) = state.listening {
                return port;
            }
            assert!(std::time::Instant::now() < deadline, "{} never listened", state.describe());
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let echoes = |mut stream: TcpStream, what: &str| {
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.write_all(what.as_bytes()).unwrap();
        let mut back = vec![0; what.len()];
        stream.read_exact(&mut back).unwrap();
        assert_eq!(String::from_utf8(back).unwrap(), what);
    };
    echoes(TcpStream::connect(("127.0.0.1", listening(local))).unwrap(), "through -L");
    // SOCKS 5, by name: CONNECT localhost:echo.
    let mut socks = TcpStream::connect(("127.0.0.1", listening(dynamic))).unwrap();
    socks.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    socks.write_all(&[5, 1, 0]).unwrap();
    let mut answer = [0; 2];
    socks.read_exact(&mut answer).unwrap();
    assert_eq!(answer, [5, 0]);
    let mut request = vec![5, 1, 0, 3, 9];
    request.extend_from_slice(b"localhost");
    request.extend_from_slice(&echo_port.to_be_bytes());
    socks.write_all(&request).unwrap();
    let mut reply = [0; 10];
    socks.read_exact(&mut reply).unwrap();
    assert_eq!(reply[1], 0, "SOCKS connected");
    echoes(socks, "through -D");
    echoes(TcpStream::connect(("127.0.0.1", listening(remote))).unwrap(), "through -R");
    let total: usize = forwards.list().iter().map(|s| s.total).sum();
    assert_eq!(total, 2, "-L and -D count their connections");
    eprintln!("{:?}", forwards.list().iter().map(|s| s.describe()).collect::<Vec<_>>());
    forwards.stop(local);
    assert!(TcpStream::connect(("127.0.0.1", listening(dynamic))).is_ok());
    assert_eq!(forwards.list().len(), 2);
}
