//! artty: the macOS app, and the `artty` command (which is all there is on
//! other systems, where it runs the server other machines' apps reach).

#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod app_state;
#[cfg(target_os = "macos")]
mod askpass;
#[cfg(target_os = "macos")]
mod boxdraw;
#[cfg(target_os = "macos")]
mod chrome;
mod cli;
#[cfg(target_os = "macos")]
mod echo;
#[cfg(target_os = "macos")]
mod edits;
#[cfg(target_os = "macos")]
mod forward;
#[cfg(target_os = "macos")]
mod fuzzy;
#[cfg(target_os = "macos")]
mod highlight;
mod hooks;
#[cfg(target_os = "macos")]
mod ids;
#[cfg(target_os = "macos")]
mod keymap;
#[cfg(target_os = "macos")]
mod keys;
#[cfg(target_os = "macos")]
mod markdown;
#[cfg(target_os = "macos")]
mod mouse;
#[cfg(target_os = "macos")]
mod notify;
#[cfg(target_os = "macos")]
mod profile;
#[cfg(target_os = "macos")]
mod secure_input;
#[cfg(target_os = "macos")]
mod settings;
#[cfg(target_os = "macos")]
mod terminal;
#[cfg(target_os = "macos")]
mod transfer;
#[cfg(target_os = "macos")]
mod window_view;
#[cfg(target_os = "macos")]
use window_view::login_sheet;

#[cfg(target_os = "macos")]
pub use app::{bind_default_keys, menus};

#[cfg(target_os = "macos")]
fn main() {
    // Run by ssh, for a question while logging in: ask the app.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [prompt] = args.as_slice()
        && let Some((socket, request)) = artty_core::remote::askpass_request(prompt)
    {
        std::process::exit(cli::askpass(&socket, &request));
    }
    app::main()
}

/// Elsewhere, the command alone: `artty server`, `artty server --stdio`
/// (what a Mac's app runs over ssh), and the commands for panes.
#[cfg(not(target_os = "macos"))]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some(command) if cli::COMMANDS.contains(&command) => std::process::exit(cli::main(args)),
        Some("-h" | "--help" | "help") => {
            std::process::exit(cli::main(vec!["help".into(), "--help".into()]))
        },
        _ => {
            eprintln!(
                "artty: the app runs on macOS; here, artty runs the server a Mac's app connects \
                 to (over ssh), and the commands for its panes (artty help)."
            );
            std::process::exit(2);
        },
    }
}
