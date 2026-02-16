use crate::error::AgentError;
use crate::models::{AgentInput, Command as AgentCommand, ProcessInfo, ProcessStatus, StatusUpdate};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::MetadataExt;

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    mem::size_of,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::Local;
use memmap2::{MmapMut, MmapOptions};
use tokio::sync::mpsc;
use tokio::process::Command;

const MAX_PROCESSES: usize = 1024;
const SHARED_MEM_SIZE: usize = size_of::<ProcessInfo>() * MAX_PROCESSES;
const SHARED_MEM_PATH: &str = "/dev/shm/agent_processes";
const SESSION_FILE_PATH: &str = "/dev/shm/agent_session";

#[derive(Clone)]
pub struct Agent {
    pub shared_mem: Arc<RwLock<MmapMut>>,
    pub process_map: Arc<RwLock<HashMap<u64, usize>>>,
    log_dir: PathBuf,
    status_tx: mpsc::Sender<StatusUpdate>,
}

impl Agent {
    /// Create an agent with custom paths, used for testing.
    #[cfg(test)]
    pub fn new_with_paths(
        shared_mem_path: &str,
        log_dir: PathBuf,
    ) -> Result<Self, AgentError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(shared_mem_path)?;
        file.set_len(SHARED_MEM_SIZE as u64)?;

        let mmap = unsafe { MmapOptions::new().map_mut(&file)? };
        let process_map = Self::rebuild_process_map(&mmap);
        let shared_mem = Arc::new(RwLock::new(mmap));
        let process_map = Arc::new(RwLock::new(process_map));

        fs::create_dir_all(&log_dir)?;

        let (status_tx, mut status_rx) = mpsc::channel::<StatusUpdate>(1024);

        let shared_mem_clone = shared_mem.clone();
        let process_map_clone = process_map.clone();
        tokio::spawn(async move {
            while let Some(update) = status_rx.recv().await {
                if let Err(e) = Self::update_process_status(
                    shared_mem_clone.clone(),
                    update.nonce,
                    update.status,
                    update.exit_code,
                ) {
                    eprintln!("Failed to update process status: {}", e);
                }
                let info_size = size_of::<ProcessInfo>();
                let offset = (update.nonce as usize % MAX_PROCESSES) * info_size;
                process_map_clone.write().unwrap().insert(update.nonce, offset);
            }
        });

