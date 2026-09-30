// Claude Code activity & usage monitor.
//
// Activity is inferred from the session transcripts Claude Code writes to
// `<config>/projects/<project>/<session>.jsonl`. An optional Notification hook
// (see README) makes permission-prompt detection exact instead of time-based.
//
// Usage comes from the unified rate-limit headers the Anthropic API returns for
// the OAuth token Claude Code stores locally. The token is only ever placed in
// the Authorization header of a request to api.anthropic.com: it is never
// logged, cached, or included in the telemetry sent to the Tinytosh.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use any_ascii::any_ascii;
use serde_json::Value;

// Activity detection
const ACTIVE_WINDOW_SECS: u64 = 600;        // A running tool this old counts as idle
const WAITING_WINDOW_SECS: u64 = 3600;      // Sessions stay monitored this long; a question can wait a while
const STALE_WORK_SECS: u64 = 300;           // A "working" transcript this old most likely crashed or was closed
const TEXT_SETTLE_SECS: u64 = 3;            // Trailing assistant text with no follow-up means the turn ended
const TAIL_BYTES: u64 = 512 * 1024;         // Tool results can be large; read enough to reach a full entry
// The hook appends one "<epoch seconds> <notification json>" line per notification, so
// several sessions waiting at once never overwrite each other.
const HOOK_LOG_NAME: &str = "tinytosh-notify.jsonl";
const LEGACY_HOOK_FILE_NAME: &str = "tinytosh-notify.json"; // Earlier hook: last notification only
const HOOK_MARKER: &str = "tinytosh-notify";                // Identifies Tinytosh entries in settings.json
const HOOK_LOG_TAIL_BYTES: u64 = 64 * 1024;
const HOOK_LOG_ROTATE_BYTES: u64 = 512 * 1024;
const MAX_LABEL_LEN: usize = 24; // The device cuts to its 21-column line with ".."

// Usage polling
pub const USAGE_POLL_SECS: u64 = 60;
const USAGE_STALE_SECS: u64 = 900;
const USAGE_TIMEOUT_SECS: u64 = 15;
const API_URL: &str = "https://api.anthropic.com/v1/messages";
const USAGE_PROBE_MODEL: &str = "claude-haiku-4-5-20251001";
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

// Ordered by display priority: when several sessions are active, the highest wins.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum Activity {
    #[default]
    Offline,
    Idle,
    Thinking,
    Tool,
    Writing,
    Permission,
    Limit,
}

impl Activity {
    pub fn as_str(self) -> &'static str {
        match self {
            Activity::Offline => "offline",
            Activity::Idle => "idle",
            Activity::Thinking => "thinking",
            Activity::Tool => "tool",
            Activity::Writing => "writing",
            Activity::Permission => "permission",
            Activity::Limit => "limit",
        }
    }
}

// What the end of a transcript says, independent of the current time.
#[derive(Clone, Debug, PartialEq)]
enum Tail {
    Unknown,
    Prompted,
    Thinking,
    Text,
    Done,
    PendingTool(String, u64), // Tool name, epoch seconds when it was requested (0 = unknown)
    ApiLimit,
}

#[derive(Clone, Debug, Default)]
pub struct ActivitySnapshot {
    pub activity: Activity,
    pub tool: String,
    pub project: String,
    pub busy_sessions: u8,
    pub last_turn: String, // e.g. "Cogitated for 54s"; set only when idle
}

struct CachedTail {
    len: u64,
    mtime: u64,
    tail: Tail,
    project: String,
    last_turn: String,
}

#[derive(Clone, Debug, PartialEq)]
struct HookEvent {
    session_id: String,
    is_permission: bool,
    at: u64,
}

// What the hook has reported for one session. Its presence alone means the hook
// is active in that session, so permission prompts no longer need to be guessed.
#[derive(Clone, Copy, Debug, Default)]
struct SessionHook {
    last_permission: u64,
}

pub struct ActivityMonitor {
    config_dir: PathBuf,
    cache: HashMap<PathBuf, CachedTail>,
    hooks: HashMap<String, SessionHook>,
}

impl ActivityMonitor {
    pub fn new() -> Self {
        Self { config_dir: claude_config_dir(), cache: HashMap::new(), hooks: HashMap::new() }
    }

    fn refresh_hooks(&mut self, active_ids: &HashSet<String>) {
        let log = self.config_dir.join(HOOK_LOG_NAME);
        if fs::metadata(&log).map_or(false, |m| m.len() > HOOK_LOG_ROTATE_BYTES) {
            let _ = fs::rename(&log, log.with_extension("jsonl.1"));
        }

        let mut events = read_file_tail(&log, HOOK_LOG_TAIL_BYTES).map(|t| parse_hook_log(&t)).unwrap_or_default();
        events.extend(read_legacy_hook_event(&self.config_dir.join(LEGACY_HOOK_FILE_NAME)));

        self.hooks.retain(|id, _| active_ids.contains(id));
        for event in events.into_iter().filter(|e| active_ids.contains(&e.session_id)) {
            let entry = self.hooks.entry(event.session_id).or_default();
            if event.is_permission {
                entry.last_permission = entry.last_permission.max(event.at);
            }
        }
    }

