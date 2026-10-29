//! Plugin packaging, checksum verification and installation.
//!
//! A distributable plugin is a directory with `eix-plugin.toml`, an
//! `eix-plugin.sums` file and, optionally, a detached Ed25519 signature
//! declared in the manifest:
//!
//! ```toml
//! [package]
//! publisher = "acme"
//! signature = "base64..."
//! ```
//!
//! The signature covers the exact bytes of the checksum file. The checksum
//! file lists every other file in the package, so a signature plus a complete
//! file walk rejects both modified and added files. Unsigned packages are only
//! installed when the caller explicitly opts in.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use ring::signature;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::plugin::{PluginError, PluginManifest};

pub const DEFAULT_CHECKSUMS: &str = "eix-plugin.sums";
pub const DEFAULT_SIGNATURE: &str = "eix-plugin.sig";
/// Installed-package lockfile, stored next to the installed plugin
/// directories. It records the version and checksums that were verified at
/// install time so drift is visible later.
pub const DEFAULT_LOCKFILE: &str = "plugins.lock";
const MAX_CHECKSUMS: u64 = 1024 * 1024;

/// One installed plugin entry in `plugins.lock`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LockedPlugin {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Relative path -> sha256, copied from the package checksum file.
    pub checksums: BTreeMap<String, String>,
}

/// `plugins.lock` contents.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginLock {
    pub plugins: BTreeMap<String, LockedPlugin>,
}

impl PluginLock {
    pub fn load(path: &Path) -> Result<Self, PluginError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).map_err(|source| PluginError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| PluginError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), PluginError> {
        let text = toml::to_string_pretty(self)
            .map_err(|err| PluginError::Invalid(format!("cannot encode lockfile: {err}")))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| PluginError::Read {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let temp = path.with_extension("lock.tmp");
        std::fs::write(&temp, text).map_err(|source| PluginError::Read {
            path: temp.clone(),
            source,
        })?;
        std::fs::rename(&temp, path).map_err(|source| PluginError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(())
    }
}

pub fn lockfile_path(plugins_root: &Path) -> PathBuf {
    plugins_root.join(DEFAULT_LOCKFILE)
}

/// Trusted publisher keys. Stored in `plugin-trust.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginTrustStore {
    /// Publisher name -> base64 Ed25519 public key (32 bytes).
    pub keys: BTreeMap<String, String>,
}

impl PluginTrustStore {
    pub fn load(path: &Path) -> Result<Self, PluginError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).map_err(|source| PluginError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| PluginError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), PluginError> {
        let text = toml::to_string_pretty(self)
            .map_err(|error| PluginError::Invalid(format!("cannot encode trust store: {error}")))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| PluginError::Read {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(path, text).map_err(|source| PluginError::Read {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn insert(&mut self, publisher: impl Into<String>, public_key: impl Into<String>) {
        self.keys.insert(publisher.into(), public_key.into());
    }
}

/// Result of a successful installation.
#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub name: String,
    pub path: PathBuf,
    pub manifest: PluginManifest,
}

/// Compute checksums for every package file and write `eix-plugin.sums`.
/// Returns the sorted `(path, sha256)` pairs.
pub fn pack(directory: &Path) -> Result<Vec<(String, String)>, PluginError> {
    let directory = directory
        .canonicalize()
        .map_err(|source| PluginError::Read {
            path: directory.to_path_buf(),
            source,
        })?;
    let manifest = PluginManifest::load(&directory.join("eix-plugin.toml")).ok();
    let checksums_relative = manifest
        .as_ref()
        .and_then(|manifest| manifest.package.as_ref())
        .and_then(|package| package.checksums.clone())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CHECKSUMS));
    let signature_relative = manifest
        .as_ref()
        .and_then(|manifest| manifest.package.as_ref())
        .and_then(|package| package.signature.clone())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SIGNATURE));
    let checksums = directory.join(&checksums_relative);
    let signature_relative = signature_relative.to_string_lossy().replace('\\', "/");
    let mut files = Vec::new();
    walk_files(&directory, &directory, &mut files)?;
    files.sort();
    let mut lines = String::new();
    let mut entries = Vec::new();
    for (relative, absolute) in files {
        if relative == checksums_relative.to_string_lossy()
            || relative == signature_relative
            || relative.ends_with(".sig")
        {
            continue;
        }
        let bytes = std::fs::read(&absolute).map_err(|source| PluginError::Read {
            path: absolute.clone(),
            source,
        })?;
        let hash = hex_digest(&bytes);
        lines.push_str(&format!("{hash}\t{relative}\n"));
        entries.push((relative, hash));
    }
    std::fs::write(&checksums, lines).map_err(|source| PluginError::Read {
        path: checksums.clone(),
        source,
    })?;
    Ok(entries)
}

