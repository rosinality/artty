//! Automatic shell integration: make zsh and bash report prompts, commands
//! and the working directory without the user editing their startup files.
//! (fish 4 does this natively.)

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

const ZSH_SCRIPT: &str = include_str!("../shell-integration/artty.zsh");
const ZSH_ENV: &str = include_str!("../shell-integration/zshenv");
const BASH_SCRIPT: &str = include_str!("../shell-integration/artty.bash");

/// The script for `shell` (`zsh` or `bash`), for sourcing by hand.
pub fn script(shell: &str) -> Option<&'static str> {
    match shell {
        "zsh" => Some(ZSH_SCRIPT),
        "bash" => Some(BASH_SCRIPT),
        _ => None,
    }
}

/// Write the scripts to a per-user directory and return it.
pub fn install() -> io::Result<PathBuf> {
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    let dir = std::env::temp_dir().join(format!("artty-shell-integration-{uid}"));
    std::fs::create_dir_all(dir.join("zsh"))?;
    std::fs::write(dir.join("artty.zsh"), ZSH_SCRIPT)?;
    std::fs::write(dir.join("artty.bash"), BASH_SCRIPT)?;
    std::fs::write(dir.join("zsh").join(".zshenv"), ZSH_ENV)?;
    Ok(dir)
}

/// How to start a shell so it loads the integration.
#[derive(Debug, Default, PartialEq)]
pub struct Injection {
    /// Replacement program and arguments; `None` keeps the original.
    pub command: Option<Vec<String>>,
    pub env: HashMap<String, String>,
}

/// Arguments that still describe a plain interactive shell.
fn plain_args(args: &[String]) -> bool {
    args.iter().all(|arg| matches!(arg.as_str(), "-l" | "--login" | "-i"))
}

/// The injection for running `command` (`None`: the login shell), if it's a
/// shell artty knows how to integrate with.
pub fn inject(dir: &Path, command: Option<&[String]>) -> Option<Injection> {
    let (program, args) = match command {
        Some([program, args @ ..]) => (program.clone(), args),
        Some([]) => return None,
        None => (crate::process::login_shell()?, &[][..]),
    };
    if !plain_args(args) {
        return None;
    }
    let name = Path::new(&program).file_name()?.to_str()?.trim_start_matches('-');
    let mut env = HashMap::from([("ARTTY_SHELL_INTEGRATION".to_owned(), dir.display().to_string())]);
    match name {
        "zsh" => {
            if let Ok(original) = std::env::var("ZDOTDIR") {
                env.insert("ARTTY_ZDOTDIR".to_owned(), original);
            }
            env.insert("ZDOTDIR".to_owned(), dir.join("zsh").display().to_string());
            Some(Injection { command: None, env })
        }
        "bash" => {
            // A login shell unless started as a plain command without -l.
            let login = command.is_none() || args.iter().any(|a| a == "-l" || a == "--login");
            env.insert("ENV".to_owned(), dir.join("artty.bash").display().to_string());
            env.insert("ARTTY_BASH_INJECT".to_owned(), "1".to_owned());
            if login {
                env.insert("ARTTY_BASH_LOGIN".to_owned(), "1".to_owned());
            }
            // Started under the name `sh`, bash reads $ENV before any startup
            // file (macOS's bash 3.2 ignores ENV with --posix).
            let command = vec![program.clone(), "-c".to_owned(), "exec -a sh \"$0\"".to_owned(), program];
            Some(Injection { command: Some(command), env })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn zsh_uses_zdotdir() {
        let injection = inject(Path::new("/x"), Some(&cmd(&["/bin/zsh", "-l"]))).unwrap();
        assert_eq!(injection.command, None);
        assert_eq!(injection.env["ZDOTDIR"], "/x/zsh");
        assert_eq!(injection.env["ARTTY_SHELL_INTEGRATION"], "/x");
    }

    #[test]
    fn bash_uses_posix_env() {
        let injection = inject(Path::new("/x"), Some(&cmd(&["/bin/bash", "--login"]))).unwrap();
        assert_eq!(injection.command, Some(cmd(&["/bin/bash", "-c", "exec -a sh \"$0\"", "/bin/bash"])));
        assert_eq!(injection.env["ENV"], "/x/artty.bash");
        assert!(injection.env.contains_key("ARTTY_BASH_LOGIN"));
        let plain = inject(Path::new("/x"), Some(&cmd(&["bash"]))).unwrap();
        assert!(!plain.env.contains_key("ARTTY_BASH_LOGIN"));
    }

    #[test]
    fn other_programs_are_left_alone() {
        assert_eq!(inject(Path::new("/x"), Some(&cmd(&["/opt/homebrew/bin/fish"]))), None);
        assert_eq!(inject(Path::new("/x"), Some(&cmd(&["zsh", "-c", "make"]))), None);
        assert_eq!(inject(Path::new("/x"), Some(&cmd(&["vim"]))), None);
    }
}
