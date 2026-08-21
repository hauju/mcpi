//! Recovering the user's real `PATH`, and resolving commands against it.
//!
//! A macOS app launched from Finder inherits `/usr/bin:/bin:/usr/sbin:/sbin`
//! and nothing else — not the `PATH` from `.zshrc`, not Homebrew, not a Node
//! version manager. Every MCP server configured as `npx …` or `uvx …` then
//! fails to spawn with a bare "No such file or directory", which is the single
//! most common MCP configuration failure there is.
//!
//! So we ask the user's login shell what `PATH` really is, once per process,
//! and resolve commands against that ourselves.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::OnceCell;

static LOGIN_PATH: OnceCell<Option<String>> = OnceCell::const_new();

/// The `PATH` an interactive login shell would see, or `None` when it could
/// not be determined (in which case the inherited `PATH` is all we have).
pub async fn login_shell_path() -> Option<String> {
    LOGIN_PATH.get_or_init(probe).await.clone()
}

async fn probe() -> Option<String> {
    let shell = std::env::var("SHELL").ok()?;

    // `printenv PATH` rather than `echo $PATH`: fish stores PATH as a list and
    // `echo` would join it with spaces instead of colons. `printenv` is an
    // external binary reading the actual exported variable, so it returns the
    // same colon-separated form under bash, zsh, and fish alike.
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        Command::new(&shell)
            .args(["-ilc", "printenv PATH"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim().to_string();
    (!path.is_empty()).then_some(path)
}

/// Find `command` on `path`, returning an absolute path.
///
/// Doing the lookup ourselves rather than leaving it to `execvp` means the
/// child is spawned by absolute path, so it cannot matter whether the platform
/// resolves against the parent's `PATH` or the child's — and a missing binary
/// produces an error naming the directories actually searched.
pub fn resolve_command(command: &str, path: Option<&str>) -> Option<PathBuf> {
    if command.contains('/') {
        let direct = PathBuf::from(command);
        return is_executable(&direct).then_some(direct);
    }

    let path = path
        .map(str::to_string)
        .or_else(|| std::env::var("PATH").ok())?;
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(command))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_binary_that_is_definitely_on_path() {
        assert!(resolve_command("sh", Some("/usr/bin:/bin")).is_some());
    }

    #[test]
    fn returns_none_for_a_missing_binary() {
        assert!(resolve_command("definitely-not-a-real-binary", Some("/usr/bin:/bin")).is_none());
    }

    #[test]
    fn absolute_paths_bypass_the_search() {
        assert_eq!(
            resolve_command("/bin/sh", Some("/nowhere")),
            Some(PathBuf::from("/bin/sh"))
        );
        assert!(resolve_command("/bin/does-not-exist", None).is_none());
    }

    #[test]
    fn empty_path_segments_are_skipped() {
        // A trailing colon means "the current directory" to some shells; we
        // deliberately do not honour that.
        assert!(resolve_command("sh", Some("/bin:")).is_some());
    }
}
