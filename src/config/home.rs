//! Ownership and atomic persistence for Vergerail's dedicated Codex home.

use crate::error::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, OwnedMutexGuard};

const MANAGED_HEADER: &str = "# VERGERAIL-MANAGED-CONFIG v1\n";
const HOME_MARKER_FILE: &str = ".vergerail-managed-home";
const HOME_LOCK_FILE: &str = ".vergerail-home.lock";
const CONFIG_FILE: &str = "config.toml";
const PROJECT_STATE_FILE: &str = "vergerail-projects.json";
const RUNTIME_WORKDIR: &str = "vergerail-runtime-workdir";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(crate) struct ManagedHome {
    root: PathBuf,
    projects: Arc<Mutex<BTreeSet<PathBuf>>>,
    image_generation: bool,
    _ownership: File,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectState {
    projects: BTreeSet<PathBuf>,
}

struct HomeInspection {
    marker_present: bool,
    state: ProjectState,
}

impl ManagedHome {
    #[cfg(test)]
    pub(crate) async fn prepare(root: PathBuf) -> Result<Arc<Self>> {
        Self::prepare_for(root, "vergerail".to_owned()).await
    }

    #[cfg(test)]
    pub(crate) async fn prepare_for(root: PathBuf, owner: String) -> Result<Arc<Self>> {
        Self::prepare_with_features(root, owner, false).await
    }

    pub(crate) async fn prepare_with_features(
        root: PathBuf,
        owner: String,
        image_generation: bool,
    ) -> Result<Arc<Self>> {
        tokio::task::spawn_blocking(move || prepare_blocking(root, owner, image_generation))
            .await
            .map_err(|error| {
                Error::new(
                    ErrorKind::Process,
                    "codex_home.prepare",
                    format!("home preparation worker failed: {error}"),
                )
            })?
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn neutral_workdir(&self) -> PathBuf {
        self.root.join(RUNTIME_WORKDIR)
    }

    pub(crate) async fn register_untrusted_project(
        self: &Arc<Self>,
        cwd: &Path,
    ) -> Result<PathBuf> {
        let canonical = canonical_project(cwd).await?;
        // Acquire admission before creating a non-cancelable blocking task. A
        // caller cancelled while waiting for this owned guard leaves no worker
        // behind; once admitted, the guard and complete managed-home owner move
        // into one detached-safe filesystem transaction.
        let projects = Arc::clone(&self.projects).lock_owned().await;
        if projects.contains(&canonical) {
            return Ok(canonical);
        }
        let home = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            commit_untrusted_project(&home.root, projects, canonical, home.image_generation)
        })
        .await
        .map_err(|error| {
            Error::new(
                ErrorKind::Process,
                "codex_home.write",
                format!("config writer failed: {error}"),
            )
        })?
    }

    /// Validates a text-only cwd without adding it to durable Codex project
    /// configuration. Text-only sessions have no execution surface to trust.
    pub(crate) async fn validate_transient_project(&self, cwd: &Path) -> Result<PathBuf> {
        canonical_project(cwd).await
    }
}

async fn canonical_project(cwd: &Path) -> Result<PathBuf> {
    let metadata = tokio::fs::symlink_metadata(cwd).await.map_err(|error| {
        Error::new(
            ErrorKind::InvalidInput,
            "session.cwd",
            format!("cannot inspect {}: {error}", cwd.display()),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "session.cwd",
            "working directory must be a real, non-symlink directory",
        ));
    }
    let canonical = tokio::fs::canonicalize(cwd).await.map_err(|error| {
        Error::new(
            ErrorKind::InvalidInput,
            "session.cwd",
            format!("cannot canonicalize {}: {error}", cwd.display()),
        )
    })?;
    if !canonical.is_dir() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "session.cwd",
            "working directory is not a directory",
        ));
    }
    validate_managed_project_path(&canonical, "session.cwd")?;
    Ok(canonical)
}

fn commit_untrusted_project(
    root: &Path,
    mut projects: OwnedMutexGuard<BTreeSet<PathBuf>>,
    canonical: PathBuf,
    image_generation: bool,
) -> Result<PathBuf> {
    // The owned admission guard remains in the blocking worker. Cancellation of
    // the awaiting caller cannot release ordering before disk and memory agree.
    let mut snapshot = projects.clone();
    snapshot.insert(canonical.clone());
    write_managed_files(root, &snapshot, image_generation)?;
    *projects = snapshot;
    Ok(canonical)
}

