use serde_json::Value;

const DIAGNOSTIC_MAX_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeProbeFailureKind {
    Authentication,
    HostBoundary,
    RateLimit,
    Network,
    ProviderUnavailable,
    Timeout,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeProbeFailure {
    kind: ClaudeProbeFailureKind,
    diagnostic: String,
}

impl ClaudeProbeFailure {
    pub fn from_output(stdout: &[u8], stderr: &[u8]) -> Self {
        let raw = combined_output(stdout, stderr);
        let kind = classify_text(&raw);
        let diagnostic = sanitize_and_bound(&raw);
        Self { kind, diagnostic }
    }

    pub fn new(kind: ClaudeProbeFailureKind, diagnostic: impl Into<String>) -> Self {
        let diagnostic = sanitize_and_bound(&diagnostic.into());
        Self { kind, diagnostic }
    }

    pub fn kind(&self) -> ClaudeProbeFailureKind {
        self.kind
    }

    pub fn diagnostic(&self) -> &str {
        &self.diagnostic
    }

    pub fn is_authentication(&self) -> bool {
        self.kind == ClaudeProbeFailureKind::Authentication
    }

    pub fn is_host_boundary(&self) -> bool {
        self.kind == ClaudeProbeFailureKind::HostBoundary
    }

    pub fn code(&self) -> &'static str {
        match self.kind {
            ClaudeProbeFailureKind::Authentication => "FLUENT_CLAUDE_AUTH_REQUIRED",
            ClaudeProbeFailureKind::HostBoundary => "FLUENT_CLAUDE_HOST_CREDENTIAL_BLOCKED",
            ClaudeProbeFailureKind::RateLimit => "FLUENT_CLAUDE_RATE_LIMITED",
            ClaudeProbeFailureKind::Network => "FLUENT_CLAUDE_NETWORK_UNAVAILABLE",
            ClaudeProbeFailureKind::ProviderUnavailable => "FLUENT_CLAUDE_PROVIDER_UNAVAILABLE",
            ClaudeProbeFailureKind::Timeout => "FLUENT_CLAUDE_PROBE_TIMEOUT",
            ClaudeProbeFailureKind::Unknown => "FLUENT_CLAUDE_PROBE_FAILED",
        }
    }

    pub fn recovery(&self) -> String {
        self.recovery_for("credential refresh")
    }

    fn recovery_for(&self, action: &str) -> String {
        match self.kind {
            ClaudeProbeFailureKind::Authentication => {
                "Run 'claude /login' to re-authenticate, then retry the same Fluent command."
                    .to_string()
            }
            ClaudeProbeFailureKind::HostBoundary => {
                crate::os::HOST_SANDBOX_RECOVERY_GUIDANCE.to_string()
            }
            ClaudeProbeFailureKind::RateLimit => {
                "Wait for Claude capacity, then retry the same Fluent command.".to_string()
            }
            ClaudeProbeFailureKind::Network => {
                "Restore network access, then retry the same Fluent command.".to_string()
            }
            ClaudeProbeFailureKind::ProviderUnavailable => {
                "Retry the same Fluent command after Claude service availability returns.".to_string()
            }
            ClaudeProbeFailureKind::Timeout => {
                let deadline = if action == "readiness" {
                    "FLUENT_CLAUDE_READINESS_DEADLINE_SECS"
                } else {
                    "FLUENT_CLAUDE_REFRESH_DEADLINE_SECS"
                };
                format!(
                    "Retry the same Fluent command; if the host is consistently slow, increase {deadline}."
                )
            }
            ClaudeProbeFailureKind::Unknown => {
                "Inspect the diagnostic, repair the Claude command or environment, then retry the same Fluent command."
                    .to_string()
            }
        }
    }

    pub fn summary(&self, action: &str) -> String {
        format!(
            "{}: Claude {action} failed: {}.",
            self.code(),
            self.diagnostic
        )
    }

    pub fn user_message(&self, action: &str) -> String {
        format!("{} {}", self.summary(action), self.recovery_for(action))
    }

    pub fn task_user_message(&self, action: &str) -> String {
        let recovery = match self.kind {
            ClaudeProbeFailureKind::Authentication => {
                "Run 'claude /login' to re-authenticate, then run 'fluent attempt run' to resume the same Task."
            }
            ClaudeProbeFailureKind::HostBoundary => {
                "Run this Fluent command outside the enclosing agent sandbox through its per-command approval flow, then run 'fluent attempt run' to resume the same Task."
            }
            ClaudeProbeFailureKind::RateLimit => {
                "Wait for Claude capacity, then run 'fluent attempt run' to resume the same Task."
            }
            ClaudeProbeFailureKind::Network => {
                "Restore network access, then run 'fluent attempt run' to resume the same Task."
            }
            ClaudeProbeFailureKind::ProviderUnavailable => {
                "After Claude service availability returns, run 'fluent attempt run' to resume the same Task."
            }
            ClaudeProbeFailureKind::Timeout => {
                "Run 'fluent attempt run' to resume the same Task; if the host is consistently slow, increase FLUENT_CLAUDE_REFRESH_DEADLINE_SECS."
            }
            ClaudeProbeFailureKind::Unknown => {
                "Inspect the diagnostic, repair the Claude command or environment, then run 'fluent attempt run' to resume the same Task."
            }
        };
        format!("{} {recovery}", self.summary(action))
    }
}

