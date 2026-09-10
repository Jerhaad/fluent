//! Parse declared references and retain native result qualification.
use serde::{Deserialize, Serialize};
use std::path::Path;
#[derive(Debug, Clone)]
pub(crate) struct TestReference {
    pub(crate) path: String,
    pub(crate) selector: Option<String>,
    pub(crate) command: Option<String>,
    pub(crate) declared: Option<ReferenceMeaning>,
}

impl TestReference {
    pub(crate) fn python_identity(&self) -> Option<PythonTestIdentity<'_>> {
        if !self.path.ends_with(".py") {
            return None;
        }
        let mut identity = PythonTestIdentity::parse_name(self.selector.as_deref()?)?;
        // Source selectors name a class and method in this file, not an import
        // alias whose module would require executing candidate Python to resolve.
        if identity.module.is_some() {
            return None;
        }
        identity.path = Some(&self.path);
        Some(identity)
    }

    pub(crate) fn rust_identity(&self) -> Option<RustTestIdentity<'_>> {
        if !self.path.ends_with(".rs") {
            return None;
        }
        let mut identity = RustTestIdentity::parse(self.selector.as_deref()?)?;
        if identity.path.is_some_and(|path| path != self.path) {
            return None;
        }
        if let Some(ReferenceMeaning::NamedTest {
            binary: Some(binary),
            ..
        }) = &self.declared
        {
            if !is_nextest_binary_id(binary)
                || identity.binary.is_some_and(|native| native != binary)
            {
                return None;
            }
            identity.binary = Some(binary);
        }
        identity.path = Some(&self.path);
        Some(identity)
    }

    pub(crate) fn source_contains_selector(&self, source: &str) -> bool {
        if self.path.ends_with(".sh") {
            // A shell script is one native test; its parenthetical describes
            // coverage and need not appear in the committed script.
            true
        } else if self.path.ends_with(".rs") {
            // Rust declares modules separately from the test function. The
            // final evidence check resolves its full native qualification.
            self.rust_identity()
                .is_some_and(|identity| source.contains(identity.short_name()))
        } else if self.path.ends_with(".py") && self.selector.is_some() {
            self.python_identity()
                .is_some_and(|identity| identity.is_defined_in(source))
        } else {
            self.selector
                .as_deref()
                .is_none_or(|selector| source.contains(selector))
        }
    }
}

#[derive(Debug)]
pub(crate) struct PythonTestIdentity<'a> {
    pub(crate) path: Option<&'a str>,
    module: Option<&'a str>,
    class: Option<&'a str>,
    method: &'a str,
}

impl<'a> PythonTestIdentity<'a> {
    pub(crate) fn parse_name(name: &'a str) -> Option<Self> {
        if !name.split('.').all(is_python_identifier) {
            return None;
        }
        let (qualification, method) = name
            .rsplit_once('.')
            .map_or((None, name), |(prefix, method)| (Some(prefix), method));
        let (module, class) = qualification.map_or((None, None), |qualification| {
            qualification
                .rsplit_once('.')
                .map_or((None, Some(qualification)), |(module, class)| {
                    (Some(module), Some(class))
                })
        });
        Some(Self {
            path: None,
            module,
            class,
            method,
        })
    }

    pub(crate) fn parse(identity: &'a str) -> Option<Self> {
        if let Some((prefix, wrapped)) = identity.split_once(" (") {
            let wrapped = wrapped.strip_suffix(')')?;
            if prefix.ends_with(".py") {
                validate_test_path(prefix).ok()?;
                let mut parsed = Self::parse_name(wrapped)?;
                parsed.path = Some(prefix);
                return Some(parsed);
            }
            // unittest displays either `method (module.Class)` or
            // `method (module.Class.method)`. Preserve the supplied class.
            if !is_python_identifier(prefix) {
                return None;
            }
            let mut parsed = Self::parse_name(wrapped)?;
            if parsed.method == prefix && parsed.class.is_some() {
                return Some(parsed);
            }
            let (module, class) = wrapped
                .rsplit_once('.')
                .map_or((None, wrapped), |(module, class)| (Some(module), class));
            parsed.module = module;
            parsed.class = Some(class);
            parsed.method = prefix;
            return Some(parsed);
        }
        if let Some((prefix, name)) = identity.split_once(".py::") {
            let path = &identity[..prefix.len() + 3];
            validate_test_path(path).ok()?;
            let mut parsed = Self::parse_name(name)?;
            parsed.path = Some(path);
            return Some(parsed);
        }
        Self::parse_name(identity)
    }

