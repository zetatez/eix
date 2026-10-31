//! On-demand installation of missing built-in language servers.
//!
//! The built-in LSP defaults name the standard server for each language. If
//! that binary is not on `PATH`, the first diagnostics request starts a
//! background install into eix's own data directory, so the user does not have
//! to configure or install anything by hand. Installs are keyed by server name
//! and never block the tool call that triggered them: the call reports that the
//! server is being installed and the next one uses it.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::Mutex;

/// How long one install step may run. Package managers can be slow on a cold
/// cache; a stuck process is stopped rather than keeping a job "installing"
/// forever.
const STEP_TIMEOUT: Duration = Duration::from_secs(600);

/// Download limit for a single release asset.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

const NPM: &str = if cfg!(windows) { "npm.cmd" } else { "npm" };
const PIP: &str = if cfg!(windows) { "python" } else { "python3" };
const EXE: &str = if cfg!(windows) { ".exe" } else { "" };

/// What the caller should do after asking for an install.
#[derive(Debug)]
pub enum Start {
    /// A job was just queued; report it once.
    Started,
    /// A job is already running; stay quiet.
    Running,
    /// A previous job finished; retry the spawn now.
    Ready,
    /// The install failed. `notify` is true for the first report only.
    Failed { message: String, notify: bool },
    /// No recipe exists, or the configured command is not the built-in one.
    Unavailable,
}

/// Tracks background installs for one daemon. The jobs map is shared with the
/// tasks so a completed job is visible on the next diagnostics request.
pub struct Installer {
    prefix: PathBuf,
    bin: PathBuf,
    jobs: Arc<Mutex<HashMap<String, Job>>>,
}

impl Default for Installer {
    fn default() -> Self {
        Self::new(eix_core::config::data_dir().join("lsp"))
    }
}

impl Installer {
    pub fn new(prefix: PathBuf) -> Self {
        // npm puts global binaries in `<prefix>/bin` on unix and directly in
        // `<prefix>` on Windows.
        let bin = if cfg!(windows) {
            prefix.clone()
        } else {
            prefix.join("bin")
        };
        Self {
            prefix,
            bin,
            jobs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Directory prepended to `PATH` for spawned servers.
    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// Start an install for a built-in server, or report the state of one that
    /// is already running or finished. `command` guards against installing for
    /// a user override that happens to use the same server name.
    pub async fn start(&self, name: &str, command: &str) -> Start {
        let Some(recipe) = recipe(name, &self.prefix, &self.bin) else {
            return Start::Unavailable;
        };
        if Path::new(command)
            .file_name()
            .and_then(|name| name.to_str())
            != Some(recipe.binary)
        {
            return Start::Unavailable;
        }

        let mut jobs = self.jobs.lock().await;
        if let Some(job) = jobs.get_mut(name) {
            match &job.state {
                State::Running => return Start::Running,
                State::Ready => return Start::Ready,
                State::Failed(message) => {
                    let message = message.clone();
                    let notify = !job.notified;
                    job.notified = true;
                    return Start::Failed { message, notify };
                }
            }
        }
        jobs.insert(
            name.to_string(),
            Job {
                state: State::Running,
                notified: false,
            },
        );
        drop(jobs);

        let jobs = Arc::clone(&self.jobs);
        let bin = self.bin.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            eprintln!("eixd: installing lsp server {name:?}");
            let result = match tokio::fs::create_dir_all(&bin).await {
                Ok(()) => run_steps(&recipe.steps).await,
                Err(err) => Err(err).context("cannot create the LSP install directory"),
            };
            let mut jobs = jobs.lock().await;
            if let Some(job) = jobs.get_mut(&name) {
                job.state = match result {
                    Ok(()) => {
                        eprintln!("eixd: installed lsp server {name:?}");
                        State::Ready
                    }
                    Err(err) => {
                        eprintln!("eixd: lsp install {name:?} failed: {err:#}");
                        State::Failed(format!("{err:#}"))
                    }
                };
            }
        });
        Start::Started
    }
}