/// Verify checksums and, when declared, the detached signature.
pub fn verify(
    directory: &Path,
    manifest: &PluginManifest,
    trust: &PluginTrustStore,
    allow_unsigned: bool,
) -> Result<(), PluginError> {
    let directory = directory
        .canonicalize()
        .map_err(|source| PluginError::Read {
            path: directory.to_path_buf(),
            source,
        })?;
    let package = manifest.package.as_ref().ok_or_else(|| {
        PluginError::Invalid(format!(
            "{} has no [package] table; run `eix plugin pack` first",
            directory.display()
        ))
    })?;
    let checksum_path = package
        .checksums
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CHECKSUMS));
    validate_relative(&checksum_path)?;
    let checksum_path = directory.join(&checksum_path);
    let meta = std::fs::metadata(&checksum_path).map_err(|source| PluginError::Read {
        path: checksum_path.clone(),
        source,
    })?;
    if meta.len() > MAX_CHECKSUMS {
        return Err(PluginError::Invalid(format!(
            "{} is larger than {MAX_CHECKSUMS} bytes",
            checksum_path.display()
        )));
    }
    let checksum_bytes = std::fs::read(&checksum_path).map_err(|source| PluginError::Read {
        path: checksum_path.clone(),
        source,
    })?;
    let checksums = parse_checksums(&checksum_bytes, &checksum_path)?;

    // Signature first: verifying a tampered checksum file against a trusted
    // key avoids hashing untrusted package files for no reason.
    let signature_path = package
        .signature
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SIGNATURE));
    validate_relative(&signature_path)?;
    let signature_path = directory.join(signature_path);
    if signature_path.exists() {
        let publisher = package.publisher.as_deref().ok_or_else(|| {
            PluginError::Invalid("a signed package needs package.publisher".into())
        })?;
        let encoded_key = trust.keys.get(publisher).ok_or_else(|| {
            PluginError::Invalid(format!(
                "publisher {publisher:?} is not trusted; add it to plugin-trust.toml"
            ))
        })?;
        let encoded =
            std::fs::read_to_string(&signature_path).map_err(|source| PluginError::Read {
                path: signature_path.clone(),
                source,
            })?;
        let key = base64::engine::general_purpose::STANDARD
            .decode(encoded_key.trim())
            .map_err(|error| PluginError::Invalid(format!("invalid public key: {error}")))?;
        let signature = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|error| PluginError::Invalid(format!("invalid signature: {error}")))?;
        signature::UnparsedPublicKey::new(&signature::ED25519, &key)
            .verify(&checksum_bytes, &signature)
            .map_err(|_| {
                PluginError::Invalid(format!(
                    "signature by publisher {publisher:?} does not match the checksum file"
                ))
            })?;
    } else if !allow_unsigned {
        return Err(PluginError::Invalid(
            "plugin is not signed; pass --allow-unsigned to install it anyway".into(),
        ));
    }

    let mut files = Vec::new();
    walk_files(&directory, &directory, &mut files)?;
    let expected_paths: BTreeSet<_> = checksums.keys().cloned().collect();
    let mut seen = BTreeSet::new();
    for (relative, absolute) in files {
        if is_checksum_path(&relative, package.checksums.as_deref())
            || is_signature_path(&relative, package.signature.as_deref())
            || relative.ends_with(".sig")
        {
            continue;
        }
        if !expected_paths.contains(&relative) {
            return Err(PluginError::Invalid(format!(
                "unlisted file {relative:?}; rerun `eix plugin pack` and re-sign"
            )));
        }
        if !seen.insert(relative.clone()) {
            return Err(PluginError::Invalid(format!("duplicate file {relative:?}")));
        }
        let expected = &checksums[&relative];
        let bytes = std::fs::read(&absolute).map_err(|source| PluginError::Read {
            path: absolute.clone(),
            source,
        })?;
        let actual = hex_digest(&bytes);
        if &actual != expected {
            return Err(PluginError::Invalid(format!(
                "checksum mismatch for {relative:?}: expected {expected}, got {actual}"
            )));
        }
    }
    let missing: Vec<_> = expected_paths.difference(&seen).cloned().collect();
    if !missing.is_empty() {
        return Err(PluginError::Invalid(format!(
            "checksum file lists missing file(s): {}",
            missing.join(", ")
        )));
    }
    Ok(())
}