    pub(crate) fn compatible_with(&self, other: &Self) -> bool {
        self.method == other.method
            && !matches!((self.path, other.path), (Some(a), Some(b)) if a != b)
            && !matches!((self.module, other.module), (Some(a), Some(b)) if a != b)
            && !matches!((self.class, other.class), (Some(a), Some(b)) if a != b)
    }

    pub(crate) fn is_defined_in(&self, source: &str) -> bool {
        let Some(statements) = python_source_statements(source) else {
            return false;
        };
        let mut scopes: Vec<(usize, Option<&str>)> = Vec::new();
        let mut classes = 0;
        let mut methods = 0;
        for (indent, statement) in &statements {
            while scopes.last().is_some_and(|(parent, _)| parent >= indent) {
                scopes.pop();
            }
            let statement = statement.trim();
            let class = strip_python_keyword(statement, "class").and_then(|rest| {
                let end = rest.find(['(', ':'])?;
                let name = rest[..end].trim();
                is_python_identifier(name).then_some(name)
            });
            if scopes.is_empty() && class.is_some() && class == self.class {
                classes += 1;
            }
            let method = strip_python_keyword(statement, "async").unwrap_or(statement);
            let method = strip_python_keyword(method, "def").and_then(|rest| {
                let (name, _) = rest.split_once('(')?;
                let name = name.trim();
                is_python_identifier(name).then_some(name)
            });
            let owner = scopes.last().and_then(|(_, class)| *class);
            if method == Some(self.method) && (self.class.is_none() || owner == self.class) {
                methods += 1;
            }
            if statement.ends_with(':') {
                // Only direct methods of a top-level class establish ownership.
                scopes.push((*indent, if scopes.is_empty() { class } else { None }));
            }
        }
        methods > 0 && (self.class.is_none() || (classes == 1 && methods == 1))
    }
}

pub(crate) fn strip_python_keyword<'a>(statement: &'a str, keyword: &str) -> Option<&'a str> {
    statement
        .strip_prefix(keyword)?
        .strip_prefix([' ', '\t'])
        .map(|rest| rest.trim_start_matches([' ', '\t']))
}

pub(crate) fn is_python_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

// Scan logical statements without importing Python or evaluating its strings.
// Mask string contents and comments before using indentation to resolve scopes.
pub(crate) fn python_source_statements(source: &str) -> Option<Vec<(usize, String)>> {
    let mut statements = Vec::new();
    let mut current = Vec::new();
    let mut indent = 0;
    let mut quote: Option<(u8, bool)> = None;
    let mut brackets = Vec::new();
    for line in source.lines() {
        if current.is_empty() {
            indent = line
                .bytes()
                .take_while(|b| matches!(b, b' ' | b'\t'))
                .fold(0, |column, b| {
                    if b == b'\t' {
                        (column / 8 + 1) * 8
                    } else {
                        column + 1
                    }
                });
        }
        let bytes = line.as_bytes();
        let mut i = 0;
        let mut continued = false;
        while i < bytes.len() {
            let b = bytes[i];
            if let Some((delimiter, triple)) = quote {
                if b == b'\\' {
                    i += 2;
                    continue;
                }
                if b == delimiter && (!triple || bytes.get(i..i + 3) == Some(&[delimiter; 3])) {
                    quote = None;
                    i += if triple { 3 } else { 1 };
                } else {
                    i += 1;
                }
                continue;
            }
            match b {
                b'#' => break,
                b'\'' | b'"' => {
                    let triple = bytes.get(i..i + 3) == Some(&[b; 3]);
                    quote = Some((b, triple));
                    current.push(b'?');
                    i += if triple { 3 } else { 1 };
                    continue;
                }
                b'(' | b'[' | b'{' => brackets.push(b),
                b')' | b']' | b'}' => {
                    let opening = brackets.pop()?;
                    if !matches!((opening, b), (b'(', b')') | (b'[', b']') | (b'{', b'}')) {
                        return None;
                    }
                }
                b'\\' if i + 1 == bytes.len() => {
                    continued = true;
                    break;
                }
                _ => {}
            }
            current.push(b);
            i += 1;
        }
        if quote.is_none() && brackets.is_empty() && !continued {
            let statement = String::from_utf8(std::mem::take(&mut current)).ok()?;
            if !statement.trim().is_empty() {
                statements.push((indent, statement));
            }
        } else {
            current.push(b' ');
        }
    }
    (quote.is_none() && brackets.is_empty() && current.is_empty()).then_some(statements)
}

