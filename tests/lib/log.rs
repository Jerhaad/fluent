use assert_cmd::assert::{Assert, OutputAssertExt};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write as _};
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::thread::JoinHandle;

pub struct LoggedCommand {
    inner: std::process::Command,
    cmd_args: Vec<String>,
    stdin: Option<Vec<u8>>,
}

impl LoggedCommand {
    pub fn cargo_bin(name: &str) -> Self {
        let program = if name == "fluent" {
            std::env::var_os("FLUENT_BIN_OVERRIDE")
                .map(PathBuf::from)
                .unwrap_or_else(|| assert_cmd::cargo::cargo_bin(name))
        } else {
            assert_cmd::cargo::cargo_bin(name)
        };
        let mut inner = std::process::Command::new(program);
        configure_process_group(&mut inner);
        Self {
            inner,
            cmd_args: vec![name.to_string()],
            stdin: None,
        }
    }

    #[cfg(test)]
    fn from_std(mut inner: std::process::Command) -> Self {
        configure_process_group(&mut inner);
        Self {
            inner,
            cmd_args: Vec::new(),
            stdin: None,
        }
    }

    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        if let Some(s) = arg.as_ref().to_str() {
            self.cmd_args.push(s.to_string());
        }
        self.inner.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args_vec: Vec<S> = args.into_iter().collect();
        for a in &args_vec {
            if let Some(s) = a.as_ref().to_str() {
                self.cmd_args.push(s.to_string());
            }
        }
        self.inner.args(args_vec);
        self
    }

    pub fn current_dir<P: AsRef<std::path::Path>>(&mut self, dir: P) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.env(key, val);
        self
    }

    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) -> &mut Self {
        self.inner.env_remove(key);
        self
    }

    pub fn write_stdin<S: Into<Vec<u8>>>(&mut self, buffer: S) -> &mut Self {
        self.stdin = Some(buffer.into());
        self
    }

    pub fn output(&mut self) -> Result<Output, std::io::Error> {
        // Keep each CLI fixture in a private process group so a descendant
        // cannot outlive the captured stdout or stderr pipes.
        self.inner.stdin(Stdio::piped());
        self.inner.stdout(Stdio::piped());
        self.inner.stderr(Stdio::piped());
        let mut child = self.inner.spawn()?;
        let group = child.id();

        let stdin = match (child.stdin.take(), self.stdin.clone()) {
            (Some(mut stdin), Some(input)) => {
                Some(std::thread::spawn(move || stdin.write_all(&input)))
            }
            (Some(_), None) | (None, _) => None,
        };
        let stdout = child.stdout.take().map(read_pipe);
        let stderr = child.stderr.take().map(read_pipe);
        let status = child.wait();
        let cleanup = terminate_process_group(group);

        if let Some(stdin) = stdin {
            let _ = stdin.join();
        }
        let stdout = join_pipe(stdout)?;
        let stderr = join_pipe(stderr)?;
        let status = status?;
        cleanup?;
        let output = Output {
            status,
            stdout,
            stderr,
        };
        write_log(&self.cmd_args, &output);
        Ok(output)
    }

    pub fn assert(&mut self) -> Assert {
        let output = self.output().expect("failed to execute command");
        output.assert()
    }
}

fn configure_process_group(_command: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        _command.process_group(0);
    }
}

fn read_pipe<R>(mut pipe: R) -> JoinHandle<Result<Vec<u8>, std::io::Error>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut output = Vec::new();
        pipe.read_to_end(&mut output).map(|_| output)
    })
}

fn join_pipe(
    pipe: Option<JoinHandle<Result<Vec<u8>, std::io::Error>>>,
) -> Result<Vec<u8>, std::io::Error> {
    pipe.map_or_else(
        || Ok(Vec::new()),
        |pipe| {
            pipe.join()
                .map_err(|_| std::io::Error::other("output reader thread panicked"))?
        },
    )
}