/// Copy a verified package to `plugins_root/<name>`.
pub fn install(
    source: &Path,
    plugins_root: &Path,
    trust: &PluginTrustStore,
    allow_unsigned: bool,
    force: bool,
) -> Result<InstalledPlugin, PluginError> {
    let source = source
        .canonicalize()
        .map_err(|source_err| PluginError::Read {
            path: source.to_path_buf(),
            source: source_err,
        })?;
    let manifest_path = source.join("eix-plugin.toml");
    let manifest = PluginManifest::load(&manifest_path)?;
    verify(&source, &manifest, trust, allow_unsigned)?;

    std::fs::create_dir_all(plugins_root).map_err(|source| PluginError::Read {
        path: plugins_root.to_path_buf(),
        source,
    })?;
    let target = plugins_root.join(&manifest.name);
    if target.exists() && !force {
        return Err(PluginError::Invalid(format!(
            "{} already exists; pass --force to replace it",
            target.display()
        )));
    }
    let temp = plugins_root.join(format!(
        ".install-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&temp).map_err(|source| PluginError::Read {
        path: temp.clone(),
        source,
    })?;

    let copy_result = copy_tree(&source, &temp);
    if let Err(err) = copy_result {
        let _ = std::fs::remove_dir_all(&temp);
        return Err(err);
    }

    let backup = target.with_extension("installing-backup");
    if target.exists() {
        let _ = std::fs::remove_dir_all(&backup);
        std::fs::rename(&target, &backup).map_err(|source| PluginError::Read {
            path: target.clone(),
            source,
        })?;
    }
    if let Err(source) = std::fs::rename(&temp, &target) {
        if backup.exists() {
            let _ = std::fs::rename(&backup, &target);
        }
        let _ = std::fs::remove_dir_all(&temp);
        return Err(PluginError::Read {
            path: target.clone(),
            source,
        });
    }
    if backup.exists() {
        let _ = std::fs::remove_dir_all(&backup);
    }
    // Record what was verified. A failure to write the lockfile must not undo
    // a successful install, but it is reported so `eix plugin verify` can tell
    // the package was installed without one.
    let lock_path = lockfile_path(plugins_root);
    let mut lock = PluginLock::load(&lock_path).unwrap_or_default();
    lock.plugins
        .insert(manifest.name.clone(), locked_entry(&manifest, &target));
    lock.save(&lock_path)?;
    Ok(InstalledPlugin {
        name: manifest.name.clone(),
        path: target,
        manifest,
    })
}

