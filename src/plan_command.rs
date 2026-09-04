//! Resolve executable commands from older Plan-row text.
//!
//! New Attempts store the result in [`crate::work_model::PlanCommand`]. These
//! helpers remain only for the explicit compatibility path used when an older
//! completion matrix has no structured command field.

use std::collections::BTreeMap;

const MAX_COMMAND_BYTES: usize = 4096;

const COMMAND_LAUNCHERS: &[&str] = &[
    "bash",
    "bazel",
    "bun",
    "bundle",
    "cargo",
    "cmake",
    "curl",
    "deno",
    "docker",
    "dotnet",
    "env",
    "git",
    "go",
    "gradle",
    "java",
    "just",
    "make",
    "mvn",
    "node",
    "ninja",
    "npm",
    "npx",
    "perl",
    "pip",
    "pip3",
    "pnpm",
    "podman",
    "pytest",
    "python",
    "python3",
    "rake",
    "rsync",
    "ruby",
    "sh",
    "shellcheck",
    "ssh",
    "swift",
    "timeout",
    "uv",
    "wget",
    "yarn",
    "zsh",
];

fn validate_plan_command_candidate(command: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !command.trim().is_empty() && command.len() <= MAX_COMMAND_BYTES,
        "focused evidence command must be between 1 and {MAX_COMMAND_BYTES} bytes"
    );
    Ok(())
}

/// Return simple leading `NAME=value` assignments from a command. The exact
/// command string remains authoritative; this map is descriptive context for
/// consumers that need to display or reproduce the recorded environment.
pub(crate) fn extract_command_environment(command: &str) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    for token in command.split_whitespace() {
        let token = trim_plan_token(token);
        let Some((name, value)) = token.split_once('=') else {
            break;
        };
        if !is_plan_environment_assignment(token) {
            break;
        }
        environment.insert(name.to_string(), value.to_string());
    }
    environment
}

/// Return one unambiguous command found in the supplied legacy Plan text.
pub(crate) fn resolve_legacy_plan_command<'a>(
    texts: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let mut candidates = texts
        .into_iter()
        .flat_map(extract_plan_command_candidates)
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.dedup();
    let [command] = candidates.as_slice() else {
        return None;
    };
    Some(command.clone())
}

pub(crate) fn extract_plan_command_candidates(text: &str) -> Vec<String> {
    let mut candidates = Vec::new();

    // Inline code is the least ambiguous representation and is common in
    // Verification prose. Ignore spans that contain ordinary path labels.
    let mut code_start = None;
    for (index, character) in text.char_indices() {
        if character != '`' {
            continue;
        }
        if let Some(start) = code_start.take() {
            if let Some(command) = normalize_plan_command_candidate(&text[start..index]) {
                candidates.push(command);
            }
        } else {
            code_start = Some(index + character.len_utf8());
        }
    }

    // Deliverables and narrative verification commonly use "Run <command>",
    // while some plans say "execute <command>" or "command: <command>".
    for marker in ["run ", "execute ", "command:"] {
        let lower = text.to_ascii_lowercase();
        let marker_lower = marker.to_ascii_lowercase();
        let mut offset = 0;
        while let Some(relative) = lower[offset..].find(&marker_lower) {
            let marker_start = offset + relative;
            let before = text[..marker_start].chars().next_back();
            if before.is_none_or(|character| !character.is_ascii_alphanumeric()) {
                let tail_start = marker_start + marker.len();
                if let Some(command) = command_from_plan_tail(&text[tail_start..]) {
                    candidates.push(command);
                }
            }
            offset = marker_start + marker.len();
        }
    }

    // Finally handle a command embedded in prose without an explicit marker,
    // such as "the exact DGX_BRING_UP_LIVE=1 ./tests/... check".
    let tokens = text.split_whitespace().collect::<Vec<_>>();
    for (index, token) in tokens.iter().enumerate() {
        let token = trim_plan_token(token);
        if !is_plan_command_start(token) {
            continue;
        }
        // The executable operand of an environment-prefixed command is not a
        // second command. The complete candidate was already collected from
        // the assignment token.
        if index > 0 && is_plan_command_start(trim_plan_token(tokens[index - 1])) {
            continue;
        }
        if let Some(command) = command_from_plan_tokens(&tokens[index..]) {
            candidates.push(command);
        }
    }

    candidates
}

fn command_from_plan_tail(tail: &str) -> Option<String> {
    let tokens = tail.split_whitespace().collect::<Vec<_>>();
    command_from_plan_tokens(&tokens)
}

