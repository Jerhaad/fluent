use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A Codex command selected once before an autonomous role reserves work.
#[derive(Clone, Debug)]
pub struct ResolvedCodexLauncher {
    executable: PathBuf,
    execution_executable: PathBuf,
    readable_roots: Vec<PathBuf>,
}

#[derive(Clone, Copy)]
struct CodexPlatformPackage {
    dependency: &'static str,
    directory: &'static str,
    target_triple: &'static str,
    version_suffix: &'static str,
    binary_name: &'static str,
}

#[derive(Debug)]
pub struct CodexLauncherError {
    message: String,
}

impl std::fmt::Display for CodexLauncherError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CodexLauncherError {}

#[derive(Debug)]
pub enum CodexWorkerPreparationError {
    Launcher(CodexLauncherError),
    Authentication(CodexAuthError),
    Configuration(crate::config::FollowUpConfigError),
    Skills(CodexSkillPreparationError),
}

impl std::fmt::Display for CodexWorkerPreparationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Launcher(error) => error.fmt(formatter),
            Self::Authentication(error) => error.fmt(formatter),
            Self::Configuration(error) => error.fmt(formatter),
            Self::Skills(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CodexWorkerPreparationError {}

impl From<CodexLauncherError> for CodexWorkerPreparationError {
    fn from(error: CodexLauncherError) -> Self {
        Self::Launcher(error)
    }
}

impl From<CodexAuthError> for CodexWorkerPreparationError {
    fn from(error: CodexAuthError) -> Self {
        Self::Authentication(error)
    }
}

impl From<crate::config::FollowUpConfigError> for CodexWorkerPreparationError {
    fn from(error: crate::config::FollowUpConfigError) -> Self {
        Self::Configuration(error)
    }
}

impl From<CodexSkillPreparationError> for CodexWorkerPreparationError {
    fn from(error: CodexSkillPreparationError) -> Self {
        Self::Skills(error)
    }
}

impl ResolvedCodexLauncher {
    pub fn resolve() -> std::result::Result<Self, CodexLauncherError> {
        let path = env::var_os("PATH").ok_or_else(|| CodexLauncherError {
            message: "cannot resolve `codex` from PATH: PATH is not set".to_string(),
        })?;
        Self::resolve_from_paths(env::split_paths(&path))
    }

    fn resolve_from_path(path: &Path) -> std::result::Result<Self, CodexLauncherError> {
        Self::resolve_from_paths(std::iter::once(path.to_path_buf()))
    }

    fn resolve_from_paths(
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> std::result::Result<Self, CodexLauncherError> {
        for directory in paths {
            let absolute_directory = if directory.is_absolute() {
                directory
            } else {
                env::current_dir()
                    .map_err(|error| CodexLauncherError {
                        message: format!(
                            "cannot resolve `codex` from PATH: cannot resolve current directory: {error}"
                        ),
                    })?
                    .join(directory)
            };
            let candidate = absolute_directory.join("codex");
            if !candidate.is_file() || !is_executable(&candidate) {
                continue;
            }

            let parent = candidate.parent().ok_or_else(|| CodexLauncherError {
                message: format!(
                    "cannot resolve Codex launcher {} safely: it has no parent",
                    candidate.display()
                ),
            })?;
            let lexical_parent = fs::canonicalize(parent).map_err(|error| CodexLauncherError {
                message: format!(
                    "cannot resolve Codex launcher parent {}: {error}",
                    parent.display()
                ),
            })?;
            let file_name = candidate.file_name().ok_or_else(|| CodexLauncherError {
                message: format!(
                    "cannot resolve Codex launcher {} safely: it has no file name",
                    candidate.display()
                ),
            })?;
            let executable = lexical_parent.join(file_name);
            let canonical_target =
                fs::canonicalize(&executable).map_err(|error| CodexLauncherError {
                    message: format!(
                        "cannot resolve Codex launcher target {}: {error}",
                        executable.display()
                    ),
                })?;
            if !canonical_target.is_file() {
                return Err(CodexLauncherError {
                    message: format!(
                        "cannot resolve Codex launcher {} safely: target {} is not a file",
                        executable.display(),
                        canonical_target.display()
                    ),
                });
            }

            let package_root = canonical_package_root(&canonical_target)?;
            let mut readable_roots = Vec::new();
            if canonical_target != executable {
                readable_roots.push(executable.clone());
                readable_roots.push(match package_root.as_ref() {
                    Some(package_root) => package_root.clone(),
                    None => canonical_target.clone(),
                });
            } else if let Some(package_root) = package_root.as_ref() {
                readable_roots.push(package_root.clone());
            } else {
                readable_roots.push(executable.clone());
            }
            let execution_executable = match package_root.as_ref() {
                Some(package_root) if canonical_target == package_root.join("bin/codex.js") => {
                    let native = resolve_packaged_native(package_root)?;
                    readable_roots.push(native.readable_root);
                    native.executable
                }
                _ => executable.clone(),
            };
            readable_roots.sort();
            readable_roots.dedup();
            return Ok(Self {
                executable,
                execution_executable,
                readable_roots,
            });
        }

        Err(CodexLauncherError {
            message: "cannot resolve `codex` from PATH: no executable command was found"
                .to_string(),
        })
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// The verified executable used for autonomous model work.
    ///
    /// Official npm installations expose a Node shim on `PATH`. Fluent keeps
    /// that selected command for readiness checks, but launches the matching
    /// packaged native binary for autonomous work so a redirected `HOME` does
    /// not add the shim's extra process boundary inside Seatbelt.
    pub fn execution_executable(&self) -> &Path {
        &self.execution_executable
    }

    pub fn readable_roots(&self) -> &[PathBuf] {
        &self.readable_roots
    }
}

struct ResolvedPackagedNative {
    executable: PathBuf,
    readable_root: PathBuf,
}

fn resolve_packaged_native(
    package_root: &Path,
) -> std::result::Result<ResolvedPackagedNative, CodexLauncherError> {
    let platform = codex_platform_package().ok_or_else(|| CodexLauncherError {
        message: "cannot resolve a native Codex executable for this platform".to_string(),
    })?;
    let core_manifest_path = package_root.join("package.json");
    let core_manifest = read_package_manifest(&core_manifest_path)?;
    let core_version = core_manifest
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CodexLauncherError {
            message: format!(
                "Codex package manifest {} has no version",
                core_manifest_path.display()
            ),
        })?;
    let expected_version = format!("{core_version}-{}", platform.version_suffix);
    let expected_dependency = format!("npm:@openai/codex@{expected_version}");
    let dependency = core_manifest
        .get("optionalDependencies")
        .and_then(|value| value.get(platform.dependency))
        .and_then(serde_json::Value::as_str);

    let package_candidates = [
        package_root
            .join("node_modules/@openai")
            .join(platform.directory),
        package_root
            .parent()
            .map(|parent| parent.join(platform.directory))
            .unwrap_or_default(),
    ];
    for candidate in package_candidates {
        let manifest_path = candidate.join("package.json");
        if !manifest_path.is_file() {
            continue;
        }
        if dependency != Some(expected_dependency.as_str()) {
            return Err(CodexLauncherError {
                message: format!(
                    "Codex package manifest {} does not pin {} to the expected native version",
                    core_manifest_path.display(),
                    platform.dependency
                ),
            });
        }
        let manifest = read_package_manifest(&manifest_path)?;
        if manifest.get("name").and_then(serde_json::Value::as_str) != Some("@openai/codex")
            || manifest.get("version").and_then(serde_json::Value::as_str)
                != Some(expected_version.as_str())
        {
            return Err(CodexLauncherError {
                message: format!(
                    "native Codex package {} has an unexpected identity or version",
                    candidate.display()
                ),
            });
        }
        let readable_root = fs::canonicalize(&candidate).map_err(|error| CodexLauncherError {
            message: format!(
                "cannot resolve native Codex package {}: {error}",
                candidate.display()
            ),
        })?;
        let executable = readable_root
            .join("vendor")
            .join(platform.target_triple)
            .join("bin")
            .join(platform.binary_name);
        let executable = resolve_native_executable(&executable, &readable_root)?;
        return Ok(ResolvedPackagedNative {
            executable,
            readable_root,
        });
    }

    let bundled = package_root
        .join("vendor")
        .join(platform.target_triple)
        .join("bin")
        .join(platform.binary_name);
    if bundled.is_file() {
        let executable = resolve_native_executable(&bundled, package_root)?;
        return Ok(ResolvedPackagedNative {
            executable,
            readable_root: package_root.to_path_buf(),
        });
    }

    Err(CodexLauncherError {
        message: format!(
            "cannot resolve the native Codex executable paired with {}",
            package_root.display()
        ),
    })
}

fn read_package_manifest(
    path: &Path,
) -> std::result::Result<serde_json::Value, CodexLauncherError> {
    serde_json::from_slice(&fs::read(path).map_err(|error| CodexLauncherError {
        message: format!(
            "cannot read Codex package manifest {}: {error}",
            path.display()
        ),
    })?)
    .map_err(|error| CodexLauncherError {
        message: format!(
            "cannot parse Codex package manifest {}: {error}",
            path.display()
        ),
    })
}

fn resolve_native_executable(
    executable: &Path,
    package_root: &Path,
) -> std::result::Result<PathBuf, CodexLauncherError> {
    let canonical = fs::canonicalize(executable).map_err(|error| CodexLauncherError {
        message: format!(
            "cannot resolve native Codex executable {}: {error}",
            executable.display()
        ),
    })?;
    if !canonical.starts_with(package_root) || !canonical.is_file() || !is_executable(&canonical) {
        return Err(CodexLauncherError {
            message: format!(
                "native Codex executable {} is missing, outside its package, or not executable",
                executable.display()
            ),
        });
    }
    Ok(canonical)
}

fn codex_platform_package() -> Option<CodexPlatformPackage> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Some(CodexPlatformPackage {
        dependency: "@openai/codex-darwin-arm64",
        directory: "codex-darwin-arm64",
        target_triple: "aarch64-apple-darwin",
        version_suffix: "darwin-arm64",
        binary_name: "codex",
    });
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return Some(CodexPlatformPackage {
        dependency: "@openai/codex-darwin-x64",
        directory: "codex-darwin-x64",
        target_triple: "x86_64-apple-darwin",
        version_suffix: "darwin-x64",
        binary_name: "codex",
    });
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return Some(CodexPlatformPackage {
        dependency: "@openai/codex-linux-arm64",
        directory: "codex-linux-arm64",
        target_triple: "aarch64-unknown-linux-musl",
        version_suffix: "linux-arm64",
        binary_name: "codex",
    });
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return Some(CodexPlatformPackage {
        dependency: "@openai/codex-linux-x64",
        directory: "codex-linux-x64",
        target_triple: "x86_64-unknown-linux-musl",
        version_suffix: "linux-x64",
        binary_name: "codex",
    });
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    return Some(CodexPlatformPackage {
        dependency: "@openai/codex-win32-arm64",
        directory: "codex-win32-arm64",
        target_triple: "aarch64-pc-windows-msvc",
        version_suffix: "win32-arm64",
        binary_name: "codex.exe",
    });
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return Some(CodexPlatformPackage {
        dependency: "@openai/codex-win32-x64",
        directory: "codex-win32-x64",
        target_triple: "x86_64-pc-windows-msvc",
        version_suffix: "win32-x64",
        binary_name: "codex.exe",
    });
    #[allow(unreachable_code)]
    None
}

fn canonical_package_root(
    target: &Path,
) -> std::result::Result<Option<PathBuf>, CodexLauncherError> {
    let target_parent = target.parent().ok_or_else(|| CodexLauncherError {
        message: format!(
            "cannot derive a readable closure for Codex launcher target {}",
            target.display()
        ),
    })?;
    for ancestor in target_parent.ancestors().take(8) {
        let manifest = ancestor.join("package.json");
        if manifest.is_file() && has_codex_package_layout(ancestor) {
            validate_codex_package_root(ancestor, &manifest)?;
            return fs::canonicalize(ancestor)
                .map(Some)
                .map_err(|error| CodexLauncherError {
                    message: format!(
                        "cannot resolve Codex package root {}: {error}",
                        ancestor.display()
                    ),
                });
        }
    }
    Ok(None)
}

fn has_codex_package_layout(package_root: &Path) -> bool {
    package_root.file_name().and_then(|name| name.to_str()) == Some("codex")
        && package_root
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("@openai")
        && package_root
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("node_modules")
}

fn validate_codex_package_root(
    package_root: &Path,
    manifest: &Path,
) -> std::result::Result<(), CodexLauncherError> {
    let package: serde_json::Value =
        serde_json::from_slice(&fs::read(manifest).map_err(|error| CodexLauncherError {
            message: format!(
                "cannot read Codex package manifest {}: {error}",
                manifest.display()
            ),
        })?)
        .map_err(|error| CodexLauncherError {
            message: format!(
                "cannot parse Codex package manifest {}: {error}",
                manifest.display()
            ),
        })?;
    if !has_codex_package_layout(package_root)
        || package.get("name").and_then(serde_json::Value::as_str) != Some("@openai/codex")
    {
        return Err(CodexLauncherError {
            message: format!(
                "cannot derive a bounded readable closure from unrecognized Codex package root {}",
                package_root.display()
            ),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Authentication failures that a user can recover by logging Codex in again.
#[derive(Debug)]
pub struct CodexAuthError {
    message: String,
}

impl std::fmt::Display for CodexAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CodexAuthError {}

impl CodexAuthError {
    fn new(reason: impl std::fmt::Display) -> Self {
        Self {
            message: format!(
                "Codex authentication is unavailable: {reason}. Run `codex login`, then resume with `fluent attempt run`."
            ),
        }
    }
}

/// A configured Codex skill root could not be copied into the private worker
/// home without weakening its filesystem boundary.
#[derive(Debug)]
pub struct CodexSkillPreparationError {
    message: String,
}

impl std::fmt::Display for CodexSkillPreparationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Codex skills are unavailable: {}", self.message)
    }
}

impl std::error::Error for CodexSkillPreparationError {}

/// Private Codex state for one autonomous launch.
///
/// The temporary directory removes the copied authentication state when this
/// value drops. From the interactive home, only `auth.json` is staged;
/// configuration, hooks, sessions, logs, cache, and unconfigured skills remain
/// outside the worker. Explicitly configured skill roots are copied separately.
pub struct CodexWorkerEnvironment {
    home_guard: tempfile::TempDir,
    home: PathBuf,
    launcher: ResolvedCodexLauncher,
}

impl CodexWorkerEnvironment {
    /// Prepare a private worker home from the effective source `CODEX_HOME`.
    pub fn prepare() -> std::result::Result<Self, CodexWorkerPreparationError> {
        Self::prepare_with_skill_roots(&[])
    }

    /// Prepare a worker with the exact skill-root snapshot configured for one
    /// project. Configuration and copying finish before any autonomous role is
    /// reserved.
    pub fn prepare_for_project(
        project_root: &Path,
    ) -> std::result::Result<Self, CodexWorkerPreparationError> {
        let config = crate::config::resolve_codex_worker_config(project_root)?;
        Self::prepare_with_skill_roots(&config.skill_roots)
    }

    fn prepare_with_skill_roots(
        skill_roots: &[PathBuf],
    ) -> std::result::Result<Self, CodexWorkerPreparationError> {
        let launcher = test_or_resolved_launcher()?;
        #[cfg(feature = "test-support")]
        if env::var_os("FLUENT_TEST_HERMETIC_PROVIDERS").is_some() {
            if let Some(source_home) = env::var_os("CODEX_HOME") {
                return Self::prepare_from_with_environment_auth_and_skills(
                    &canonical_existing_path(PathBuf::from(source_home)),
                    has_environment_auth(),
                    launcher,
                    skill_roots,
                );
            }
            if let Some(source_home) = env::var_os("FLUENT_TEST_FIXTURE_CODEX_HOME") {
                return Self::prepare_from_with_environment_auth_and_skills(
                    &canonical_existing_path(PathBuf::from(source_home)),
                    has_environment_auth(),
                    launcher,
                    skill_roots,
                );
            }
            if has_environment_auth() {
                return Self::prepare_hermetic_fixture_worker_with_environment_auth(
                    launcher,
                    skill_roots,
                );
            }
            return Self::prepare_hermetic_fixture_worker(launcher, skill_roots);
        }

        #[cfg(test)]
        {
            return Self::prepare_test_worker(launcher, skill_roots);
        }

        #[cfg(not(test))]
        Self::prepare_from_with_environment_auth_and_skills(
            &effective_source_home(),
            has_environment_auth(),
            launcher,
            skill_roots,
        )
    }

    /// Give launch-route unit tests a private, authenticated worker home without
    /// reading the developer's interactive Codex state. External tests exercise
    /// the public production entry point with an explicit authentication source.
    #[cfg(test)]
    fn prepare_test_worker(
        launcher: ResolvedCodexLauncher,
        skill_roots: &[PathBuf],
    ) -> std::result::Result<Self, CodexWorkerPreparationError> {
        Self::prepare_hermetic_fixture_worker(launcher, skill_roots)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn prepare_hermetic_fixture_worker(
        launcher: ResolvedCodexLauncher,
        skill_roots: &[PathBuf],
    ) -> std::result::Result<Self, CodexWorkerPreparationError> {
        let source = tempfile::tempdir().map_err(|error| {
            CodexAuthError::new(format!("cannot create test authentication source: {error}"))
        })?;
        fs::write(source.path().join("auth.json"), "test authentication").map_err(|error| {
            CodexAuthError::new(format!("cannot write test authentication source: {error}"))
        })?;
        Self::prepare_from_with_environment_auth_and_skills(
            source.path(),
            false,
            launcher,
            skill_roots,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    fn prepare_hermetic_fixture_worker_with_environment_auth(
        launcher: ResolvedCodexLauncher,
        skill_roots: &[PathBuf],
    ) -> std::result::Result<Self, CodexWorkerPreparationError> {
        let source = tempfile::tempdir().map_err(|error| {
            CodexAuthError::new(format!("cannot create test authentication source: {error}"))
        })?;
        Self::prepare_from_with_environment_auth_and_skills(
            source.path(),
            true,
            launcher,
            skill_roots,
        )
    }

    #[cfg(test)]
    fn prepare_from(source_home: &Path) -> std::result::Result<Self, CodexWorkerPreparationError> {
        Self::prepare_from_with_environment_auth(source_home, false, test_launcher())
    }

    #[cfg(test)]
    fn prepare_from_with_environment_auth(
        source_home: &Path,
        environment_auth: bool,
        launcher: ResolvedCodexLauncher,
    ) -> std::result::Result<Self, CodexWorkerPreparationError> {
        Self::prepare_from_with_environment_auth_and_skills(
            source_home,
            environment_auth,
            launcher,
            &[],
        )
    }

    fn prepare_from_with_environment_auth_and_skills(
        source_home: &Path,
        environment_auth: bool,
        launcher: ResolvedCodexLauncher,
        skill_roots: &[PathBuf],
    ) -> std::result::Result<Self, CodexWorkerPreparationError> {
        Self::prepare_from_with_environment_auth_in(
            source_home,
            environment_auth,
            None,
            launcher,
            skill_roots,
        )
    }

    fn prepare_from_with_environment_auth_in(
        source_home: &Path,
        environment_auth: bool,
        temporary_root: Option<&Path>,
        launcher: ResolvedCodexLauncher,
        skill_roots: &[PathBuf],
    ) -> std::result::Result<Self, CodexWorkerPreparationError> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("fluent-codex-worker-");
        let home_guard = match temporary_root {
            Some(root) => builder.tempdir_in(root),
            None => builder.tempdir(),
        }
        .map_err(|error| {
            CodexAuthError::new(format!("cannot create a private worker home: {error}"))
        })?;
        set_private_mode(home_guard.path(), 0o700).map_err(|error| {
            CodexAuthError::new(format!("cannot secure the worker home: {error}"))
        })?;
        let home = fs::canonicalize(home_guard.path()).map_err(|error| {
            CodexAuthError::new(format!("cannot resolve the worker home: {error}"))
        })?;

        if !environment_auth {
            let source_auth = source_home.join("auth.json");
            let worker_auth = home.join("auth.json");
            fs::copy(&source_auth, &worker_auth).map_err(|error| {
                CodexAuthError::new(format!(
                    "cannot copy authentication from {}: {error}",
                    source_auth.display()
                ))
            })?;
            set_private_mode(&worker_auth, 0o600).map_err(|error| {
                CodexAuthError::new(format!("cannot secure copied authentication: {error}"))
            })?;
        }

        stage_skill_roots(&home, skill_roots)?;

        Ok(Self {
            home_guard,
            home,
            launcher,
        })
    }

    /// The only Codex state directory an autonomous launch may use.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Add this worker's home to a per-launch command environment.
    pub fn launch_env(&self) -> (String, String) {
        (
            "CODEX_HOME".to_string(),
            self.home.to_string_lossy().to_string(),
        )
    }

    pub fn launcher(&self) -> &ResolvedCodexLauncher {
        &self.launcher
    }

    /// Persist only Codex rollout files needed to resume a Writer session.
    pub fn snapshot_sessions_to(&self, destination: &Path) -> Result<()> {
        let source = self.home.join("sessions");
        if !source.is_dir() {
            anyhow::bail!(
                "Codex worker produced no resumable session directory at {}",
                source.display()
            );
        }
        copy_private_tree_atomic(&source, destination).with_context(|| {
            format!(
                "snapshot Codex Writer sessions from {} to {}",
                source.display(),
                destination.display()
            )
        })
    }

    /// Restore a prior Writer's rollout files into this launch's private home.
    pub fn restore_sessions_from(&self, source: &Path) -> Result<()> {
        let destination = self.home.join("sessions");
        copy_private_tree_atomic(source, &destination).with_context(|| {
            format!(
                "restore Codex Writer sessions from {} to {}",
                source.display(),
                destination.display()
            )
        })
    }

    pub fn has_sessions(&self) -> bool {
        self.home.join("sessions").is_dir()
    }

    /// Ask Codex itself whether this worker home is authenticated.
    pub fn preflight(&self) -> std::result::Result<(), CodexAuthError> {
        #[cfg(test)]
        {
            // Route unit tests inject a recording coder, not a Codex CLI. Keep
            // their focus on launch wiring; `preflight_with` tests the CLI
            // contract directly and external tests cover the production route.
            return Ok(());
        }

        #[cfg(not(test))]
        self.preflight_with(self.launcher.executable())
    }

    fn preflight_with(&self, binary: &Path) -> std::result::Result<(), CodexAuthError> {
        let status = Command::new(binary)
            // The private worker home contains authentication only, so login
            // status cannot load interactive configuration or hooks. Keep
            // exec-only flags off this command: Codex does not accept
            // `--ignore-user-config` at the top-level login boundary.
            .args(["login", "status"])
            .env("CODEX_HOME", self.home())
            .status()
            .map_err(|error| {
                CodexAuthError::new(format!("cannot run `codex login status`: {error}"))
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(CodexAuthError::new(
                "`codex login status` reports no valid login",
            ))
        }
    }
}

fn copy_private_tree_atomic(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        anyhow::bail!(
            "private tree destination {} already exists",
            destination.display()
        );
    }
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("private tree destination has no parent"))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create private tree parent {}", parent.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".fluent-codex-session-")
        .tempdir_in(parent)
        .with_context(|| format!("stage private tree beside {}", destination.display()))?;
    let staged_tree = staging.path().join("sessions");
    copy_private_tree(source, &staged_tree)?;
    fs::rename(&staged_tree, destination).with_context(|| {
        format!(
            "publish private tree {} to {}",
            staged_tree.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn copy_private_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspect private tree {}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "private tree source {} is not a directory",
            source.display()
        );
    }
    fs::create_dir_all(destination)
        .with_context(|| format!("create private directory {}", destination.display()))?;
    set_private_mode(destination, 0o700)?;
    for entry in fs::read_dir(source)
        .with_context(|| format!("read private directory {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            anyhow::bail!(
                "private tree {} contains unsupported symlink {}",
                source.display(),
                source_path.display()
            );
        }
        if file_type.is_dir() {
            copy_private_tree(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path).with_context(|| {
                format!(
                    "copy private file {} to {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
            set_private_mode(&destination_path, 0o600)?;
        } else {
            anyhow::bail!(
                "private tree {} contains unsupported entry {}",
                source.display(),
                source_path.display()
            );
        }
    }
    Ok(())
}

fn stage_skill_roots(
    worker_home: &Path,
    skill_roots: &[PathBuf],
) -> std::result::Result<(), CodexSkillPreparationError> {
    if skill_roots.is_empty() {
        return Ok(());
    }
    let destination_root = worker_home.join("skills");
    fs::create_dir(&destination_root).map_err(|error| CodexSkillPreparationError {
        message: format!(
            "cannot create private skill directory {}: {error}",
            destination_root.display()
        ),
    })?;
    set_private_mode(&destination_root, 0o700).map_err(|error| CodexSkillPreparationError {
        message: format!("cannot secure private skill directory: {error:#}"),
    })?;

    for root in skill_roots {
        let metadata = fs::symlink_metadata(root).map_err(|error| CodexSkillPreparationError {
            message: format!("cannot inspect configured root {}: {error}", root.display()),
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(CodexSkillPreparationError {
                message: format!(
                    "configured root {} must be a real directory, not a symlink",
                    root.display()
                ),
            });
        }
        let mut entries = fs::read_dir(root)
            .map_err(|error| CodexSkillPreparationError {
                message: format!("cannot read configured root {}: {error}", root.display()),
            })?
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| CodexSkillPreparationError {
                message: format!(
                    "cannot enumerate configured root {}: {error}",
                    root.display()
                ),
            })?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let source = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| CodexSkillPreparationError {
                    message: format!("cannot inspect {}: {error}", source.display()),
                })?;
            if file_type.is_symlink() {
                return Err(CodexSkillPreparationError {
                    message: format!(
                        "configured root {} contains unsupported symlink {}",
                        root.display(),
                        source.display()
                    ),
                });
            }
            if !file_type.is_dir() {
                continue;
            }
            let destination = destination_root.join(entry.file_name());
            if destination.exists() {
                return Err(CodexSkillPreparationError {
                    message: format!(
                        "skill name {:?} appears in more than one configured root",
                        entry.file_name()
                    ),
                });
            }
            copy_private_tree(&source, &destination).map_err(|error| {
                CodexSkillPreparationError {
                    message: format!(
                        "cannot stage configured skill directory {}: {error:#}",
                        source.display()
                    ),
                }
            })?;
        }
    }
    Ok(())
}

fn test_or_resolved_launcher() -> std::result::Result<ResolvedCodexLauncher, CodexLauncherError> {
    #[cfg(test)]
    {
        Ok(test_launcher())
    }
    #[cfg(not(test))]
    {
        ResolvedCodexLauncher::resolve()
    }
}

#[cfg(test)]
fn test_launcher() -> ResolvedCodexLauncher {
    let executable = env::current_exe().expect("test executable path");
    ResolvedCodexLauncher {
        readable_roots: vec![executable.clone()],
        execution_executable: executable.clone(),
        executable,
    }
}

/// Return the interactive Codex home that supplies autonomous authentication.
pub fn effective_source_home() -> PathBuf {
    effective_source_home_from(
        env::var_os("CODEX_HOME").map(PathBuf::from),
        env::var_os("HOME").map(PathBuf::from),
    )
}

fn effective_source_home_from(codex_home: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    canonical_existing_path(
        codex_home
            .or_else(|| home.map(|home| home.join(".codex")))
            .unwrap_or_else(|| PathBuf::from(".codex")),
    )
}

fn canonical_existing_path(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn has_environment_auth() -> bool {
    [
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "CODEX_ACCESS_TOKEN",
        "CODEX_AUTH_JSON",
    ]
    .iter()
    .any(|name| env::var_os(name).is_some_and(|value| !value.is_empty()))
}

#[cfg(unix)]
fn set_private_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("set permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn install_packaged_wrapper(
        fixture: &tempfile::TempDir,
        native_version: Option<&str>,
    ) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let platform = codex_platform_package().expect("supported test platform");
        let bin = fixture.path().join("bin");
        let package = fixture.path().join("lib/node_modules/@openai/codex");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(package.join("bin")).unwrap();
        fs::write(
            package.join("package.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "@openai/codex",
                "version": "1.2.3",
                "optionalDependencies": {
                    platform.dependency: format!(
                        "npm:@openai/codex@1.2.3-{}",
                        platform.version_suffix
                    )
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let target = package.join("bin/codex.js");
        fs::write(&target, "#!/usr/bin/env node\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let lexical = bin.join("codex");
        symlink(&target, &lexical).unwrap();

        let native_package = package
            .join("node_modules/@openai")
            .join(platform.directory);
        let native = native_package
            .join("vendor")
            .join(platform.target_triple)
            .join("bin")
            .join(platform.binary_name);
        if let Some(version) = native_version {
            fs::create_dir_all(native.parent().unwrap()).unwrap();
            fs::write(
                native_package.join("package.json"),
                serde_json::to_vec(&serde_json::json!({
                    "name": "@openai/codex",
                    "version": version
                }))
                .unwrap(),
            )
            .unwrap();
            fs::write(&native, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&native, fs::Permissions::from_mode(0o700)).unwrap();
        }
        (bin, lexical, package, native)
    }

    #[cfg(unix)]
    #[test]
    fn resolved_launcher_retains_wrapper_and_uses_verified_native_for_execution() {
        let fixture = tempfile::tempdir().unwrap();
        let (bin, _lexical, package, native) =
            install_packaged_wrapper(&fixture, Some("1.2.3-test-platform"));
        let platform = codex_platform_package().unwrap();
        let expected_version = format!("1.2.3-{}", platform.version_suffix);
        fs::write(
            package
                .join("node_modules/@openai")
                .join(platform.directory)
                .join("package.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "@openai/codex",
                "version": expected_version
            }))
            .unwrap(),
        )
        .unwrap();

        let launcher = ResolvedCodexLauncher::resolve_from_path(&bin).unwrap();

        let lexical = fs::canonicalize(&bin).unwrap().join("codex");
        assert_eq!(launcher.executable(), lexical);
        assert_eq!(
            launcher.execution_executable(),
            fs::canonicalize(native).unwrap()
        );
        assert!(launcher.readable_roots().contains(&lexical));
        assert!(
            launcher
                .readable_roots()
                .contains(&fs::canonicalize(package).unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolved_packaged_wrapper_rejects_missing_native_runtime() {
        let fixture = tempfile::tempdir().unwrap();
        let (bin, _, _, _) = install_packaged_wrapper(&fixture, None);

        let error = ResolvedCodexLauncher::resolve_from_path(&bin).unwrap_err();

        assert!(
            error.to_string().contains("native Codex executable"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolved_packaged_wrapper_rejects_mismatched_native_version() {
        let fixture = tempfile::tempdir().unwrap();
        let (bin, _, _, _) = install_packaged_wrapper(&fixture, Some("9.9.9-wrong"));

        let error = ResolvedCodexLauncher::resolve_from_path(&bin).unwrap_err();

        assert!(error.to_string().contains("version"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn resolved_packaged_wrapper_rejects_native_symlink_escape() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let fixture = tempfile::tempdir().unwrap();
        let platform = codex_platform_package().unwrap();
        let expected_version = format!("1.2.3-{}", platform.version_suffix);
        let (bin, _, _, native) = install_packaged_wrapper(&fixture, Some(&expected_version));
        fs::remove_file(&native).unwrap();
        let outside = fixture.path().join("outside-native");
        fs::write(&outside, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&outside, &native).unwrap();

        let error = ResolvedCodexLauncher::resolve_from_path(&bin).unwrap_err();

        assert!(error.to_string().contains("outside its package"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn resolved_direct_packaged_launcher_retains_package_root() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().unwrap();
        let package = fixture.path().join("lib/node_modules/@openai/codex");
        let bin = package.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"@openai/codex"}"#).unwrap();
        let executable = bin.join("codex");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

        let launcher = ResolvedCodexLauncher::resolve_from_path(&bin).unwrap();

        let executable = fs::canonicalize(executable).unwrap();
        assert_eq!(launcher.executable(), executable);
        assert_eq!(launcher.execution_executable(), executable);
        assert_eq!(
            launcher.readable_roots(),
            &[fs::canonicalize(package).unwrap()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolved_direct_standalone_launcher_retains_only_executable() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().unwrap();
        let bin = fixture.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("codex");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

        let launcher = ResolvedCodexLauncher::resolve_from_path(&bin).unwrap();

        let executable = fs::canonicalize(executable).unwrap();
        assert_eq!(launcher.executable(), executable);
        assert_eq!(launcher.readable_roots(), &[executable]);
    }

    #[cfg(unix)]
    #[test]
    fn resolved_symlinked_standalone_launcher_retains_only_launcher_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let fixture = tempfile::tempdir().unwrap();
        let bin = fixture.path().join("bin");
        let standalone = fixture.path().join("standalone");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&standalone).unwrap();
        let target = standalone.join("codex-runtime");
        fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let lexical = bin.join("codex");
        symlink(&target, &lexical).unwrap();

        let launcher = ResolvedCodexLauncher::resolve_from_path(&bin).unwrap();

        let lexical = fs::canonicalize(&bin).unwrap().join("codex");
        let target = fs::canonicalize(target).unwrap();
        assert_eq!(launcher.executable(), lexical);
        assert_eq!(launcher.readable_roots(), &[lexical, target]);
    }

    #[cfg(unix)]
    #[test]
    fn resolved_launcher_does_not_grant_unrecognized_package_ancestor() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let fixture = tempfile::tempdir().unwrap();
        let operator_home = fixture.path().join("operator-home");
        let bin = operator_home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(operator_home.join("package.json"), "{}").unwrap();
        let target = operator_home.join("codex");
        fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let lexical = bin.join("codex");
        symlink(&target, &lexical).unwrap();

        let launcher = ResolvedCodexLauncher::resolve_from_path(&bin).unwrap();

        let lexical = fs::canonicalize(&bin).unwrap().join("codex");
        let target = fs::canonicalize(target).unwrap();
        assert_eq!(launcher.readable_roots(), &[lexical, target]);
        assert!(!launcher.readable_roots().contains(&operator_home));
    }

    #[cfg(unix)]
    #[test]
    fn resolved_launcher_rejects_unrecognized_codex_package_identity() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let fixture = tempfile::tempdir().unwrap();
        let bin = fixture.path().join("bin");
        let package = fixture.path().join("node_modules/@openai/codex");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(package.join("bin")).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"other"}"#).unwrap();
        let target = package.join("bin/codex");
        fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, bin.join("codex")).unwrap();

        let error = ResolvedCodexLauncher::resolve_from_path(&bin).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unrecognized Codex package root"),
            "{error}"
        );
    }

    #[test]
    fn resolved_launcher_rejects_a_missing_codex_command() {
        let empty = tempfile::tempdir().unwrap();

        let error = ResolvedCodexLauncher::resolve_from_path(empty.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("cannot resolve `codex` from PATH"),
            "{error}"
        );
    }

    #[test]
    fn worker_home_stages_only_auth_from_effective_codex_home() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("auth.json"), "auth").unwrap();
        fs::write(source.path().join("config.toml"), "hooks = true").unwrap();
        fs::create_dir(source.path().join("sessions")).unwrap();

        let worker = CodexWorkerEnvironment::prepare_from(source.path()).unwrap();

        assert_eq!(
            fs::read_to_string(worker.home().join("auth.json")).unwrap(),
            "auth"
        );
        assert!(!worker.home().join("config.toml").exists());
        assert!(!worker.home().join("sessions").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(worker.home()).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(worker.home().join("auth.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn configured_skills_are_snapshotted_into_the_private_codex_home() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("auth.json"), "auth").unwrap();
        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join("project-review");
        fs::create_dir(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: project-review\ndescription: Review this project.\n---\n",
        )
        .unwrap();
        fs::write(root.path().join("README.md"), "not a skill directory").unwrap();

        let worker = CodexWorkerEnvironment::prepare_from_with_environment_auth_and_skills(
            source.path(),
            false,
            test_launcher(),
            &[root.path().to_path_buf()],
        )
        .unwrap();
        fs::write(skill.join("SKILL.md"), "changed after preparation").unwrap();

        assert!(
            worker
                .home()
                .join("skills/project-review/SKILL.md")
                .is_file()
        );
        assert!(
            fs::read_to_string(worker.home().join("skills/project-review/SKILL.md"))
                .unwrap()
                .contains("Review this project")
        );
        assert!(!worker.home().join("skills/README.md").exists());
    }

    #[test]
    fn duplicate_configured_skill_names_fail_closed() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("auth.json"), "auth").unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for root in [first.path(), second.path()] {
            let skill = root.join("same-name");
            fs::create_dir(&skill).unwrap();
            fs::write(skill.join("SKILL.md"), "---\nname: same-name\n---\n").unwrap();
        }

        let error = CodexWorkerEnvironment::prepare_from_with_environment_auth_and_skills(
            source.path(),
            false,
            test_launcher(),
            &[first.path().to_path_buf(), second.path().to_path_buf()],
        )
        .err()
        .expect("duplicate skill names must fail preparation");

        assert!(error.to_string().contains("appears in more than one"));
    }

    #[cfg(unix)]
    #[test]
    fn configured_skill_tree_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("auth.json"), "auth").unwrap();
        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join("unsafe-skill");
        fs::create_dir(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "---\nname: unsafe-skill\n---\n").unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), skill.join("reference.md")).unwrap();

        let error = CodexWorkerEnvironment::prepare_from_with_environment_auth_and_skills(
            source.path(),
            false,
            test_launcher(),
            &[root.path().to_path_buf()],
        )
        .err()
        .expect("skill symlinks must fail preparation");

        assert!(error.to_string().contains("unsupported symlink"));
        assert!(
            !error
                .to_string()
                .contains(&outside.path().display().to_string())
        );
    }

    #[test]
    fn worker_home_is_removed_after_launch_scope() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("auth.json"), "auth").unwrap();
        let worker = CodexWorkerEnvironment::prepare_from(source.path()).unwrap();
        let path = worker.home().to_path_buf();
        drop(worker);
        assert!(!path.exists());
    }

    #[test]
    fn writer_session_snapshot_round_trips_without_auth_or_configuration() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("auth.json"), "auth").unwrap();
        let first = CodexWorkerEnvironment::prepare_from(source.path()).unwrap();
        let rollout = first
            .home()
            .join("sessions/2026/08/04/rollout-writer-thread.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(&rollout, "writer session").unwrap();
        fs::write(first.home().join("config.toml"), "hooks = true").unwrap();
        let artifact = tempfile::tempdir().unwrap();
        let snapshot = artifact.path().join("codex-session");

        first.snapshot_sessions_to(&snapshot).unwrap();

        assert!(
            snapshot
                .join("2026/08/04/rollout-writer-thread.jsonl")
                .is_file()
        );
        assert!(!snapshot.join("auth.json").exists());
        assert!(!snapshot.join("config.toml").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&snapshot).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(snapshot.join("2026/08/04/rollout-writer-thread.jsonl"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let second = CodexWorkerEnvironment::prepare_from(source.path()).unwrap();
        second.restore_sessions_from(&snapshot).unwrap();

        assert_eq!(
            fs::read_to_string(
                second
                    .home()
                    .join("sessions/2026/08/04/rollout-writer-thread.jsonl")
            )
            .unwrap(),
            "writer session"
        );
        assert_eq!(
            fs::read_to_string(second.home().join("auth.json")).unwrap(),
            "auth"
        );
        assert!(!second.home().join("config.toml").exists());
    }

    #[cfg(unix)]
    #[test]
    fn writer_session_snapshot_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("auth.json"), "auth").unwrap();
        let worker = CodexWorkerEnvironment::prepare_from(source.path()).unwrap();
        let sessions = worker.home().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), sessions.join("rollout.jsonl")).unwrap();
        let artifact = tempfile::tempdir().unwrap();

        let snapshot = artifact.path().join("codex-session");
        let error = worker.snapshot_sessions_to(&snapshot).unwrap_err();

        assert!(error.to_string().contains("snapshot Codex Writer sessions"));
        assert!(format!("{error:#}").contains("unsupported symlink"));
        assert!(!snapshot.exists());
    }

    #[cfg(unix)]
    #[test]
    fn worker_home_exposes_canonical_path_from_aliased_temp_root() {
        use std::os::unix::fs::symlink;

        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("auth.json"), "auth").unwrap();
        let real_root = tempfile::tempdir().unwrap();
        let alias_parent = tempfile::tempdir().unwrap();
        let alias = alias_parent.path().join("temporary-root");
        symlink(real_root.path(), &alias).unwrap();

        let worker = CodexWorkerEnvironment::prepare_from_with_environment_auth_in(
            source.path(),
            false,
            Some(&alias),
            test_launcher(),
            &[],
        )
        .unwrap();

        assert_eq!(worker.home(), fs::canonicalize(worker.home()).unwrap());
        assert!(!worker.home().starts_with(&alias));
        assert_eq!(worker.launch_env().1, worker.home().to_string_lossy());
    }

    #[test]
    fn canonical_existing_path_preserves_missing_path_spelling() {
        let missing = PathBuf::from("missing-codex-home");
        assert_eq!(canonical_existing_path(missing.clone()), missing);
    }

    #[cfg(unix)]
    #[test]
    fn effective_source_home_canonicalizes_an_existing_aliased_home() {
        use std::os::unix::fs::symlink;

        let real_home = tempfile::tempdir().unwrap();
        let alias_parent = tempfile::tempdir().unwrap();
        let alias = alias_parent.path().join("codex-home");
        symlink(real_home.path(), &alias).unwrap();

        assert_eq!(
            effective_source_home_from(Some(alias), None),
            fs::canonicalize(real_home.path()).unwrap()
        );
    }

    #[test]
    fn login_status_preflight_accepts_authenticated_worker_home() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("auth.json"), "auth").unwrap();
        let worker = CodexWorkerEnvironment::prepare_from(source.path()).unwrap();
        let fake = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            fake.path(),
            "#!/bin/sh\ntest \"$1 $2\" = 'login status'\ntest -z \"${3:-}\"\ntest -f \"$CODEX_HOME/auth.json\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        set_private_mode(fake.path(), 0o700).unwrap();

        worker.preflight_with(fake.path()).unwrap();
    }

    #[test]
    fn preflight_failure_names_login_and_resume_actions() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("auth.json"), "auth").unwrap();
        let worker = CodexWorkerEnvironment::prepare_from(source.path()).unwrap();
        let fake = tempfile::NamedTempFile::new().unwrap();
        fs::write(fake.path(), "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        set_private_mode(fake.path(), 0o700).unwrap();

        let error = worker.preflight_with(fake.path()).unwrap_err();
        assert!(error.to_string().contains("codex login"));
        assert!(error.to_string().contains("fluent attempt run"));
    }

    #[test]
    fn environment_auth_does_not_require_source_auth_file() {
        let source = tempfile::tempdir().unwrap();
        let worker = CodexWorkerEnvironment::prepare_from_with_environment_auth(
            source.path(),
            true,
            test_launcher(),
        )
        .unwrap();
        assert!(!worker.home().join("auth.json").exists());
    }
}