fn prepare_blocking(
    root: PathBuf,
    owner: String,
    image_generation: bool,
) -> Result<Arc<ManagedHome>> {
    let marker = home_marker(&owner);
    ensure_real_directory(&root, true, "codex_home.create")?;
    let root = root.canonicalize().map_err(|error| {
        Error::new(
            ErrorKind::Process,
            "codex_home.canonicalize",
            error.to_string(),
        )
    })?;
    // Establish explicit ownership before mutating the caller's directory.
    // Unmarked non-empty CODEX_HOME directories are never adopted.
    inspect_managed_home(&root, marker.as_bytes())?;
    set_private_dir_permissions(&root)?;
    let ownership = lock_managed_home(&root)?;
    // Re-inspect while holding the ownership lock. This closes the gap between
    // the non-mutating preflight and lock acquisition.
    let inspection = inspect_managed_home(&root, marker.as_bytes())?;
    if !inspection.marker_present {
        atomic_write(&root.join(HOME_MARKER_FILE), marker.as_bytes())?;
    }
    let mut state = inspection.state;
    let removed_stale_projects = prune_stale_projects(&mut state.projects);

    let expected = render_config(&state.projects, image_generation)?;
    let config_path = root.join(CONFIG_FILE);
    if existing_regular_file(&config_path, "codex_home.config")? {
        let existing = fs::read_to_string(&config_path).map_err(|error| {
            Error::new(ErrorKind::Process, "codex_home.config", error.to_string())
        })?;
        if !existing.starts_with(MANAGED_HEADER) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "codex_home.config",
                format!(
                    "{} is not Vergerail-managed; use a dedicated CODEX_HOME",
                    config_path.display()
                ),
            ));
        }
        if removed_stale_projects {
            write_managed_files(&root, &state.projects, image_generation)?;
        } else if existing != expected {
            atomic_write(&config_path, expected.as_bytes())?;
        }
    } else {
        write_managed_files(&root, &state.projects, image_generation)?;
    }

    let workdir = root.join(RUNTIME_WORKDIR);
    ensure_real_directory(&workdir, false, "codex_home.workdir")?;
    set_private_dir_permissions(&workdir)?;

    Ok(Arc::new(ManagedHome {
        root,
        projects: Arc::new(Mutex::new(state.projects)),
        image_generation,
        _ownership: ownership,
    }))
}

