use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use encoding_rs::GB18030;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::error::{Result, SvnHistoryToGitError};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Adaptive UTF-8 -> GB18030 decoder for Windows CLI output (svn/git localized errors).
pub fn decode_bytes(raw: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(raw) {
        return s.to_string();
    }
    let (decoded, _, _) = GB18030.decode(raw);
    decoded.into_owned()
}

/// RAII wrapper around a Win32 Job Object configured with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Dropping this handle atomically kills
/// every process in the tree (e.g. `git`, `perl`, `git-remote-https`, `svn`).
#[cfg(windows)]
pub struct JobGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
unsafe impl Send for JobGuard {}
#[cfg(windows)]
unsafe impl Sync for JobGuard {}

#[cfg(windows)]
impl JobGuard {
    pub fn new() -> Result<Self> {
        use std::mem::{size_of, zeroed};
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(SvnHistoryToGitError::Io(std::io::Error::last_os_error()));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                let err = std::io::Error::last_os_error();
                CloseHandle(handle);
                return Err(SvnHistoryToGitError::Io(err));
            }
            Ok(Self { handle })
        }
    }

    pub fn assign_pid(&self, pid: u32) -> Result<()> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };

        unsafe {
            let proc_handle = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if proc_handle.is_null() {
                return Err(SvnHistoryToGitError::Io(std::io::Error::last_os_error()));
            }
            let ok = AssignProcessToJobObject(self.handle, proc_handle);
            let err = std::io::Error::last_os_error();
            CloseHandle(proc_handle);
            if ok == 0 {
                return Err(SvnHistoryToGitError::Io(err));
            }
            Ok(())
        }
    }

    pub fn terminate_all(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        unsafe {
            let _ = TerminateJobObject(self.handle, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for JobGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(not(windows))]
pub struct JobGuard;

#[cfg(not(windows))]
impl JobGuard {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
    pub fn assign_pid(&self, _pid: u32) -> Result<()> {
        Ok(())
    }
    pub fn terminate_all(&self) {}
}

/// Tracks active Job Objects per task so cancellation kills the entire process tree immediately.
#[derive(Clone, Default)]
pub struct ProcessRegistry {
    jobs: Arc<Mutex<HashMap<i64, Vec<Arc<JobGuard>>>>>,
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, task_id: i64, job: Arc<JobGuard>) {
        if let Ok(mut map) = self.jobs.lock() {
            map.entry(task_id).or_default().push(job);
        }
    }

    pub fn unregister(&self, task_id: i64, job: &Arc<JobGuard>) {
        if let Ok(mut map) = self.jobs.lock() {
            if let Some(list) = map.get_mut(&task_id) {
                list.retain(|j| !Arc::ptr_eq(j, job));
                if list.is_empty() {
                    map.remove(&task_id);
                }
            }
        }
    }

    pub fn cancel_task(&self, task_id: i64) -> bool {
        if let Ok(mut map) = self.jobs.lock() {
            if let Some(list) = map.remove(&task_id) {
                for job in list {
                    job.terminate_all();
                }
                return true;
            }
        }
        false
    }
}

/// Configure a `tokio::process::Command` with Windows `CREATE_NO_WINDOW`.
pub fn configure_silent_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub env: HashMap<String, String>,
    pub stdin_data: Option<Vec<u8>>,
    pub timeout_secs: Option<u64>,
    pub log_output: bool,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: None,
            env: HashMap::new(),
            stdin_data: None,
            timeout_secs: None,
            log_output: true,
        }
    }

    pub fn cwd(mut self, path: impl AsRef<Path>) -> Self {
        self.cwd = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn envs(mut self, vars: HashMap<String, String>) -> Self {
        self.env.extend(vars);
        self
    }

    pub fn stdin_bytes(mut self, data: Vec<u8>) -> Self {
        self.stdin_data = Some(data);
        self
    }

    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    pub fn log_output(mut self, enabled: bool) -> Self {
        self.log_output = enabled;
        self
    }

    pub fn display_command(&self) -> String {
        let mut parts = Vec::with_capacity(self.args.len() + 1);
        parts.push(self.program.clone());
        let mut mask_next = false;
        for arg in &self.args {
            if mask_next {
                parts.push("******".to_string());
                mask_next = false;
                continue;
            }
            if arg == "--password" {
                mask_next = true;
                parts.push(arg.clone());
            } else {
                parts.push(arg.clone());
            }
        }
        parts.join(" ")
    }
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

pub type LineCallback = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Spawns a child process bound to a Win32 Job Object and streams decoded lines.
pub async fn run_command(
    registry: &ProcessRegistry,
    task_id: i64,
    spec: CommandSpec,
    on_line: Option<LineCallback>,
) -> Result<CommandOutput> {
    let display_cmd = spec.display_command();
    if let Some(cb) = &on_line {
        cb("command", &format!("$ {display_cmd}"));
    }

    let mut cmd = configure_silent_command(&spec.program);
    cmd.args(&spec.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if spec.stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true);

    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    let mut child: Child = cmd.spawn().map_err(|e| {
        // With no custom cwd, a lookup failure identifies the executable.
        if spec.cwd.is_none() && e.kind() == std::io::ErrorKind::NotFound {
            return SvnHistoryToGitError::ExecutableNotFound(spec.program.clone());
        }
        SvnHistoryToGitError::CommandFailed {
            command: display_cmd.clone(),
            code: None,
            stderr: e.to_string(),
        }
    })?;

    let job = Arc::new(JobGuard::new()?);
    if let Some(pid) = child.id() {
        let _ = job.assign_pid(pid);
    }
    registry.register(task_id, Arc::clone(&job));

    if let (Some(mut stdin), Some(data)) = (child.stdin.take(), spec.stdin_data) {
        tokio::spawn(async move {
            let _ = stdin.write_all(&data).await;
            let _ = stdin.shutdown().await;
        });
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let (tx, mut rx) = mpsc::channel::<(bool, String)>(512);

    if let Some(out) = stdout_pipe {
        let tx_out = tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(out);
            let mut buf = Vec::with_capacity(4096);
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = decode_bytes(&buf)
                            .trim_end_matches(['\r', '\n'])
                            .to_string();
                        if tx_out.send((false, line)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }

    if let Some(err) = stderr_pipe {
        let tx_err = tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(err);
            let mut buf = Vec::with_capacity(4096);
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = decode_bytes(&buf)
                            .trim_end_matches(['\r', '\n'])
                            .to_string();
                        if tx_err.send((true, line)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    drop(tx);

    let log_output = spec.log_output;
    let collect_handle = tokio::spawn(async move {
        let mut stdout_lines = Vec::new();
        let mut stderr_lines = Vec::new();
        while let Some((is_err, line)) = rx.recv().await {
            if log_output {
                if let Some(cb) = &on_line {
                    cb(if is_err { "warn" } else { "info" }, &line);
                }
            }
            if is_err {
                stderr_lines.push(line);
            } else {
                stdout_lines.push(line);
            }
        }
        (stdout_lines.join("\n"), stderr_lines.join("\n"))
    });

    let wait_res = if let Some(secs) = spec.timeout_secs {
        match tokio::time::timeout(Duration::from_secs(secs), child.wait()).await {
            Ok(res) => res,
            Err(_) => {
                job.terminate_all();
                let _ = child.kill().await;
                registry.unregister(task_id, &job);
                return Err(SvnHistoryToGitError::CommandTimeout {
                    command: display_cmd,
                    seconds: secs,
                });
            }
        }
    } else {
        child.wait().await
    };

    let (stdout, stderr) = collect_handle.await.unwrap_or_default();
    registry.unregister(task_id, &job);

    let status = wait_res?;
    if !status.success() {
        return Err(SvnHistoryToGitError::CommandFailed {
            command: display_cmd,
            code: status.code(),
            stderr: if stderr.is_empty() { stdout } else { stderr },
        });
    }

    Ok(CommandOutput {
        stdout,
        stderr,
        exit_code: status.code(),
    })
}