#[derive(Clone)]
enum State {
    Running,
    Ready,
    Failed(String),
}

struct Job {
    state: State,
    /// The failure has already been surfaced to the model once.
    notified: bool,
}

struct Recipe {
    /// File name the servers' `command` must refer to, so a custom binary with
    /// the same server name is never replaced behind the user's back.
    binary: &'static str,
    steps: Vec<Step>,
}

enum Step {
    Run {
        program: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Download {
        url: String,
        gzip: bool,
        dest: PathBuf,
    },
}

async fn run_steps(steps: &[Step]) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("eix/", env!("CARGO_PKG_VERSION")))
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .context("cannot build download client")?;
    for step in steps {
        match step {
            Step::Run { program, args, env } => {
                let mut command = tokio::process::Command::new(program);
                command
                    .args(args)
                    .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped());
                let output = tokio::time::timeout(STEP_TIMEOUT, command.output())
                    .await
                    .with_context(|| format!("{program} timed out"))?
                    .with_context(|| format!("cannot run {program}"))?;
                if !output.status.success() {
                    bail!("{program} failed: {}", tail(&output.stderr));
                }
            }
            Step::Download { url, gzip, dest } => {
                download(&client, url, dest, *gzip).await?;
            }
        }
    }
    Ok(())
}

async fn download(client: &reqwest::Client, url: &str, dest: &Path, gzip: bool) -> Result<()> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot download {url}"))?
        .error_for_status()
        .with_context(|| format!("cannot download {url}"))?;
    let bytes = response.bytes().await.context("download interrupted")?;
    // Write beside the target and rename, so an interrupted download never
    // leaves a truncated executable that a later spawn would try to run.
    let temp = match dest.file_name().and_then(|name| name.to_str()) {
        Some(name) => dest.with_file_name(format!("{name}.download")),
        None => bail!("invalid install path {}", dest.display()),
    };
    if gzip {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_ref())
            .read_to_end(&mut out)
            .context("cannot unpack downloaded server")?;
        tokio::fs::write(&temp, &out).await?;
    } else {
        tokio::fs::write(&temp, &bytes).await?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
    }
    tokio::fs::rename(&temp, dest)
        .await
        .with_context(|| format!("cannot install {}", dest.display()))?;
    Ok(())
}

fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.is_empty() {
        return "no error output".into();
    }
    let mut lines: Vec<&str> = text.lines().rev().take(4).collect();
    lines.reverse();
    lines.join(" | ")
}

fn recipe(name: &str, prefix: &Path, bin: &Path) -> Option<Recipe> {
    let npm = |package: &str| Step::Run {
        program: NPM.into(),
        args: vec![
            "install".into(),
            "--global".into(),
            "--prefix".into(),
            prefix.display().to_string(),
            package.into(),
        ],
        env: Vec::new(),
    };
    let pip = |package: &str| Step::Run {
        program: PIP.into(),
        args: vec![
            "-m".into(),
            "pip".into(),
            "install".into(),
            "--prefix".into(),
            prefix.display().to_string(),
            "--upgrade".into(),
            package.into(),
        ],
        env: Vec::new(),
    };
    let go = |module: &str| Step::Run {
        program: "go".into(),
        args: vec!["install".into(), module.into()],
        env: vec![("GOBIN".into(), bin.display().to_string())],
    };
    let rustup = || Step::Run {
        program: "rustup".into(),
        args: vec!["component".into(), "add".into(), "rust-analyzer".into()],
        env: Vec::new(),
    };
    let download = |base: &str, asset: &str, gzip: bool, file: &str| Step::Download {
        url: format!("{base}/{asset}"),
        gzip,
        dest: bin.join(file),
    };

    let (binary, steps) = match name {
        "bash" => ("bash-language-server", vec![npm("bash-language-server")]),
        "cpp" => ("clangd", vec![pip("clangd")]),
        "css" => (
            "vscode-css-language-server",
            vec![npm("vscode-langservers-extracted")],
        ),
        "go" => ("gopls", vec![go("golang.org/x/tools/gopls@latest")]),
        "html" => (
            "vscode-html-language-server",
            vec![npm("vscode-langservers-extracted")],
        ),
        "json" => (
            "vscode-json-language-server",
            vec![npm("vscode-langservers-extracted")],
        ),
        "markdown" => (
            "marksman",
            vec![download(
                "https://github.com/artempyanykh/marksman/releases/latest/download",
                marksman_asset()?,
                false,
                &format!("marksman{EXE}"),
            )],
        ),
        "python" => ("pyright-langserver", vec![npm("pyright")]),
        "rust" => ("rust-analyzer", vec![rustup()]),
        "sql" => ("sqls", vec![go("github.com/sqls-server/sqls@latest")]),
        "toml" => (
            "taplo",
            vec![download(
                "https://github.com/tamasfe/taplo/releases/latest/download",
                taplo_asset()?,
                true,
                &format!("taplo{EXE}"),
            )],
        ),
        "typescript" => (
            "typescript-language-server",
            vec![npm("typescript-language-server"), npm("typescript")],
        ),
        "yaml" => ("yaml-language-server", vec![npm("yaml-language-server")]),
        _ => return None,
    };
    Some(Recipe { binary, steps })
}

