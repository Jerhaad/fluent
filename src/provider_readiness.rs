#[cfg(not(test))]
use std::process::Command;

use serde::Deserialize;

use crate::coder::CoderKind;

/// A provider checked for local command and authentication readiness.
///
/// Codex keeps its private worker home alive so the caller can use the exact
/// authenticated boundary that passed the check for its subsequent launch.
pub struct ProviderReadiness {
    codex_worker: Option<crate::codex_worker::CodexWorkerEnvironment>,
}

#[derive(Debug)]
pub enum ProviderReadinessError {
    Claude(crate::claude_diagnostics::ClaudeProbeFailure),
    Provider {
        provider: CoderKind,
        condition: String,
    },
    Codex(crate::codex_worker::CodexWorkerPreparationError),
}

impl std::fmt::Display for ProviderReadinessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Claude(failure) => write!(formatter, "{}", failure.user_message("readiness")),
            Self::Provider {
                provider,
                condition,
            } => {
                write!(formatter, "{}: {condition}", provider.as_str())
            }
            Self::Codex(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ProviderReadinessError {}

impl ProviderReadiness {
    /// Verify readiness without sending a model prompt.
    pub fn prepare(provider: CoderKind) -> Result<Self, ProviderReadinessError> {
        Self::prepare_with_project(provider, None)
    }

    /// Verify readiness and freeze the project's configured Codex skills into
    /// the same private worker home retained for the subsequent launch.
    pub fn prepare_for_project(
        provider: CoderKind,
        project_root: &std::path::Path,
    ) -> Result<Self, ProviderReadinessError> {
        Self::prepare_with_project(provider, Some(project_root))
    }

    fn prepare_with_project(
        provider: CoderKind,
        project_root: Option<&std::path::Path>,
    ) -> Result<Self, ProviderReadinessError> {
        let prepare_codex = || {
            match project_root {
                Some(project_root) => {
                    crate::codex_worker::CodexWorkerEnvironment::prepare_for_project(project_root)
                }
                None => crate::codex_worker::CodexWorkerEnvironment::prepare(),
            }
            .map_err(ProviderReadinessError::Codex)
        };
        #[cfg(test)]
        {
            let codex_worker = if provider == CoderKind::Codex {
                Some(prepare_codex()?)
            } else {
                None
            };
            return Ok(Self { codex_worker });
        }

        #[cfg(not(test))]
        match provider {
            CoderKind::Codex => {
                let worker = prepare_codex()?;
                worker.preflight().map_err(|error| {
                    ProviderReadinessError::Codex(
                        crate::codex_worker::CodexWorkerPreparationError::Authentication(error),
                    )
                })?;
                Ok(Self {
                    codex_worker: Some(worker),
                })
            }
            CoderKind::Claude => {
                let output = Command::new("claude")
                    .args(["auth", "status", "--json"])
                    .output()
                    .map_err(|error| {
                        ProviderReadinessError::Claude(
                            crate::claude_diagnostics::ClaudeProbeFailure::new(
                                crate::claude_diagnostics::ClaudeProbeFailureKind::Unknown,
                                format!("cannot run `claude auth status --json`: {error}"),
                            ),
                        )
                    })?;
                if !output.status.success() {
                    return Err(ProviderReadinessError::Claude(
                        crate::claude_diagnostics::ClaudeProbeFailure::from_output(
                            &output.stdout,
                            &output.stderr,
                        ),
                    ));
                }
                let status: ClaudeAuthStatus = serde_json::from_slice(&output.stdout).map_err(
                    |error| {
                        let output_failure =
                            crate::claude_diagnostics::ClaudeProbeFailure::from_output(
                                &output.stdout,
                                &output.stderr,
                            );
                        let failure = if output_failure.kind()
                            == crate::claude_diagnostics::ClaudeProbeFailureKind::Unknown
                        {
                            crate::claude_diagnostics::ClaudeProbeFailure::new(
                                crate::claude_diagnostics::ClaudeProbeFailureKind::Unknown,
                                format!(
                                    "`claude auth status --json` returned invalid JSON: {error}; {}",
                                    output_failure.diagnostic()
                                ),
                            )
                        } else {
                            output_failure
                        };
                        ProviderReadinessError::Claude(failure)
                    },
                )?;
                if !status.logged_in {
                    let failure = claude_keychain_boundary_failure().unwrap_or_else(|| {
                        crate::claude_diagnostics::ClaudeProbeFailure::new(
                            crate::claude_diagnostics::ClaudeProbeFailureKind::Authentication,
                            "`claude auth status --json` reports no active login",
                        )
                    });
                    return Err(ProviderReadinessError::Claude(failure));
                }
                warn_for_old_claude_version();
                Ok(Self { codex_worker: None })
            }
            CoderKind::Pi => {
                let status = Command::new("pi")
                    .arg("--version")
                    .status()
                    .map_err(|error| ProviderReadinessError::Provider {
                        provider,
                        condition: format!("command is not installed: {error}"),
                    })?;
                if !status.success() {
                    return Err(ProviderReadinessError::Provider {
                        provider,
                        condition: "command readiness check failed".to_string(),
                    });
                }
                Ok(Self { codex_worker: None })
            }
        }
    }

    pub fn codex_worker(&self) -> Option<&crate::codex_worker::CodexWorkerEnvironment> {
        self.codex_worker.as_ref()
    }
}

#[derive(Deserialize)]
struct ClaudeAuthStatus {
    #[serde(rename = "loggedIn")]
    logged_in: bool,
}

impl ProviderReadinessError {
    pub fn is_authentication_error(&self) -> bool {
        matches!(
            self,
            Self::Codex(crate::codex_worker::CodexWorkerPreparationError::Authentication(_))
        ) || matches!(self, Self::Claude(failure) if failure.is_authentication())
    }

    pub fn claude_failure(&self) -> Option<&crate::claude_diagnostics::ClaudeProbeFailure> {
        match self {
            Self::Claude(failure) => Some(failure),
            _ => None,
        }
    }
}

#[cfg(not(test))]
fn claude_keychain_boundary_failure() -> Option<crate::claude_diagnostics::ClaudeProbeFailure> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code-credentials"])
        .output()
        .ok()?;
    if output.status.success() {
        return None;
    }
    let failure = crate::claude_diagnostics::ClaudeProbeFailure::from_output(&[], &output.stderr);
    failure.is_host_boundary().then_some(failure)
}

