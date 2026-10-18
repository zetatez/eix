//! Optional bubblewrap isolation for `run_command`.
//!
//! The sandbox keeps the host read-only and exposes only the session workspace
//! as writable, so a command cannot touch files outside it even when it ignores
//! the path checks used by the file tools. Detection and argument building live
//! here so the daemon and `eix doctor` agree on what is available.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Host paths mounted read-only inside the sandbox. Missing paths are skipped
/// (`--ro-bind-try`), which keeps this portable across distro layouts.
const READ_ONLY_PATHS: &[&str] = &[
    "/bin", "/etc", "/lib", "/lib32", "/lib64", "/opt", "/run", "/sbin", "/usr", "/var",
];

/// True when an executable `bwrap` is on `PATH`.
pub fn bwrap_available() -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| is_executable(&dir.join("bwrap")))
}

/// The user's home directory, when it exists.
pub fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_dir())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Arguments for `bwrap` that expose the system read-only, `workspace` (and its
/// contents) read-write, and start in `chdir`. The workspace keeps its host path
/// so `cwd` values and tool output stay meaningful.
///
/// `home` is mounted read-only when given, so toolchain caches and `git` config
/// stay readable without letting a command write there; when it is `None`,
/// `$HOME` is redirected to the sandbox's scratch `/tmp`.
pub fn bwrap_args(
    workspace: &Path,
    chdir: &Path,
    network: bool,
    home: Option<&Path>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--die-with-parent",
        "--new-session",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
    ]
    .iter()
    .map(OsString::from)
    .collect();

    for path in READ_ONLY_PATHS {
        args.push("--ro-bind-try".into());
        args.push((*path).into());
        args.push((*path).into());
    }
    if let Some(home) = home {
        args.push("--ro-bind-try".into());
        args.push(home.as_os_str().to_owned());
        args.push(home.as_os_str().to_owned());
    }
    if !network {
        args.push("--unshare-net".into());
    }

    // The workspace bind comes last so it stays writable even when it sits
    // inside the read-only home directory.
    args.push("--bind".into());
    args.push(workspace.as_os_str().to_owned());
    args.push(workspace.as_os_str().to_owned());
    args.push("--chdir".into());
    args.push(chdir.as_os_str().to_owned());
    args.push("--setenv".into());
    args.push("TMPDIR".into());
    args.push("/tmp".into());
    if home.is_none() {
        args.push("--setenv".into());
        args.push("HOME".into());
        args.push("/tmp".into());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn binds_the_workspace_writable_and_the_system_read_only() {
        let args = text(&bwrap_args(
            Path::new("/home/me/project"),
            Path::new("/home/me/project/src"),
            true,
            Some(Path::new("/home/me")),
        ));
        let workspace = args.iter().filter(|arg| *arg == "/home/me/project").count();
        assert_eq!(workspace, 2, "{args:?}");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ro-bind-try", "/usr"]),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ro-bind-try", "/home/me"]),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--chdir", "/home/me/project/src"]),
            "{args:?}"
        );
        // The workspace bind must follow the read-only home bind.
        let home = args.iter().position(|arg| arg == "/home/me").unwrap();
        let bind = args
            .windows(2)
            .position(|pair| pair == ["--bind", "/home/me/project"])
            .unwrap();
        assert!(bind > home, "{args:?}");
        assert!(!args.iter().any(|arg| arg == "--unshare-net"), "{args:?}");
        assert!(args.iter().any(|arg| arg == "--die-with-parent"));
    }

    #[test]
    fn hides_home_behind_a_scratch_directory_when_disabled() {
        let args = text(&bwrap_args(Path::new("/w"), Path::new("/w"), true, None));
        let ro_binds = args.iter().filter(|arg| *arg == "--ro-bind-try").count();
        assert_eq!(ro_binds, READ_ONLY_PATHS.len(), "{args:?}");
        assert!(
            args.windows(2).any(|pair| pair == ["--setenv", "HOME"]),
            "{args:?}"
        );
    }

    #[test]
    fn can_keep_the_network_private() {
        let args = text(&bwrap_args(Path::new("/w"), Path::new("/w"), false, None));
        assert!(args.iter().any(|arg| arg == "--unshare-net"), "{args:?}");
    }
}
