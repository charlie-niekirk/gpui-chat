//! Finding the `streamlink` executable.
//!
//! A GUI app started from Finder does not inherit the shell's `PATH`, so a plain `PATH` search
//! misses Homebrew. Order: the settings override (used as given, never silently replaced), then
//! `command -v streamlink` in the user's login shell, then the usual install directories.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Searched last, in order. `~/.local/bin` is relative to the home directory.
pub const FALLBACK_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "~/.local/bin"];

const BINARY: &str = "streamlink";
/// A login shell with a heavy profile can be slow; never wait longer than this for it.
const SHELL_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// The path from settings is not an executable file.
    #[error("the configured streamlink path {0} is not an executable file")]
    OverrideInvalid(PathBuf),
    /// Nothing found anywhere.
    #[error("streamlink was not found")]
    NotFound,
}

/// What the resolver asks of the machine, so tests can answer instead.
pub trait Environment {
    /// The path `command -v <name>` prints in the user's login shell, if it found one.
    fn login_shell_lookup(&self, name: &str) -> Option<PathBuf>;
    /// Whether `path` is a file that can be executed.
    fn is_executable(&self, path: &Path) -> bool;
    fn home_dir(&self) -> Option<PathBuf>;
}

/// Finds `streamlink`. `configured` is the settings override; when it is set it is the only
/// candidate (a typo is reported instead of quietly using another binary).
pub fn resolve(configured: Option<&Path>, env: &dyn Environment) -> Result<PathBuf, ResolveError> {
    if let Some(configured) = configured.filter(|path| !path.as_os_str().is_empty()) {
        let path = expand_home(configured, env);
        return if env.is_executable(&path) {
            Ok(path)
        } else {
            Err(ResolveError::OverrideInvalid(configured.to_path_buf()))
        };
    }
    if let Some(found) = env.login_shell_lookup(BINARY) {
        if env.is_executable(&found) {
            return Ok(found);
        }
        tracing::debug!(path = %found.display(), "login shell found streamlink but it is not executable");
    }
    FALLBACK_DIRS
        .iter()
        .map(|dir| expand_home(&Path::new(dir).join(BINARY), env))
        .find(|candidate| env.is_executable(candidate))
        .ok_or(ResolveError::NotFound)
}

/// `~` and `~/x` → the home directory (when known).
fn expand_home(path: &Path, env: &dyn Environment) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => match env.home_dir() {
            Some(home) => home.join(rest),
            None => path.to_path_buf(),
        },
        Err(_) => path.to_path_buf(),
    }
}

/// The real machine.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemEnvironment;

impl Environment for SystemEnvironment {
    fn login_shell_lookup(&self, name: &str) -> Option<PathBuf> {
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|shell| shell.is_absolute())
            .unwrap_or_else(|| {
                PathBuf::from(if cfg!(target_os = "macos") {
                    "/bin/zsh"
                } else {
                    "/bin/sh"
                })
            });
        let mut child = Command::new(shell)
            .args(["-l", "-c", &format!("command -v {name}")])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdout = child.stdout.take()?;
        // Read on a helper thread so a shell that hangs cannot hang the caller.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut out = String::new();
            let _ = stdout.read_to_string(&mut out);
            let _ = tx.send(out);
        });
        match rx.recv_timeout(SHELL_TIMEOUT) {
            Ok(out) => {
                let _ = child.wait();
                parse_which_output(&out)
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::debug!("login shell lookup of streamlink timed out");
                None
            }
        }
    }

    fn is_executable(&self, path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    fn home_dir(&self) -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
    }
}

