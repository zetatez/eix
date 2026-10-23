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

/// Arguments for `bwrap` that expose the system read-only, `workspace` (and
/// its extra writable roots) read-write, and start in `chdir`.
///
/// `read_paths` are mounted read-only individually (toolchain caches by
/// default). The whole home directory is deliberately not mounted.
pub fn bwrap_args(
    workspace: &Path,
    chdir: &Path,
    network: bool,
    read_paths: &[PathBuf],
    writable_roots: &[PathBuf],
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
    let home = home_dir();
    for path in read_paths {
        if path.exists() {
            args.push("--ro-bind-try".into());
            args.push(path.as_os_str().to_owned());
            args.push(path.as_os_str().to_owned());
        }
    }
    if !network {
        args.push("--unshare-net".into());
    }

    // Extra writable roots are bound before the workspace. The workspace bind
    // comes last so it stays writable even when it sits under a read-only path.
    for path in writable_roots {
        if path.exists() {
            args.push("--bind".into());
            args.push(path.as_os_str().to_owned());
            args.push(path.as_os_str().to_owned());
        }
    }
    args.push("--bind".into());
    args.push(workspace.as_os_str().to_owned());
    args.push(workspace.as_os_str().to_owned());
    args.push("--chdir".into());
    args.push(chdir.as_os_str().to_owned());
    args.push("--setenv".into());
    args.push("TMPDIR".into());
    args.push("/tmp".into());
    // Keep the real HOME when any read cache lives below it so cargo/rustup
    // find their caches; otherwise point HOME at the sandbox scratch dir.
    let has_home_read_path = home.as_ref().is_some_and(|home| {
        read_paths
            .iter()
            .any(|path| path == home || path.starts_with(home))
    });
    if let Some(home) = home.as_ref().filter(|_| has_home_read_path) {
        args.push("--setenv".into());
        args.push("HOME".into());
        args.push(home.as_os_str().to_owned());
    } else {
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
    fn binds_the_workspace_writable_and_caches_read_only() {
        let workspace = Path::new("/home/me/project");
        let cache = PathBuf::from("/tmp/eix-sandbox-cache-test");
        let _ = std::fs::create_dir_all(&cache);
        let args = text(&bwrap_args(
            workspace,
            workspace,
            true,
            std::slice::from_ref(&cache),
            &[],
        ));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--bind", "/home/me/project"]),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ro-bind-try", "/usr"]),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ro-bind-try", "/tmp/eix-sandbox-cache-test"]),
            "{args:?}"
        );
        assert!(
            !args
                .windows(2)
                .any(|pair| pair == ["--ro-bind-try", "/home/me"]),
            "the whole home directory must not be mounted: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--chdir", "/home/me/project"]),
            "{args:?}"
        );
        assert!(!args.iter().any(|arg| arg == "--unshare-net"), "{args:?}");
        assert!(args.iter().any(|arg| arg == "--die-with-parent"));
    }

    #[test]
    fn hides_home_behind_a_scratch_directory_when_no_cache_is_mounted() {
        let args = text(&bwrap_args(
            Path::new("/w"),
            Path::new("/w"),
            true,
            &[],
            &[],
        ));
        let ro_binds = args.iter().filter(|arg| *arg == "--ro-bind-try").count();
        assert_eq!(ro_binds, READ_ONLY_PATHS.len(), "{args:?}");
        assert!(
            args.windows(2).any(|pair| pair == ["--setenv", "HOME"]),
            "{args:?}"
        );
    }

    #[test]
    fn can_keep_the_network_private() {
        let args = text(&bwrap_args(
            Path::new("/w"),
            Path::new("/w"),
            false,
            &[],
            &[],
        ));
        assert!(args.iter().any(|arg| arg == "--unshare-net"), "{args:?}");
    }

    #[test]
    fn extra_writable_roots_are_bound() {
        let _ = std::fs::create_dir_all("/tmp/eix-sandbox-test-root");
        let root = PathBuf::from("/tmp/eix-sandbox-test-root");
        let args = text(&bwrap_args(
            Path::new("/w"),
            Path::new("/w"),
            true,
            &[],
            std::slice::from_ref(&root),
        ));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--bind", "/tmp/eix-sandbox-test-root"]),
            "{args:?}"
        );
    }
}
