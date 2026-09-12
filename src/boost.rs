use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::account_store::{Store, UsageRecord, now_epoch};
use crate::app_server::{CancellationToken, query_profile_cancellable, set_baseline_defaults};
use crate::boost_policy::{
    BASELINE_EFFORT, BASELINE_TIER, BOOST_EFFORT, BOOST_MODEL, FAST_TIER, has_weekly_quota,
    parse_deadline, weekly_reset_detected,
};
use crate::config::Config;
use crate::control::ControlClient;
use crate::fs::{ExclusiveLock, atomic_write, remove_file_if_exists};
use crate::{Error, Result};

const POLL_INTERVAL: Duration = Duration::from_secs(15);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Subcommand)]
pub enum BoostCommand {
    /// Boost tasks on connected Codex servers until a reset or deadline.
    Start {
        /// Required RFC 3339 deadline, including Z or a numeric timezone offset.
        #[arg(long)]
        until: String,
        /// Existing app-server Unix socket. Repeat for multiple servers.
        #[arg(long)]
        socket: Vec<PathBuf>,
    },
    /// Show controller state, deadline, and any unfinished cleanup.
    Status,
    /// Interrupt affected tasks and set Astra, Medium, and Standard everywhere.
    Stop,
    #[command(hide = true)]
    Run {
        #[arg(long)]
        until: String,
        #[arg(long)]
        socket: Vec<PathBuf>,
        #[arg(long)]
        run_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum Phase {
    Preparing,
    Active,
    Stopping,
    Stopped,
    CleanupRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
struct Task {
    socket: PathBuf,
    thread_id: String,
    child: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct State {
    run_id: String,
    phase: Phase,
    deadline: i64,
    sockets: Vec<PathBuf>,
    tasks: BTreeSet<Task>,
    activated: bool,
    defaults_pending: bool,
    reason: Option<String>,
    errors: Vec<String>,
}

impl State {
    fn preparing(run_id: String, deadline: i64, sockets: Vec<PathBuf>) -> Self {
        Self {
            run_id,
            phase: Phase::Preparing,
            deadline,
            sockets,
            tasks: BTreeSet::new(),
            activated: false,
            defaults_pending: false,
            reason: None,
            errors: Vec::new(),
        }
    }
}

struct Files {
    state: PathBuf,
    lock: PathBuf,
    stop: PathBuf,
    log: PathBuf,
}

impl Files {
    fn new(config: &Config) -> Self {
        let directory = config.account_store.join("boost");
        Self {
            state: directory.join("state.json"),
            lock: directory.join("controller.lock"),
            stop: directory.join("stop"),
            log: directory.join("controller.log"),
        }
    }

    fn read(&self) -> Result<Option<State>> {
        match fs::read(&self.state) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| Error::json(&self.state, error)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::io(&self.state, error)),
        }
    }

    fn save(&self, state: &State) -> Result<()> {
        let bytes =
            serde_json::to_vec_pretty(state).map_err(|error| Error::Protocol(error.to_string()))?;
        atomic_write(&self.state, &bytes, 0o600)
    }
}

pub fn run(command: BoostCommand, config: Config) -> Result<()> {
    match command {
        BoostCommand::Start { until, socket } => start(&config, &until, socket),
        BoostCommand::Status => status(&config),
        BoostCommand::Stop => stop(&config),
        BoostCommand::Run {
            until,
            socket,
            run_id,
        } => {
            let deadline = parse_deadline(&until, now_epoch())?;
            monitor(&config, deadline, socket, run_id)
        }
    }
}

fn sockets(config: &Config, values: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let values = if values.is_empty() {
        vec![
            config
                .codex_home
                .join("app-server-control/app-server-control.sock"),
        ]
    } else {
        values
    };
    let mut unique = BTreeSet::new();
    for path in values {
        if !path.is_absolute() {
            return Err(Error::Message(
                "Boost socket paths must be absolute.".into(),
            ));
        }
        unique.insert(path);
    }
    Ok(unique.into_iter().collect())
}

fn connect(config: &Config, path: &Path) -> Result<ControlClient> {
    ControlClient::connect_with_timeout(config, path, CONTROL_TIMEOUT).map_err(|error| Error::Message(format!(
        "Cannot control Codex at {}: {error}. Boost requires an existing app-server Unix socket; desktop sessions using private stdio are not covered. No process is restarted or replaced.",
        path.display()
    )))
}

fn preflight(config: &Config, paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        let mut client = connect(config, path)?;
        let account = client.request("account/read", json!({"refreshToken": false}))?;
        if account.pointer("/account/type").and_then(Value::as_str) != Some("chatgpt") {
            return Err(Error::Message(format!(
                "Boost requires a ChatGPT account on {}; API-key sessions are not supported.",
                path.display()
            )));
        }
        require_boost_model(&mut client)?;
        loaded_tasks(&mut client, path)?;
    }
    Ok(())
}

fn start(config: &Config, until: &str, supplied_sockets: Vec<PathBuf>) -> Result<()> {
    let deadline = parse_deadline(until, now_epoch())?;
    let paths = sockets(config, supplied_sockets)?;
    let files = Files::new(config);
    let lock = ExclusiveLock::try_acquire(&files.lock)?.ok_or_else(|| {
        Error::Message("Boost already has a running controller. Use `cxa boost status`.".into())
    })?;
    if files
        .read()?
        .is_some_and(|state| state.phase != Phase::Stopped)
    {
        return Err(Error::Message(
            "An earlier boost needs cleanup. Run `cxa boost stop` before starting another.".into(),
        ));
    }
    preflight(config, &paths)?;
    if deadline <= now_epoch() {
        return Err(Error::Message(
            "Boost deadline passed during preflight; nothing was changed.".into(),
        ));
    }
    let run_id = format!(
        "{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_micros()
    );
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&files.log)
        .map_err(|error| Error::io(&files.log, error))?;
    let mut command =
        Command::new(std::env::current_exe().map_err(|error| Error::io("cxa", error))?);
    command.args(["boost", "run", "--until", until, "--run-id", &run_id]);
    for path in &paths {
        command.arg("--socket").arg(path);
    }
    command
        .stdin(Stdio::null())
        .stdout(
            log.try_clone()
                .map_err(|error| Error::io(&files.log, error))?,
        )
        .stderr(log);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    remove_file_if_exists(&files.stop)?;
    files.save(&State::preparing(run_id.clone(), deadline, paths.clone()))?;
    drop(lock);
    let mut child = command.spawn().map_err(|error| {
        Error::Message(format!("Could not start boost controller: {error}. Run `cxa boost stop` to clear the pending start."))
    })?;
    let wait_until = Instant::now() + Duration::from_secs(5);
    loop {
        let controller_running = ExclusiveLock::try_acquire(&files.lock)?.is_none();
        if let Some(state) = files.read()?.filter(|state| {
            state.run_id == run_id && (state.phase != Phase::Preparing || controller_running)
        }) {
            if !matches!(state.phase, Phase::Preparing | Phase::Active) {
                return Err(Error::Message(format!(
                    "Boost did not activate: {}. Inspect `cxa boost status`.",
                    state.reason.as_deref().unwrap_or("controller stopped")
                )));
            }
            println!(
                "Boost controller started; preparing {} server(s).",
                state.sockets.len()
            );
            println!("Deadline: {until}. Cutoff: Astra / Medium / Standard.");
            println!("Only tasks on these sockets are covered:");
            for path in &paths {
                println!("  {}", path.display());
            }
            println!(
                "Run `cxa boost status` to check activation, or `cxa boost stop` to stop early."
            );
            return Ok(());
        }
        if let Some(exit) = child
            .try_wait()
            .map_err(|error| Error::io("cxa boost controller", error))?
        {
            return Err(Error::Message(format!(
                "Boost controller exited ({exit}); see {}.",
                files.log.display()
            )));
        }
        if Instant::now() >= wait_until {
            return Err(Error::Message(format!(
                "Controller startup is not confirmed; inspect `cxa boost status` and {} before retrying.",
                files.log.display()
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn status(config: &Config) -> Result<()> {
    let files = Files::new(config);
    let Some(state) = files.read()? else {
        println!(
            "Boost is off. Start it with `cxa boost start --until <RFC3339> --socket <PATH>`. "
        );
        return Ok(());
    };
    let running = ExclusiveLock::try_acquire(&files.lock)?.is_none();
    println!(
        "Boost: {:?}; controller {}",
        state.phase,
        if running { "running" } else { "not running" }
    );
    if let Some(time) = chrono::DateTime::from_timestamp(state.deadline, 0) {
        println!("Deadline: {}", time.to_rfc3339());
    }
    println!(
        "Tracked tasks: {}. Cutoff settings: Astra / Medium / Standard.",
        state.tasks.len()
    );
    for path in &state.sockets {
        println!("Server: {}", path.display());
    }
    if let Some(reason) = &state.reason {
        println!("Reason: {reason}");
    }
    for error in &state.errors {
        println!("Unfinished: {error}");
    }
    if !running && state.phase != Phase::Stopped {
        println!(
            "Cleanup is required. Run `cxa boost stop`; do not assume the recorded tasks stopped."
        );
    }
    Ok(())
}

fn stop(config: &Config) -> Result<()> {
    let files = Files::new(config);
    if let Some(_lock) = ExclusiveLock::try_acquire(&files.lock)? {
        let Some(mut state) = files.read()? else {
            println!("Boost is off.");
            return Ok(());
        };
        if state.phase == Phase::Stopped {
            println!("Boost is already stopped.");
            return Ok(());
        }
        state.reason = Some("Manual stop or recovery".into());
        cleanup(config, &files, &mut state)?;
        println!("Boost stopped. Affected tasks and defaults use Astra / Medium / Standard.");
        return Ok(());
    }
    atomic_write(&files.stop, b"stop\n", 0o600)?;
    println!(
        "Stop requested. The controller will interrupt tasks and apply Astra / Medium / Standard."
    );
    status(config)
}

extern "C" fn handle_stop(_: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
}

fn monitor(config: &Config, deadline: i64, paths: Vec<PathBuf>, run_id: String) -> Result<()> {
    let files = Files::new(config);
    let _lock = ExclusiveLock::try_acquire(&files.lock)?
        .ok_or_else(|| Error::Message("Another boost controller won the startup race.".into()))?;
    if let Some(previous) = files.read()? {
        if previous.run_id == run_id && previous.phase == Phase::Stopped {
            return Ok(());
        }
        let pending_start =
            previous.run_id == run_id && previous.phase == Phase::Preparing && !previous.activated;
        if previous.phase != Phase::Stopped && !pending_start {
            return Err(Error::Message(
                "Previous boost cleanup is incomplete.".into(),
            ));
        }
        if previous.run_id != run_id && previous.phase == Phase::Stopped {
            remove_file_if_exists(&files.stop)?;
        }
    }
    STOP_REQUESTED.store(false, Ordering::Relaxed);
    unsafe {
        libc::signal(libc::SIGINT, handle_stop as *const () as libc::sighandler_t);
        libc::signal(
            libc::SIGTERM,
            handle_stop as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGHUP, handle_stop as *const () as libc::sighandler_t);
    }
    let mut state = State::preparing(run_id, deadline, paths);
    files.save(&state)?;
    let result = match active_loop(config, &files, &mut state) {
        Err(_) if cutoff(&files, deadline).is_some() => {
            Ok(cutoff(&files, deadline).unwrap_or_else(|| "Cutoff reached".into()))
        }
        result => result,
    };
    state.reason = Some(match &result {
        Ok(reason) => reason.clone(),
        Err(error) => format!("Stopped after error: {error}"),
    });
    cleanup(config, &files, &mut state)?;
    result.map(|_| ())
}

fn cutoff(files: &Files, deadline: i64) -> Option<String> {
    if STOP_REQUESTED.load(Ordering::Relaxed) || files.stop.exists() {
        Some("Stop requested".into())
    } else if now_epoch() >= deadline {
        Some("Deadline reached".into())
    } else {
        None
    }
}

fn active_loop(config: &Config, files: &Files, state: &mut State) -> Result<String> {
    preflight(config, &state.sockets)?;
    let quotas = Quotas::start(config.clone());
    let mut previous = loop {
        if let Some(reason) = cutoff(files, state.deadline) {
            return Ok(reason);
        }
        match quotas.receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => break result?,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::Message("Quota monitor stopped.".into()));
            }
        }
    };
    if let Some(reason) = cutoff(files, state.deadline) {
        return Ok(reason);
    }
    state.defaults_pending = true;
    state.activated = true;
    files.save(state)?;
    state.phase = Phase::Active;
    files.save(state)?;
    let mut last_quota = Instant::now();
    loop {
        if let Some(reason) = cutoff(files, state.deadline) {
            return Ok(reason);
        }
        while let Ok(result) = quotas.receiver.try_recv() {
            let current = result?;
            for (slot, usage) in &current {
                if previous
                    .get(slot)
                    .is_some_and(|old| weekly_reset_detected(old, usage))
                {
                    return Ok(format!(
                        "Codex weekly quota reset observed on account {slot}"
                    ));
                }
            }
            previous = current;
            last_quota = Instant::now();
        }
        if last_quota.elapsed() > Duration::from_secs(60) {
            return Err(Error::Message(
                "No fresh quota observations for 60 seconds.".into(),
            ));
        }
        for path in state.sockets.clone() {
            let mut client = connect(config, &path)?;
            client.set_timeout(CONTROL_TIMEOUT);
            let remaining = state
                .deadline
                .saturating_mul(1000)
                .saturating_sub(chrono::Utc::now().timestamp_millis())
                .max(0) as u64;
            client.set_deadline(Instant::now() + Duration::from_millis(remaining));
            for task in loaded_tasks(&mut client, &path)? {
                if let Some(reason) = cutoff(files, state.deadline) {
                    return Ok(reason);
                }
                let Some(settings) = live_settings(&mut client, &task.thread_id)? else {
                    continue;
                };
                let newly_enrolled = state.tasks.insert(task.clone());
                if newly_enrolled {
                    files.save(state)?;
                }
                boost_task(
                    &mut client,
                    &task,
                    &settings,
                    files,
                    state.deadline,
                    newly_enrolled,
                )?;
            }
        }
        for _ in 0..10 {
            if let Some(reason) = cutoff(files, state.deadline) {
                return Ok(reason);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn require_boost_model(client: &mut ControlClient) -> Result<()> {
    let mut cursor = Value::Null;
    let mut cursors = BTreeSet::new();
    loop {
        let page = client.request(
            "model/list",
            json!({"cursor": cursor, "includeHidden": false}),
        )?;
        for model in array(&page, "data")? {
            if model.get("model").and_then(Value::as_str) != Some(BOOST_MODEL) {
                continue;
            }
            let effort = |name| {
                model
                    .get("supportedReasoningEfforts")
                    .and_then(Value::as_array)
                    .is_some_and(|values| {
                        values.iter().any(|value| {
                            value.get("reasoningEffort").and_then(Value::as_str) == Some(name)
                        })
                    })
            };
            let fast = model
                .get("serviceTiers")
                .and_then(Value::as_array)
                .is_some_and(|values| {
                    values
                        .iter()
                        .any(|value| is_fast_tier(value.get("id").and_then(Value::as_str)))
                });
            if effort(BOOST_EFFORT) && effort(BASELINE_EFFORT) && fast {
                return Ok(());
            }
            return Err(Error::Message("This Codex account does not advertise Astra with Ultra, Medium, and Fast; nothing was boosted.".into()));
        }
        let Some(next) = page.get("nextCursor").and_then(Value::as_str) else {
            break;
        };
        if !cursors.insert(next.to_owned()) {
            return Err(Error::Protocol("model/list repeated its cursor".into()));
        }
        cursor = json!(next);
    }
    Err(Error::Message(
        "Astra is not available in this server's live model catalog; nothing was boosted.".into(),
    ))
}

fn array<'a>(value: &'a Value, field: &str) -> Result<&'a Vec<Value>> {
    value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol(format!("Missing {field} array")))
}

fn loaded_tasks(client: &mut ControlClient, socket: &Path) -> Result<Vec<Task>> {
    let mut tasks = Vec::new();
    let mut cursor = Value::Null;
    let mut cursors = BTreeSet::new();
    loop {
        let page = client.request("thread/loaded/list", json!({"cursor":cursor, "limit":100}))?;
        for id in array(&page, "data")? {
            let id = id
                .as_str()
                .ok_or_else(|| Error::Protocol("Invalid loaded task ID".into()))?;
            let metadata =
                client.request("thread/read", json!({"threadId":id, "includeTurns":false}))?;
            let task = metadata
                .get("thread")
                .ok_or_else(|| Error::Protocol("Missing task metadata".into()))?;
            if task.get("modelProvider").and_then(Value::as_str) != Some("openai") {
                continue;
            }
            let child = task
                .get("parentThreadId")
                .is_some_and(|value| !value.is_null())
                || task.get("canAcceptDirectInput") == Some(&Value::Bool(false));
            tasks.push(Task {
                socket: socket.to_owned(),
                thread_id: id.to_owned(),
                child,
            });
        }
        let Some(next) = page.get("nextCursor").and_then(Value::as_str) else {
            break;
        };
        if !cursors.insert(next.to_owned()) {
            return Err(Error::Protocol(
                "thread/loaded/list repeated its cursor".into(),
            ));
        }
        cursor = json!(next);
    }
    Ok(tasks)
}

fn live_settings(client: &mut ControlClient, id: &str) -> Result<Option<Value>> {
    match client.request(
        "thread/resume",
        json!({"threadId": id, "excludeTurns":true}),
    ) {
        Ok(settings) => Ok(Some(settings)),
        Err(Error::Protocol(message))
            if message == format!("thread/resume failed: no rollout found for thread id {id}") =>
        {
            let metadata =
                client.request("thread/read", json!({"threadId":id, "includeTurns":false}))?;
            // A loaded task can exist before its first message saves a rollout.
            // It cannot be rejoined yet; enroll it after real work starts.
            if metadata
                .pointer("/thread/status/type")
                .and_then(Value::as_str)
                == Some("idle")
            {
                Ok(None)
            } else {
                Err(Error::Protocol(message))
            }
        }
        Err(error) => Err(error),
    }
}

fn active_turn(client: &mut ControlClient, id: &str) -> Result<Option<String>> {
    let page = client.request(
        "thread/turns/list",
        json!({"threadId":id, "limit":1, "sortDirection":"desc", "itemsView":"notLoaded"}),
    )?;
    let turns = array(&page, "data")?;
    Ok(turns
        .first()
        .filter(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned))
}

fn interrupt(client: &mut ControlClient, id: &str) -> Result<bool> {
    let Some(turn_id) = active_turn(client, id)? else {
        return Ok(false);
    };
    client.request("turn/interrupt", json!({"threadId":id, "turnId":turn_id}))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while active_turn(client, id)?.is_some() {
        if Instant::now() >= deadline {
            return Err(Error::Message(format!(
                "Task {id} did not stop after interruption."
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(true)
}

fn is_fast_tier(tier: Option<&str>) -> bool {
    matches!(tier, Some("fast" | "priority"))
}

fn settings_match(settings: &Value, boosted: bool) -> bool {
    settings.get("model").and_then(Value::as_str) == Some(BOOST_MODEL)
        && settings.get("reasoningEffort").and_then(Value::as_str)
            == Some(if boosted {
                BOOST_EFFORT
            } else {
                BASELINE_EFFORT
            })
        && if boosted {
            is_fast_tier(settings.get("serviceTier").and_then(Value::as_str))
        } else {
            settings.get("serviceTier").and_then(Value::as_str) == Some(BASELINE_TIER)
        }
}

fn set_task_settings(client: &mut ControlClient, id: &str, boosted: bool) -> Result<()> {
    client.request(
        "thread/settings/update",
        json!({
            "threadId":id, "model":BOOST_MODEL,
            "effort":if boosted { BOOST_EFFORT } else { BASELINE_EFFORT },
            "serviceTier":if boosted { FAST_TIER } else { BASELINE_TIER },
        }),
    )?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if live_settings(client, id)?.is_some_and(|settings| settings_match(&settings, boosted)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Message(format!(
                "Task {id} did not apply its requested settings."
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn boost_task(
    client: &mut ControlClient,
    task: &Task,
    settings: &Value,
    files: &Files,
    deadline: i64,
    newly_enrolled: bool,
) -> Result<()> {
    let id = &task.thread_id;
    if !newly_enrolled && settings_match(settings, true) {
        return Ok(());
    }
    if cutoff(files, deadline).is_some() {
        return Ok(());
    }
    if task.child {
        // Parents own continuation of subagents. Change their active settings
        // without injecting a new user turn or severing that relationship.
        if let Some(turn) = active_turn(client, id)? {
            client.request(
                "turn/settings/update",
                json!({"threadId":id, "turnId":turn,
                "model":BOOST_MODEL, "effort":BOOST_EFFORT, "serviceTier":FAST_TIER}),
            )?;
        }
        return set_task_settings(client, id, true);
    }
    let was_running = interrupt(client, id)?;
    if cutoff(files, deadline).is_some() {
        return Ok(());
    }
    set_task_settings(client, id, true)?;
    if was_running && cutoff(files, deadline).is_none() {
        client.request("turn/start", json!({
            "threadId":id, "model":BOOST_MODEL, "effort":BOOST_EFFORT, "serviceTier":FAST_TIER,
            "input":[{"type":"text", "text":"Continue the interrupted task from its current state.", "text_elements":[]}]
        }))?;
    }
    Ok(())
}

fn cleanup(config: &Config, files: &Files, state: &mut State) -> Result<()> {
    state.phase = Phase::Stopping;
    state.errors.clear();
    save_cleanup(files, state);
    // Discover tasks born immediately before cutoff without ever boosting them.
    if state.activated {
        for path in &state.sockets {
            match connect(config, path).and_then(|mut client| loaded_tasks(&mut client, path)) {
                Ok(tasks) => state.tasks.extend(tasks),
                Err(error) => state.errors.push(error.to_string()),
            }
        }
        save_cleanup(files, state);
    }
    for task in state.tasks.clone() {
        let result = (|| {
            let mut client = connect(config, &task.socket)?;
            client.set_timeout(CONTROL_TIMEOUT);
            if live_settings(&mut client, &task.thread_id)?.is_none() {
                return Ok(());
            }
            interrupt(&mut client, &task.thread_id)?;
            set_task_settings(&mut client, &task.thread_id, false)
        })();
        match result {
            Ok(()) => {
                state.tasks.remove(&task);
            }
            Err(error) => state.errors.push(format!(
                "Task {} on {}: {error}",
                task.thread_id,
                task.socket.display()
            )),
        }
        save_cleanup(files, state);
    }
    if state.defaults_pending {
        match set_baseline_defaults(config) {
            Ok(()) => state.defaults_pending = false,
            Err(error) => state.errors.push(format!("Global defaults: {error}")),
        }
    }
    state.phase = if state.errors.is_empty() && !state.defaults_pending && state.tasks.is_empty() {
        state.activated = false;
        Phase::Stopped
    } else {
        Phase::CleanupRequired
    };
    save_cleanup(files, state);
    if let Err(error) = remove_file_if_exists(&files.stop) {
        state.errors.push(error.to_string());
        state.phase = Phase::CleanupRequired;
        save_cleanup(files, state);
    }
    if state.phase == Phase::Stopped {
        Ok(())
    } else {
        Err(Error::Message(format!(
            "Boost cleanup is incomplete; run `cxa boost status`, restore the server connection, then `cxa boost stop`. See {}.",
            files.state.display()
        )))
    }
}

fn save_cleanup(files: &Files, state: &mut State) {
    // A full disk must not prevent the remaining tasks from being stopped.
    if let Err(error) = files.save(state) {
        let message = format!("Recovery state could not be saved: {error}");
        if !state.errors.contains(&message) {
            state.errors.push(message);
        }
        state.phase = Phase::CleanupRequired;
    }
}

struct Quotas {
    receiver: Receiver<Result<BTreeMap<u32, UsageRecord>>>,
    stop: Arc<AtomicBool>,
    cancellation: CancellationToken,
    worker: Option<thread::JoinHandle<()>>,
}

impl Quotas {
    fn start(config: Config) -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let cancellation = CancellationToken::default();
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                let result = collect_quotas(&config, &worker_stop, &worker_cancellation);
                // Never let an unread snapshot prevent cancellation and cleanup.
                match sender.try_send(result) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => return,
                }
                let next = Instant::now() + POLL_INTERVAL;
                while Instant::now() < next && !worker_stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Self {
            receiver,
            stop,
            cancellation,
            worker: Some(worker),
        }
    }
}

impl Drop for Quotas {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn collect_quotas(
    config: &Config,
    stop: &AtomicBool,
    cancellation: &CancellationToken,
) -> Result<BTreeMap<u32, UsageRecord>> {
    let store = Store::new(config.clone());
    let _lock = loop {
        if stop.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if let Some(lock) = store.try_lock()? {
            break lock;
        }
        thread::sleep(Duration::from_millis(100));
    };
    let profiles = store.profiles()?;
    if profiles.is_empty() {
        return Err(Error::Message(
            "No enrolled accounts. Run `cxa init` first.".into(),
        ));
    }
    thread::scope(|scope| {
        let workers: Vec<_> = profiles
            .iter()
            .map(|profile| {
                let cancellation = cancellation.clone();
                scope.spawn(move || {
                    let (usage, _) = query_profile_cancellable(
                        config,
                        &config.profile_auth(profile.slot),
                        cancellation,
                    )?;
                    if !has_weekly_quota(&usage) {
                        return Err(Error::Message(format!(
                            "Codex weekly quota unavailable for account {}; stopping boost.",
                            profile.slot
                        )));
                    }
                    Ok((profile.slot, usage))
                })
            })
            .collect();
        let mut observations = BTreeMap::new();
        for worker in workers {
            let (slot, usage) = worker
                .join()
                .map_err(|_| Error::Message("Quota worker failed.".into()))??;
            observations.insert(slot, usage);
        }
        Ok(observations)
    })
}
