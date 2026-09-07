use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ProcessEntry {
    pub pid: u32,
    pub kind: String,
    pub settlement_directory: PathBuf,
}

pub(crate) trait ProcessInventoryCollector {
    fn collect(&self, settlement_directory: &Path) -> Result<Vec<ProcessEntry>>;
}

pub(crate) struct HostProcessInventoryCollector;

impl ProcessInventoryCollector for HostProcessInventoryCollector {
    fn collect(&self, settlement_directory: &Path) -> Result<Vec<ProcessEntry>> {
        Self::collect_with_probe(settlement_directory, &mut |command, args| {
            Command::new(command).args(args).output()
        })
    }
}

type ProcessProbe<'a> = dyn FnMut(&str, &[&str]) -> std::io::Result<Output> + 'a;

impl HostProcessInventoryCollector {
    pub(crate) fn collect_with_probe(
        settlement_directory: &Path,
        probe: &mut ProcessProbe<'_>,
    ) -> Result<Vec<ProcessEntry>> {
        let output = probe("ps", &["-axo", "pid=,command="])
            .context("collecting the host process table for process settlement")?;
        if !output.status.success() {
            anyhow::bail!(
                "collecting the host process table for process settlement exited {}: {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if output.stdout.iter().all(u8::is_ascii_whitespace) {
            anyhow::bail!(
                "the host process table collector returned no inventory for process settlement"
            );
        }

        collect_scoped_processes(
            &String::from_utf8_lossy(&output.stdout),
            settlement_directory,
            |pid| process_cwd(pid, probe),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessCwd {
    Exited,
    Directory(PathBuf),
}

fn collect_scoped_processes<F>(
    process_table: &str,
    settlement_directory: &Path,
    mut cwd_for_pid: F,
) -> Result<Vec<ProcessEntry>>
where
    F: FnMut(u32) -> Result<ProcessCwd>,
{
    let mut entries = Vec::new();
    for line in process_table.lines() {
        let Some((pid, command)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid.trim().parse() else {
            continue;
        };
        let Some(kind) = classify_process(command.trim()) else {
            continue;
        };
        let ProcessCwd::Directory(cwd) = cwd_for_pid(pid)? else {
            continue;
        };
        if cwd.starts_with(settlement_directory) {
            entries.push(ProcessEntry {
                pid,
                kind: kind.to_string(),
                settlement_directory: settlement_directory.to_path_buf(),
            });
        }
    }
    Ok(entries)
}

fn process_cwd(pid: u32, probe: &mut ProcessProbe<'_>) -> Result<ProcessCwd> {
    const MAX_PROBES: usize = 3;
    for attempt in 1..=MAX_PROBES {
        let output = probe("lsof", &["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
            .with_context(|| {
                format!("reading cwd for host process {pid} during process settlement")
            })?;
        let context = format!(
            "reading cwd for host process {pid} during process settlement: lsof exited {} (stdout: {:?}, stderr: {:?})",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        if output.status.success() && output.stderr.is_empty() {
            let record = std::str::from_utf8(&output.stdout).with_context(|| context.clone())?;
            return parse_cwd_record(record, pid).with_context(|| context);
        }
        // Only an empty no-selection result can represent a lifecycle race.
        // Warnings or partial records cannot prove that the process is absent.
        if !no_process_selected(&output) {
            anyhow::bail!(context);
        }
        if process_exited(pid, probe).with_context(|| context.clone())? {
            return Ok(ProcessCwd::Exited);
        }
        if attempt == MAX_PROBES {
            anyhow::bail!(
                "{context}; process remained live without a cwd after {MAX_PROBES} probes"
            );
        }
    }
    unreachable!("the last cwd probe returns a result")
}

fn no_process_selected(output: &Output) -> bool {
    output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty()
}

fn parse_cwd_record(output: &str, pid: u32) -> Result<ProcessCwd> {
    let mut lines = output.lines();
    let expected_pid = format!("p{pid}");
    if lines.next() != Some(expected_pid.as_str()) {
        anyhow::bail!(
            "reading cwd for host process {pid} during process settlement returned a mismatched or missing PID record"
        );
    }
    let mut lines = lines.peekable();
    if lines.peek() == Some(&"fcwd") {
        lines.next();
    }
    let mut cwd = None;
    for line in lines {
        if let Some(directory) = line.strip_prefix('n')
            && Path::new(directory).is_absolute()
            && !directory.contains('\0')
            && cwd.is_none()
        {
            cwd = Some(PathBuf::from(directory));
        } else {
            anyhow::bail!(
                "reading cwd for host process {pid} during process settlement returned no cwd record or a malformed cwd record: {line:?}"
            );
        }
    }
    cwd.map(ProcessCwd::Directory).ok_or_else(|| {
        anyhow::anyhow!(
            "reading cwd for host process {pid} during process settlement returned no cwd record"
        )
    })
}

fn process_exited(pid: u32, probe: &mut ProcessProbe<'_>) -> Result<bool> {
    // Unlike kill -0, ps distinguishes a zombie from a live process. Select
    // only this PID so an empty no-selection result also proves absence.
    let output = probe("ps", &["-p", &pid.to_string(), "-o", "stat="]).with_context(|| {
        format!("checking state for host process {pid} during process settlement")
    })?;
    if no_process_selected(&output) {
        return Ok(true);
    }
    let context = format!(
        "checking state for host process {pid} during process settlement: ps exited {} (stdout: {:?}, stderr: {:?})",
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    if !output.status.success() || !output.stderr.is_empty() {
        anyhow::bail!(context);
    }
    let state = std::str::from_utf8(&output.stdout)
        .with_context(|| context.clone())?
        .trim();
    // Validate one macOS/Linux stat token, including documented modifiers.
    // Unknown states remain unavailable evidence; only Z proves termination.
    let mut characters = state.chars();
    let run_state = characters.next();
    if !run_state.is_some_and(|state| "IDRHSTtUWXPZ".contains(state))
        || !characters.all(|flag| "+<>AELNSVsWXl".contains(flag))
    {
        anyhow::bail!("{context}; returned a malformed process state");
    }
    Ok(run_state == Some('Z'))
}

fn classify_process(command: &str) -> Option<&'static str> {
    let command = command.to_ascii_lowercase();
    if command.contains("fluent scheduler") {
        Some("fluent scheduler")
    } else if command.contains("fluent auto-merge") {
        Some("fluent auto-merge")
    } else if command.contains("fluent post-merge-review") {
        Some("fluent post-merge-review")
    } else if command.contains("fluent") {
        Some("fluent")
    } else if command.contains("claude") {
        Some("claude")
    } else if command.contains("codex") {
        Some("codex")
    } else if command
        .split_whitespace()
        .any(|word| word == "pi" || word.ends_with("/pi"))
    {
        Some("pi")
    } else {
        None
    }
}

pub(crate) struct ProcessSettlement {
    pub directory: PathBuf,
    before: BTreeSet<ProcessEntry>,
}

impl ProcessSettlement {
    pub(crate) fn prepare_directory(directory: PathBuf) -> Result<PathBuf> {
        let existed = directory.exists();
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("creating settlement directory {}", directory.display()))?;
        if !directory.is_dir() {
            anyhow::bail!(
                "settlement path {} exists but is not a directory",
                directory.display()
            );
        }
        match std::fs::canonicalize(&directory) {
            Ok(directory) => Ok(directory),
            Err(source) => {
                if !existed {
                    let _ = std::fs::remove_dir_all(&directory);
                }
                Err(source).with_context(|| {
                    format!(
                        "canonicalizing settlement directory {}",
                        directory.display()
                    )
                })
            }
        }
    }

    pub(crate) fn begin_prepared(
        directory: PathBuf,
        collector: &dyn ProcessInventoryCollector,
    ) -> Result<Self> {
        let directory = std::fs::canonicalize(&directory).with_context(|| {
            format!(
                "canonicalizing prepared settlement directory {}",
                directory.display()
            )
        })?;
        let before = collector.collect(&directory)?.into_iter().collect();
        Ok(Self { directory, before })
    }

    pub(crate) fn new_processes(
        &self,
        collector: &dyn ProcessInventoryCollector,
    ) -> Result<Vec<ProcessEntry>> {
        let mut leaks = Vec::new();
        for attempt in 0..5 {
            let after = collector.collect(&self.directory)?;
            leaks = after
                .into_iter()
                .filter(|entry| !self.before.contains(entry))
                .collect();
            if leaks.is_empty() {
                return Ok(leaks);
            }
            if attempt < 4 {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        Ok(leaks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn output(code: i32, stdout: &str, stderr: &str) -> std::io::Result<Output> {
        Ok(Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    fn collect_scripted(steps: Vec<(&str, std::io::Result<Output>)>) -> Result<Vec<ProcessEntry>> {
        let mut steps = steps.into_iter();
        let result = HostProcessInventoryCollector::collect_with_probe(
            Path::new("/tmp/settlement"),
            &mut |command, args| {
                let (expected, output) = steps.next().expect("unexpected extra probe");
                assert_eq!(command, expected);
                match (command, args) {
                    ("ps", ["-axo", "pid=,command="])
                    | ("ps", ["-p", "101", "-o", "stat="])
                    | ("lsof", ["-a", "-p", "101", "-d", "cwd", "-Fn"]) => {}
                    _ => panic!("unexpected probe: {command} {args:?}"),
                }
                output
            },
        );
        assert!(steps.next().is_none(), "expected another probe");
        result
    }

    #[test]
    fn transient_or_zombie_process_without_cwd_is_omitted() {
        for state in [
            output(1, "", ""),
            output(0, "Z\n", ""),
            output(0, "Z+\n", ""),
        ] {
            let entries = collect_scripted(vec![
                ("ps", output(0, "101 codex exec\n", "")),
                ("lsof", output(1, "", "")),
                ("ps", state),
            ])
            .unwrap();
            assert!(entries.is_empty());
        }
    }

    #[test]
    fn transient_missing_cwd_is_retried() {
        for last_probe in [
            vec![("lsof", output(0, "p101\nn/tmp/settlement/child\n", ""))],
            vec![("lsof", output(1, "", "")), ("ps", output(1, "", ""))],
            vec![("lsof", output(1, "", "")), ("ps", output(0, "Z\n", ""))],
        ] {
            let finds_directory = last_probe.len() == 1;
            let mut steps = vec![
                ("ps", output(0, "101 codex exec\n", "")),
                ("lsof", output(1, "", "")),
                ("ps", output(0, "S+\n", "")),
                ("lsof", output(1, "", "")),
                ("ps", output(0, "R\n", "")),
            ];
            steps.extend(last_probe);
            let entries = collect_scripted(steps).unwrap();
            assert_eq!(entries.len(), usize::from(finds_directory));
        }
    }

    #[test]
    fn persistent_or_unsafe_cwd_failure_remains_an_error() {
        let mut steps = vec![("ps", output(0, "101 codex exec\n", ""))];
        // Even a reused PID that remains live must exhaust the same bound.
        for _ in 0..3 {
            steps.push(("lsof", output(1, "", "")));
            steps.push(("ps", output(0, "S\n", "")));
        }
        let error = format!("{:#}", collect_scripted(steps).unwrap_err());
        assert!(error.contains("host process 101"), "{error}");
        assert!(
            error.contains("lsof") && error.contains("exited 1"),
            "{error}"
        );
        assert!(error.contains("3 probes"), "{error}");

        for unsafe_output in [
            output(1, "", "permission denied"),
            output(1, "p101\n", ""),
            output(2, "", ""),
            output(0, "p101\n", ""),
            output(0, "p101\nn\n", ""),
            output(0, "p101\nnrelative\n", ""),
            output(0, "p102\nn/tmp/settlement\n", ""),
            output(0, "p101\nn/tmp/elsewhere\nn/tmp/settlement\n", ""),
            output(0, "p101\nn/tmp/settlement\n", "permission denied"),
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "lsof missing",
            )),
        ] {
            let error = collect_scripted(vec![
                ("ps", output(0, "101 codex exec\n", "")),
                ("lsof", unsafe_output),
            ])
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("host process 101"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn unsafe_process_state_remains_an_error() {
        for state in [
            output(1, "", "permission denied"),
            output(2, "", ""),
            output(0, "", ""),
            output(1, "Z\n", ""),
            output(0, "Z\nS\n", ""),
            output(0, "Zombie\n", ""),
            output(0, "?\n", ""),
            output(0, "Z\n", "permission denied"),
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "ps denied",
            )),
        ] {
            let error = collect_scripted(vec![
                ("ps", output(0, "101 codex exec\n", "")),
                ("lsof", output(1, "", "")),
                ("ps", state),
            ])
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("host process 101"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn scoped_live_process_remains_visible() {
        for (cwd, included) in [
            ("/tmp/settlement", true),
            ("/tmp/settlement/child", true),
            ("/tmp/settlement-other", false),
            ("/tmp/elsewhere", false),
        ] {
            let entries = collect_scripted(vec![
                ("ps", output(0, "101 codex exec\n", "")),
                ("lsof", output(0, &format!("p101\nn{cwd}\n"), "")),
            ])
            .unwrap();
            let expected = if included {
                vec![ProcessEntry {
                    pid: 101,
                    kind: "codex".to_string(),
                    settlement_directory: PathBuf::from("/tmp/settlement"),
                }]
            } else {
                vec![]
            };
            assert_eq!(entries, expected);
        }
    }

    #[test]
    fn cwd_probe_accepts_an_explicit_cwd_descriptor() {
        let entries = collect_scripted(vec![
            ("ps", output(0, "101 codex exec\n", "")),
            ("lsof", output(0, "p101\nfcwd\nn/tmp/settlement\n", "")),
        ])
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pid, 101);
    }

    #[test]
    fn invalid_utf8_probe_output_remains_an_error() {
        let mut cwd = output(0, "p101\nn/tmp/", "").unwrap();
        cwd.stdout.push(0xff);
        let mut state = output(0, "Z", "").unwrap();
        state.stdout.push(0xff);
        for probes in [
            vec![("lsof", Ok(cwd))],
            vec![("lsof", output(1, "", "")), ("ps", Ok(state))],
        ] {
            let mut steps = vec![("ps", output(0, "101 codex exec\n", ""))];
            steps.extend(probes);
            let error = collect_scripted(steps).unwrap_err();
            assert!(
                format!("{error:#}").contains("host process 101"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn collector_classifies_only_scoped_provider_processes() {
        let root = Path::new("/tmp/settlement");
        let entries = collect_scoped_processes(
            "101 fluent scheduler\n102 /usr/local/bin/claude\n103 codex exec\n104 /opt/bin/pi\n105 fluent auto-merge\n106 codex exec\n",
            root,
            |pid| match pid {
                101..=105 => Ok(ProcessCwd::Directory(root.join(pid.to_string()))),
                106 => Ok(ProcessCwd::Directory(PathBuf::from("/tmp/elsewhere"))),
                _ => unreachable!(),
            },
        )
        .unwrap();

        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.pid, entry.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (101, "fluent scheduler"),
                (102, "claude"),
                (103, "codex"),
                (104, "pi"),
                (105, "fluent auto-merge"),
            ]
        );
    }

    #[test]
    fn collector_rejects_unavailable_or_malformed_cwd_evidence() {
        let root = Path::new("/tmp/settlement");
        let unavailable =
            collect_scoped_processes("101 codex exec\n", root, |_| anyhow::bail!("lsof denied"));
        assert!(unavailable.unwrap_err().to_string().contains("lsof denied"));

        let malformed = collect_scoped_processes("101 codex exec\n", root, |_| {
            anyhow::bail!("returned no cwd record")
        });
        assert!(
            malformed
                .unwrap_err()
                .to_string()
                .contains("returned no cwd record")
        );

        let malformed_record = parse_cwd_record("p101\nf1\n", 101);
        assert!(
            malformed_record
                .unwrap_err()
                .to_string()
                .contains("returned no cwd record")
        );
    }

    #[test]
    fn collector_accepts_an_empty_inventory() {
        let entries =
            collect_scoped_processes("", Path::new("/tmp/settlement"), |_| unreachable!()).unwrap();
        assert!(entries.is_empty());
    }
}