    pub fn poll(&mut self) -> ActivitySnapshot {
        let now = unix_now();
        let sessions = recent_transcripts(&self.config_dir.join("projects"), now);
        let active_ids: HashSet<String> =
            sessions.iter().filter_map(|(p, _, _)| p.file_stem().and_then(|s| s.to_str()).map(str::to_string)).collect();
        self.refresh_hooks(&active_ids);

        self.cache.retain(|path, _| sessions.iter().any(|(p, _, _)| p == path));

        let mut best: Option<(Activity, u64, String, String, String)> = None;
        let mut busy_sessions: u8 = 0;

        for (path, len, mtime) in sessions {
            let fresh = self.cache.get(&path).map_or(true, |c| c.len != len || c.mtime != mtime);
            if fresh {
                let (tail, project, last_turn) = read_tail(&path).unwrap_or((Tail::Unknown, String::new(), String::new()));
                self.cache.insert(path.clone(), CachedTail { len, mtime, tail, project, last_turn });
            }
            let cached = &self.cache[&path];

            let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            // A permission notification only counts for a tool call that is still unanswered and
            // was requested before the notification fired. Comparing against the file mtime is not
            // enough: Claude Code appends metadata lines and parallel tool results after the prompt.
            let hook = self.hooks.get(session_id);
            let hook_says_permission = match (&cached.tail, hook) {
                (Tail::PendingTool(_, since), Some(h)) => h.last_permission > 0 && h.last_permission >= *since,
                _ => false,
            };

            let (activity, tool) = if hook.map_or(false, |h| hook_prompt_open(h.last_permission, mtime, now)) {
                let tool = match &cached.tail { Tail::PendingTool(name, _) => name.clone(), _ => String::new() };
                (Activity::Permission, tool)
            } else {
                resolve(&cached.tail, now, mtime, hook_says_permission, hook.is_some())
            };
            if activity >= Activity::Thinking {
                busy_sessions = busy_sessions.saturating_add(1);
            }

            let project = project_name(&path, &cached.project);
            let better = best.as_ref().map_or(true, |(a, m, _, _, _)| (activity, mtime) > (*a, *m));
            if better {
                let last_turn = if activity == Activity::Idle { cached.last_turn.clone() } else { String::new() };
                best = Some((activity, mtime, tool, project, last_turn));
            }
        }

        match best {
            Some((activity, _, tool, project, last_turn)) => ActivitySnapshot {
                activity,
                tool: sanitize_label(&tool),
                project: sanitize_label(&project),
                busy_sessions,
                last_turn: sanitize_label(&last_turn),
            },
            None => ActivitySnapshot::default(),
        }
    }
}

// Claude Code writes a pending question or permission request to the transcript only
// after it is answered, so the transcript alone cannot show the wait. A permission
// notification newer than the last transcript write means the prompt is still open;
// the answer is written to the transcript, which then makes it older and clears it.
fn hook_prompt_open(last_permission: u64, transcript_mtime: u64, now: u64) -> bool {
    last_permission > 0 && last_permission >= transcript_mtime && now.saturating_sub(last_permission) < WAITING_WINDOW_SECS
}

// `session_hooked`: the hook has reported for this session, so its silence means
// a pending tool is running, not waiting. Other sessions (started before the hook
// was installed, or where it does not fire) fall back to the time-based guess.
fn resolve(tail: &Tail, now: u64, mtime: u64, hook_says_permission: bool, session_hooked: bool) -> (Activity, String) {
    let age = now.saturating_sub(mtime);
    match tail {
        Tail::PendingTool(name, since) => {
            // Parallel tool results keep touching the file, so measure from the request itself.
            let age = if *since > 0 { now.saturating_sub(*since) } else { age };
            let guessed = !session_hooked && permission_guess_secs(name).map_or(false, |limit| age >= limit);
            if (hook_says_permission || guessed || waits_for_user(name)) && age < WAITING_WINDOW_SECS {
                (Activity::Permission, name.clone())
            } else if age < ACTIVE_WINDOW_SECS {
                let activity = if is_write_tool(name) { Activity::Writing } else { Activity::Tool };
                (activity, name.clone())
            } else {
                (Activity::Idle, String::new())
            }
        }
        Tail::Prompted | Tail::Thinking if age < STALE_WORK_SECS => (Activity::Thinking, String::new()),
        Tail::Text if age < TEXT_SETTLE_SECS => (Activity::Thinking, String::new()),
        Tail::ApiLimit => (Activity::Limit, String::new()),
        _ => (Activity::Idle, String::new()),
    }
}

// Tools whose whole purpose is to wait for the user's answer.
fn waits_for_user(name: &str) -> bool {
    matches!(name, "AskUserQuestion" | "ExitPlanMode")
}

fn is_write_tool(name: &str) -> bool {
    matches!(name, "Edit" | "MultiEdit" | "Write" | "NotebookEdit")
}

// Without the hook, a tool call that never produced a result is either still
// running or blocked on a permission prompt. Fast tools that stall are almost
// certainly waiting on the user; slow ones get more slack. Subagents are never
// guessed because they legitimately run for minutes.
fn permission_guess_secs(name: &str) -> Option<u64> {
    match name {
        "Task" | "Agent" => None,
        "Read" | "Glob" | "Grep" | "LS" | "TodoWrite" | "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => Some(5),
        _ => Some(20),
    }
}

fn recent_transcripts(projects_dir: &Path, now: u64) -> Vec<(PathBuf, u64, u64)> {
    let mut out = Vec::new();
    let Ok(projects) = fs::read_dir(projects_dir) else { return out };

    for project in projects.flatten() {
        let Ok(entries) = fs::read_dir(project.path()) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let mtime = meta.modified().ok().map(system_time_secs).unwrap_or(0);
            if now.saturating_sub(mtime) <= WAITING_WINDOW_SECS {
                out.push((path, meta.len(), mtime));
            }
        }
    }
    out
}

fn read_tail(path: &Path) -> Option<(Tail, String, String)> {
    read_file_tail(path, TAIL_BYTES).map(|body| classify_lines(&body))
}

// Last `max_bytes` of a file as whole lines (a partial first line is dropped).
fn read_file_tail(path: &Path, max_bytes: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;

    let mut buf = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();

    if start > 0 {
        return Some(text.split_once('\n').map_or(String::new(), |(_, rest)| rest.to_string()));
    }
    Some(text)
}

// Walks the current turn backwards. The newest entry decides the base state, but
// Claude often calls several tools in parallel: a fast result can land after a
// question that is still waiting. So every tool_use of the turn is matched
// against the tool_results seen after it, and an unanswered call wins.
// Returns (tail, project cwd, last turn summary). The summary is Claude Code's own
// end-of-turn line ("Cogitated for 54s") for the most recent finished turn.
fn classify_lines(body: &str) -> (Tail, String, String) {
    let mut project = String::new();
    let mut last_turn = String::new();
    let mut newest: Option<Tail> = None;
    let mut answered: HashSet<String> = HashSet::new();
    let mut pending: Option<(String, u64)> = None;

    for line in body.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else { continue };

        if project.is_empty() {
            if let Some(cwd) = obj.get("cwd").and_then(Value::as_str) {
                project = cwd.to_string();
            }
        }

        if obj.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }

        match obj.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let since = obj.get("timestamp").and_then(Value::as_str).map_or(0, parse_timestamp);
                for (id, name) in tool_uses(&obj) {
                    if answered.contains(id) {
                        continue;
                    }
                    let prefer = pending.as_ref().map_or(true, |(current, _)| waits_for_user(name) && !waits_for_user(current));
                    if prefer {
                        pending = Some((name.to_string(), since));
                    }
                }
                if newest.is_none() {
                    newest = classify_assistant(&obj);
                }
            }
            Some("system") if obj.get("subtype").and_then(Value::as_str) == Some("turn_duration") => {
                // Written at the end of a turn, so the newest one belongs to the last finished turn.
                if last_turn.is_empty() {
                    last_turn = turn_summary(&obj).unwrap_or_default();
                }
            }
            Some("user") if obj.get("isMeta").and_then(Value::as_bool) != Some(true) => {
                let result_ids = tool_result_ids(&obj);
                let starts_turn = result_ids.is_empty();
                answered.extend(result_ids.into_iter().map(str::to_string));
                if newest.is_none() {
                    newest = classify_user(&obj);
                }
                // A typed prompt opens the turn; nothing before it can still be waiting.
                if starts_turn {
                    break;
                }
            }
            _ => {}
        }
    }

    let tail = match (newest, pending) {
        (Some(done @ (Tail::Done | Tail::ApiLimit)), _) => done,
        (_, Some((name, since))) => Tail::PendingTool(name, since),
        (Some(tail), None) => tail,
        (None, None) => Tail::Unknown,
    };
    (tail, project, last_turn)
}