/// Compare installed packages against `plugins.lock`. Returns the checked
/// plugin names.
pub fn verify_lock(plugins_root: &Path) -> Result<Vec<String>, PluginError> {
    let lock_path = lockfile_path(plugins_root);
    let lock = PluginLock::load(&lock_path)?;
    if lock.plugins.is_empty() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for (name, entry) in &lock.plugins {
        let directory = plugins_root.join(name);
        let manifest_path = directory.join("eix-plugin.toml");
        if !manifest_path.exists() {
            return Err(PluginError::Invalid(format!(
                "{name} is in {} but {} is missing",
                lock_path.display(),
                directory.display()
            )));
        }
        let manifest = PluginManifest::load(&manifest_path)?;
        if manifest.version != entry.version {
            return Err(PluginError::Invalid(format!(
                "{name}: lockfile pins {} but installed version is {}",
                entry.version, manifest.version
            )));
        }
        for (relative, expected) in &entry.checksums {
            let path = directory.join(relative);
            let bytes = std::fs::read(&path).map_err(|source| PluginError::Read {
                path: path.clone(),
                source,
            })?;
            if &hex_digest(&bytes) != expected {
                return Err(PluginError::Invalid(format!(
                    "{name}: {relative} does not match {}",
                    lock_path.display()
                )));
            }
        }
        names.push(name.clone());
    }
    Ok(names)
}

/// Drop `name` from `plugins.lock` after an uninstall. A missing lockfile or
/// entry is not an error; returns whether an entry was removed.
pub fn forget_locked_plugin(plugins_root: &Path, name: &str) -> Result<bool, PluginError> {
    let path = lockfile_path(plugins_root);
    let mut lock = PluginLock::load(&path)?;
    if lock.plugins.remove(name).is_none() {
        return Ok(false);
    }
    lock.save(&path)?;
    Ok(true)
}

/// Snapshot the package metadata that a lockfile should pin.
fn locked_entry(manifest: &PluginManifest, directory: &Path) -> LockedPlugin {
    let package = manifest.package.as_ref();
    let checksums_path = package
        .and_then(|package| package.checksums.clone())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CHECKSUMS));
    let checksums = std::fs::read(directory.join(&checksums_path))
        .ok()
        .and_then(|bytes| parse_checksums(&bytes, &checksums_path).ok())
        .unwrap_or_default();
    LockedPlugin {
        version: manifest.version.clone(),
        publisher: package.and_then(|package| package.publisher.clone()),
        checksums,
    }
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), PluginError> {
    for entry in std::fs::read_dir(source).map_err(|source_err| PluginError::Read {
        path: source.to_path_buf(),
        source: source_err,
    })? {
        let entry = entry.map_err(|source_err| PluginError::Read {
            path: source.to_path_buf(),
            source: source_err,
        })?;
        let file_type = entry.file_type().map_err(|source_err| PluginError::Read {
            path: entry.path(),
            source: source_err,
        })?;
        if file_type.is_symlink() {
            return Err(PluginError::Invalid(format!(
                "refusing to install symlink {}",
                entry.path().display()
            )));
        }
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            std::fs::create_dir_all(&destination).map_err(|source_err| PluginError::Read {
                path: destination.clone(),
                source: source_err,
            })?;
            copy_tree(&entry.path(), &destination)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &destination).map_err(|source_err| PluginError::Read {
                path: destination.clone(),
                source: source_err,
            })?;
        }
    }
    Ok(())
}

