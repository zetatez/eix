//! Per-workspace pane state that survives a TUI detach.
//!
//! Only session ids, the focused pane and the pane layout are stored. The
//! daemon owns the sessions themselves, so a stale id is dropped when the next
//! session list arrives.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "workspaces.json";

/// Pane arrangement persisted with the active sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceLayout {
    #[default]
    Grid,
    Horizontal,
    Tabby,
    Fibonacci,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveWorkspace {
    #[serde(default)]
    pub sessions: Vec<String>,
    #[serde(default)]
    pub focused: Option<String>,
    #[serde(default)]
    pub layout: WorkspaceLayout,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkspaceFile {
    #[serde(default)]
    workspaces: BTreeMap<String, ActiveWorkspace>,
}

/// Load the active pane state for `cwd`, or an empty state when none exists.
pub fn load(data_dir: &Path, cwd: &Path) -> ActiveWorkspace {
    let Ok(file) = read_file(data_dir) else {
        return ActiveWorkspace::default();
    };
    let mut active = file
        .workspaces
        .get(&workspace_key(cwd))
        .cloned()
        .unwrap_or_default();
    let mut seen = HashSet::new();
    active
        .sessions
        .retain(|id| !id.is_empty() && seen.insert(id.clone()));
    if active
        .focused
        .as_ref()
        .is_some_and(|focused| !active.sessions.contains(focused))
    {
        active.focused = None;
    }
    active
}

/// Save the active pane state for `cwd`. An empty pane list clears it.
pub fn save(data_dir: &Path, cwd: &Path, active: &ActiveWorkspace) -> Result<()> {
    if active.sessions.is_empty() {
        return clear(data_dir, cwd);
    }
    let mut file = read_file(data_dir)?;
    file.workspaces.insert(workspace_key(cwd), active.clone());
    write_file(data_dir, &file)
}

/// Forget the active pane state for `cwd`.
pub fn clear(data_dir: &Path, cwd: &Path) -> Result<()> {
    let Ok(mut file) = read_file(data_dir) else {
        return Ok(());
    };
    if file.workspaces.remove(&workspace_key(cwd)).is_none() {
        return Ok(());
    }
    if file.workspaces.is_empty() {
        let path = state_path(data_dir);
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("cannot remove {}", path.display())),
        };
    }
    write_file(data_dir, &file)
}

fn workspace_key(cwd: &Path) -> String {
    cwd.canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE_NAME)
}

fn read_file(data_dir: &Path) -> Result<WorkspaceFile> {
    let path = state_path(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("cannot parse {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(WorkspaceFile::default()),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn write_file(data_dir: &Path, file: &WorkspaceFile) -> Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("cannot create {}", data_dir.display()))?;
    let path = state_path(data_dir);
    let temp = data_dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(file).context("cannot encode workspace state")?;
    std::fs::write(&temp, bytes).with_context(|| format!("cannot write {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("cannot replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "eix-workspaces-{}-{}-{}",
            std::process::id(),
            eix_core::now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn saves_and_loads_active_sessions_per_workspace() {
        let data = temp_dir();
        let first = ActiveWorkspace {
            sessions: vec!["s1".into(), "s2".into()],
            focused: Some("s2".into()),
            layout: WorkspaceLayout::Tabby,
        };
        let second = ActiveWorkspace {
            sessions: vec!["s3".into()],
            focused: None,
            layout: WorkspaceLayout::Fibonacci,
        };
        save(&data, Path::new("/tmp/eix-workspace-a"), &first).unwrap();
        save(&data, Path::new("/tmp/eix-workspace-b"), &second).unwrap();

        assert_eq!(load(&data, Path::new("/tmp/eix-workspace-a")), first);
        assert_eq!(load(&data, Path::new("/tmp/eix-workspace-b")), second);
        assert_eq!(
            load(&data, Path::new("/tmp/eix-workspace-c")),
            ActiveWorkspace::default()
        );
    }

    #[test]
    fn missing_layout_defaults_to_grid() {
        let active: ActiveWorkspace =
            serde_json::from_str(r#"{"sessions":["s1"],"focused":"s1"}"#).unwrap();
        assert_eq!(active.layout, WorkspaceLayout::Grid);
    }

    #[test]
    fn clearing_removes_only_that_workspace() {
        let data = temp_dir();
        let first = ActiveWorkspace {
            sessions: vec!["s1".into()],
            focused: Some("s1".into()),
            layout: WorkspaceLayout::Grid,
        };
        let second = ActiveWorkspace {
            sessions: vec!["s2".into()],
            focused: None,
            layout: WorkspaceLayout::Horizontal,
        };
        save(&data, Path::new("/tmp/eix-workspace-a"), &first).unwrap();
        save(&data, Path::new("/tmp/eix-workspace-b"), &second).unwrap();

        clear(&data, Path::new("/tmp/eix-workspace-a")).unwrap();
        assert_eq!(
            load(&data, Path::new("/tmp/eix-workspace-a")),
            ActiveWorkspace::default()
        );
        assert_eq!(load(&data, Path::new("/tmp/eix-workspace-b")), second);
    }

    #[test]
    fn empty_sessions_clear_the_workspace_and_drop_a_stale_focus() {
        let data = temp_dir();
        let active = ActiveWorkspace {
            sessions: vec!["s1".into()],
            focused: Some("s1".into()),
            layout: WorkspaceLayout::Tabby,
        };
        save(&data, Path::new("/tmp/eix-workspace-a"), &active).unwrap();
        save(
            &data,
            Path::new("/tmp/eix-workspace-a"),
            &ActiveWorkspace::default(),
        )
        .unwrap();
        assert_eq!(
            load(&data, Path::new("/tmp/eix-workspace-a")),
            ActiveWorkspace::default()
        );

        let mut file = WorkspaceFile::default();
        file.workspaces.insert(
            workspace_key(Path::new("/tmp/eix-workspace-a")),
            ActiveWorkspace {
                sessions: vec!["s1".into()],
                focused: Some("missing".into()),
                layout: WorkspaceLayout::Fibonacci,
            },
        );
        write_file(&data, &file).unwrap();
        assert_eq!(load(&data, Path::new("/tmp/eix-workspace-a")).focused, None);
    }
}
