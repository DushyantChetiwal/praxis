use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use util::ResultExt as _;
use uuid::Uuid;

pub const UNDER_WATCHDOG_ENV_VAR: &str = "PRAXIS_UNDER_WATCHDOG";

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_secs(2);
const HEARTBEAT_STALL_TIMEOUT_MS: u64 = 45_000;
const RESTART_REQUEST_MAX_AGE_MS: u64 = 60_000;
const SHORT_RUN_THRESHOLD_MS: u64 = 10_000;
const MAX_CONSECUTIVE_SHORT_RUNS: u32 = 3;
const CRASH_WINDOW_MS: u64 = 5 * 60 * 1_000;
const MAX_RESTARTS_IN_WINDOW: usize = 5;
const BACKOFF_SCHEDULE_MS: [u64; 3] = [1_000, 2_000, 5_000];

const HEARTBEAT_FILE: &str = "heartbeat.json";
const RESTART_REQUEST_FILE: &str = "restart_request.json";
const STATUS_FILE: &str = "status.json";
const TOKEN_FILE: &str = "watchdog.token";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogHealth {
    pub supervised: bool,
    pub supervisor_pid: Option<u32>,
    pub child_pid: Option<u32>,
    pub restarts: u32,
    pub heartbeat_age_ms: Option<u64>,
    pub last_exit_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Heartbeat {
    pid: u32,
    started_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    headless: bool,
    clean_exit: bool,
    auth_token_sha256: String,
    open_workspaces: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RestartRequest {
    token: String,
    requested_at_unix_ms: u64,
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WatchdogStatus {
    supervisor_pid: u32,
    child_pid: Option<u32>,
    restarts: u32,
    last_exit_reason: Option<String>,
    updated_at_unix_ms: u64,
}

struct HeartbeatRuntimeState {
    directory: PathBuf,
    pid: u32,
    started_at_unix_ms: u64,
    headless: bool,
    auth_token_sha256: String,
    open_workspaces: Vec<String>,
}

static CLEAN_EXIT: AtomicBool = AtomicBool::new(false);
static HEARTBEAT_STARTED: AtomicBool = AtomicBool::new(false);
static HEARTBEAT_STATE: OnceLock<Mutex<HeartbeatRuntimeState>> = OnceLock::new();

pub fn watchdog_dir() -> PathBuf {
    paths::support_dir().join("watchdog")
}

pub fn set_open_workspaces(workspaces: Vec<String>) {
    if let Some(state_lock) = HEARTBEAT_STATE.get() {
        let mut state = state_lock.lock();
        state.open_workspaces = workspaces;
    }
}

pub fn start_heartbeat_writer(headless: bool) {
    if HEARTBEAT_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    let directory = watchdog_dir();
    if let Err(error) = ensure_watchdog_dir(&directory) {
        log::error!("Failed to initialize watchdog directory: {error:#}");
        return;
    }

    let auth_token_sha256 = match load_or_create_token(&directory) {
        Ok(token) => sha256_hex(token.as_bytes()),
        Err(error) => {
            log::error!("Failed to initialize watchdog token: {error:#}");
            String::new()
        }
    };

    let now_ms = current_unix_ms();
    let initial_state = HeartbeatRuntimeState {
        directory,
        pid: std::process::id(),
        started_at_unix_ms: now_ms,
        headless,
        auth_token_sha256,
        open_workspaces: Vec::new(),
    };

    let state_lock = HEARTBEAT_STATE.get_or_init(|| Mutex::new(initial_state));
    write_current_heartbeat(state_lock, CLEAN_EXIT.load(Ordering::SeqCst));

    let spawn_result = thread::Builder::new()
        .name("praxis-watchdog-heartbeat".to_string())
        .spawn(move || {
            loop {
                thread::sleep(HEARTBEAT_INTERVAL);
                let clean_exit = CLEAN_EXIT.load(Ordering::SeqCst);
                write_current_heartbeat(state_lock, clean_exit);
                if clean_exit {
                    break;
                }
            }
        });

    if let Err(error) = spawn_result {
        log::error!("Failed to spawn watchdog heartbeat thread: {error:#}");
    }
}

pub fn mark_clean_shutdown() {
    CLEAN_EXIT.store(true, Ordering::SeqCst);
    if let Some(state_lock) = HEARTBEAT_STATE.get() {
        write_current_heartbeat(state_lock, true);
    }
}

pub fn read_watchdog_health() -> WatchdogHealth {
    read_watchdog_health_in(&watchdog_dir(), current_unix_ms())
}

pub fn read_watchdog_health_in(directory: &Path, now_ms: u64) -> WatchdogHealth {
    let status = read_json::<WatchdogStatus>(&directory.join(STATUS_FILE));
    let heartbeat = read_json::<Heartbeat>(&directory.join(HEARTBEAT_FILE));

    let heartbeat_age_ms = heartbeat.map(|entry| now_ms.saturating_sub(entry.updated_at_unix_ms));

    match status {
        Some(status) => WatchdogHealth {
            supervised: status.child_pid.is_some(),
            supervisor_pid: Some(status.supervisor_pid),
            child_pid: status.child_pid,
            restarts: status.restarts,
            heartbeat_age_ms,
            last_exit_reason: status.last_exit_reason,
        },
        None => WatchdogHealth {
            supervised: false,
            supervisor_pid: None,
            child_pid: None,
            restarts: 0,
            heartbeat_age_ms,
            last_exit_reason: None,
        },
    }
}

pub fn request_supervised_restart(reason: &str) -> Result<bool> {
    request_restart_in(&watchdog_dir(), reason, current_unix_ms())
}

pub fn request_restart_in(directory: &Path, reason: &str, now_ms: u64) -> Result<bool> {
    let token_path = directory.join(TOKEN_FILE);
    let token = match fs::read_to_string(&token_path) {
        Ok(contents) => contents.trim().to_string(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", token_path.display()));
        }
    };

    if token.is_empty() {
        return Ok(false);
    }

    let request = RestartRequest {
        token,
        requested_at_unix_ms: now_ms,
        reason: reason.to_string(),
    };
    let payload = serde_json::to_vec_pretty(&request)?;
    write_atomic(&directory.join(RESTART_REQUEST_FILE), &payload)?;
    Ok(true)
}

pub fn run_watchdog_supervisor(child_args: Vec<std::ffi::OsString>) -> Result<()> {
    let directory = watchdog_dir();
    ensure_watchdog_dir(&directory)?;
    let token = load_or_create_token(&directory)?;

    let executable = env::current_exe().context("resolving current executable for watchdog")?;
    let filtered_args: Vec<std::ffi::OsString> = child_args
        .into_iter()
        .filter(|argument| argument != "--watchdog")
        .collect();

    let supervisor_pid = std::process::id();
    let mut restarts: u32 = 0;
    let mut last_exit_reason: Option<String> = None;
    let mut crash_guard = CrashLoopGuard::default();

    let mut child = spawn_supervised_child(&executable, &filtered_args)?;
    let mut child_started_at = Instant::now();
    write_status(
        &directory,
        supervisor_pid,
        Some(child.id()),
        restarts,
        last_exit_reason.clone(),
    );

    loop {
        thread::sleep(SUPERVISOR_POLL_INTERVAL);
        let now_ms = current_unix_ms();

        if let Some(reason) = consume_valid_restart_request(&directory, &token, now_ms) {
            terminate_child(&mut child);
            thread::sleep(Duration::from_millis(500));
            restarts = restarts.saturating_add(1);
            last_exit_reason = Some(format!("restart_requested: {reason}"));
            child = spawn_supervised_child(&executable, &filtered_args)?;
            child_started_at = Instant::now();
            write_status(
                &directory,
                supervisor_pid,
                Some(child.id()),
                restarts,
                last_exit_reason.clone(),
            );
            continue;
        }

        let child_pid = child.id();
        match child.try_wait() {
            Ok(Some(exit_status)) => {
                let heartbeat = read_json::<Heartbeat>(&directory.join(HEARTBEAT_FILE));
                let clean_exit = exit_status.success()
                    || heartbeat
                        .as_ref()
                        .is_some_and(|entry| entry.clean_exit && entry.pid == child_pid);
                let lived_ms = child_started_at.elapsed().as_millis() as u64;

                match crash_guard.should_restart_after_exit(clean_exit, lived_ms, now_ms) {
                    RestartDecision::CleanExit => {
                        write_status(
                            &directory,
                            supervisor_pid,
                            None,
                            restarts,
                            Some("clean_exit".to_string()),
                        );
                        return Ok(());
                    }
                    RestartDecision::AbortCrashLoop => {
                        write_status(
                            &directory,
                            supervisor_pid,
                            None,
                            restarts,
                            Some("crash_loop_aborted".to_string()),
                        );
                        anyhow::bail!(
                            "Praxis crashed repeatedly (last status: {exit_status}); aborting watchdog restart loop"
                        );
                    }
                    RestartDecision::Restart { backoff_ms } => {
                        restarts = restarts.saturating_add(1);
                        last_exit_reason = Some(format!("exit_status: {exit_status}"));
                        write_status(
                            &directory,
                            supervisor_pid,
                            None,
                            restarts,
                            last_exit_reason.clone(),
                        );
                        thread::sleep(Duration::from_millis(backoff_ms));
                        child = spawn_supervised_child(&executable, &filtered_args)?;
                        child_started_at = Instant::now();
                        write_status(
                            &directory,
                            supervisor_pid,
                            Some(child.id()),
                            restarts,
                            last_exit_reason.clone(),
                        );
                    }
                }
            }
            Ok(None) => {
                let lived_ms = child_started_at.elapsed().as_millis() as u64;
                if lived_ms > HEARTBEAT_STALL_TIMEOUT_MS
                    && let Some(heartbeat) = read_json::<Heartbeat>(&directory.join(HEARTBEAT_FILE))
                    && heartbeat.pid == child_pid
                    && !heartbeat.clean_exit
                    && now_ms.saturating_sub(heartbeat.updated_at_unix_ms)
                        > HEARTBEAT_STALL_TIMEOUT_MS
                {
                    terminate_child(&mut child);
                    match crash_guard.should_restart_after_exit(false, lived_ms, now_ms) {
                        RestartDecision::CleanExit => unreachable!(),
                        RestartDecision::AbortCrashLoop => {
                            write_status(
                                &directory,
                                supervisor_pid,
                                None,
                                restarts,
                                Some("crash_loop_aborted".to_string()),
                            );
                            anyhow::bail!(
                                "Praxis heartbeat stalled repeatedly; aborting watchdog restart loop"
                            );
                        }
                        RestartDecision::Restart { backoff_ms } => {
                            restarts = restarts.saturating_add(1);
                            last_exit_reason = Some("heartbeat_stalled".to_string());
                            write_status(
                                &directory,
                                supervisor_pid,
                                None,
                                restarts,
                                last_exit_reason.clone(),
                            );
                            thread::sleep(Duration::from_millis(backoff_ms));
                            child = spawn_supervised_child(&executable, &filtered_args)?;
                            child_started_at = Instant::now();
                            write_status(
                                &directory,
                                supervisor_pid,
                                Some(child.id()),
                                restarts,
                                last_exit_reason.clone(),
                            );
                        }
                    }
                }
            }
            Err(error) => {
                log::error!("Watchdog failed to poll child process: {error:#}");
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartDecision {
    CleanExit,
    Restart { backoff_ms: u64 },
    AbortCrashLoop,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct CrashLoopGuard {
    consecutive_short_runs: u32,
    recent_restart_timestamps_ms: Vec<u64>,
}

impl CrashLoopGuard {
    pub(crate) fn should_restart_after_exit(
        &mut self,
        clean_exit: bool,
        lived_ms: u64,
        now_ms: u64,
    ) -> RestartDecision {
        if clean_exit {
            return RestartDecision::CleanExit;
        }

        if lived_ms < SHORT_RUN_THRESHOLD_MS {
            self.consecutive_short_runs = self.consecutive_short_runs.saturating_add(1);
        } else {
            self.consecutive_short_runs = 0;
        }

        if self.consecutive_short_runs >= MAX_CONSECUTIVE_SHORT_RUNS {
            return RestartDecision::AbortCrashLoop;
        }

        self.recent_restart_timestamps_ms
            .retain(|&timestamp_ms| now_ms.saturating_sub(timestamp_ms) <= CRASH_WINDOW_MS);

        if self.recent_restart_timestamps_ms.len() >= MAX_RESTARTS_IN_WINDOW {
            return RestartDecision::AbortCrashLoop;
        }

        self.recent_restart_timestamps_ms.push(now_ms);
        let backoff_index = self
            .consecutive_short_runs
            .saturating_sub(1)
            .min((BACKOFF_SCHEDULE_MS.len() - 1) as u32) as usize;

        RestartDecision::Restart {
            backoff_ms: BACKOFF_SCHEDULE_MS[backoff_index],
        }
    }
}

fn spawn_supervised_child(
    executable: &Path,
    filtered_args: &[std::ffi::OsString],
) -> Result<Child> {
    Command::new(executable)
        .args(filtered_args)
        .env(UNDER_WATCHDOG_ENV_VAR, "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning supervised child {}", executable.display()))
}

fn terminate_child(child: &mut Child) {
    if let Err(error) = child.kill() {
        log::warn!("Failed to kill child process {}: {error:#}", child.id());
    }
    if let Err(error) = child.wait() {
        log::warn!("Failed to wait for child process {}: {error:#}", child.id());
    }
}

fn consume_valid_restart_request(
    directory: &Path,
    expected_token: &str,
    now_ms: u64,
) -> Option<String> {
    let request_path = directory.join(RESTART_REQUEST_FILE);
    let contents = match fs::read(&request_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            log::warn!("Failed to read restart request: {error:#}");
            return None;
        }
    };

    if let Err(error) = fs::remove_file(&request_path)
        && error.kind() != io::ErrorKind::NotFound
    {
        log::warn!("Failed to remove restart request file: {error:#}");
    }

    let request: RestartRequest = match serde_json::from_slice(&contents) {
        Ok(parsed) => parsed,
        Err(error) => {
            log::warn!("Ignoring malformed restart request: {error:#}");
            return None;
        }
    };

    if !validate_restart_request(&request, expected_token, now_ms) {
        log::warn!("Ignoring unauthorized or expired restart request");
        return None;
    }

    Some(request.reason)
}

fn validate_restart_request(request: &RestartRequest, expected_token: &str, now_ms: u64) -> bool {
    let token_matches = constant_time_eq(
        request.token.trim().as_bytes(),
        expected_token.trim().as_bytes(),
    );
    let not_expired =
        now_ms.saturating_sub(request.requested_at_unix_ms) <= RESTART_REQUEST_MAX_AGE_MS;
    let not_far_future =
        request.requested_at_unix_ms.saturating_sub(now_ms) <= RESTART_REQUEST_MAX_AGE_MS;

    token_matches && not_expired && not_far_future
}

fn write_current_heartbeat(state_lock: &Mutex<HeartbeatRuntimeState>, clean_exit: bool) {
    let (directory, heartbeat) = {
        let state = state_lock.lock();
        let heartbeat = Heartbeat {
            pid: state.pid,
            started_at_unix_ms: state.started_at_unix_ms,
            updated_at_unix_ms: current_unix_ms(),
            headless: state.headless,
            clean_exit,
            auth_token_sha256: state.auth_token_sha256.clone(),
            open_workspaces: state.open_workspaces.clone(),
        };
        (state.directory.clone(), heartbeat)
    };

    if let Some(payload) = serde_json::to_vec_pretty(&heartbeat).log_err() {
        write_atomic(&directory.join(HEARTBEAT_FILE), &payload).log_err();
    }
}

fn write_status(
    directory: &Path,
    supervisor_pid: u32,
    child_pid: Option<u32>,
    restarts: u32,
    last_exit_reason: Option<String>,
) {
    let status = WatchdogStatus {
        supervisor_pid,
        child_pid,
        restarts,
        last_exit_reason,
        updated_at_unix_ms: current_unix_ms(),
    };
    if let Some(payload) = serde_json::to_vec_pretty(&status).log_err() {
        write_atomic(&directory.join(STATUS_FILE), &payload).log_err();
    }
}

fn ensure_watchdog_dir(directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn load_or_create_token(directory: &Path) -> io::Result<String> {
    let token_path = directory.join(TOKEN_FILE);
    if let Ok(existing) = fs::read_to_string(&token_path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let mut random_bytes = [0u8; 32];
    random_bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    random_bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    let token = encode_hex(&random_bytes);

    write_atomic(&token_path, token.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("watchdog");
    let temp_path = path.with_file_name(format!("{file_name}.tmp"));
    fs::write(&temp_path, contents)?;
    fs::rename(&temp_path, path)?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (&left_byte, &right_byte) in left.iter().zip(right.iter()) {
        diff |= left_byte ^ right_byte;
    }
    diff == 0
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        output.push(HEX_DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn sha256_hex(input: &[u8]) -> String {
    const INITIAL_STATE: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const ROUND_CONSTANTS: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = input.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut hash = INITIAL_STATE;
    for chunk in padded.chunks_exact(64) {
        let mut schedule = [0u32; 64];
        for (index, word_bytes) in chunk.chunks_exact(4).enumerate() {
            schedule[index] =
                u32::from_be_bytes([word_bytes[0], word_bytes[1], word_bytes[2], word_bytes[3]]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = hash;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(ROUND_CONSTANTS[index])
                .wrapping_add(schedule[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        hash[0] = hash[0].wrapping_add(a);
        hash[1] = hash[1].wrapping_add(b);
        hash[2] = hash[2].wrapping_add(c);
        hash[3] = hash[3].wrapping_add(d);
        hash[4] = hash[4].wrapping_add(e);
        hash[5] = hash[5].wrapping_add(f);
        hash[6] = hash[6].wrapping_add(g);
        hash[7] = hash[7].wrapping_add(h);
    }

    let mut digest = [0u8; 32];
    for (index, word) in hash.iter().enumerate() {
        digest[index * 4..(index + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    encode_hex(&digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = env::temp_dir().join(format!("praxis-watchdog-test-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sha256_matches_nist_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn atomic_heartbeat_and_health_read() {
        let temp_dir = TestDir::new();
        let dir = temp_dir.path().join("watchdog");
        ensure_watchdog_dir(&dir).unwrap();
        let token = load_or_create_token(&dir).unwrap();
        assert_eq!(token.len(), 64);

        write_status(
            &dir,
            1001,
            Some(2002),
            2,
            Some("exit_status: 1".to_string()),
        );
        let heartbeat = Heartbeat {
            pid: 2002,
            started_at_unix_ms: 10_000,
            updated_at_unix_ms: 25_000,
            headless: true,
            clean_exit: false,
            auth_token_sha256: sha256_hex(token.as_bytes()),
            open_workspaces: vec!["/repo/a".to_string()],
        };
        write_atomic(
            &dir.join(HEARTBEAT_FILE),
            &serde_json::to_vec_pretty(&heartbeat).unwrap(),
        )
        .unwrap();

        let health = read_watchdog_health_in(&dir, 30_000);
        assert_eq!(
            health,
            WatchdogHealth {
                supervised: true,
                supervisor_pid: Some(1001),
                child_pid: Some(2002),
                restarts: 2,
                heartbeat_age_ms: Some(5_000),
                last_exit_reason: Some("exit_status: 1".to_string()),
            }
        );
    }

    #[test]
    fn authenticated_restart_request_validation() {
        let temp_dir = TestDir::new();
        let dir = temp_dir.path().join("watchdog");
        ensure_watchdog_dir(&dir).unwrap();

        assert!(!request_restart_in(&dir, "no_token_yet", 50_000).unwrap());

        let token = load_or_create_token(&dir).unwrap();
        assert!(request_restart_in(&dir, "user_requested", 50_000).unwrap());

        assert_eq!(
            consume_valid_restart_request(&dir, &token, 60_000),
            Some("user_requested".to_string())
        );
        // Consumed once, not replayed.
        assert_eq!(consume_valid_restart_request(&dir, &token, 60_000), None);

        // Wrong token is rejected.
        let wrong_request = RestartRequest {
            token: "wrong-token".to_string(),
            requested_at_unix_ms: 50_000,
            reason: "bad".to_string(),
        };
        write_atomic(
            &dir.join(RESTART_REQUEST_FILE),
            &serde_json::to_vec(&wrong_request).unwrap(),
        )
        .unwrap();
        assert_eq!(consume_valid_restart_request(&dir, &token, 55_000), None);

        // Expired request (>60s old) is rejected.
        assert!(request_restart_in(&dir, "stale", 10_000).unwrap());
        assert_eq!(consume_valid_restart_request(&dir, &token, 80_000), None);
    }

    #[test]
    fn crash_loop_circuit_breaker_and_clean_exit() {
        let mut guard = CrashLoopGuard::default();

        // Clean exit never restarts.
        assert_eq!(
            guard.should_restart_after_exit(true, 100, 1_000),
            RestartDecision::CleanExit
        );

        // 3 consecutive short runs (<10s) abort on the 3rd failure.
        assert_eq!(
            guard.should_restart_after_exit(false, 2_000, 10_000),
            RestartDecision::Restart { backoff_ms: 1_000 }
        );
        assert_eq!(
            guard.should_restart_after_exit(false, 3_000, 15_000),
            RestartDecision::Restart { backoff_ms: 2_000 }
        );
        assert_eq!(
            guard.should_restart_after_exit(false, 4_000, 20_000),
            RestartDecision::AbortCrashLoop
        );

        // Longer runs (>10s) reset consecutive short run count, but >5 restarts in 5 minutes aborts.
        let mut window_guard = CrashLoopGuard::default();
        for index in 0..5 {
            let now_ms = 100_000 + (index as u64) * 20_000;
            assert_eq!(
                window_guard.should_restart_after_exit(false, 15_000, now_ms),
                RestartDecision::Restart { backoff_ms: 1_000 }
            );
        }
        assert_eq!(
            window_guard.should_restart_after_exit(false, 15_000, 220_000),
            RestartDecision::AbortCrashLoop
        );
    }
}