#[cfg(unix)]
fn terminate_process_group(leader: u32) -> Result<(), std::io::Error> {
    let group = i32::try_from(leader)
        .map_err(|_| std::io::Error::other("fixture process group id is out of range"))?;
    if !signal_process_group(group, libc::SIGTERM)? {
        return Ok(());
    }
    if process_group_is_gone(group, std::time::Duration::from_secs(2)) {
        return Ok(());
    }
    if !signal_process_group(group, libc::SIGKILL)? {
        return Ok(());
    }
    if process_group_is_gone(group, std::time::Duration::from_secs(2)) {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "fixture process group {group} survived SIGTERM and SIGKILL"
        )))
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_leader: u32) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
fn signal_process_group(group: i32, signal: i32) -> Result<bool, std::io::Error> {
    if unsafe { libc::kill(-group, signal) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH)
        || (cfg!(target_os = "macos") && error.raw_os_error() == Some(libc::EPERM))
    {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn process_group_is_gone(group: i32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if unsafe { libc::kill(-group, 0) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn logged_command_reaps_descendants_before_returning() {
    let tmp = tempfile::tempdir().unwrap();
    let pid_path = tmp.path().join("descendant.pid");
    let mut command = std::process::Command::new("/bin/sh");
    command.args([
        "-c",
        "sleep 2 & echo $! > \"$1\"",
        "logged-command-process-group",
        pid_path.to_str().unwrap(),
    ]);

    let started = std::time::Instant::now();
    let output = LoggedCommand::from_std(command).output().unwrap();
    assert!(output.status.success());
    let descendant: i32 = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "LoggedCommand waited for the descendant instead of sweeping it"
    );
    assert!(
        unsafe { libc::kill(descendant, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
        "LoggedCommand left descendant process {descendant}"
    );
}

fn write_log(cmd_args: &[String], output: &Output) {
    if std::env::var("FLUENT_TESTS_SKIP_LOG").as_deref() == Ok("1") {
        return;
    }

    let test_name = current_test_name();
    let log_dir = log_dir_path();

    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!(
            "warning: cannot create test log directory {}: {e}",
            log_dir.display()
        );
        return;
    }

    let log_path = log_dir.join(format!("{test_name}.log"));
    let exit_display = output
        .status
        .code()
        .map_or("signal".to_string(), |c| c.to_string());
    let content = format!(
        "=== {test_name} ===\ncommand: {}\nexit: {exit_display}\n---stdout---\n{}---stderr---\n{}",
        cmd_args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    if let Err(e) = fs::write(&log_path, &content) {
        eprintln!(
            "warning: cannot write test log to {}: {e}",
            log_path.display()
        );
        return;
    }

    if !output.status.success() {
        let failed_path = log_dir.join(".failed");
        let abs_log = fs::canonicalize(&log_path).unwrap_or(log_path);
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&failed_path)
        {
            let _ = writeln!(f, "{}", abs_log.display());
        }
    }
}

fn current_test_name() -> String {
    std::thread::current()
        .name()
        .unwrap_or("unknown_test")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn log_dir_path() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest).join("tests").join("output")
}

#[allow(dead_code)]
pub fn print_failed_summary() {
    let log_dir = log_dir_path();
    let failed_path = log_dir.join(".failed");
    let content = match fs::read_to_string(&failed_path) {
        Ok(c) if !c.trim().is_empty() => c,
        _ => return,
    };

    eprintln!("\nFailing case logs:");
    for line in content.lines() {
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        eprintln!("  {path}");
        if let Ok(log_content) = fs::read_to_string(path) {
            eprintln!("  --- last 20 lines ---");
            let lines: Vec<&str> = log_content.lines().collect();
            let start = lines.len().saturating_sub(20);
            for l in &lines[start..] {
                eprintln!("    {l}");
            }
        }
    }
}

#[allow(dead_code)]
pub fn test_log_dir_path() -> PathBuf {
    log_dir_path()
}

#[allow(dead_code)]
pub fn test_current_test_name() -> String {
    current_test_name()
}

#[allow(dead_code)]
pub fn clear_failed_sentinel() {
    let log_dir = log_dir_path();
    let failed_path = log_dir.join(".failed");
    let _ = fs::remove_file(failed_path);
}