fn command_from_plan_tokens(tokens: &[&str]) -> Option<String> {
    let mut selected: Vec<String> = Vec::new();
    for raw in tokens {
        let token = trim_plan_token(raw);
        if token.is_empty() {
            break;
        }
        if !selected.is_empty()
            && selected
                .iter()
                .any(|selected| is_plan_script_operand(selected))
            && !token.starts_with('-')
        {
            break;
        }
        let lower = token.to_ascii_lowercase();
        if !selected.is_empty()
            && matches!(
                lower.as_str(),
                "unchanged" | "on" | "at" | "against" | "then" | "and" | "while" | "with"
            )
        {
            break;
        }
        if selected.is_empty() && !is_plan_command_start(token) {
            return None;
        }
        selected.push(token.to_string());
        if raw.chars().next_back().is_some_and(|character| {
            matches!(
                character,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}'
            )
        }) {
            break;
        }
    }
    let candidate = selected.join(" ");
    normalize_plan_command_candidate(&candidate)
}

fn normalize_plan_command_candidate(raw: &str) -> Option<String> {
    let mut candidate = raw.trim();
    while candidate.chars().next_back().is_some_and(|character| {
        matches!(
            character,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}'
        )
    }) {
        candidate = candidate[..candidate.len() - 1].trim_end();
    }
    if !is_plan_command(candidate) {
        return None;
    }
    validate_plan_command_candidate(candidate).ok()?;
    Some(candidate.to_string())
}

fn trim_plan_token(token: &str) -> &str {
    token.trim_matches(|character: char| matches!(character, '`' | '(' | '[' | '{' | '"' | '\''))
}

fn is_plan_script_operand(token: &str) -> bool {
    let token = trim_plan_token(token).trim_end_matches(['.', ',', ';', ':', '!', '?']);
    (token.starts_with("./") || token.contains('/'))
        && [".sh", ".bash", ".py", ".rb", ".js", ".ts"]
            .iter()
            .any(|suffix| token.ends_with(suffix))
}

fn is_plan_command_start(token: &str) -> bool {
    let token = trim_plan_token(token);
    is_plan_environment_assignment(token)
        || token.starts_with("./")
        || token.starts_with('/')
        || is_plan_script_operand(token)
        || COMMAND_LAUNCHERS.iter().any(|launcher| {
            token == *launcher
                || token
                    .strip_prefix(launcher)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
}

fn is_plan_command(token: &str) -> bool {
    let mut words = token.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if is_plan_environment_assignment(first) {
        let Some(second) = words.next() else {
            return false;
        };
        return is_plan_command_start(second) || second.starts_with('-');
    }
    if COMMAND_LAUNCHERS.iter().any(|launcher| {
        first == *launcher
            || first
                .strip_prefix(launcher)
                .is_some_and(|suffix| suffix.starts_with('/'))
    }) {
        return words.next().is_some();
    }
    is_plan_command_start(first)
}

fn is_plan_environment_assignment(token: &str) -> bool {
    let Some((name, value)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !value.is_empty()
        && name.chars().enumerate().all(|(index, character)| {
            (index == 0 && (character == '_' || character.is_ascii_alphabetic()))
                || (index > 0 && (character == '_' || character.is_ascii_alphanumeric()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_command_from_deliverable_and_explanation_separately() {
        let command = resolve_legacy_plan_command([
            "Run DGX_BRING_UP_LIVE=1 ./tests/dgx-bring-up-smoke.sh unchanged on spark1.",
            "The live smoke preserves isolation and reports content-free evidence.",
        ]);
        assert_eq!(
            command.as_deref(),
            Some("DGX_BRING_UP_LIVE=1 ./tests/dgx-bring-up-smoke.sh")
        );
    }

    #[test]
    fn rejects_ambiguous_legacy_plan_text() {
        let command =
            resolve_legacy_plan_command(["Run ./tests/first.sh and then run ./tests/second.sh."]);
        assert_eq!(command, None);
    }

    #[test]
    fn records_leading_environment_assignments() {
        let environment = extract_command_environment(
            "DGX_BRING_UP_LIVE=1 FLUENT_MODE=smoke ./tests/dgx-bring-up-smoke.sh",
        );
        assert_eq!(
            environment,
            BTreeMap::from([
                ("DGX_BRING_UP_LIVE".to_string(), "1".to_string()),
                ("FLUENT_MODE".to_string(), "smoke".to_string()),
            ])
        );
    }
}