// Claude Code's end-of-turn line. It picks the verb from a fixed list by hashing the
// turn_duration entry's uuid (a 32-bit "h * 31 + c" string hash over UTF-16 code
// units), so the result matches what the terminal showed for that turn.
const TURN_VERBS: [&str; 8] = ["Baked", "Brewed", "Churned", "Cogitated", "Cooked", "Crunched", "Sautéed", "Worked"];

fn turn_summary(obj: &Value) -> Option<String> {
    let uuid = obj.get("uuid").and_then(Value::as_str)?;
    let duration_ms = obj.get("durationMs").and_then(Value::as_u64)?;
    let verb = TURN_VERBS[(string_hash(uuid) as u32 % TURN_VERBS.len() as u32) as usize];
    Some(format!("{} for {}", verb, format_turn_duration(duration_ms)))
}

fn string_hash(s: &str) -> i32 {
    s.encode_utf16().fold(0i32, |h, unit| h.wrapping_mul(31).wrapping_add(unit as i32))
}

// Same rules as Claude Code: whole seconds under a minute, otherwise rounded seconds
// with carry into minutes, hours, and days.
fn format_turn_duration(ms: u64) -> String {
    if ms < 60_000 {
        return format!("{}s", ms / 1000);
    }
    let (mut days, mut hours, mut minutes) = (ms / 86_400_000, ms % 86_400_000 / 3_600_000, ms % 3_600_000 / 60_000);
    let mut seconds = ((ms % 60_000) as f64 / 1000.0).round() as u64;
    if seconds == 60 { seconds = 0; minutes += 1; }
    if minutes == 60 { minutes = 0; hours += 1; }
    if hours == 24 { hours = 0; days += 1; }

    if days > 0 {
        format!("{}d {}h {}m", days, hours, minutes)
    } else if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    }
}

fn tool_uses(obj: &Value) -> Vec<(&str, &str)> {
    content_blocks(obj)
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|b| Some((b.get("id")?.as_str()?, b.get("name").and_then(Value::as_str).unwrap_or("Tool"))))
        .collect()
}

fn tool_result_ids(obj: &Value) -> Vec<&str> {
    content_blocks(obj)
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| b.get("tool_use_id")?.as_str())
        .collect()
}

fn content_blocks(obj: &Value) -> impl Iterator<Item = &Value> {
    obj.get("message").and_then(|m| m.get("content")).and_then(Value::as_array).into_iter().flatten()
}

fn classify_assistant(obj: &Value) -> Option<Tail> {
    if obj.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
        return Some(if is_limit_error(obj) { Tail::ApiLimit } else { Tail::Done });
    }

    let message = obj.get("message")?;
    let last = message.get("content")?.as_array()?.last()?;

    Some(match last.get("type").and_then(Value::as_str) {
        Some("tool_use") => Tail::PendingTool(
            last.get("name").and_then(Value::as_str).unwrap_or("Tool").to_string(),
            obj.get("timestamp").and_then(Value::as_str).map_or(0, parse_timestamp),
        ),
        Some("thinking") | Some("redacted_thinking") => Tail::Thinking,
        Some("text") => {
            if message.get("stop_reason").and_then(Value::as_str) == Some("end_turn") { Tail::Done } else { Tail::Text }
        }
        _ => Tail::Text,
    })
}

fn classify_user(obj: &Value) -> Option<Tail> {
    if obj.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }

    let content = obj.get("message")?.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(classify_prompt_text(text));
    }

    let items = content.as_array()?;
    let mut first_text: Option<&str> = None;

    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("tool_result") => {
                let body = item.get("content").map(|c| c.to_string()).unwrap_or_default();
                if body.contains("doesn't want to proceed") || body.contains("[Request interrupted by user") {
                    return Some(Tail::Done);
                }
                return Some(Tail::Prompted);
            }
            Some("text") if first_text.is_none() => first_text = item.get("text").and_then(Value::as_str),
            _ => {}
        }
    }

    first_text.map(classify_prompt_text)
}

fn classify_prompt_text(text: &str) -> Tail {
    let text = text.trim_start();
    if text.starts_with("[Request interrupted by user")
        || text.starts_with("<local-command-stdout>")
        || text.starts_with("<local-command-stderr>")
    {
        Tail::Done
    } else {
        Tail::Prompted
    }
}

fn is_limit_error(obj: &Value) -> bool {
    if obj.get("apiErrorStatus").and_then(Value::as_u64) == Some(429) {
        return true;
    }
    let error = obj.get("error").map(|e| e.to_string().to_lowercase()).unwrap_or_default();
    if error.contains("rate_limit") {
        return true;
    }
    let text = obj.get("message").and_then(|m| m.get("content")).map(|c| c.to_string().to_lowercase()).unwrap_or_default();
    text.contains("limit reached") || text.contains("hit your") && text.contains("limit")
}

// Parses "<epoch> <json>" lines written by the hook. Malformed lines are skipped.
fn parse_hook_log(text: &str) -> Vec<HookEvent> {
    text.lines()
        .filter_map(|line| {
            // PowerShell's UTF-8 writer may prepend a BOM.
            let (epoch, json) = line.trim_start_matches('\u{feff}').trim().split_once(' ')?;
            hook_event(json, epoch.parse().ok()?)
        })
        .collect()
}