fn combined_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stdout = String::from_utf8_lossy(stdout);
    match (stderr.trim(), stdout.trim()) {
        ("", "") => "no diagnostic output".to_string(),
        ("", stdout) => stdout.to_string(),
        (stderr, "") => stderr.to_string(),
        (stderr, stdout) => format!("{stderr}\n{stdout}"),
    }
}

fn classify_text(raw: &str) -> ClaudeProbeFailureKind {
    let text = raw.to_ascii_lowercase();
    if contains_any(
        &text,
        &[
            "sandbox_apply",
            "operation not permitted",
            "keychain access",
            "securityd",
            "user interaction is not allowed",
        ],
    ) {
        ClaudeProbeFailureKind::HostBoundary
    } else if contains_any(
        &text,
        &[
            "rate limit",
            "rate-limit",
            "usage limit",
            "session limit",
            "status 429",
            "http 429",
        ],
    ) {
        ClaudeProbeFailureKind::RateLimit
    } else if contains_any(
        &text,
        &[
            "not logged in",
            "authentication_failed",
            "authentication failed",
            "unauthorized",
            "invalid token",
            "expired token",
            "token expired",
            "status 401",
            "http 401",
        ],
    ) {
        ClaudeProbeFailureKind::Authentication
    } else if contains_any(
        &text,
        &[
            "network",
            "connection refused",
            "connection reset",
            "could not resolve host",
            "name or service not known",
            "dns",
            "offline",
        ],
    ) {
        ClaudeProbeFailureKind::Network
    } else if contains_any(
        &text,
        &[
            "service unavailable",
            "temporarily unavailable",
            "overloaded",
            "maintenance",
            "status 502",
            "status 503",
            "status 504",
            "http 502",
            "http 503",
            "http 504",
        ],
    ) {
        ClaudeProbeFailureKind::ProviderUnavailable
    } else if contains_any(&text, &["timed out", "timeout"]) {
        ClaudeProbeFailureKind::Timeout
    } else {
        ClaudeProbeFailureKind::Unknown
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn sanitize_and_bound(raw: &str) -> String {
    let trimmed = raw.trim();
    let sanitized = serde_json::from_str::<Value>(trimmed)
        .map(|mut value| {
            redact_json(&mut value);
            serde_json::to_string(&value).unwrap_or_else(|_| "invalid diagnostic JSON".to_string())
        })
        .unwrap_or_else(|_| redact_free_text(trimmed));
    truncate_chars(&sanitized, DIAGNOSTIC_MAX_CHARS)
}

fn redact_json(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if sensitive_key(key) {
                    *value = Value::String("[REDACTED]".to_string());
                } else {
                    redact_json(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact_json),
        _ => {}
    }
}

fn redact_free_text(raw: &str) -> String {
    let raw = redact_sensitive_assignments(raw);
    let mut redact_next = false;
    raw.split_whitespace()
        .map(|word| {
            if redact_next {
                redact_next = false;
                return "[REDACTED]".to_string();
            }
            let lowercase = word.to_ascii_lowercase();
            let colon_key = word
                .trim_end_matches(':')
                .trim_matches(|character| character == '"' || character == '\'');
            if lowercase == "bearer" || (sensitive_key(colon_key) && word.ends_with(':')) {
                redact_next = true;
                return word.to_string();
            }
            if let Some((key, _)) = word.split_once('=')
                && sensitive_key(
                    key.trim_matches(|character| character == '"' || character == '\''),
                )
            {
                return format!("{key}=[REDACTED]");
            }
            if looks_like_secret(word) {
                "[REDACTED]".to_string()
            } else {
                word.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_sensitive_assignments(raw: &str) -> String {
    let chars = raw.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(raw.len());
    let mut index = 0;
    while index < chars.len() {
        let quoted_key = matches!(chars[index], '"' | '\'');
        let key_start = index + usize::from(quoted_key);
        let mut key_end = key_start;
        while key_end < chars.len()
            && (chars[key_end].is_ascii_alphanumeric() || matches!(chars[key_end], '-' | '_'))
        {
            key_end += 1;
        }
        if key_end == key_start {
            output.push(chars[index]);
            index += 1;
            continue;
        }

        let mut delimiter = key_end;
        if quoted_key && delimiter < chars.len() && chars[delimiter] == chars[index] {
            delimiter += 1;
        }
        while delimiter < chars.len() && chars[delimiter].is_whitespace() {
            delimiter += 1;
        }
        if !sensitive_key(&chars[key_start..key_end].iter().collect::<String>())
            || delimiter >= chars.len()
            || !matches!(chars[delimiter], ':' | '=')
        {
            output.push(chars[index]);
            index += 1;
            continue;
        }

        output.extend(chars[index..=delimiter].iter());
        index = delimiter + 1;
        while index < chars.len() && chars[index].is_whitespace() {
            output.push(chars[index]);
            index += 1;
        }

        let quote = chars
            .get(index)
            .copied()
            .filter(|character| matches!(character, '"' | '\''));
        if let Some(quote) = quote {
            output.push(quote);
            index += 1;
            let mut escaped = false;
            while index < chars.len() {
                let character = chars[index];
                index += 1;
                if character == quote && !escaped {
                    break;
                }
                escaped = character == '\\' && !escaped;
                if character != '\\' {
                    escaped = false;
                }
            }
            output.push_str("[REDACTED]");
            if index <= chars.len() && chars.get(index - 1) == Some(&quote) {
                output.push(quote);
            }
        } else {
            while index < chars.len()
                && !chars[index].is_whitespace()
                && !matches!(chars[index], ',' | ';' | '}' | ']')
            {
                index += 1;
            }
            output.push_str("[REDACTED]");
        }
    }
    output
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "token",
        "accesstoken",
        "refreshtoken",
        "apikey",
        "authorization",
        "credential",
        "credentials",
        "secret",
        "password",
    ]
    .iter()
    .any(|sensitive| key == *sensitive || key.ends_with(sensitive))
}

fn looks_like_secret(word: &str) -> bool {
    let trimmed = word.trim_matches(|character: char| {
        character.is_ascii_punctuation() && character != '-' && character != '_'
    });
    (trimmed.starts_with("sk-") || trimmed.starts_with("eyJ")) && trimmed.len() >= 16
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    const SUFFIX: &str = "…[truncated]";
    let retained = max_chars.saturating_sub(SUFFIX.chars().count());
    let mut truncated = value.chars().take(retained).collect::<String>();
    truncated.push_str(SUFFIX);
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_auth_rate_network_provider_and_host_failures() {
        for (diagnostic, expected) in [
            (
                "HTTP 401 unauthorized",
                ClaudeProbeFailureKind::Authentication,
            ),
            ("HTTP 429 rate limit", ClaudeProbeFailureKind::RateLimit),
            ("connection refused", ClaudeProbeFailureKind::Network),
            (
                "service unavailable status 503",
                ClaudeProbeFailureKind::ProviderUnavailable,
            ),
            (
                "sandbox_apply: Operation not permitted",
                ClaudeProbeFailureKind::HostBoundary,
            ),
            ("request timed out", ClaudeProbeFailureKind::Timeout),
            ("unexpected local failure", ClaudeProbeFailureKind::Unknown),
        ] {
            assert_eq!(
                ClaudeProbeFailure::from_output(&[], diagnostic.as_bytes()).kind(),
                expected,
                "diagnostic: {diagnostic}"
            );
        }
    }

    #[test]
    fn category_codes_are_stable() {
        for (kind, code) in [
            (
                ClaudeProbeFailureKind::Authentication,
                "FLUENT_CLAUDE_AUTH_REQUIRED",
            ),
            (
                ClaudeProbeFailureKind::HostBoundary,
                "FLUENT_CLAUDE_HOST_CREDENTIAL_BLOCKED",
            ),
            (
                ClaudeProbeFailureKind::RateLimit,
                "FLUENT_CLAUDE_RATE_LIMITED",
            ),
            (
                ClaudeProbeFailureKind::Network,
                "FLUENT_CLAUDE_NETWORK_UNAVAILABLE",
            ),
            (
                ClaudeProbeFailureKind::ProviderUnavailable,
                "FLUENT_CLAUDE_PROVIDER_UNAVAILABLE",
            ),
            (
                ClaudeProbeFailureKind::Timeout,
                "FLUENT_CLAUDE_PROBE_TIMEOUT",
            ),
            (
                ClaudeProbeFailureKind::Unknown,
                "FLUENT_CLAUDE_PROBE_FAILED",
            ),
        ] {
            assert_eq!(ClaudeProbeFailure::new(kind, "diagnostic").code(), code);
        }
    }

    #[test]
    fn authentication_is_the_only_recovery_that_names_login() {
        for kind in [
            ClaudeProbeFailureKind::HostBoundary,
            ClaudeProbeFailureKind::RateLimit,
            ClaudeProbeFailureKind::Network,
            ClaudeProbeFailureKind::ProviderUnavailable,
            ClaudeProbeFailureKind::Timeout,
            ClaudeProbeFailureKind::Unknown,
        ] {
            let failure = ClaudeProbeFailure::new(kind, "diagnostic");
            assert!(!failure.recovery().contains("/login"), "kind: {kind:?}");
        }
        assert!(
            ClaudeProbeFailure::new(ClaudeProbeFailureKind::Authentication, "HTTP 401")
                .recovery()
                .contains("claude /login")
        );
    }

    #[test]
    fn readiness_timeout_names_the_readiness_deadline() {
        let message = ClaudeProbeFailure::new(ClaudeProbeFailureKind::Timeout, "timed out")
            .user_message("readiness");
        assert!(message.contains("FLUENT_CLAUDE_READINESS_DEADLINE_SECS"));
        assert!(!message.contains("FLUENT_CLAUDE_REFRESH_DEADLINE_SECS"));
    }

    #[test]
    fn diagnostic_redacts_structured_and_free_text_secrets() {
        let structured = ClaudeProbeFailure::from_output(
            br#"{"error":"unauthorized","accessToken":"sk-ant-secret-value","nested":{"password":"hunter2"}}"#,
            &[],
        );
        assert!(!structured.diagnostic().contains("sk-ant-secret-value"));
        assert!(!structured.diagnostic().contains("hunter2"));
        let parsed: Value = serde_json::from_str(structured.diagnostic()).unwrap();
        assert_eq!(parsed["accessToken"], "[REDACTED]");
        assert_eq!(parsed["nested"]["password"], "[REDACTED]");

        let free = ClaudeProbeFailure::from_output(
            &[],
            b"HTTP 401 token=sk-ant-secret-value Authorization: Bearer eyJsecretpayload password: hunter2",
        );
        assert!(!free.diagnostic().contains("sk-ant-secret-value"));
        assert!(!free.diagnostic().contains("eyJsecretpayload"));
        assert!(!free.diagnostic().contains("hunter2"));

        let mixed = ClaudeProbeFailure::from_output(
            br#"{"accessToken":"opaque-json-value","nested":{"password":"opaque-password"}}"#,
            b"HTTP 401 unauthorized",
        );
        assert!(!mixed.diagnostic().contains("opaque-json-value"));
        assert!(!mixed.diagnostic().contains("opaque-password"));
    }

    #[test]
    fn diagnostic_is_bounded_on_unicode_boundaries() {
        let failure = ClaudeProbeFailure::from_output("é".repeat(600).as_bytes(), &[]);
        assert!(failure.diagnostic().chars().count() <= DIAGNOSTIC_MAX_CHARS);
        assert!(failure.diagnostic().ends_with("…[truncated]"));
    }
}