        Ok(Self {
            shared_mem,
            process_map,
            log_dir,
            status_tx,
        })
    }

    pub fn new() -> Result<Self, AgentError> {
        // Create shared memory file
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(SHARED_MEM_PATH)?;
        file.set_len(SHARED_MEM_SIZE as u64)?;

        // Map shared memory
        let mmap = unsafe { MmapOptions::new().map_mut(&file)? };
        let process_map = Self::rebuild_process_map(&mmap);
        let shared_mem = Arc::new(RwLock::new(mmap));
        let process_map = Arc::new(RwLock::new(process_map));

        // Resolve log directory (reuse existing session or create new)
        let log_dir = Self::resolve_log_dir()?;

        // Setup status channel
        let (status_tx, mut status_rx) = mpsc::channel::<StatusUpdate>(1024);

        // Start status monitor thread
        let shared_mem_clone = shared_mem.clone();
        let process_map_clone = process_map.clone();
        tokio::spawn(async move {
            while let Some(update) = status_rx.recv().await {
                if let Err(e) = Self::update_process_status(
                    shared_mem_clone.clone(),
                    update.nonce,
                    update.status,
                    update.exit_code,
                ) {
                    eprintln!("Failed to update process status: {}", e);
                }
                // Update process_map so StatusMonitor can see this nonce
                let info_size = size_of::<ProcessInfo>();
                let offset = (update.nonce as usize % MAX_PROCESSES) * info_size;
                process_map_clone.write().unwrap().insert(update.nonce, offset);
            }
        });

        Ok(Self {
            shared_mem,
            process_map,
            log_dir,
            status_tx,
        })
    }

    fn resolve_log_dir() -> Result<PathBuf, AgentError> {
        if let Ok(existing) = fs::read_to_string(SESSION_FILE_PATH) {
            let path = PathBuf::from(existing.trim());
            if path.is_dir() {
                return Ok(path);
            }
        }
        let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
        let log_dir = PathBuf::from(format!("/var/log/agent/{}", timestamp));
        fs::create_dir_all(&log_dir)?;
        fs::write(SESSION_FILE_PATH, log_dir.to_string_lossy().as_bytes())?;
        Ok(log_dir)
    }

    fn rebuild_process_map(mmap: &MmapMut) -> HashMap<u64, usize> {
        let mut map = HashMap::new();
        let info_size = size_of::<ProcessInfo>();
        for i in 0..MAX_PROCESSES {
            let offset = i * info_size;
            let info = unsafe {
                std::ptr::read(mmap[offset..offset + info_size].as_ptr() as *const ProcessInfo)
            };
            if info.nonce != 0 {
                map.insert(info.nonce, offset);
            }
        }
        map
    }

    fn update_process_status(
        shared_mem: Arc<RwLock<MmapMut>>,
        nonce: u64,
        status: ProcessStatus,
        exit_code: i32,
    ) -> Result<(), AgentError> {
        let mut mmap = shared_mem.write().unwrap();
        let info_size = size_of::<ProcessInfo>();
        let offset = (nonce as usize % MAX_PROCESSES) * info_size;

        // Read existing entry to preserve PID
        let existing = unsafe {
            std::ptr::read(mmap[offset..offset + info_size].as_ptr() as *const ProcessInfo)
        };

        let info = ProcessInfo {
            nonce,
            pid: existing.pid,
            status,
            exit_code,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };

        let bytes = unsafe {
            std::slice::from_raw_parts(
                &info as *const ProcessInfo as *const u8,
                size_of::<ProcessInfo>(),
            )
        };
        mmap[offset..offset + info_size].copy_from_slice(bytes);
        Ok(())
    }

    async fn exec_as_agent(&self, cmd: &AgentCommand) -> Result<(), AgentError> {
        let command = cmd.command.as_ref().ok_or_else(|| {
            AgentError::Process("Command string is required for execAsAgent".to_string())
        })?;
    
        // Handle dependencies if any
        if let Some(dep_nonce) = cmd.depending_nonce {
            let wait = cmd.wait.unwrap_or(false);
            let expected_status = cmd.expected_status.unwrap_or(0);
    
            if !self.check_dependency(dep_nonce, expected_status, wait).await? {
                self.status_tx
                    .send(StatusUpdate {
                        nonce: cmd.nonce,
                        status: ProcessStatus::Skipped,
                        exit_code: 0,
                    })
                    .await
                    .map_err(|e| AgentError::Process(e.to_string()))?;
                return Ok(());
            }
        }
    
        // Replace $NONCE references
        let command = self.replace_nonce_refs(command)?;
    
        // Setup output files with append mode to prevent truncation
        let stdout_path = self.log_dir.join(format!("{}_stdout.log", cmd.nonce));
        let stderr_path = self.log_dir.join(format!("{}_stderr.log", cmd.nonce));
    
        let stdout_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stdout_path)?;
        let stderr_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stderr_path)?;
    
        // Execute command
        let display_id = cmd.display.unwrap_or(1);
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(&command)
            .env("DISPLAY", format!(":{}", display_id))
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file))
            .spawn()?;
    
        // Update process info in shared memory
        let pid = child.id().unwrap_or(0) as i32;
        self.update_process_info(cmd.nonce, pid, ProcessStatus::Running, 0)?;
    
        // Monitor process in background
        let nonce = cmd.nonce;
        let status_tx = self.status_tx.clone();
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => {
                    let exit_code = status.code().unwrap_or(-1);
                    let process_status = if exit_code == 0 {
                        ProcessStatus::Completed
                    } else {
                        ProcessStatus::Failed
                    };
                    if let Err(e) = status_tx
                        .send(StatusUpdate {
                            nonce,
                            status: process_status,
                            exit_code,
                        })
                        .await
                    {
                        eprintln!("Failed to send status update: {}", e);
                    }
                }
                Err(e) => {
                    eprintln!("Failed to wait for process: {}", e);
                    if let Err(e) = status_tx
                        .send(StatusUpdate {
                            nonce,
                            status: ProcessStatus::Failed,
                            exit_code: -1,
                        })
                        .await
                    {
                        eprintln!("Failed to send status update: {}", e);
                    }
                }
            }
        });
    
        Ok(())
    }

    async fn capture_screen(&self, cmd: &AgentCommand) -> Result<(), AgentError> {
        let display = cmd.display.unwrap_or(1);
        let screenshot_path = self.log_dir.join(format!("screenshot_{}.png", cmd.nonce));

        // Handle dependencies similarly to exec_as_agent
        if let Some(dep_nonce) = cmd.depending_nonce {
            let wait = cmd.wait.unwrap_or(false);
            let expected_status = cmd.expected_status.unwrap_or(0);

            if !self.check_dependency(dep_nonce, expected_status, wait).await? {
                self.status_tx
                    .send(StatusUpdate {
                        nonce: cmd.nonce,
                        status: ProcessStatus::Skipped,
                        exit_code: 0,
                    })
                    .await
                    .map_err(|e| AgentError::Process(e.to_string()))?;
                return Ok(());
            }
        }

        // Use import command from ImageMagick
        let status = Command::new("import")
            .args([
                "-window",
                "root",
                "-display",
                &format!(":{}", display),
                &screenshot_path.to_string_lossy(),
            ])
            .status()
	    .await?;
        let exit_code = status.code().unwrap_or(-1);
        let process_status = if status.success() {
            ProcessStatus::Completed
        } else {
            ProcessStatus::Failed
        };

        self.status_tx
            .send(StatusUpdate {
                nonce: cmd.nonce,
                status: process_status,
                exit_code,
            })
            .await
            .map_err(|e| AgentError::Process(e.to_string()))?;

        Ok(())
    }

    fn fetch_status(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let status_type = cmd.status_type.as_ref().ok_or_else(|| {
            AgentError::Process("status_type is required for fetchStatus".to_string())
        })?;

        match status_type.as_str() {
            "status" => {
                let info = self.get_process_info(cmd.nonce)?;
                Ok((info.status as u8 as char).to_string())
            }
            "stdout" => {
                let path = self.log_dir.join(format!("{}_stdout.log", cmd.nonce));
                Ok(fs::read_to_string(path).unwrap_or_default())
            }
            "stderr" => {
                let path = self.log_dir.join(format!("{}_stderr.log", cmd.nonce));
                Ok(fs::read_to_string(path).unwrap_or_default())
            }
            "exit_code" => {
                let info = self.get_process_info(cmd.nonce)?;
                Ok(info.exit_code.to_string())
            }
            _ => Err(AgentError::Process(format!(
                "Invalid status_type: {}",
                status_type
            ))),
        }
    }

    fn inspect_path(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let path_str = cmd.path.as_ref().ok_or_else(|| {
            AgentError::Process("path is required for inspectPath".to_string())
        })?;
        let path = std::path::Path::new(path_str);

        if !path.exists() {
            return Ok(serde_json::json!({
                "exists": false,
                "path": path_str
            }).to_string());
        }

        let symlink_meta = fs::symlink_metadata(path)?;
        let file_type = if symlink_meta.file_type().is_symlink() {
            "symlink"
        } else if symlink_meta.is_dir() {
            "directory"
        } else if symlink_meta.is_file() {
            "file"
        } else {
            "other"
        };

        let meta = fs::metadata(path)?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Ok(serde_json::json!({
            "exists": true,
            "path": path_str,
            "type": file_type,
            "size": meta.len(),
            "permissions": format!("{:o}", meta.mode() & 0o7777),
            "modified": modified,
            "uid": meta.uid(),
            "gid": meta.gid()
        }).to_string())
    }

    pub async fn process_input(&self, input: AgentInput) -> Result<Vec<String>, AgentError> {
        let mut results = Vec::new();

        // Pre-register all async command nonces to avoid dependency race conditions
        for cmd in &input.commands {
            match cmd.function.as_str() {
                "execAsAgent" | "captureScreen" => {
                    self.update_process_info(cmd.nonce, 0, ProcessStatus::Waiting, 0)?;
                }
                _ => {}
            }
        }

        // Start all commands asynchronously without waiting for completion
        for cmd in input.commands {
            match cmd.function.as_str() {
                "execAsAgent" => {
                    let agent = self.clone();
                    let cmd_clone = cmd.clone();
                    tokio::spawn(async move {
                        if let Err(e) = agent.exec_as_agent(&cmd_clone).await {
                            eprintln!("Error executing command {}: {}", cmd_clone.nonce, e);
                        }
                    });
                    if cmd.depending_nonce.is_some() {
                        results.push(format!("{}w0", cmd.nonce));
                    } else {
                        results.push(format!("{}r0", cmd.nonce));
                    }
                }
                "captureScreen" => {
                    let agent = self.clone();
                    let cmd_clone = cmd.clone();
                    tokio::spawn(async move {
                        if let Err(e) = agent.capture_screen(&cmd_clone).await {
                            eprintln!("Error capturing screen {}: {}", cmd_clone.nonce, e);
                        }
                    });
                    if cmd.depending_nonce.is_some() {
                        results.push(format!("{}w0", cmd.nonce));
                    } else {
                        results.push(format!("{}r0", cmd.nonce));
                    }
                }
                "fetchStatus" => {
                    // fetchStatus is synchronous and immediate
                    match self.fetch_status(&cmd) {
                        Ok(status) => results.push(status),
                        Err(e) => results.push(format!("Error: {}", e)),
                    }
                }
                "inspectPath" => {
                    match self.inspect_path(&cmd) {
                        Ok(result) => results.push(result),
                        Err(e) => results.push(format!("Error: {}", e)),
                    }
                }
                _ => {
                    return Err(AgentError::Process(format!(
                        "Unknown function: {}",
                        cmd.function
                    )))
                }
            }
        }
    
        // Wait for the specified duration if requested
        if let Some(wait_time) = input.wait_for_status {
            tokio::time::sleep(Duration::from_millis(wait_time)).await;
        }
    
        // Return current results without waiting for completion
        Ok(results)
    }

    // Helper methods
    fn update_process_info(
        &self,
        nonce: u64,
        pid: i32,
        status: ProcessStatus,
        exit_code: i32,
    ) -> Result<(), AgentError> {
        let mut mmap = self.shared_mem.write().unwrap();
        let info_size = size_of::<ProcessInfo>();
        let offset = (nonce as usize % MAX_PROCESSES) * info_size;

        let info = ProcessInfo {
            nonce,
            pid,
            status,
            exit_code,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };

        let bytes = unsafe {
            std::slice::from_raw_parts(
                &info as *const ProcessInfo as *const u8,
                size_of::<ProcessInfo>(),
            )
        };
        mmap[offset..offset + info_size].copy_from_slice(bytes);
        
        // Update process map
        let mut map = self.process_map.write().unwrap();
        map.insert(nonce, offset);
        
        Ok(())
    }

    fn get_process_info(&self, nonce: u64) -> Result<ProcessInfo, AgentError> {
        let map = self.process_map.read().unwrap();
        let offset = *map
            .get(&nonce)
            .ok_or_else(|| AgentError::InvalidNonce(nonce))?;

        let mmap = self.shared_mem.read().unwrap();
        let info_slice = &mmap[offset..offset + size_of::<ProcessInfo>()];
        
        let info = unsafe {
            std::ptr::read(info_slice.as_ptr() as *const ProcessInfo)
        };
        
        Ok(info)
    }

    async fn check_dependency(
        &self,
        depending_nonce: u64,
        expected_status: i32,
        wait: bool,
    ) -> Result<bool, AgentError> {
        let mut retries = if wait { 100 } else { 1 };

        while retries > 0 {
            match self.get_process_info(depending_nonce) {
                Ok(info) => {
                    match info.status {
                        ProcessStatus::Completed if info.exit_code == expected_status => {
                            return Ok(true)
                        }
                        ProcessStatus::Completed | ProcessStatus::Failed | ProcessStatus::Skipped => {
                            // Terminal states - no point retrying
                            return Ok(false)
                        }
                        _ if !wait => {
                            return Ok(false)
                        }
                        _ => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            retries -= 1;
                            continue;
                        }
                    }
                }
                Err(AgentError::InvalidNonce(_)) => {
                    if !wait {
                        return Ok(false);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    retries -= 1;
                }
                Err(_) if !wait => return Ok(false),
                Err(e) => {
                    eprintln!("Error checking dependency: {}", e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    retries -= 1;
                }
            }
        }

        Ok(false)
    }

    fn replace_nonce_refs(&self, command: &str) -> Result<String, AgentError> {
        let re = regex::Regex::new(r"\$NONCE\[(\d+)\]").unwrap();
        let mut result = command.to_string();

        for cap in re.captures_iter(command) {
            let nonce: u64 = cap[1].parse().map_err(|_| {
                AgentError::Process(format!("Invalid nonce reference: {}", &cap[1]))
            })?;

            let info = self.get_process_info(nonce)?;
            result = result.replace(&cap[0], &info.pid.to_string());
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create a test agent with temp directories
    fn create_test_agent() -> (Agent, TempDir, TempDir) {
        let shm_dir = TempDir::new().unwrap();
        let log_dir = TempDir::new().unwrap();
        let shm_path = shm_dir.path().join("test_processes");
        let agent = Agent::new_with_paths(
            shm_path.to_str().unwrap(),
            log_dir.path().to_path_buf(),
        )
        .unwrap();
        (agent, shm_dir, log_dir)
    }

    #[tokio::test]
    async fn update_and_get_process_info() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 1234, ProcessStatus::Running, 0)
            .unwrap();
        let info = agent.get_process_info(1).unwrap();
        assert_eq!(info.nonce, 1);
        assert_eq!(info.pid, 1234);
        assert_eq!(info.status, ProcessStatus::Running);
        assert_eq!(info.exit_code, 0);
    }

    #[tokio::test]
    async fn get_process_info_invalid_nonce() {
        let (agent, _shm, _log) = create_test_agent();
        let result = agent.get_process_info(999);
        assert!(result.is_err());
        match result.unwrap_err() {
            AgentError::InvalidNonce(n) => assert_eq!(n, 999),
            other => panic!("expected InvalidNonce, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn replace_nonce_refs_single() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 4567, ProcessStatus::Running, 0)
            .unwrap();
        let result = agent.replace_nonce_refs("kill $NONCE[1]").unwrap();
        assert_eq!(result, "kill 4567");
    }

    #[tokio::test]
    async fn replace_nonce_refs_multiple() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Running, 0)
            .unwrap();
        agent
            .update_process_info(2, 200, ProcessStatus::Running, 0)
            .unwrap();
        let result = agent
            .replace_nonce_refs("echo $NONCE[1] and $NONCE[2]")
            .unwrap();
        assert_eq!(result, "echo 100 and 200");
    }

    #[tokio::test]
    async fn replace_nonce_refs_no_refs() {
        let (agent, _shm, _log) = create_test_agent();
        let result = agent.replace_nonce_refs("echo hello").unwrap();
        assert_eq!(result, "echo hello");
    }

    #[tokio::test]
    async fn replace_nonce_refs_invalid_nonce() {
        let (agent, _shm, _log) = create_test_agent();
        let result = agent.replace_nonce_refs("kill $NONCE[999]");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn inspect_path_existing_file() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();

        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            command: None,
            nonce: 1,
            depending_nonce: None,
            expected_status: None,
            wait: None,
            return_stdout: None,
            return_stderr: None,
            display: None,
            status_type: None,
            path: Some(file_path.to_string_lossy().to_string()),
        };
        let result = agent.inspect_path(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["exists"], true);
        assert_eq!(parsed["type"], "file");
        assert_eq!(parsed["size"], 5);
    }

    #[tokio::test]
    async fn inspect_path_nonexistent() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            command: None,
            nonce: 1,
            depending_nonce: None,
            expected_status: None,
            wait: None,
            return_stdout: None,
            return_stderr: None,
            display: None,
            status_type: None,
            path: Some("/nonexistent/path/xyz".to_string()),
        };
        let result = agent.inspect_path(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["exists"], false);
    }

    #[tokio::test]
    async fn inspect_path_directory() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            command: None,
            nonce: 1,
            depending_nonce: None,
            expected_status: None,
            wait: None,
            return_stdout: None,
            return_stderr: None,
            display: None,
            status_type: None,
            path: Some(tmp.path().to_string_lossy().to_string()),
        };
        let result = agent.inspect_path(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["exists"], true);
        assert_eq!(parsed["type"], "directory");
    }

    #[tokio::test]
    async fn inspect_path_missing_path_field() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            command: None,
            nonce: 1,
            depending_nonce: None,
            expected_status: None,
            wait: None,
            return_stdout: None,
            return_stderr: None,
            display: None,
            status_type: None,
            path: None,
        };
        let result = agent.inspect_path(&cmd);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_status_returns_status_char() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(5, 1000, ProcessStatus::Running, 0)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            command: None,
            nonce: 5,
            depending_nonce: None,
            expected_status: None,
            wait: None,
            return_stdout: None,
            return_stderr: None,
            display: None,
            status_type: Some("status".to_string()),
            path: None,
        };
        let result = agent.fetch_status(&cmd).unwrap();
        assert_eq!(result, "r");
    }

    #[tokio::test]
    async fn fetch_status_returns_exit_code() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(5, 1000, ProcessStatus::Failed, 127)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            command: None,
            nonce: 5,
            depending_nonce: None,
            expected_status: None,
            wait: None,
            return_stdout: None,
            return_stderr: None,
            display: None,
            status_type: Some("exit_code".to_string()),
            path: None,
        };
        let result = agent.fetch_status(&cmd).unwrap();
        assert_eq!(result, "127");
    }

    #[tokio::test]
    async fn fetch_status_stdout_reads_log() {
        let (agent, _shm, log_dir) = create_test_agent();
        agent
            .update_process_info(3, 100, ProcessStatus::Completed, 0)
            .unwrap();
        // Create the log file manually
        fs::write(log_dir.path().join("3_stdout.log"), "hello world").unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            command: None,
            nonce: 3,
            depending_nonce: None,
            expected_status: None,
            wait: None,
            return_stdout: None,
            return_stderr: None,
            display: None,
            status_type: Some("stdout".to_string()),
            path: None,
        };
        let result = agent.fetch_status(&cmd).unwrap();
        assert_eq!(result, "hello world");
    }

    #[tokio::test]
    async fn fetch_status_missing_status_type() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            command: None,
            nonce: 1,
            depending_nonce: None,
            expected_status: None,
            wait: None,
            return_stdout: None,
            return_stderr: None,
            display: None,
            status_type: None,
            path: None,
        };
        let result = agent.fetch_status(&cmd);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_status_invalid_status_type() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Running, 0)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            command: None,
            nonce: 1,
            depending_nonce: None,
            expected_status: None,
            wait: None,
            return_stdout: None,
            return_stderr: None,
            display: None,
            status_type: Some("invalid".to_string()),
            path: None,
        };
        let result = agent.fetch_status(&cmd);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_status_stdout_missing_log_file() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(99, 100, ProcessStatus::Waiting, 0)
            .unwrap();
        // Don't create the log file - it doesn't exist for Waiting processes
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            command: None,
            nonce: 99,
            depending_nonce: None,
            expected_status: None,
            wait: None,
            return_stdout: None,
            return_stderr: None,
            display: None,
            status_type: Some("stdout".to_string()),
            path: None,
        };
        let result = agent.fetch_status(&cmd).unwrap();
        assert_eq!(result, "", "should return empty string for missing log file");
    }

    #[tokio::test]
    async fn check_dependency_completed_matching_exit() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 0)
            .unwrap();
        let result = agent.check_dependency(1, 0, false).await.unwrap();
        assert!(result, "should pass when exit code matches");
    }

    #[tokio::test]
    async fn check_dependency_completed_wrong_exit_no_wait() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 1)
            .unwrap();
        let result = agent.check_dependency(1, 0, false).await.unwrap();
        assert!(!result, "should fail when exit code doesn't match");
    }

    #[tokio::test]
    async fn check_dependency_completed_wrong_exit_with_wait() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 1)
            .unwrap();
        let start = std::time::Instant::now();
        let result = agent.check_dependency(1, 0, true).await.unwrap();
        let elapsed = start.elapsed();
        assert!(!result, "should fail when exit code doesn't match");
        // Completed is a terminal state, should return immediately even with wait=true
        assert!(
            elapsed.as_secs() < 2,
            "check_dependency took {:?} - should be instant for completed process with wrong exit code",
            elapsed
        );
    }

    #[tokio::test]
    async fn check_dependency_failed() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Failed, 1)
            .unwrap();
        let result = agent.check_dependency(1, 0, true).await.unwrap();
        assert!(!result, "should fail on Failed status");
    }

    #[tokio::test]
    async fn check_dependency_skipped() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Skipped, 0)
            .unwrap();
        let result = agent.check_dependency(1, 0, true).await.unwrap();
        assert!(!result, "should fail on Skipped status");
    }

    #[tokio::test]
    async fn check_dependency_invalid_nonce_no_wait() {
        let (agent, _shm, _log) = create_test_agent();
        let result = agent.check_dependency(999, 0, false).await.unwrap();
        assert!(!result, "should fail for invalid nonce without wait");
    }

    #[tokio::test]
    async fn process_input_exec_returns_running() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "execAsAgent".to_string(),
                command: Some("echo hello".to_string()),
                nonce: 1,
                depending_nonce: None,
                expected_status: None,
                wait: None,
                return_stdout: None,
                return_stderr: None,
                display: Some(1),
                status_type: None,
                path: None,
            }],
            wait_for_status: None,
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "1r0");
    }

    #[tokio::test]
    async fn process_input_exec_with_dependency_returns_waiting() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "execAsAgent".to_string(),
                command: Some("echo hello".to_string()),
                nonce: 2,
                depending_nonce: Some(1),
                expected_status: Some(0),
                wait: Some(true),
                return_stdout: None,
                return_stderr: None,
                display: Some(1),
                status_type: None,
                path: None,
            }],
            wait_for_status: None,
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "2w0");
    }

    #[tokio::test]
    async fn process_input_unknown_function() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "unknownFunc".to_string(),
                command: None,
                nonce: 1,
                depending_nonce: None,
                expected_status: None,
                wait: None,
                return_stdout: None,
                return_stderr: None,
                display: None,
                status_type: None,
                path: None,
            }],
            wait_for_status: None,
        };
        let result = agent.process_input(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn process_input_inspect_path() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "inspectPath".to_string(),
                command: None,
                nonce: 1,
                depending_nonce: None,
                expected_status: None,
                wait: None,
                return_stdout: None,
                return_stderr: None,
                display: None,
                status_type: None,
                path: Some("/tmp".to_string()),
            }],
            wait_for_status: None,
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(parsed["exists"], true);
        assert_eq!(parsed["type"], "directory");
    }

    #[tokio::test]
    async fn process_input_with_wait_for_status() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "execAsAgent".to_string(),
                command: Some("echo fast".to_string()),
                nonce: 1,
                depending_nonce: None,
                expected_status: None,
                wait: None,
                return_stdout: None,
                return_stderr: None,
                display: Some(1),
                status_type: None,
                path: None,
            }],
            wait_for_status: Some(200),
        };
        let start = std::time::Instant::now();
        let results = agent.process_input(input).await.unwrap();
        let elapsed = start.elapsed();
        assert_eq!(results.len(), 1);
        // Should have waited at least 200ms
        assert!(elapsed.as_millis() >= 150, "should have waited ~200ms, took {:?}", elapsed);
    }

    #[tokio::test]
    async fn exec_as_agent_creates_log_files() {
        let (agent, _shm, log_dir) = create_test_agent();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            command: Some("echo test_output".to_string()),
            nonce: 10,
            depending_nonce: None,
            expected_status: None,
            wait: None,
            return_stdout: None,
            return_stderr: None,
            display: Some(1),
            status_type: None,
            path: None,
        };
        agent.exec_as_agent(&cmd).await.unwrap();
        // Give the command time to complete
        tokio::time::sleep(Duration::from_millis(500)).await;

        let stdout_path = log_dir.path().join("10_stdout.log");
        let stderr_path = log_dir.path().join("10_stderr.log");
        assert!(stdout_path.exists(), "stdout log should be created");
        assert!(stderr_path.exists(), "stderr log should be created");
        let stdout_content = fs::read_to_string(stdout_path).unwrap();
        assert_eq!(stdout_content.trim(), "test_output");
    }

    #[tokio::test]
    async fn exec_as_agent_missing_command() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            command: None,
            nonce: 1,
            depending_nonce: None,
            expected_status: None,
            wait: None,
            return_stdout: None,
            return_stderr: None,
            display: None,
            status_type: None,
            path: None,
        };
        let result = agent.exec_as_agent(&cmd).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn update_process_status_preserves_pid() {
        let (agent, _shm, _log) = create_test_agent();
        // Set a process with a real PID
        agent
            .update_process_info(1, 9999, ProcessStatus::Running, 0)
            .unwrap();

        // Simulate status update (what happens when process completes)
        Agent::update_process_status(
            agent.shared_mem.clone(),
            1,
            ProcessStatus::Completed,
            0,
        )
        .unwrap();

        // Read the process info back - PID should still be 9999
        let info = agent.get_process_info(1).unwrap();
        assert_eq!(info.status, ProcessStatus::Completed);
        assert_eq!(
            info.pid, 9999,
            "PID should be preserved after status update"
        );
    }

    #[tokio::test]
    async fn rebuild_process_map_finds_existing_entries() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(10, 100, ProcessStatus::Running, 0)
            .unwrap();
        agent
            .update_process_info(20, 200, ProcessStatus::Completed, 0)
            .unwrap();

        let mmap = agent.shared_mem.read().unwrap();
        let map = Agent::rebuild_process_map(&mmap);
        assert!(map.contains_key(&10));
        assert!(map.contains_key(&20));
        assert!(!map.contains_key(&30));
    }
}