// The previous hook overwrote a single file; its mtime is the event time.
fn read_legacy_hook_event(path: &Path) -> Option<HookEvent> {
    let at = fs::metadata(path).ok()?.modified().ok().map(system_time_secs)?;
    let raw = fs::read_to_string(path).ok()?;
    hook_event(raw.trim_start_matches('\u{feff}').trim(), at)
}

fn hook_event(json: &str, at: u64) -> Option<HookEvent> {
    let obj: Value = serde_json::from_str(json).ok()?;
    let kind = obj.get("notification_type").and_then(Value::as_str).unwrap_or_default();
    let message = obj.get("message").and_then(Value::as_str).unwrap_or_default().to_lowercase();

    // Only accept characters a session ID may contain; the value is compared against file names.
    let session_id: String = obj
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if session_id.is_empty() {
        return None;
    }

    Some(HookEvent {
        session_id,
        is_permission: kind == "permission_prompt" || (kind.is_empty() && message.contains("permission")),
        at,
    })
}

// ---------------------------------------------------------------------------
// Notification hook installer
// ---------------------------------------------------------------------------
//
// Adds or removes one Notification hook in Claude Code's settings.json that
// copies the notification payload to HOOK_FILE_NAME. Every other setting and
// hook is left untouched; the previous file is kept as a backup, and a file
// that is not valid JSON is never overwritten.

const SETTINGS_FILE_NAME: &str = "settings.json";
const SETTINGS_BACKUP_SUFFIX: &str = "bak-tinytosh";

pub fn hook_enabled() -> bool {
    let path = claude_config_dir().join(SETTINGS_FILE_NAME);
    let Ok(raw) = fs::read_to_string(path) else { return false };
    let Ok(settings) = serde_json::from_str::<Value>(&raw) else { return false };
    settings
        .pointer("/hooks/Notification")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .any(is_tinytosh_hook)
}

pub fn set_hook_enabled(enable: bool) -> Result<bool, String> {
    let config_dir = claude_config_dir();
    if !config_dir.is_dir() {
        return Err("Claude Code is not set up on this PC yet. Run `claude` once, then try again.".into());
    }
    let path = config_dir.join(SETTINGS_FILE_NAME);

    let mut settings = match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<Value>(&raw)
            .map_err(|_| "Claude Code settings.json is not valid JSON, so it was left unchanged.".to_string())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Default::default()),
        Err(e) => return Err(format!("Cannot read Claude Code settings: {}", e)),
    };

    apply_hook(&mut settings, enable, &hook_command(&config_dir.join(HOOK_LOG_NAME)))?;
    write_settings(&path, &settings)?;

    if !enable {
        let _ = fs::remove_file(config_dir.join(HOOK_LOG_NAME));
        let _ = fs::remove_file(config_dir.join(LEGACY_HOOK_FILE_NAME));
    }
    Ok(enable)
}

// Rewrites an installed Tinytosh hook that still uses an older command (e.g. the
// single-file version) to the current one. Leaves settings untouched otherwise.
pub fn upgrade_hook_if_outdated() -> Result<bool, String> {
    let config_dir = claude_config_dir();
    let current = hook_command(&config_dir.join(HOOK_LOG_NAME));
    let Ok(raw) = fs::read_to_string(config_dir.join(SETTINGS_FILE_NAME)) else { return Ok(false) };
    let Ok(settings) = serde_json::from_str::<Value>(&raw) else { return Ok(false) };

    let commands: Vec<&str> = settings
        .pointer("/hooks/Notification")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter(|h| is_tinytosh_hook(h))
        .filter_map(|h| h.get("command").and_then(Value::as_str))
        .collect();

    if commands.is_empty() || commands.iter().all(|c| *c == current) {
        return Ok(false);
    }
    set_hook_enabled(true)
}

fn apply_hook(settings: &mut Value, enable: bool, command: &str) -> Result<(), String> {
    const SHAPE_ERROR: &str = "Claude Code settings.json has an unexpected layout, so it was left unchanged.";

    let root = settings.as_object_mut().ok_or(SHAPE_ERROR)?;
    let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Default::default())).as_object_mut().ok_or(SHAPE_ERROR)?;
    let groups = hooks.entry("Notification").or_insert_with(|| Value::Array(Vec::new())).as_array_mut().ok_or(SHAPE_ERROR)?;

    // Remove any previous Tinytosh entry (including hand-written ones), keep everything else.
    for group in groups.iter_mut() {
        if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            list.retain(|h| !is_tinytosh_hook(h));
        }
    }
    groups.retain(|group| group.get("hooks").and_then(Value::as_array).map_or(true, |list| !list.is_empty()));

    if enable {
        groups.push(serde_json::json!({ "matcher": "", "hooks": [{ "type": "command", "command": command }] }));
    }

    if groups.is_empty() {
        hooks.remove("Notification");
    }
    if hooks.is_empty() {
        root.remove("hooks");
    }
    Ok(())
}

fn is_tinytosh_hook(hook: &Value) -> bool {
    hook.get("command").and_then(Value::as_str).map_or(false, |c| c.contains(HOOK_MARKER))
}

// The hook receives the notification JSON on stdin; the command only appends it,
// prefixed with the current epoch, as one line to a fixed file. A single write per
// line keeps concurrent sessions from interleaving.
#[cfg(not(target_os = "windows"))]
fn hook_command(target: &Path) -> String {
    let quoted = target.to_string_lossy().replace('\'', "'\\''");
    format!("line=\"$(date +%s) $(tr -d '\\n')\"; printf '%s\\n' \"$line\" >> '{}'", quoted)
}

#[cfg(target_os = "windows")]
fn hook_command(target: &Path) -> String {
    let quoted = target.to_string_lossy().replace('\'', "''");
    format!(
        "powershell -NoProfile -Command \"$l = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds().ToString() + ' ' + (($input | Out-String) -replace '[\\r\\n]', ''); Add-Content -LiteralPath '{}' -Value $l -Encoding utf8\"",
        quoted
    )
}