fn walk_files(
    root: &Path,
    directory: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), PluginError> {
    for entry in std::fs::read_dir(directory).map_err(|source| PluginError::Read {
        path: directory.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| PluginError::Read {
            path: directory.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| PluginError::Read {
            path: entry.path(),
            source,
        })?;
        if file_type.is_symlink() {
            return Err(PluginError::Invalid(format!(
                "plugin packages must not contain symlinks: {}",
                entry.path().display()
            )));
        }
        let path = entry.path();
        if file_type.is_dir() {
            // VCS metadata is never part of a package.
            if entry.file_name() == ".git" {
                continue;
            }
            walk_files(root, &path, out)?;
        } else if file_type.is_file() {
            let relative = path.strip_prefix(root).map_err(|_| {
                PluginError::Invalid(format!("{} is outside the package", path.display()))
            })?;
            let relative = relative.to_str().ok_or_else(|| {
                PluginError::Invalid(format!("non-UTF-8 file name {}", relative.display()))
            })?;
            if relative.contains('\n') || relative.contains('\t') {
                return Err(PluginError::Invalid(format!(
                    "file name {relative:?} must not contain tabs or newlines"
                )));
            }
            if relative == ".DS_Store" {
                continue;
            }
            out.push((relative.replace('\\', "/"), path));
        }
    }
    Ok(())
}

fn parse_checksums(bytes: &[u8], path: &Path) -> Result<BTreeMap<String, String>, PluginError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| PluginError::Invalid(format!("{} is not valid UTF-8", path.display())))?;
    let mut out = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (hash, relative) = line.split_once('\t').ok_or_else(|| {
            PluginError::Invalid(format!(
                "{}:{}: expected `<sha256>\\t<path>`",
                path.display(),
                index + 1
            ))
        })?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(PluginError::Invalid(format!(
                "{}:{}: invalid SHA-256 digest",
                path.display(),
                index + 1
            )));
        }
        validate_relative(Path::new(relative))?;
        if out
            .insert(relative.to_string(), hash.to_ascii_lowercase())
            .is_some()
        {
            return Err(PluginError::Invalid(format!(
                "{}:{}: duplicate path {relative:?}",
                path.display(),
                index + 1
            )));
        }
    }
    if out.is_empty() {
        return Err(PluginError::Invalid(format!(
            "{} has no entries",
            path.display()
        )));
    }
    Ok(out)
}

fn validate_relative(path: &Path) -> Result<(), PluginError> {
    if path.is_absolute()
        || path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::RootDir))
    {
        return Err(PluginError::Invalid(format!(
            "package path {} must be relative and stay inside the package",
            path.display()
        )));
    }
    Ok(())
}

fn is_checksum_path(relative: &str, configured: Option<&Path>) -> bool {
    configured
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .as_deref()
        .map(|path| path == relative)
        .unwrap_or(relative == DEFAULT_CHECKSUMS)
}