fn prune_stale_projects(projects: &mut BTreeSet<PathBuf>) -> bool {
    let original_len = projects.len();
    projects.retain(|project| match fs::symlink_metadata(project) {
        Ok(metadata) => metadata.is_dir() && !metadata.file_type().is_symlink(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    });
    projects.len() != original_len
}

fn inspect_managed_home(root: &Path, expected_marker: &[u8]) -> Result<HomeInspection> {
    let marker_path = root.join(HOME_MARKER_FILE);
    let marker_present = existing_regular_file(&marker_path, "codex_home.ownership")?;
    if marker_present {
        let marker = fs::read(&marker_path).map_err(|error| {
            Error::new(
                ErrorKind::Process,
                "codex_home.ownership",
                format!("cannot read {}: {error}", marker_path.display()),
            )
        })?;
        if marker != expected_marker {
            let message = if recognized_owner_marker(&marker) {
                format!(
                    "{} belongs to a different Vergerail consumer; use this application's dedicated CODEX_HOME",
                    marker_path.display()
                )
            } else {
                format!(
                    "{} is not a recognized Vergerail managed-home marker",
                    marker_path.display()
                )
            };
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "codex_home.ownership",
                message,
            ));
        }
    }

    let config_path = root.join(CONFIG_FILE);
    let managed_config_present = existing_regular_file(&config_path, "codex_home.config")?;
    if managed_config_present {
        let existing = fs::read_to_string(&config_path).map_err(|error| {
            Error::new(
                ErrorKind::Process,
                "codex_home.config",
                format!("cannot read {}: {error}", config_path.display()),
            )
        })?;
        if !existing.starts_with(MANAGED_HEADER) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "codex_home.config",
                format!(
                    "{} is not Vergerail-managed; use a dedicated CODEX_HOME",
                    config_path.display()
                ),
            ));
        }
    }

    let state_path = root.join(PROJECT_STATE_FILE);
    let state_present = existing_regular_file(&state_path, "codex_home.state")?;
    let state = if state_present {
        serde_json::from_reader(fs::File::open(&state_path).map_err(|error| {
            Error::new(
                ErrorKind::Process,
                "codex_home.state",
                format!("cannot read {}: {error}", state_path.display()),
            )
        })?)
        .map_err(|error| {
            Error::new(
                ErrorKind::Process,
                "codex_home.state",
                format!("invalid Vergerail project state: {error}"),
            )
        })?
    } else {
        ProjectState::default()
    };
    for project in &state.projects {
        validate_managed_project_path(project, "codex_home.state")?;
    }

    // The lock file may survive a crashed owner and is safe to reclaim. Every
    // other pre-existing entry requires the explicit marker; this prevents
    // accidental auth/database reuse or ownership claims over existing homes.
    let _ = existing_regular_file(&root.join(HOME_LOCK_FILE), "codex_home.lock")?;
    if !marker_present {
        for entry in fs::read_dir(root).map_err(|error| {
            Error::new(
                ErrorKind::Process,
                "codex_home.ownership",
                format!("cannot inspect {}: {error}", root.display()),
            )
        })? {
            let entry = entry.map_err(|error| {
                Error::new(
                    ErrorKind::Process,
                    "codex_home.ownership",
                    format!("cannot inspect an entry in {}: {error}", root.display()),
                )
            })?;
            if entry.file_name() != std::ffi::OsStr::new(HOME_LOCK_FILE) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "codex_home.ownership",
                    format!(
                        "{} is non-empty and has no Vergerail ownership marker; use a new dedicated CODEX_HOME",
                        root.display()
                    ),
                ));
            }
        }
    }

    Ok(HomeInspection {
        marker_present,
        state,
    })
}

fn home_marker(owner: &str) -> String {
    format!("VERGERAIL-MANAGED-HOME v2\nowner={owner}\n")
}

fn recognized_owner_marker(marker: &[u8]) -> bool {
    std::str::from_utf8(marker)
        .ok()
        .and_then(|marker| marker.strip_prefix("VERGERAIL-MANAGED-HOME v2\nowner="))
        .and_then(|owner| owner.strip_suffix('\n'))
        .is_some_and(super::valid_home_owner)
}

fn lock_managed_home(root: &Path) -> Result<File> {
    let path = root.join(HOME_LOCK_FILE);
    let _ = existing_regular_file(&path, "codex_home.lock")?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| {
            Error::new(
                ErrorKind::Process,
                "codex_home.lock",
                format!("cannot open {}: {error}", path.display()),
            )
        })?;
    verify_open_lock_path(&path, &file)?;
    set_private_open_file_permissions(&file)?;
    fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
        let kind = if error.kind() == fs2::lock_contended_error().kind() {
            ErrorKind::InvalidInput
        } else {
            ErrorKind::Process
        };
        Error::new(
            kind,
            "codex_home.lock",
            format!(
                "cannot acquire exclusive ownership of {}: {error}; use a different dedicated CODEX_HOME or close the existing Vergerail client",
                root.display()
            ),
        )
    })?;
    verify_open_lock_path(&path, &file)?;
    Ok(file)
}

fn verify_open_lock_path(path: &Path, file: &File) -> Result<()> {
    let path_metadata = fs::symlink_metadata(path).map_err(|error| {
        Error::new(
            ErrorKind::Process,
            "codex_home.lock",
            format!("cannot inspect {}: {error}", path.display()),
        )
    })?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "codex_home.lock",
            format!("{} must be a regular non-symlink lock file", path.display()),
        ));
    }
    verify_open_file_identity(path, &path_metadata, file, "codex_home.lock")
}

#[cfg(unix)]
fn verify_open_file_identity(
    path: &Path,
    path_metadata: &fs::Metadata,
    file: &File,
    operation: &'static str,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let file_metadata = file.metadata().map_err(|error| {
        Error::new(
            ErrorKind::Process,
            operation,
            format!("cannot inspect open {}: {error}", path.display()),
        )
    })?;
    if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            operation,
            format!("{} changed while it was being opened", path.display()),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_open_file_identity(
    _path: &Path,
    _path_metadata: &fs::Metadata,
    _file: &File,
    _operation: &'static str,
) -> Result<()> {
    Ok(())
}