#[derive(Debug)]
pub(crate) struct RustTestIdentity<'a> {
    pub(crate) path: Option<&'a str>,
    pub(crate) binary: Option<&'a str>,
    pub(crate) name: &'a str,
}

impl<'a> RustTestIdentity<'a> {
    pub(crate) fn parse(identity: &'a str) -> Option<Self> {
        let (path, native) = if let Some((path, wrapped)) = identity.split_once(" (") {
            (Some(path), wrapped.strip_suffix(')')?)
        } else if let Some((path, native)) = identity.split_once(".rs::") {
            (Some(&identity[..path.len() + 3]), native)
        } else {
            (None, identity)
        };
        if let Some(path) = path
            && (!path.ends_with(".rs") || validate_test_path(path).is_err())
        {
            return None;
        }
        let (binary, name) = if let Some((binary, name)) = native.split_once('$') {
            (Some(binary), name)
        } else if let Some((binary, name)) = native.split_once(' ') {
            (Some(binary), name)
        } else {
            (None, native)
        };
        // Admit native names and the known nextest separators, never arbitrary
        // display prose or suffixes. Keep every supplied qualifier for matching.
        let valid_name = |name: &str| {
            name.split("::").all(|part| {
                let part = part.strip_prefix("r#").unwrap_or(part);
                let mut chars = part.chars();
                chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
                    && chars.all(|c| c == '_' || c.is_alphanumeric())
            })
        };
        if !valid_name(name) || binary.is_some_and(|binary| !is_nextest_binary_id(binary)) {
            return None;
        }
        Some(Self { path, binary, name })
    }

    pub(crate) fn short_name(&self) -> &str {
        self.name.rsplit("::").next().unwrap_or(self.name)
    }

    pub(crate) fn compatible_with(&self, other: &Self) -> bool {
        !matches!((self.path, other.path), (Some(a), Some(b)) if a != b)
            && !matches!((self.binary, other.binary), (Some(a), Some(b)) if a != b)
            && (self.name == other.name
                || ((!self.name.contains("::") || !other.name.contains("::"))
                    && self.short_name() == other.short_name()))
    }
}

pub(crate) fn is_nextest_binary_id(binary: &str) -> bool {
    // Nextest identifies libraries as `package`, integration tests as
    // `package::target`, and other targets as `package::kind/target`.
    // Validate those separators separately from Rust module qualification and
    // retain the whole binary ID, including the kind and target, for matching.
    let valid_component = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c == '_' || c == '-' || c.is_alphanumeric())
    };
    let Some((package, target)) = binary.split_once("::") else {
        return valid_component(binary);
    };
    valid_component(package)
        && match target.split_once('/') {
            Some((kind, target)) => valid_component(kind) && valid_component(target),
            None => valid_component(target),
        }
}