fn is_signature_path(relative: &str, configured: Option<&Path>) -> bool {
    configured
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .as_deref()
        .map(|path| path == relative)
        .unwrap_or(relative == DEFAULT_SIGNATURE)
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Write a checksum file atomically. `pack` is the user-facing wrapper; this
/// helper keeps the write logic testable without a directory walk.
pub fn write_checksums(path: &Path, entries: &[(String, String)]) -> Result<(), PluginError> {
    let mut text = String::new();
    for (relative, hash) in entries {
        text.push_str(&format!("{hash}\t{relative}\n"));
    }
    let temp = path.with_extension("sums.tmp");
    {
        let mut file = std::fs::File::create(&temp).map_err(|source| PluginError::Read {
            path: temp.clone(),
            source,
        })?;
        file.write_all(text.as_bytes())
            .map_err(|source| PluginError::Read {
                path: temp.clone(),
                source,
            })?;
    }
    std::fs::rename(&temp, path).map_err(|source| PluginError::Read {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair as _;

    fn package(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("eix-plugin-package-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("eix-plugin.toml"),
            format!(
                "schema = 2\nname = \"{name}\"\napi_version = 2\n[package]\n\n[components.server]\ncommand = \"python3\"\nentry = \"plugin.py\"\n"
            ),
        )
        .unwrap();
        std::fs::write(root.join("plugin.py"), "print('hi')\n").unwrap();
        root
    }

    #[test]
    fn pack_then_verify_accepts_an_unchanged_package() {
        let root = package("ok");
        let manifest = PluginManifest::load(&root.join("eix-plugin.toml")).unwrap();
        pack(&root).unwrap();
        verify(&root, &manifest, &PluginTrustStore::default(), true).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_rejects_modified_files_and_unlisted_extra_files() {
        let root = package("tamper");
        let manifest = PluginManifest::load(&root.join("eix-plugin.toml")).unwrap();
        pack(&root).unwrap();
        std::fs::write(root.join("plugin.py"), "print('tampered')\n").unwrap();
        assert!(verify(&root, &manifest, &PluginTrustStore::default(), true).is_err());
        pack(&root).unwrap();
        std::fs::write(root.join("extra.sh"), "echo hi\n").unwrap();
        assert!(verify(&root, &manifest, &PluginTrustStore::default(), true).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unsigned_packages_need_an_explicit_opt_in() {
        let root = package("unsigned");
        let manifest = PluginManifest::load(&root.join("eix-plugin.toml")).unwrap();
        pack(&root).unwrap();
        assert!(verify(&root, &manifest, &PluginTrustStore::default(), false).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_copies_the_package_into_the_plugins_root() {
        let root = package("installed");
        pack(&root).unwrap();
        let target = root.with_extension("target");
        let installed = install(&root, &target, &PluginTrustStore::default(), true, false).unwrap();
        assert_eq!(installed.name, "installed");
        assert!(installed.path.join("plugin.py").exists());
        assert!(installed.path.join(DEFAULT_CHECKSUMS).exists());
        assert!(install(&root, &target, &PluginTrustStore::default(), true, false).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lockfile_records_installs_detects_drift_and_forgets_uninstalls() {
        let root = package("locked");
        pack(&root).unwrap();
        let target = root.with_extension("locked-target");
        let _ = std::fs::remove_dir_all(&target);
        install(&root, &target, &PluginTrustStore::default(), true, false).unwrap();
        assert_eq!(verify_lock(&target).unwrap(), vec!["locked".to_string()]);

        std::fs::write(target.join("locked/plugin.py"), "print('drift')\n").unwrap();
        assert!(verify_lock(&target).is_err());

        assert!(forget_locked_plugin(&target, "locked").unwrap());
        assert!(!forget_locked_plugin(&target, "locked").unwrap());
        assert!(verify_lock(&target).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn paths_cannot_escape_the_package() {
        assert!(validate_relative(Path::new("plugin.py")).is_ok());
        assert!(validate_relative(Path::new("../escape")).is_err());
        assert!(validate_relative(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn signed_packages_verify_against_the_trust_store() {
        let root = std::env::temp_dir().join(format!("eix-plugin-signed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("eix-plugin.toml"),
            "schema = 2\nname = \"signed\"\napi_version = 2\n[package]\npublisher = \"acme\"\n\n[components.server]\ncommand = \"python3\"\nentry = \"plugin.py\"\n",
        )
        .unwrap();
        std::fs::write(root.join("plugin.py"), "print('hi')\n").unwrap();
        pack(&root).unwrap();

        let keypair = signature::Ed25519KeyPair::from_seed_unchecked(&[7_u8; 32]).unwrap();
        let checksums = std::fs::read(root.join(DEFAULT_CHECKSUMS)).unwrap();
        let signature = keypair.sign(&checksums);
        std::fs::write(
            root.join(DEFAULT_SIGNATURE),
            base64::engine::general_purpose::STANDARD.encode(signature.as_ref()),
        )
        .unwrap();

        let manifest = PluginManifest::load(&root.join("eix-plugin.toml")).unwrap();
        let mut trust = PluginTrustStore::default();
        trust.insert(
            "acme",
            base64::engine::general_purpose::STANDARD.encode(keypair.public_key().as_ref()),
        );
        verify(&root, &manifest, &trust, false).unwrap();

        // A package whose checksum file changed no longer matches the signature.
        std::fs::write(root.join("plugin.py"), "print('tampered')\n").unwrap();
        assert!(verify(&root, &manifest, &trust, false).is_err());

        // An unknown publisher is rejected even when the signature itself is valid.
        let other = signature::Ed25519KeyPair::from_seed_unchecked(&[9_u8; 32]).unwrap();
        std::fs::write(
            root.join(DEFAULT_SIGNATURE),
            base64::engine::general_purpose::STANDARD.encode(other.sign(&checksums).as_ref()),
        )
        .unwrap();
        assert!(verify(&root, &manifest, &PluginTrustStore::default(), false).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