fn validate_managed_project_path(path: &Path, operation: &'static str) -> Result<()> {
    if !path.is_absolute() || path.as_os_str().is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            operation,
            "managed project paths must be non-empty and absolute",
        ));
    }
    if path.to_str().is_none() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            operation,
            "managed project paths must be valid UTF-8 for the JSON/TOML protocol boundary",
        ));
    }
    Ok(())
}

fn ensure_real_directory(path: &Path, recursive: bool, operation: &'static str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    operation,
                    format!("{} must be a real, non-symlink directory", path.display()),
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let result = if recursive {
                fs::create_dir_all(path)
            } else {
                fs::create_dir(path)
            };
            result.map_err(|error| {
                Error::new(
                    ErrorKind::Process,
                    operation,
                    format!("cannot create {}: {error}", path.display()),
                )
            })?;
            let metadata = fs::symlink_metadata(path).map_err(|error| {
                Error::new(
                    ErrorKind::Process,
                    operation,
                    format!("cannot inspect {} after creation: {error}", path.display()),
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    operation,
                    format!("{} is not a real directory after creation", path.display()),
                ));
            }
        }
        Err(error) => {
            return Err(Error::new(
                ErrorKind::Process,
                operation,
                format!("cannot inspect {}: {error}", path.display()),
            ));
        }
    }
    Ok(())
}

fn existing_regular_file(path: &Path, operation: &'static str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(Error::new(
                ErrorKind::InvalidInput,
                operation,
                format!("{} must be a regular non-symlink file", path.display()),
            ))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::new(
            ErrorKind::Process,
            operation,
            format!("cannot inspect {}: {error}", path.display()),
        )),
    }
}

fn write_managed_files(
    root: &Path,
    projects: &BTreeSet<PathBuf>,
    image_generation: bool,
) -> Result<()> {
    let state = serde_json::to_vec_pretty(&ProjectState {
        projects: projects.clone(),
    })
    .map_err(|error| Error::new(ErrorKind::Process, "codex_home.state", error.to_string()))?;
    // `vergerail-projects.json` is the authoritative commit marker. Write the
    // derived Codex config first so a failed config replacement cannot commit
    // a project that the current process did not accept. If the final state
    // write fails, the next prepare regenerates config from the older state.
    let config = render_config(projects, image_generation)?;
    atomic_write(&root.join(CONFIG_FILE), config.as_bytes())?;
    atomic_write(&root.join(PROJECT_STATE_FILE), &state)?;
    Ok(())
}

fn render_config(projects: &BTreeSet<PathBuf>, image_generation: bool) -> Result<String> {
    let mut output = String::from(MANAGED_HEADER);
    output.push_str(
        "check_for_update_on_startup = false\n\
         allow_login_shell = false\n\
         web_search = \"disabled\"\n\
         approval_policy = \"never\"\n\
         sandbox_mode = \"read-only\"\n\n\
         [analytics]\n\
         enabled = false\n\n\
         [shell_environment_policy]\n\
         inherit = \"core\"\n\
         ignore_default_excludes = false\n\n\
         [features]\n\
         apps = false\n\
         auth_elicitation = false\n\
         browser_use = false\n\
         browser_use_external = false\n\
         browser_use_full_cdp_access = false\n\
         computer_use = false\n\
         goals = false\n\
         hooks = false\n\
         image_generation = ",
    );
    output.push_str(if image_generation {
        "true\n"
    } else {
        "false\n"
    });
    output.push_str(
        "\
         in_app_browser = false\n\
         multi_agent = false\n\
         plugin_sharing = false\n\
         plugins = false\n\
         remote_plugin = false\n\
         skill_mcp_dependency_install = false\n\
         tool_call_mcp_elicitation = false\n\
         workspace_dependencies = false\n",
    );
    for project in projects {
        validate_managed_project_path(project, "codex_home.config")?;
        let project = project.to_str().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "codex_home.config",
                "managed project paths must be valid UTF-8 for TOML rendering",
            )
        })?;
        output.push_str("\n[projects.\"");
        output.push_str(&escape_toml_string(project));
        output.push_str("\"]\ntrust_level = \"untrusted\"\n");
    }
    Ok(output)
}