fn write_settings(path: &Path, settings: &Value) -> Result<(), String> {
    let body = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())? + "\n";
    let original = fs::metadata(path).ok();

    if original.is_some() {
        let backup = path.with_extension(format!("json.{}", SETTINGS_BACKUP_SUFFIX));
        fs::copy(path, &backup).map_err(|e| format!("Cannot back up Claude Code settings: {}", e))?;
    }

    // Write next to the original, then swap, so Claude Code never reads a half-written file.
    let tmp = path.with_extension("json.tmp-tinytosh");
    fs::write(&tmp, body).map_err(|e| format!("Cannot write Claude Code settings: {}", e))?;
    if let Some(meta) = original {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("Cannot save Claude Code settings: {}", e)
    })
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub five_hour_pct: u8,
    pub five_hour_reset: u64,
    pub weekly_pct: u8,
    pub weekly_reset: u64,
    pub limited: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UsageError {
    NoToken,
    Expired,
    Rejected(u16),
    Network,
    NoHeaders,
}

impl UsageError {
    pub fn code(&self) -> &'static str {
        match self {
            UsageError::NoToken => "no_login",
            UsageError::Expired => "expired",
            UsageError::Rejected(_) => "rejected",
            UsageError::Network => "network",
            UsageError::NoHeaders => "unsupported",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct UsageState {
    pub last: Option<Usage>,
    pub fetched_at: u64,
    pub error: Option<UsageError>,
}

pub struct UsagePoller {
    config_dir: PathBuf,
    agent: ureq::Agent,
    rejected_token: Option<u64>,
}

impl UsagePoller {
    pub fn new() -> Self {
        let agent = ureq::builder().timeout(Duration::from_secs(USAGE_TIMEOUT_SECS)).build();
        Self { config_dir: claude_config_dir(), agent, rejected_token: None }
    }

    pub fn poll(&mut self, state: &mut UsageState) {
        match self.fetch() {
            Ok(usage) => {
                state.last = Some(usage);
                state.fetched_at = unix_now();
                state.error = None;
            }
            Err(e) => state.error = Some(e),
        }
    }

    fn fetch(&mut self) -> Result<Usage, UsageError> {
        let token = read_access_token(&self.config_dir)?;

        // Claude Code owns the token and refreshes it; never retry one the API already rejected.
        let fingerprint = hash_str(&token);
        if self.rejected_token == Some(fingerprint) {
            return Err(UsageError::Expired);
        }

        let body = serde_json::json!({
            "model": USAGE_PROBE_MODEL,
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "hi" }]
        });

        let response = self
            .agent
            .post(API_URL)
            .set("Authorization", &format!("Bearer {}", token))
            .set("anthropic-version", "2023-06-01")
            .set("anthropic-beta", "oauth-2025-04-20")
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());

        let response = match response {
            Ok(r) => r,
            Err(ureq::Error::Status(code @ (401 | 403), _)) => {
                self.rejected_token = Some(fingerprint);
                return Err(UsageError::Rejected(code));
            }
            // 429 still carries the rate-limit headers, which is exactly what we want.
            Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(_)) => return Err(UsageError::Network),
        };

        self.rejected_token = None;
        parse_usage_headers(|name| response.header(name).map(str::to_string))
    }
}

fn parse_usage_headers(header: impl Fn(&str) -> Option<String>) -> Result<Usage, UsageError> {
    const PREFIX: &str = "anthropic-ratelimit-unified-";
    let get = |suffix: &str| header(&format!("{}{}", PREFIX, suffix));

    let five_hour = get("5h-utilization").ok_or(UsageError::NoHeaders)?;
    let pct = |raw: Option<String>| -> u8 {
        let v = raw.and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(0.0);
        (v * 100.0).round().clamp(0.0, 100.0) as u8
    };
    let epoch = |raw: Option<String>| -> u64 { raw.and_then(|s| s.trim().parse::<f64>().ok()).map_or(0, |v| v.max(0.0) as u64) };

    let five_hour_pct = pct(Some(five_hour));
    let weekly_pct = pct(get("7d-utilization"));
    let rejected = [get("status"), get("5h-status"), get("7d-status")]
        .iter()
        .any(|s| s.as_deref().map(str::trim) == Some("rejected"));

    Ok(Usage {
        five_hour_pct,
        five_hour_reset: epoch(get("5h-reset")),
        weekly_pct,
        weekly_reset: epoch(get("7d-reset")),
        limited: rejected || five_hour_pct >= 100 || weekly_pct >= 100,
    })
}

fn read_access_token(config_dir: &Path) -> Result<String, UsageError> {
    if let Ok(raw) = fs::read_to_string(config_dir.join(".credentials.json")) {
        return extract_access_token(&raw, unix_now());
    }
    #[cfg(target_os = "macos")]
    if let Some(raw) = read_keychain_credentials() {
        return extract_access_token(&raw, unix_now());
    }
    Err(UsageError::NoToken)
}

#[cfg(target_os = "macos")]
fn read_keychain_credentials() -> Option<String> {
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?.trim().to_string();
    // `security -w` hex-dumps secrets containing non-printable bytes. JSON is never pure hex.
    if !raw.is_empty() && raw.len() % 2 == 0 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        let bytes: Option<Vec<u8>> = (0..raw.len()).step_by(2).map(|i| u8::from_str_radix(&raw[i..i + 2], 16).ok()).collect();
        return bytes.and_then(|b| String::from_utf8(b).ok());
    }
    Some(raw)
}

fn extract_access_token(raw: &str, now: u64) -> Result<String, UsageError> {
    let obj: Value = serde_json::from_str(raw.trim()).map_err(|_| UsageError::NoToken)?;
    let oauth = obj.get("claudeAiOauth").unwrap_or(&obj);

    let token = oauth.get("accessToken").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).ok_or(UsageError::NoToken)?;

    // expiresAt is in milliseconds. Skip the request instead of earning a 401.
    if let Some(expires_ms) = oauth.get("expiresAt").and_then(Value::as_u64) {
        if expires_ms / 1000 <= now {
            return Err(UsageError::Expired);
        }
    }
    Ok(token.to_string())
}

// ---------------------------------------------------------------------------
// Telemetry payload
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, Clone, Default)]
pub struct ClaudeStats {
    pub claude_state: String,
    pub claude_tool: String,
    pub claude_proj: String,
    pub claude_sessions: u8,
    pub claude_ok: bool,
    pub claude_5h: u8,
    pub claude_5h_reset: u32,
    pub claude_7d: u8,
    pub claude_7d_reset: u32,
    pub claude_err: String,
    pub claude_done: String,
}