/// Picks the path out of a login shell's output. Profiles may print banners, so the last line
/// that is an absolute path wins (`command -v` of an alias or function prints something else,
/// which is correctly ignored).
pub fn parse_which_output(output: &str) -> Option<PathBuf> {
    output
        .lines()
        .map(str::trim)
        .rfind(|line| line.starts_with('/'))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[derive(Default)]
    struct Fake {
        shell: Option<PathBuf>,
        executable: HashSet<PathBuf>,
        home: Option<PathBuf>,
    }

    impl Environment for Fake {
        fn login_shell_lookup(&self, _: &str) -> Option<PathBuf> {
            self.shell.clone()
        }
        fn is_executable(&self, path: &Path) -> bool {
            self.executable.contains(path)
        }
        fn home_dir(&self) -> Option<PathBuf> {
            self.home.clone()
        }
    }

    fn env(shell: Option<&str>, executables: &[&str]) -> Fake {
        Fake {
            shell: shell.map(PathBuf::from),
            executable: executables.iter().map(PathBuf::from).collect(),
            home: Some(PathBuf::from("/Users/me")),
        }
    }

    #[test]
    fn the_settings_override_wins_over_everything() {
        let env = env(
            Some("/opt/homebrew/bin/streamlink"),
            &["/custom/streamlink", "/opt/homebrew/bin/streamlink"],
        );
        assert_eq!(
            resolve(Some(Path::new("/custom/streamlink")), &env),
            Ok(PathBuf::from("/custom/streamlink"))
        );
    }

    #[test]
    fn a_bad_override_is_an_error_not_a_fallback() {
        let env = env(
            Some("/opt/homebrew/bin/streamlink"),
            &["/opt/homebrew/bin/streamlink"],
        );
        assert_eq!(
            resolve(Some(Path::new("/typo/streamlink")), &env),
            Err(ResolveError::OverrideInvalid(PathBuf::from(
                "/typo/streamlink"
            )))
        );
    }

    #[test]
    fn an_empty_override_means_unset() {
        let env = env(Some("/usr/bin/streamlink"), &["/usr/bin/streamlink"]);
        assert_eq!(
            resolve(Some(Path::new("")), &env),
            Ok(PathBuf::from("/usr/bin/streamlink"))
        );
    }

    #[test]
    fn the_override_may_use_a_home_prefix() {
        let env = env(None, &["/Users/me/bin/streamlink"]);
        assert_eq!(
            resolve(Some(Path::new("~/bin/streamlink")), &env),
            Ok(PathBuf::from("/Users/me/bin/streamlink"))
        );
    }

    #[test]
    fn the_login_shell_comes_before_the_fixed_directories() {
        let env = env(
            Some("/Users/me/pipx/streamlink"),
            &["/Users/me/pipx/streamlink", "/opt/homebrew/bin/streamlink"],
        );
        assert_eq!(
            resolve(None, &env),
            Ok(PathBuf::from("/Users/me/pipx/streamlink"))
        );
    }

    #[test]
    fn a_shell_answer_that_is_not_executable_is_skipped() {
        let env = env(Some("/gone/streamlink"), &["/usr/local/bin/streamlink"]);
        assert_eq!(
            resolve(None, &env),
            Ok(PathBuf::from("/usr/local/bin/streamlink"))
        );
    }

    #[test]
    fn fixed_directories_are_tried_in_order() {
        let both = env(
            None,
            &["/usr/local/bin/streamlink", "/opt/homebrew/bin/streamlink"],
        );
        assert_eq!(
            resolve(None, &both),
            Ok(PathBuf::from("/opt/homebrew/bin/streamlink"))
        );
        let local = env(None, &["/Users/me/.local/bin/streamlink"]);
        assert_eq!(
            resolve(None, &local),
            Ok(PathBuf::from("/Users/me/.local/bin/streamlink"))
        );
    }

    #[test]
    fn nothing_found_is_not_found() {
        assert_eq!(resolve(None, &env(None, &[])), Err(ResolveError::NotFound));
        // No home directory: `~/.local/bin` cannot be expanded, and that is fine.
        let no_home = Fake::default();
        assert_eq!(resolve(None, &no_home), Err(ResolveError::NotFound));
    }

    #[test]
    fn which_output_ignores_banners_and_non_paths() {
        assert_eq!(
            parse_which_output("Welcome back!\n/opt/homebrew/bin/streamlink\n"),
            Some(PathBuf::from("/opt/homebrew/bin/streamlink"))
        );
        assert_eq!(parse_which_output("alias streamlink='x'\n"), None);
        assert_eq!(parse_which_output(""), None);
    }

    #[cfg(unix)]
    #[test]
    fn system_environment_checks_the_execute_bit() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("streamlink");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        let sys = SystemEnvironment;
        assert!(!sys.is_executable(&file));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(sys.is_executable(&file));
        assert!(!sys.is_executable(dir.path()));
        assert!(!sys.is_executable(&dir.path().join("missing")));
    }

    #[cfg(unix)]
    #[test]
    fn system_environment_asks_a_real_login_shell() {
        // `sh` exists wherever this runs; whatever the shell prints must come back as a path.
        let found = SystemEnvironment.login_shell_lookup("sh");
        assert!(found.is_some_and(|path| path.is_absolute()));
        assert_eq!(
            SystemEnvironment.login_shell_lookup("no-such-binary-gpui-chat"),
            None
        );
    }
}