pub(crate) fn parse_test_reference(reference: &str) -> std::result::Result<TestReference, String> {
    if reference.trim().starts_with('{') {
        let meaning: ReferenceMeaning =
            serde_json::from_str(reference.trim()).map_err(|error| error.to_string())?;
        let (path, selector, command) = match &meaning {
            ReferenceMeaning::WholeScript { path, .. } => {
                validate_test_path(path)?;
                (path.clone(), None, None)
            }
            ReferenceMeaning::NamedTest {
                path, name, binary, ..
            } => {
                validate_test_path(path)?;
                if name.trim().is_empty() || name.trim() != name {
                    return Err("native name must be nonempty and exact".into());
                }
                if binary.is_some() && !path.ends_with(".rs") {
                    return Err("binary is only a Rust qualifier".into());
                }
                (path.clone(), Some(name.clone()), None)
            }
            ReferenceMeaning::Command { command, .. } => {
                if command.trim().is_empty() {
                    return Err("command is empty".into());
                }
                (String::new(), None, Some(command.clone()))
            }
        };
        let reference = TestReference {
            path,
            selector,
            command,
            declared: Some(meaning),
        };
        if reference.selector.is_some()
            && ((reference.path.ends_with(".rs") && reference.rust_identity().is_none())
                || (reference.path.ends_with(".py") && reference.python_identity().is_none()))
        {
            return Err("native name or qualifiers contradict the declared source".into());
        }
        return Ok(reference);
    }
    let reference = reference.trim().trim_matches('`').trim();
    if reference.is_empty() {
        return Err("reference is empty".to_string());
    }
    if let Some(command) = parse_python_test_command(reference)? {
        return Ok(command);
    }
    let (path, selector) = if let Some(prefix) = reference.strip_suffix(')') {
        if let Some((path, selector)) = prefix.rsplit_once(" (") {
            (path.trim(), Some(selector.trim().to_string()))
        } else {
            (reference, None)
        }
    } else if let Some((path, selector)) = reference.split_once("::") {
        (path.trim(), Some(selector.trim().to_string()))
    } else {
        (reference, None)
    };
    // Planning documents commonly format repository paths as inline Markdown.
    // The backticks are presentation, not part of the committed path that Git
    // should validate.
    let path = path.trim_matches('`').trim();
    validate_test_path(path)?;
    if selector
        .as_ref()
        .is_some_and(|selector| selector.is_empty())
    {
        return Err("test identifier is empty".to_string());
    }
    Ok(TestReference {
        path: path.to_string(),
        selector,
        command: None,
        declared: None,
    })
}

pub(crate) fn parse_python_test_command(
    reference: &str,
) -> std::result::Result<Option<TestReference>, String> {
    let fields = reference.split_whitespace().collect::<Vec<_>>();
    let mut executable = 0;
    while fields
        .get(executable)
        .is_some_and(|field| is_environment_assignment(field))
    {
        executable += 1;
    }
    let Some(launcher) = fields.get(executable) else {
        return Ok(None);
    };
    if !is_python_launcher(launcher) {
        return Ok(None);
    }
    if fields
        .iter()
        .any(|field| field.contains(['\'', '"', '\\', ';', '|', '&', '<', '>']))
    {
        return Err("Python command must not contain quoting or shell operators".to_string());
    }
    let mut operand = executable + 1;
    while let Some(field) = fields.get(operand) {
        if *field == "--" {
            operand += 1;
            break;
        }
        if matches!(
            *field,
            "-B" | "-b"
                | "-bb"
                | "-d"
                | "-E"
                | "-I"
                | "-O"
                | "-OO"
                | "-P"
                | "-q"
                | "-s"
                | "-S"
                | "-u"
                | "-v"
                | "-V"
                | "-x"
        ) {
            operand += 1;
            continue;
        }
        if field.starts_with('-') {
            return Err(format!(
                "unsupported Python option {field:?}; Test: must name a project script directly"
            ));
        }
        break;
    }
    let path = fields
        .get(operand)
        .ok_or_else(|| "Python command has no project script operand".to_string())?;
    validate_test_path(path)?;
    Ok(Some(TestReference {
        path: (*path).to_string(),
        selector: None,
        command: Some(reference.to_string()),
        declared: None,
    }))
}