fn marksman_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("marksman-linux-x64"),
        ("linux", "aarch64") => Some("marksman-linux-arm64"),
        ("macos", _) => Some("marksman-macos"),
        ("windows", _) => Some("marksman.exe"),
        _ => None,
    }
}

fn taplo_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("taplo-linux-x86_64.gz"),
        ("linux", "aarch64") => Some("taplo-linux-aarch64.gz"),
        ("macos", "x86_64") => Some("taplo-darwin-x86_64.gz"),
        ("macos", "aarch64") => Some("taplo-darwin-aarch64.gz"),
        ("windows", "x86_64") => Some("taplo-windows-x86_64.gz"),
        ("windows", "aarch64") => Some("taplo-windows-aarch64.gz"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_servers_have_install_recipes() {
        let dir = std::env::temp_dir().join("eix-lsp-recipes");
        let bin = dir.join("bin");
        for name in [
            "bash",
            "cpp",
            "css",
            "go",
            "html",
            "json",
            "markdown",
            "python",
            "rust",
            "sql",
            "toml",
            "typescript",
            "yaml",
        ] {
            assert!(recipe(name, &dir, &bin).is_some(), "{name} has no recipe");
        }
        assert!(recipe("lua", &dir, &bin).is_none());
        assert!(recipe("unknown", &dir, &bin).is_none());
    }

    #[tokio::test]
    async fn a_user_override_is_never_replaced() {
        let installer = Installer::new(std::env::temp_dir().join("eix-lsp-gate"));
        assert!(matches!(
            installer.start("rust", "my-rust-analyzer").await,
            Start::Unavailable
        ));
        assert!(matches!(
            installer.start("unknown", "anything").await,
            Start::Unavailable
        ));
    }

    #[tokio::test]
    #[ignore = "downloads release assets from GitHub"]
    async fn downloads_markdown_and_toml_servers() {
        let dir = std::env::temp_dir().join(format!("eix-lsp-download-{}", std::process::id()));
        let installer = Installer::new(dir.clone());
        for (name, command, binary) in [
            ("markdown", "marksman", format!("marksman{EXE}")),
            ("toml", "taplo", format!("taplo{EXE}")),
        ] {
            assert!(matches!(
                installer.start(name, command).await,
                Start::Started
            ));
            let deadline = std::time::Instant::now() + Duration::from_secs(180);
            loop {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{name} install timed out"
                );
                match installer.start(name, command).await {
                    Start::Running => tokio::time::sleep(Duration::from_millis(200)).await,
                    Start::Ready => break,
                    other => panic!("{name} install: {other:?}"),
                }
            }
            let path = dir.join("bin").join(&binary);
            assert!(path.is_file(), "missing {}", path.display());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                assert_eq!(mode & 0o111, 0o111, "{} is not executable", path.display());
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