pub fn build_stats(activity: &ActivitySnapshot, usage: &UsageState) -> ClaudeStats {
    let now = unix_now();
    let fresh = usage.last.as_ref().filter(|_| now.saturating_sub(usage.fetched_at) < USAGE_STALE_SECS);
    let minutes_until = |epoch: u64| -> u32 { (epoch.saturating_sub(now) / 60).min(u32::MAX as u64) as u32 };

    let limited = fresh.map_or(false, |u| u.limited && (u.five_hour_reset == 0 || u.five_hour_reset > now));
    let state = if limited { Activity::Limit } else { activity.activity };

    let mut stats = ClaudeStats {
        claude_state: state.as_str().to_string(),
        claude_tool: activity.tool.clone(),
        claude_proj: activity.project.clone(),
        claude_sessions: activity.busy_sessions,
        claude_err: usage.error.as_ref().map(|e| e.code().to_string()).unwrap_or_default(),
        claude_done: if state == Activity::Idle { activity.last_turn.clone() } else { String::new() },
        ..Default::default()
    };

    if let Some(u) = fresh {
        stats.claude_ok = true;
        stats.claude_5h = u.five_hour_pct;
        stats.claude_5h_reset = minutes_until(u.five_hour_reset);
        stats.claude_7d = u.weekly_pct;
        stats.claude_7d_reset = minutes_until(u.weekly_reset);
    }
    stats
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn claude_config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default();
    PathBuf::from(home).join(".claude")
}

// Claude Code names each project folder after the launch directory with every
// non-alphanumeric character replaced by '-'. The cwd may be a subfolder of it,
// so cut the cwd back to the folder's length to recover the project root.
fn project_name(transcript: &Path, cwd: &str) -> String {
    let folder = transcript.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or_default();

    let cwd_chars: Vec<char> = cwd.chars().collect();
    let folder_chars: Vec<char> = folder.chars().collect();
    let is_prefix = !folder_chars.is_empty()
        && folder_chars.len() <= cwd_chars.len()
        && folder_chars.iter().zip(&cwd_chars).all(|(f, c)| *f == if c.is_ascii_alphanumeric() { *c } else { '-' });

    if is_prefix {
        return path_basename(&cwd_chars[..folder_chars.len()].iter().collect::<String>());
    }
    if !cwd.is_empty() {
        return path_basename(cwd);
    }
    folder.rsplit('-').find(|s| !s.is_empty()).unwrap_or_default().to_string()
}

fn path_basename(path: &str) -> String {
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or_default().to_string()
}

// The OLED font only covers printable ASCII.
fn sanitize_label(raw: &str) -> String {
    any_ascii(raw).chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(MAX_LABEL_LEN).collect::<String>().trim().to_string()
}