pub(crate) fn is_environment_assignment(field: &str) -> bool {
    let Some((name, _)) = field.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

pub(crate) fn is_python_launcher(field: &str) -> bool {
    if field == "python" {
        return true;
    }
    field.strip_prefix("python").is_some_and(|version| {
        version.chars().any(|character| character.is_ascii_digit())
            && version
                .chars()
                .all(|character| character.is_ascii_digit() || character == '.')
    })
}

pub(crate) fn validate_test_path(path: &str) -> std::result::Result<(), String> {
    let parsed = Path::new(path);
    if parsed
        .components()
        .collect::<std::path::PathBuf>()
        .as_os_str()
        != parsed.as_os_str()
        || parsed.is_absolute()
        || path.is_empty()
        || parsed
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("path must be a normalized project-relative path".to_string());
    }
    Ok(())
}

pub(crate) fn rust_evidence_matches_reference(
    expected: &RustTestIdentity<'_>,
    tests: &[(RustTestIdentity<'_>, &crate::tester::TestResult)],
) -> bool {
    let matching = tests
        .iter()
        .filter(|(identity, _)| expected.compatible_with(identity))
        .collect::<Vec<_>>();
    !matching.is_empty()
        && matching.iter().all(|(identity, test)| {
            test.status == "pass"
                && matching.iter().all(|(other, _)| identity.compatible_with(other))
                // A result that omitted qualifiers cannot stand in for this
                // reference if it could instead name a conflicting identity.
                && tests.iter().all(|(other, _)| {
                    !identity.compatible_with(other) || expected.compatible_with(other)
                })
        })
}

pub(crate) fn python_evidence_matches_reference(
    expected: &PythonTestIdentity<'_>,
    tests: &[(PythonTestIdentity<'_>, &crate::tester::TestResult)],
) -> bool {
    let matching = tests
        .iter()
        .filter(|(identity, _)| expected.compatible_with(identity))
        .collect::<Vec<_>>();
    !matching.is_empty()
        && matching.iter().all(|(identity, test)| {
            test.status == "pass"
                && matching
                    .iter()
                    .all(|(other, _)| identity.compatible_with(other))
                && tests.iter().all(|(other, _)| {
                    !identity.compatible_with(other) || expected.compatible_with(other)
                })
        })
}

/// Approved meaning; descriptions never participate in identity or matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ReferenceMeaning {
    WholeScript {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    NamedTest {
        path: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    Command {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_index: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
}

impl TestReference {
    pub(crate) fn meaning(&self) -> ReferenceMeaning {
        self.declared.clone().unwrap_or_else(|| {
            if let Some(command) = &self.command {
                ReferenceMeaning::Command {
                    command: command.clone(),
                    command_index: None,
                    description: None,
                }
            } else if self.path.ends_with(".sh") || self.selector.is_none() {
                ReferenceMeaning::WholeScript {
                    path: self.path.clone(),
                    description: self.selector.clone(),
                }
            } else {
                ReferenceMeaning::NamedTest {
                    path: self.path.clone(),
                    name: self.selector.clone().unwrap(),
                    binary: None,
                    description: None,
                }
            }
        })
    }
}

/// Native identity kept beside the unchanged producer id and status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum NativeTestIdentity {
    WholeScript {
        path: String,
    },
    NamedTest {
        path: String,
        name: String,
    },
    Rust {
        path: Option<String>,
        binary: Option<String>,
        name: String,
    },
    Python {
        path: Option<String>,
        module: Option<String>,
        class: Option<String>,
        name: String,
    },
}

pub(crate) fn normalize_native_identity(id: &str) -> Option<NativeTestIdentity> {
    if let Some((path, name)) = id.split_once(".sh::") {
        let path = format!("{path}.sh");
        if validate_test_path(&path).is_ok() && !name.is_empty() && name.trim() == name {
            return Some(NativeTestIdentity::NamedTest {
                path,
                name: name.into(),
            });
        }
        return None;
    }
    if id.ends_with(".sh") && validate_test_path(id).is_ok() {
        return Some(NativeTestIdentity::WholeScript { path: id.into() });
    }
    if let Some(identity) = PythonTestIdentity::parse(id) {
        return Some(NativeTestIdentity::Python {
            path: identity.path.map(str::to_owned),
            module: identity.module.map(str::to_owned),
            class: identity.class.map(str::to_owned),
            name: identity.method.into(),
        });
    }
    RustTestIdentity::parse(id).map(|identity| NativeTestIdentity::Rust {
        path: identity.path.map(str::to_owned),
        binary: identity.binary.map(str::to_owned),
        name: identity.name.into(),
    })
}

/// Validate supplied qualifiers against every qualifier retained in native text.
/// A free-form display label may accompany a structured identity.
pub(crate) fn normalized_result(
    test: &crate::tester::TestResult,
) -> Result<crate::tester::TestResult, String> {
    let Some(identity) = &test.identity else {
        return Ok(test.clone());
    };
    let id = match identity {
        NativeTestIdentity::WholeScript { path } => {
            validate_test_path(path)?;
            if let Some(native) = normalize_native_identity(&test.id) {
                if native != *identity {
                    return Err("native script identity contradicts producer text".into());
                }
            }
            path.clone()
        }
        NativeTestIdentity::NamedTest { path, name } => {
            validate_test_path(path)?;
            if name.trim().is_empty() || name.trim() != name {
                return Err("native name is empty or malformed".into());
            }
            if let Some(native) = normalize_native_identity(&test.id) {
                if native != *identity {
                    return Err("native test identity contradicts producer text".into());
                }
            }
            format!("{path}::{name}")
        }
        NativeTestIdentity::Rust { path, binary, name } => {
            let native = binary
                .as_ref()
                .map_or_else(|| name.clone(), |binary| format!("{binary}${name}"));
            let id = path
                .as_ref()
                .map_or_else(|| native.clone(), |path| format!("{path} ({native})"));
            let parsed = RustTestIdentity::parse(&id).ok_or("malformed Rust identity")?;
            if RustTestIdentity::parse(&test.id).is_none()
                && normalize_native_identity(&test.id).is_some()
            {
                return Err("Rust identity replaces a different native identity kind".into());
            }
            if RustTestIdentity::parse(&test.id).is_some_and(|raw| {
                !parsed.compatible_with(&raw)
                    || raw.path.is_some_and(|path| parsed.path != Some(path))
                    || raw
                        .binary
                        .is_some_and(|binary| parsed.binary != Some(binary))
                    || (raw.name.contains("::") && parsed.name != raw.name)
            }) {
                return Err("Rust identity contradicts or omits native qualifiers".into());
            }
            id
        }
        NativeTestIdentity::Python {
            path,
            module,
            class,
            name,
        } => {
            if module.is_some() && class.is_none() {
                return Err("Python module needs a class qualifier".into());
            }
            let name = [module.as_deref(), class.as_deref(), Some(name.as_str())]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(".");
            let id = path
                .as_ref()
                .map_or_else(|| name.clone(), |path| format!("{path}::{name}"));
            let parsed = PythonTestIdentity::parse(&id).ok_or("malformed Python identity")?;
            if PythonTestIdentity::parse(&test.id).is_none()
                && normalize_native_identity(&test.id).is_some()
            {
                return Err("Python identity replaces a different native identity kind".into());
            }
            if PythonTestIdentity::parse(&test.id).is_some_and(|raw| {
                !parsed.compatible_with(&raw)
                    || raw.path.is_some_and(|path| parsed.path != Some(path))
                    || raw
                        .module
                        .is_some_and(|module| parsed.module != Some(module))
                    || raw.class.is_some_and(|class| parsed.class != Some(class))
            }) {
                return Err("Python identity contradicts or omits native qualifiers".into());
            }
            id
        }
    };
    let mut result = test.clone();
    result.id = id;
    Ok(result)
}