fn escape_toml_string(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value if value.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{:04X}", u32::from(value));
            }
            value => output.push(value),
        }
    }
    output
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let _ = existing_regular_file(path, "codex_home.write")?;
    let parent = path.parent().ok_or_else(|| {
        Error::new(
            ErrorKind::Process,
            "codex_home.write",
            format!("{} has no parent directory", path.display()),
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        Error::new(
            ErrorKind::Process,
            "codex_home.write",
            format!("{} has no file name", path.display()),
        )
    })?;
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{}.vergerail.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        sequence
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| {
                Error::new(
                    ErrorKind::Process,
                    "codex_home.write",
                    format!("cannot create {}: {error}", temporary.display()),
                )
            })?;
        set_private_open_file_permissions(&file)?;
        file.write_all(bytes).map_err(|error| {
            Error::new(ErrorKind::Process, "codex_home.write", error.to_string())
        })?;
        file.sync_all().map_err(|error| {
            Error::new(ErrorKind::Process, "codex_home.write", error.to_string())
        })?;
        fs::rename(&temporary, path).map_err(|error| {
            Error::new(
                ErrorKind::Process,
                "codex_home.write",
                format!("cannot replace {}: {error}", path.display()),
            )
        })?;
        sync_directory(parent)?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => match fs::remove_file(&temporary) {
            Ok(()) => Err(error),
            Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(error),
            Err(cleanup) => Err(error.with_related_error(
                "temporary managed-config cleanup also failed",
                &Error::new(
                    ErrorKind::Process,
                    "codex_home.write",
                    format!("cannot remove {}: {cleanup}", temporary.display()),
                ),
            )),
        },
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    let directory = fs::File::open(path).map_err(|error| {
        Error::new(
            ErrorKind::Process,
            "codex_home.write",
            format!("cannot open {} for sync: {error}", path.display()),
        )
    })?;
    directory.sync_all().map_err(|error| {
        Error::new(
            ErrorKind::Process,
            "codex_home.write",
            format!("cannot sync {}: {error}", path.display()),
        )
    })
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let path_metadata = fs::symlink_metadata(path).map_err(|error| {
        Error::new(
            ErrorKind::Process,
            "codex_home.permissions",
            format!("cannot inspect {}: {error}", path.display()),
        )
    })?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_dir() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "codex_home.permissions",
            format!("{} must be a real, non-symlink directory", path.display()),
        ));
    }
    let directory = File::open(path).map_err(|error| {
        Error::new(
            ErrorKind::Process,
            "codex_home.permissions",
            format!("cannot open {}: {error}", path.display()),
        )
    })?;
    verify_open_file_identity(path, &path_metadata, &directory, "codex_home.permissions")?;
    directory
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|error| {
            Error::new(
                ErrorKind::Process,
                "codex_home.permissions",
                error.to_string(),
            )
        })?;
    let current_metadata = fs::symlink_metadata(path).map_err(|error| {
        Error::new(
            ErrorKind::Process,
            "codex_home.permissions",
            format!("cannot re-inspect {}: {error}", path.display()),
        )
    })?;
    verify_open_file_identity(
        path,
        &current_metadata,
        &directory,
        "codex_home.permissions",
    )
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_open_file_permissions(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| {
            Error::new(
                ErrorKind::Process,
                "codex_home.permissions",
                error.to_string(),
            )
        })
}