#[cfg(not(test))]
fn warn_for_old_claude_version() {
    let Some(warning) = Command::new("claude")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| claude_version_warning(&output.stdout))
    else {
        return;
    };
    eprintln!("  Warning: {warning}");
}

fn claude_version_warning(output: &[u8]) -> Option<String> {
    let installed = String::from_utf8_lossy(output)
        .split_whitespace()
        .find_map(|part| {
            crate::update::Version::parse(part.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '.'
            }))
        })?;
    let minimum = crate::update::Version::parse("2.1.211").expect("valid Claude minimum version");
    installed.is_behind(&minimum).then(|| {
        format!(
            "Claude Code {installed} predates 2.1.211, which fixes parallel session token renewal; upgrade Claude Code before parallel Fluent runs"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_status_requires_an_active_login() {
        let status: ClaudeAuthStatus = serde_json::from_str(r#"{"loggedIn":false}"#).unwrap();
        assert!(!status.logged_in);
    }

    #[test]
    fn old_claude_version_warns_but_current_version_does_not() {
        let warning = claude_version_warning(b"2.1.191 (Claude Code)").unwrap();
        assert!(warning.contains("2.1.211"));
        assert!(warning.contains("parallel session token renewal"));
        assert!(claude_version_warning(b"2.1.211 (Claude Code)").is_none());
        assert!(claude_version_warning(b"2.2.0 (Claude Code)").is_none());
    }

    #[test]
    fn unknown_claude_version_does_not_block_readiness() {
        assert!(claude_version_warning(b"Claude Code development build").is_none());
    }
}
