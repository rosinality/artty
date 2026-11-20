//! Agents started in git worktrees of their own (`artty new --worktree
//! NAME`): a checkout of the repository beside it, in
//! `<repo>.worktrees/<NAME>`, on branch NAME (made from the repository's
//! HEAD, or the branch as it is if there's one).
//!
//! The tab makes it, then starts the agent in it (a short script before the
//! agent's command): a long checkout doesn't hold the server up, and what
//! git says shows in the tab. When the tab closes, the worktree is removed
//! if git will without forcing it (nothing uncommitted, nothing untracked);
//! its branch stays, with whatever was committed on it.

use std::path::{Path, PathBuf};

/// Makes the worktree (or takes the one there), then runs the command in it.
/// `$1`: the repository, `$2`: the worktree, `$3`: the branch; the rest: the
/// command. Failing, it says so and leaves a shell in the repository.
const SCRIPT: &str = r#"top=$1 dir=$2 branch=$3; shift 3
if [ ! -d "$dir" ]; then
  if git -C "$top" show-ref --verify --quiet "refs/heads/$branch"; then
    git -C "$top" worktree add "$dir" "$branch"
  else
    git -C "$top" worktree add -b "$branch" "$dir"
  fi || {
    echo "artty: couldn't make the worktree $dir (see above)"
    exec "${SHELL:-/bin/sh}"
  }
fi
cd "$dir" && exec "$@""#;

/// A worktree an agent was started in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    /// The repository's top folder.
    pub repo: PathBuf,
    pub dir: PathBuf,
}

/// A name git takes as a branch, and that's safe as a folder's: letters,
/// digits, `-`, `_`, `.`, `/` between parts.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        && !name.starts_with(['-', '.', '/'])
        && !name.ends_with(['/', '.'])
        && !name.contains("..")
        && !name.contains("//")
        && !name.ends_with(".lock");
    match ok {
        true => Ok(()),
        false => Err(format!(
            "--worktree {name:?}: a branch name (letters, digits, - _ . /), like fix-login"
        )),
    }
}

/// The repository `folder` is in: its top folder.
pub fn repo_of(folder: &Path) -> Result<PathBuf, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|err| format!("--worktree: running git: {err}"))?;
    if !out.status.success() {
        return Err(format!("--worktree: {} isn't in a git repository", folder.display()));
    }
    let top = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok(PathBuf::from(top))
}

/// Where worktree `name` of `repo` goes: `<repo>.worktrees/<name>`, beside it.
pub fn dir_for(repo: &Path, name: &str) -> PathBuf {
    let mut beside = repo.as_os_str().to_owned();
    beside.push(".worktrees");
    PathBuf::from(beside).join(name)
}

/// The command a tab runs to make the worktree and start `command` in it.
pub fn command(worktree: &Worktree, branch: &str, command: Vec<String>) -> Vec<String> {
    let mut argv =
        vec!["sh".to_owned(), "-c".to_owned(), SCRIPT.to_owned(), "artty-worktree".to_owned()];
    argv.push(worktree.repo.display().to_string());
    argv.push(worktree.dir.display().to_string());
    argv.push(branch.to_owned());
    argv.extend(command);
    argv
}

/// Remove `worktree` if git will without forcing (in the background): a
/// worktree with changes stays.
pub fn remove(worktree: Worktree) {
    let _ = std::thread::Builder::new().name("artty-worktree".into()).spawn(move || {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree.repo)
            .args(["worktree", "remove"])
            .arg(&worktree.dir)
            .output();
        match out {
            Ok(out) if out.status.success() => {
                log::info!("removed the worktree {}", worktree.dir.display())
            },
            Ok(out) => log::info!(
                "kept the worktree {}: {}",
                worktree.dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(err) => log::warn!("removing the worktree {}: {err}", worktree.dir.display()),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for good in ["fix-login", "docs/plan_v2", "a.b", "x1"] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in ["", "-x", ".x", "a..b", "a b", "a/", "x.lock", "a//b", "$(rm)", "a;b"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn beside_the_repository() {
        assert_eq!(dir_for(Path::new("/w/notes"), "plan"), Path::new("/w/notes.worktrees/plan"));
    }
}