// Parses the UTC timestamps Claude Code writes ("2026-09-27T04:15:31.492Z") to epoch seconds.
// Returns 0 for anything else, which callers treat as "unknown".
fn parse_timestamp(ts: &str) -> u64 {
    let b = ts.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || !ts.ends_with('Z') {
        return 0;
    }
    let num = |r: std::ops::Range<usize>| ts.get(r).and_then(|s| s.parse::<i64>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) = (num(0..4), num(5..7), num(8..10), num(11..13), num(14..16), num(17..19)) else {
        return 0;
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return 0;
    }
    // Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days * 86_400 + h * 3_600 + mi * 60 + s).max(0) as u64
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn system_time_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn unix_now() -> u64 {
    system_time_secs(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tail_of(lines: &[&str]) -> Tail {
        classify_lines(&lines.join("\n")).0
    }

    const PROMPT: &str = r#"{"type":"user","cwd":"/home/me/Tinytosh","message":{"role":"user","content":"fix the bug"}}"#;
    const THINK: &str = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"..."}],"stop_reason":null}}"#;
    const EDIT: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Edit","input":{}}],"stop_reason":"tool_use"}}"#;
    const RESULT: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;
    const DENIED: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"The user doesn't want to proceed with this tool use."}]}}"#;
    const DONE: &str = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"All set."}],"stop_reason":"end_turn"}}"#;
    const TEXT: &str = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Working on it"}],"stop_reason":null}}"#;
    const LIMIT: &str = r#"{"type":"assistant","isApiErrorMessage":true,"apiErrorStatus":429,"message":{"content":[{"type":"text","text":"You've hit your session limit"}]}}"#;
    const META: &str = r#"{"type":"user","isMeta":true,"message":{"content":"caveat"}}"#;
    const SNAPSHOT: &str = r#"{"type":"file-history-snapshot","snapshot":{}}"#;
    const BASH: &str = r#"{"type":"assistant","timestamp":"2026-09-27T04:15:28.842Z","message":{"content":[{"type":"tool_use","id":"b1","name":"Bash","input":{}}]}}"#;
    const ASK: &str = r#"{"type":"assistant","timestamp":"2026-09-27T04:15:31.492Z","message":{"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}}]}}"#;
    const BASH_RESULT: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b1","content":"started"}]}}"#;
    const ASK_RESULT: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"q1","content":"answered"}]}}"#;
    const TITLE: &str = r#"{"type":"ai-title","title":"x"}"#;

    #[test]
    fn classifies_transcript_tails() {
        assert_eq!(tail_of(&[PROMPT]), Tail::Prompted);
        assert_eq!(tail_of(&[PROMPT, THINK]), Tail::Thinking);
        assert_eq!(tail_of(&[PROMPT, EDIT]), Tail::PendingTool("Edit".into(), 0));
        assert_eq!(tail_of(&[PROMPT, EDIT, RESULT]), Tail::Prompted);
        assert_eq!(tail_of(&[PROMPT, EDIT, DENIED]), Tail::Done);
        assert_eq!(tail_of(&[PROMPT, DONE]), Tail::Done);
        assert_eq!(tail_of(&[PROMPT, TEXT]), Tail::Text);
        assert_eq!(tail_of(&[PROMPT, LIMIT]), Tail::ApiLimit);
        assert_eq!(tail_of(&[PROMPT, DONE, META, SNAPSHOT]), Tail::Done);
        assert_eq!(tail_of(&["{not json", PROMPT, "garbage"]), Tail::Prompted);
        assert_eq!(tail_of(&[]), Tail::Unknown);
    }

    // Regression: a parallel tool result landing after a question must not hide the question.
    #[test]
    fn keeps_unanswered_call_across_parallel_results() {
        let asked_at = parse_timestamp("2026-09-27T04:15:31.492Z");
        assert_eq!(tail_of(&[PROMPT, BASH, ASK, BASH_RESULT, TITLE]), Tail::PendingTool("AskUserQuestion".into(), asked_at));
        assert_eq!(tail_of(&[PROMPT, ASK, BASH, BASH_RESULT]), Tail::PendingTool("AskUserQuestion".into(), asked_at));
        assert_eq!(tail_of(&[PROMPT, BASH, ASK, BASH_RESULT, ASK_RESULT]), Tail::Prompted);
        // A pending call from an earlier turn is cut off by the next typed prompt.
        assert_eq!(tail_of(&[ASK, PROMPT, THINK]), Tail::Thinking);
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), 0);
        assert_eq!(parse_timestamp("2026-09-27T04:15:31.492Z"), 1790482531);
        assert_eq!(parse_timestamp("2024-02-29T12:00:00Z"), 1709208000);
        assert_eq!(parse_timestamp("garbage"), 0);
        assert_eq!(parse_timestamp("2026-13-01T00:00:00Z"), 0);
    }

    #[test]
    fn extracts_project_from_cwd() {
        assert_eq!(classify_lines(PROMPT).1, "/home/me/Tinytosh");
        assert_eq!(classify_lines(THINK).1, "");
    }

    #[test]
    fn resolves_activity_over_time() {
        const T0: u64 = 1_000_000;
        let at = |tail: &Tail, age: u64, hook: bool, installed: bool| resolve(tail, T0 + age, T0, hook, installed).0;
        let edit = Tail::PendingTool("Edit".into(), 0);
        let bash = Tail::PendingTool("Bash".into(), 0);
        let task = Tail::PendingTool("Task".into(), 0);

        assert_eq!(at(&edit, 1, false, false), Activity::Writing);
        assert_eq!(at(&edit, 6, false, false), Activity::Permission);
        assert_eq!(at(&edit, 6, false, true), Activity::Writing);
        assert_eq!(at(&edit, 1, true, true), Activity::Permission);
        assert_eq!(at(&bash, 10, false, false), Activity::Tool);
        assert_eq!(at(&bash, 25, false, false), Activity::Permission);
        assert_eq!(at(&task, 500, false, false), Activity::Tool);
        assert_eq!(at(&Tail::PendingTool("AskUserQuestion".into(), 0), 0, false, true), Activity::Permission);
        assert_eq!(at(&Tail::Prompted, 10, false, false), Activity::Thinking);
        assert_eq!(at(&Tail::Prompted, STALE_WORK_SECS, false, false), Activity::Idle);
        assert_eq!(at(&Tail::Text, 1, false, false), Activity::Thinking);
        assert_eq!(at(&Tail::Text, 5, false, false), Activity::Idle);
        assert_eq!(at(&Tail::Done, 1, false, false), Activity::Idle);

        // Tool age counts from the request, not from the last file write.
        let old_bash = Tail::PendingTool("Bash".into(), T0);
        assert_eq!(resolve(&old_bash, T0 + 30, T0 + 29, false, false).0, Activity::Permission);
    }

    #[test]
    fn parses_usage_headers() {
        let headers: HashMap<&str, &str> = [
            ("anthropic-ratelimit-unified-5h-utilization", "0.423"),
            ("anthropic-ratelimit-unified-5h-reset", "1790000000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.18"),
            ("anthropic-ratelimit-unified-7d-reset", "1790500000"),
            ("anthropic-ratelimit-unified-status", "allowed"),
        ]
        .into_iter()
        .collect();
        let u = parse_usage_headers(|n| headers.get(n).map(|v| v.to_string())).unwrap();
        assert_eq!((u.five_hour_pct, u.weekly_pct, u.limited), (42, 18, false));
        assert_eq!((u.five_hour_reset, u.weekly_reset), (1790000000, 1790500000));

        let rejected: HashMap<&str, &str> =
            [("anthropic-ratelimit-unified-5h-utilization", "1.2"), ("anthropic-ratelimit-unified-status", "rejected")].into_iter().collect();
        let u = parse_usage_headers(|n| rejected.get(n).map(|v| v.to_string())).unwrap();
        assert_eq!((u.five_hour_pct, u.limited), (100, true));

        assert_eq!(parse_usage_headers(|_| None).unwrap_err(), UsageError::NoHeaders);
    }

    #[test]
    fn extracts_token_and_respects_expiry() {
        let blob = r#"{"claudeAiOauth":{"accessToken":"test-access-token","expiresAt":2000000000000}}"#;
        assert_eq!(extract_access_token(blob, 1_000).unwrap(), "test-access-token");
        assert_eq!(extract_access_token(blob, 2_000_000_001).unwrap_err(), UsageError::Expired);
        assert_eq!(extract_access_token("{}", 0).unwrap_err(), UsageError::NoToken);
        assert_eq!(extract_access_token("not json", 0).unwrap_err(), UsageError::NoToken);
    }

    #[test]
    fn installs_and_removes_hook_without_touching_other_settings() {
        let mut settings = serde_json::json!({
            "theme": "auto",
            "hooks": {
                "Stop": [{ "matcher": "", "hooks": [{ "type": "command", "command": "other-tool" }] }],
                "Notification": [{ "matcher": "", "hooks": [
                    { "type": "command", "command": "cat > ~/.claude/tinytosh-notify.json" },
                    { "type": "command", "command": "say hi" }
                ] }]
            },
            "effortLevel": "high"
        });
        let cmd = "cat > '/home/me/.claude/tinytosh-notify.json'";

        apply_hook(&mut settings, true, cmd).unwrap();
        let groups = settings["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "old Tinytosh entry replaced, other hook kept");
        assert_eq!(groups[0]["hooks"][0]["command"], "say hi");
        assert_eq!(groups[1]["hooks"][0]["command"], cmd);
        assert_eq!(settings["hooks"]["Stop"][0]["hooks"][0]["command"], "other-tool");

        // Enabling twice must not duplicate the entry.
        apply_hook(&mut settings, true, cmd).unwrap();
        assert_eq!(settings["hooks"]["Notification"].as_array().unwrap().len(), 2);

        apply_hook(&mut settings, false, cmd).unwrap();
        assert_eq!(settings["hooks"]["Notification"].as_array().unwrap().len(), 1);
        assert_eq!(settings["hooks"]["Notification"][0]["hooks"][0]["command"], "say hi");

        // Key order is preserved (serde_json preserve_order).
        let keys: Vec<&String> = settings.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["theme", "hooks", "effortLevel"]);
    }

    #[test]
    fn removes_empty_hook_sections_and_rejects_bad_layouts() {
        let mut settings = serde_json::json!({});
        apply_hook(&mut settings, true, "cmd tinytosh-notify.json").unwrap();
        apply_hook(&mut settings, false, "cmd tinytosh-notify.json").unwrap();
        assert_eq!(settings, serde_json::json!({}));

        assert!(apply_hook(&mut serde_json::json!([]), true, "x").is_err());
        assert!(apply_hook(&mut serde_json::json!({ "hooks": "nope" }), true, "x").is_err());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn quotes_hook_path_for_the_shell() {
        assert_eq!(
            hook_command(Path::new("/Users/o'neil/.claude/tinytosh-notify.jsonl")),
            "line=\"$(date +%s) $(tr -d '\\n')\"; printf '%s\\n' \"$line\" >> '/Users/o'\\''neil/.claude/tinytosh-notify.jsonl'"
        );
    }

    // The generated command must really append one "<epoch> <json>" line per call.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn hook_command_appends_parseable_lines() {
        let dir = std::env::temp_dir().join(format!("tinytosh-hook-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("tinytosh-notify.jsonl");
        let _ = fs::remove_file(&log);

        for payload in [r#"{"session_id":"s1","notification_type":"permission_prompt"}"#, "{\"session_id\":\"s2\",\n\"message\":\"x\"}"] {
            let mut child = std::process::Command::new("sh")
                .args(["-c", &hook_command(&log)])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
            assert!(child.wait().unwrap().success());
        }

        let events = parse_hook_log(&fs::read_to_string(&log).unwrap());
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].session_id.as_str(), events[0].is_permission), ("s1", true));
        assert_eq!((events[1].session_id.as_str(), events[1].is_permission), ("s2", false));
        assert!(events[0].at > 1_700_000_000);
    }

    #[test]
    fn matches_claude_code_turn_summary() {
        // Same 32-bit hash as Java's String.hashCode, including overflow.
        assert_eq!(string_hash("abc"), 96354);
        assert_eq!(string_hash("polygenelubricants"), i32::MIN);
        let verb = |uuid: &str| TURN_VERBS[(string_hash(uuid) as u32 % 8) as usize];
        assert_eq!(verb("abc"), "Churned");
        assert_eq!(verb("polygenelubricants"), "Baked", "negative hashes wrap like JavaScript's >>> 0");

        for (ms, text) in [
            (0, "0s"), (54_321, "54s"), (59_999, "59s"), (83_000, "1m 23s"), (119_600, "2m 0s"),
            (3_723_000, "1h 2m 3s"), (3_599_600, "1h 0m 0s"), (90_061_000, "1d 1h 1m"),
        ] {
            assert_eq!(format_turn_duration(ms), text, "{} ms", ms);
        }

        let line = r#"{"type":"system","subtype":"turn_duration","uuid":"abc","durationMs":54000}"#;
        let (_, _, summary) = classify_lines(&[PROMPT, DONE, line].join("\n"));
        assert_eq!(summary, "Churned for 54s");
        // A newer prompt starts a new turn, so the old summary no longer applies.
        let (_, _, summary) = classify_lines(&[line, PROMPT].join("\n"));
        assert_eq!(summary, "");
    }

    #[test]
    fn parses_hook_log() {
        let log = concat!(
            "1790000000 {\"session_id\":\"abc-1\",\"notification_type\":\"permission_prompt\"}\n",
            "garbage line\n",
            "\u{feff}1790000050 {\"session_id\":\"abc-1\",\"notification_type\":\"idle_prompt\"}\n",
            "1790000060 {\"notification_type\":\"setup\"}\n",
            "1790000070 {\"session_id\":\"../../evil\",\"message\":\"Claude needs your permission\"}\n",
        );
        let events = parse_hook_log(log);
        assert_eq!(events.len(), 3, "malformed and session-less lines are skipped");
        assert_eq!(events[0], HookEvent { session_id: "abc-1".into(), is_permission: true, at: 1790000000 });
        assert!(!events[1].is_permission);
        assert_eq!(events[2].session_id, "evil", "path characters are stripped from session ids");
        assert!(events[2].is_permission);
    }

    #[test]
    fn open_prompt_follows_the_hook_until_the_transcript_moves_on() {
        const T0: u64 = 1_000_000;
        // Prompt at T0+6 while the transcript last changed at T0 (question not written yet).
        assert!(hook_prompt_open(T0 + 6, T0, T0 + 10));
        // Answer written at T0+30: the notification is now older than the transcript.
        assert!(!hook_prompt_open(T0 + 6, T0 + 30, T0 + 31));
        assert!(!hook_prompt_open(0, T0, T0 + 1), "no notification");
        assert!(!hook_prompt_open(T0, T0, T0 + WAITING_WINDOW_SECS), "expires");
    }

    #[test]
    fn waiting_states_expire_after_the_waiting_window() {
        const T0: u64 = 1_000_000;
        let ask = Tail::PendingTool("AskUserQuestion".into(), T0);
        assert_eq!(resolve(&ask, T0 + 900, T0, false, true).0, Activity::Permission, "still waiting after 15 min");
        assert_eq!(resolve(&ask, T0 + WAITING_WINDOW_SECS, T0, false, true).0, Activity::Idle);
    }

    #[test]
    fn sanitizes_labels() {
        assert_eq!(sanitize_label("Café\u{0007} project with a very long name"), "Cafe project with a very");
        assert_eq!(path_basename("C:\\Users\\me\\repo\\"), "repo");
        let transcript = Path::new("/x/projects/-Users-me-my-app/abc.jsonl");
        assert_eq!(project_name(transcript, "/Users/me/my-app/src/lib"), "my-app");
        assert_eq!(project_name(transcript, "/Users/me/my-app"), "my-app");
        assert_eq!(project_name(transcript, "/elsewhere/tool"), "tool");
        assert_eq!(project_name(transcript, ""), "app");
    }
}

#[cfg(test)]
mod live {
    // Reads the local transcripts only (no network). Run with: cargo test live_ -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_activity_snapshot() {
        let snap = super::ActivityMonitor::new().poll();
        println!("{:?}", snap);
    }

    // Sends one real 1-token request with the local Claude Code login. Prints parsed numbers only.
    #[test]
    #[ignore]
    fn live_usage_poll() {
        let mut state = super::UsageState::default();
        super::UsagePoller::new().poll(&mut state);
        println!("usage={:?} error={:?}", state.last, state.error);
    }
}