#[cfg(not(unix))]
fn set_private_open_file_permissions(_file: &File) -> Result<()> {
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_project_table_keys() {
        assert_eq!(escape_toml_string("a\\b\"c"), "a\\\\b\\\"c");
    }

    #[tokio::test]
    async fn rejects_foreign_config() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("config.toml"), "model = \"other\"\n")
            .expect("write config");
        let error = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect_err("foreign config must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.operation(), "codex_home.config");
        assert!(
            !directory.path().join(HOME_LOCK_FILE).exists(),
            "a foreign home must not receive a Vergerail ownership artifact"
        );
        assert!(
            !directory.path().join(HOME_MARKER_FILE).exists(),
            "a foreign home must not receive a Vergerail ownership marker"
        );
        assert!(
            !directory.path().join("vergerail-runtime-workdir").exists(),
            "a foreign home must not receive a Vergerail work directory"
        );
        assert!(
            !directory.path().join("vergerail-projects.json").exists(),
            "a foreign home must not receive Vergerail project state"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_nonempty_unmarked_codex_home_without_mutation() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755))
            .expect("foreign home permissions");
        let auth_path = directory.path().join("auth.json");
        fs::write(&auth_path, b"foreign credentials").expect("write foreign auth");

        let error = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect_err("an unrelated CODEX_HOME must not be adopted");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.operation(), "codex_home.ownership");
        assert_eq!(
            fs::read(&auth_path).expect("foreign auth remains"),
            b"foreign credentials"
        );
        assert_eq!(
            fs::metadata(directory.path())
                .expect("foreign home metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(!directory.path().join(HOME_MARKER_FILE).exists());
        assert!(!directory.path().join(HOME_LOCK_FILE).exists());
        assert!(!directory.path().join("config.toml").exists());
        assert!(!directory.path().join("vergerail-projects.json").exists());
        assert!(!directory.path().join("vergerail-runtime-workdir").exists());
    }

    #[tokio::test]
    async fn empty_home_receives_exact_ownership_marker() {
        let directory = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect("prepare empty home");
        drop(home);

        assert_eq!(
            fs::read(directory.path().join(HOME_MARKER_FILE)).expect("managed-home marker"),
            home_marker("vergerail").as_bytes()
        );
    }

    #[tokio::test]
    async fn marked_home_accepts_runtime_owned_files() {
        let directory = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect("prepare managed home");
        drop(home);
        fs::write(directory.path().join("auth.json"), b"runtime-owned")
            .expect("runtime-owned file");

        let reopened = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect("explicitly marked homes may contain app-server state");
        drop(reopened);
        assert_eq!(
            fs::read(directory.path().join("auth.json")).expect("runtime-owned file remains"),
            b"runtime-owned"
        );
    }

    #[tokio::test]
    async fn managed_home_rejects_a_different_consumer_owner() {
        let directory = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::prepare_for(directory.path().to_path_buf(), "vergerail".to_owned())
            .await
            .expect("first owner");
        drop(home);

        let error = ManagedHome::prepare_for(directory.path().to_path_buf(), "upagent".to_owned())
            .await
            .expect_err("a different application must not adopt the home");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.operation(), "codex_home.ownership");
        assert!(error.to_string().contains("different Vergerail consumer"));
        assert_eq!(
            fs::read(directory.path().join(HOME_MARKER_FILE)).expect("original marker remains"),
            home_marker("vergerail").as_bytes()
        );
    }

    #[tokio::test]
    async fn rejects_unmarked_managed_config_without_adoption() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            render_config(&BTreeSet::new(), false).expect("render managed config"),
        )
        .expect("write unmarked managed config");

        let error = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect_err("unmarked managed config must not be adopted");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.operation(), "codex_home.ownership");
        assert!(config_path.exists());
        assert!(!directory.path().join(HOME_MARKER_FILE).exists());
        assert!(!directory.path().join(HOME_LOCK_FILE).exists());
    }

    #[tokio::test]
    async fn stale_lock_only_home_remains_recoverable() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join(HOME_LOCK_FILE), b"").expect("stale lock file");

        let home = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect("an unlocked stale lock file is recoverable");
        drop(home);
        assert_eq!(
            fs::read(directory.path().join(HOME_MARKER_FILE)).expect("managed marker"),
            home_marker("vergerail").as_bytes()
        );
    }

    #[tokio::test]
    async fn rejects_unrecognized_ownership_marker() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join(HOME_MARKER_FILE), b"not Vergerail\n")
            .expect("write invalid marker");

        let error = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect_err("an invalid marker must not claim the home");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.operation(), "codex_home.ownership");
        assert!(!directory.path().join(HOME_LOCK_FILE).exists());
        assert!(!directory.path().join("config.toml").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinked_ownership_marker() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let target = directory.path().join("marker-target");
        fs::write(&target, home_marker("vergerail")).expect("marker target");
        symlink(&target, directory.path().join(HOME_MARKER_FILE)).expect("marker symlink");

        let error = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect_err("marker symlinks must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.operation(), "codex_home.ownership");
        assert!(!directory.path().join(HOME_LOCK_FILE).exists());
    }

    #[tokio::test]
    async fn rejects_concurrent_owners_of_the_same_managed_home() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect("first owner");
        let error = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect_err("second owner must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.operation(), "codex_home.lock");

        drop(first);
        ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect("ownership must be released with the final handle");
    }

    #[tokio::test]
    async fn managed_home_arc_retains_ownership_until_the_last_clone_drops() {
        let directory = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect("first owner");
        let retained = Arc::clone(&home);
        drop(home);

        let error = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect_err("a retained managed-home capability must retain the file lock");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.operation(), "codex_home.lock");

        drop(retained);
        ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect("the final owner drop must release the file lock");
    }

    #[tokio::test]
    async fn repairs_stale_vergerail_managed_config_from_authoritative_state() {
        let directory = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect("prepare");
        drop(home);
        fs::write(
            directory.path().join("config.toml"),
            format!("{MANAGED_HEADER}stale = true\n"),
        )
        .expect("write stale managed config");

        let repaired = ManagedHome::prepare(directory.path().to_path_buf())
            .await
            .expect("managed config must be repaired");
        drop(repaired);
        assert_eq!(
            fs::read_to_string(directory.path().join("config.toml")).expect("config"),
            render_config(&BTreeSet::new(), false).expect("render empty config")
        );
        assert_eq!(
            fs::read(directory.path().join(HOME_MARKER_FILE)).expect("managed marker"),
            home_marker("vergerail").as_bytes()
        );
    }

    #[tokio::test]
    async fn image_generation_is_explicit_and_survives_project_registration() {
        let home_directory = tempfile::tempdir().expect("home tempdir");
        let project_directory = tempfile::tempdir().expect("project tempdir");
        let home = ManagedHome::prepare_with_features(
            home_directory.path().to_path_buf(),
            "vergerail".to_owned(),
            true,
        )
        .await
        .expect("image-enabled managed home");

        assert!(
            fs::read_to_string(home_directory.path().join(CONFIG_FILE))
                .expect("enabled config")
                .contains("image_generation = true")
        );
        home.register_untrusted_project(project_directory.path())
            .await
            .expect("project registration");
        assert!(
            fs::read_to_string(home_directory.path().join(CONFIG_FILE))
                .expect("updated enabled config")
                .contains("image_generation = true")
        );

        drop(home);
        let disabled =
            ManagedHome::prepare_for(home_directory.path().to_path_buf(), "vergerail".to_owned())
                .await
                .expect("default-disabled managed home");
        drop(disabled);
        assert!(
            fs::read_to_string(home_directory.path().join(CONFIG_FILE))
                .expect("disabled config")
                .contains("image_generation = false")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_non_utf8_project_paths_at_the_protocol_boundary() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let home_directory = tempfile::tempdir().expect("home tempdir");
        let project_parent = tempfile::tempdir().expect("project parent");
        let project = project_parent
            .path()
            .join(OsString::from_vec(vec![b'p', 0xff]));
        if let Err(error) = fs::create_dir(&project) {
            // macOS filesystems reject byte sequences that are not valid UTF-8,
            // so this boundary case cannot be materialized on that host.
            if error.raw_os_error() == Some(92) {
                return;
            }
            panic!("non-UTF-8 project directory: {error}");
        }
        let home = ManagedHome::prepare(home_directory.path().to_path_buf())
            .await
            .expect("prepare");

        let error = home
            .register_untrusted_project(&project)
            .await
            .expect_err("non-UTF-8 paths cannot cross JSON/TOML string boundaries");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.operation(), "session.cwd");
    }

    #[tokio::test]
    async fn transient_project_validation_does_not_persist_trust_configuration() {
        let home_directory = tempfile::tempdir().expect("home tempdir");
        let project_directory = tempfile::tempdir().expect("project tempdir");
        let home = ManagedHome::prepare(home_directory.path().to_path_buf())
            .await
            .expect("prepare");

        let canonical = home
            .validate_transient_project(project_directory.path())
            .await
            .expect("validate transient project");

        assert_eq!(
            canonical,
            project_directory.path().canonicalize().expect("canonical")
        );
        let state: ProjectState = serde_json::from_reader(
            File::open(home_directory.path().join("vergerail-projects.json")).expect("state file"),
        )
        .expect("state JSON");
        assert!(state.projects.is_empty());
        let config = fs::read_to_string(home_directory.path().join("config.toml")).expect("config");
        assert!(!config.contains(&canonical.to_string_lossy().to_string()));
    }

    #[tokio::test]
    async fn reopening_a_home_prunes_missing_project_entries_atomically() {
        let home_directory = tempfile::tempdir().expect("home tempdir");
        let project_directory = tempfile::tempdir().expect("project tempdir");
        let canonical = project_directory.path().canonicalize().expect("canonical");
        let home = ManagedHome::prepare(home_directory.path().to_path_buf())
            .await
            .expect("prepare");
        home.register_untrusted_project(&canonical)
            .await
            .expect("register project");
        drop(home);
        project_directory.close().expect("remove project");

        let reopened = ManagedHome::prepare(home_directory.path().to_path_buf())
            .await
            .expect("reopen and prune");
        drop(reopened);

        let state: ProjectState = serde_json::from_reader(
            File::open(home_directory.path().join("vergerail-projects.json")).expect("state file"),
        )
        .expect("state JSON");
        assert!(state.projects.is_empty());
        assert_eq!(
            fs::read_to_string(home_directory.path().join("config.toml")).expect("config"),
            render_config(&BTreeSet::new(), false).expect("empty config")
        );
    }

    #[tokio::test]
    async fn detached_commit_cannot_be_overtaken_into_lost_state() {
        let home_directory = tempfile::tempdir().expect("home tempdir");
        let first_project = tempfile::tempdir().expect("first project");
        let second_project = tempfile::tempdir().expect("second project");
        let first = first_project
            .path()
            .canonicalize()
            .expect("first canonical");
        let second = second_project
            .path()
            .canonicalize()
            .expect("second canonical");
        let projects = Arc::new(Mutex::new(BTreeSet::new()));

        let first_guard = Arc::clone(&projects).lock_owned().await;
        let detached_root = home_directory.path().to_path_buf();
        let detached_first = first.clone();
        let detached = tokio::task::spawn_blocking(move || {
            commit_untrusted_project(&detached_root, first_guard, detached_first, false)
        });
        drop(detached);

        // Admission for the second transaction cannot complete until the
        // detached first transaction has committed both disk and memory state.
        let second_guard = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            Arc::clone(&projects).lock_owned(),
        )
        .await
        .expect("detached first commit must release admission");
        let second_root = home_directory.path().to_path_buf();
        let second_value = second.clone();
        tokio::task::spawn_blocking(move || {
            commit_untrusted_project(&second_root, second_guard, second_value, false)
        })
        .await
        .expect("second worker")
        .expect("second commit");

        let state: ProjectState = serde_json::from_reader(
            File::open(home_directory.path().join("vergerail-projects.json")).expect("state file"),
        )
        .expect("state JSON");
        assert_eq!(state.projects, BTreeSet::from([first, second]));
    }

    #[tokio::test]
    async fn failed_config_commit_does_not_update_in_memory_projects() {
        let home_directory = tempfile::tempdir().expect("home tempdir");
        let project_directory = tempfile::tempdir().expect("project tempdir");
        let home = ManagedHome::prepare(home_directory.path().to_path_buf())
            .await
            .expect("prepare");
        let config_path = home_directory.path().join("config.toml");
        fs::remove_file(&config_path).expect("remove config");
        fs::create_dir(&config_path).expect("block config replacement");

        let error = home
            .register_untrusted_project(project_directory.path())
            .await
            .expect_err("config commit must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        let state = fs::read_to_string(home_directory.path().join("vergerail-projects.json"))
            .expect("authoritative state");
        assert!(
            !state.contains(&project_directory.path().to_string_lossy().to_string()),
            "a failed config replacement must not commit the project"
        );

        fs::remove_dir(&config_path).expect("remove blocking directory");
        let canonical = home
            .register_untrusted_project(project_directory.path())
            .await
            .expect("retry must write state and config");
        let config = fs::read_to_string(&config_path).expect("config");
        assert!(config.contains(&escape_toml_string(&canonical.to_string_lossy())));
    }
}
