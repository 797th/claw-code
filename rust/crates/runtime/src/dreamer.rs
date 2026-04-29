//! Project-local persistent memory and consolidation ("dreaming").
//!
//! Agents append raw durable notes to daily logs under `.claw/memory/logs/`.
//! A dream pass reads those logs newest-first, asks the configured provider to
//! synthesize them, and writes `MEMORY.md` plus optional topic files at the
//! memory directory root.

use std::fmt::{Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use walkdir::WalkDir;

use crate::config::MemoryConfig;
use crate::conversation::{ApiClient, ApiRequest, AssistantEvent};
use crate::session::{ContentBlock, ConversationMessage, MessageRole};
use crate::session_control::SessionStore;

/// Primary memory file loaded into future prompts.
pub const MEMORY_FILENAME: &str = "MEMORY.md";
/// Legacy filename kept only so old consolidated output is not re-read as a raw log.
pub const CONSOLIDATED_MEMORY_FILENAME: &str = "CONSOLIDATED_MEMORY.md";
/// Lock file used to prevent concurrent consolidation passes.
pub const DREAM_LOCK_FILENAME: &str = ".consolidate-lock";
/// Marker touched after successful dream passes and used by auto-dream gates.
pub const LAST_DREAM_FILENAME: &str = ".last-dream";

const MAX_LOG_INPUT_BYTES: usize = 128 * 1024;
const MAX_MEMORY_PROMPT_BYTES: usize = 32 * 1024;
const MAX_MEMORY_PROMPT_LINES: usize = 400;
const AUTO_DREAM_MIN_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const AUTO_DREAM_MIN_TOUCHED_SESSIONS: usize = 5;

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

const DREAMER_SYSTEM_PROMPT: &str = r#"You are the Memory Consolidation Engine ("Dreamer") for an agentic coding CLI.

Objective: Read project-local raw memory logs and synthesize durable memory for future agent sessions. Produce one or more markdown files. The primary file must be MEMORY.md. Optional topic files may be emitted for stable, high-volume domains.

## Core Directives

1. De-duplicate and synthesize: Merge overlapping notes into a single definitive statement. Preserve the latest stable truth, not every historical mention.
2. Resolve time references: Remove relative dates such as "yesterday," "last week," or "recently." Convert completed work into durable facts and delete obsolete step-by-step plans.
3. Respect user corrections: Treat explicit user corrections and preferences as authoritative. Newer explicit instructions override older conflicting notes.
4. Purge obsolete references: Remove notes about deprecated functions, deleted directories, abandoned approaches, superseded decisions, and dead-end debugging paths.
5. Optimize for tokens: Keep MEMORY.md concise. Put bulky but still-current topic details in separate topic files.
6. Separate facts from uncertainty: Keep unresolved questions only when they are still relevant. Mark them clearly as open questions.

## High-Fidelity Preservation Rules

While compressing, do not alter, summarize, or approximate the following technical elements when they remain current and relevant:

- Algorithms and architecture: Preserve exact logic, routing rules, threshold values, invariants, and architectural constraints.
- Research and benchmarking data: Preserve hyperparameters, dataset splits, model names, evaluation metrics, and experiment results exactly.
- Environment configuration: Preserve exact port numbers, paths, environment variables, version constraints, service names, and orchestration steps.
- User preferences: Preserve durable coding, communication, workflow, and tool-use preferences without rephrasing them into weaker guidance.

## Reliability Rules

- Do not invent facts, decisions, file paths, preferences, or project state.
- Do not keep stale TODOs when later logs show the work was completed or abandoned.
- Do not preserve raw log ordering unless chronology is necessary to understand the final state.
- Do not include a preamble, apology, analysis notes, or conversational text outside the required file output.

## Output Format

Use one file block per output file:

--- FILE: MEMORY.md ---
# Memory

## User Preferences
- Durable preferences about communication, coding style, tools, review expectations, and workflow.

## Project State
- Current repository, product, architecture, and implementation facts.

## Technical Decisions
- Active decisions, constraints, invariants, and rationale that future agents must preserve.

## Environment
- Current paths, commands, ports, dependencies, credentials handling rules, and runtime assumptions.

## Open Questions
- Relevant unresolved questions or decisions still needing user input.
--- END FILE ---

If a section has no durable information, write `- None known.`. Optional topic files must also use `--- FILE: topic-name.md ---` blocks and must stay at the memory directory root."#;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A raw memory log file loaded from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryLog {
    /// Path relative to the memory directory, used as a prompt label.
    pub name: String,
    /// UTF-8 text content after any budget truncation.
    pub content: String,
}

/// Configuration for a dreamer pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamerConfig {
    /// Directory that contains `MEMORY.md`, topic files, and raw logs.
    pub memory_dir: PathBuf,
    /// Maximum combined input bytes before truncation.
    pub max_input_bytes: usize,
    /// Whether time/session gates should be bypassed. Locking is never bypassed.
    pub force: bool,
}

impl DreamerConfig {
    /// Construct config with default input budget.
    #[must_use]
    pub fn new(memory_dir: impl Into<PathBuf>) -> Self {
        Self {
            memory_dir: memory_dir.into(),
            max_input_bytes: MAX_LOG_INPUT_BYTES,
            force: false,
        }
    }

    /// Override maximum input bytes.
    #[must_use]
    pub fn with_max_input_bytes(mut self, max_input_bytes: usize) -> Self {
        self.max_input_bytes = max_input_bytes;
        self
    }

    /// Bypass auto-dream time/session gates while still respecting the lock.
    #[must_use]
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }
}

/// A single file emitted by a dream pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamerFileOutput {
    /// Relative root-level path, usually `MEMORY.md`.
    pub path: PathBuf,
    /// Markdown text ready to write.
    pub markdown: String,
}

/// The synthesized files produced by a dreamer pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamerOutput {
    /// Markdown for `MEMORY.md`, kept for compatibility with callers that only
    /// care about the primary memory file.
    pub markdown: String,
    /// All files to write at the memory directory root.
    pub files: Vec<DreamerFileOutput>,
    /// Number of raw log files fed to the model.
    pub log_count: usize,
    /// Combined byte size of all logs fed to the model.
    pub input_bytes: usize,
}

/// Result metadata for a completed dream pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamRun {
    pub memory_dir: PathBuf,
    pub files_written: Vec<PathBuf>,
    pub log_count: usize,
    pub input_bytes: usize,
}

/// Auto-dream gate decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DreamGate {
    Ready,
    Disabled,
    Locked,
    TooSoon { remaining: Duration },
    TooFewSessions { touched: usize, required: usize },
}

/// Errors raised during memory or dream operations.
#[derive(Debug)]
pub enum DreamerError {
    Io(std::io::Error),
    Api(String),
    NoLogs,
    Locked,
    InvalidOutput(String),
}

impl Display for DreamerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "dreamer I/O error: {error}"),
            Self::Api(msg) => write!(f, "dreamer API error: {msg}"),
            Self::NoLogs => write!(f, "dreamer: no memory log files found in directory"),
            Self::Locked => write!(f, "dreamer: another consolidation pass is already running"),
            Self::InvalidOutput(msg) => write!(f, "dreamer produced invalid output: {msg}"),
        }
    }
}

impl std::error::Error for DreamerError {}

impl From<std::io::Error> for DreamerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Runtime API for project-local memory operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryManager {
    cwd: PathBuf,
    config: MemoryConfig,
}

impl MemoryManager {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, config: MemoryConfig) -> Self {
        Self {
            cwd: cwd.into(),
            config,
        }
    }

    #[must_use]
    pub fn config(&self) -> &MemoryConfig {
        &self.config
    }

    #[must_use]
    pub fn memory_dir(&self) -> PathBuf {
        resolve_memory_dir(&self.cwd, &self.config)
    }

    pub fn append_daily_log(&self, note: impl AsRef<str>) -> Result<PathBuf, DreamerError> {
        append_daily_log(&self.memory_dir(), note)
    }

    pub fn load_memory_prompt(&self) -> Result<Option<String>, DreamerError> {
        if !self.config.auto_memory_enabled() {
            return Ok(None);
        }
        load_memory_prompt(&self.memory_dir()).map_err(DreamerError::Io)
    }

    pub fn dream_config(&self) -> DreamerConfig {
        DreamerConfig::new(self.memory_dir())
    }

    pub fn auto_dream_gate(&self) -> Result<DreamGate, DreamerError> {
        auto_dream_gate(
            &self.memory_dir(),
            &self.cwd,
            self.config.auto_dream_enabled(),
            false,
        )
    }

    pub fn run_dream(
        &self,
        client: &mut impl ApiClient,
        force: bool,
    ) -> Result<DreamRun, DreamerError> {
        run_dreamer_pass(&self.dream_config().with_force(force), client)
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Resolve the configured memory directory. Relative config paths are resolved
/// from the workspace cwd. The default is `.claw/memory`.
#[must_use]
pub fn resolve_memory_dir(cwd: &Path, config: &MemoryConfig) -> PathBuf {
    match config.auto_memory_directory() {
        Some(path) => {
            let configured = PathBuf::from(path);
            if configured.is_absolute() {
                configured
            } else {
                cwd.join(configured)
            }
        }
        None => cwd.join(".claw").join("memory"),
    }
}

/// Append a durable note to today's raw memory log.
pub fn append_daily_log(memory_dir: &Path, note: impl AsRef<str>) -> Result<PathBuf, DreamerError> {
    append_daily_log_for_time(memory_dir, note.as_ref(), SystemTime::now())
}

/// Load `MEMORY.md` for prompt injection with fixed byte and line caps.
pub fn load_memory_prompt(memory_dir: &Path) -> io::Result<Option<String>> {
    let path = memory_dir.join(MEMORY_FILENAME);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let truncated = truncate_for_prompt(&content, MAX_MEMORY_PROMPT_BYTES, MAX_MEMORY_PROMPT_LINES);
    if truncated.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "# Persistent Memory\n\
         Loaded from {}. Treat this as durable project-local context. During normal turns, do not edit `{MEMORY_FILENAME}` directly; append new durable notes to today's log under `logs/YYYY/MM/YYYY-MM-DD.md` and use `/dream` to consolidate.\n\n{}",
        path.display(),
        truncated.trim_end()
    )))
}

/// Recursively load `.md` memory logs newest-first, skipping generated memory
/// files, lock/marker files, and empty files.
pub fn collect_memory_logs(
    memory_dir: &Path,
    max_input_bytes: usize,
) -> Result<Vec<MemoryLog>, DreamerError> {
    let mut entries = Vec::new();
    if !memory_dir.exists() {
        return Ok(Vec::new());
    }

    for entry in WalkDir::new(memory_dir).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return Err(DreamerError::Io(io::Error::other(error.to_string()))),
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !is_memory_log_candidate(path) {
            continue;
        }
        let relative = path
            .strip_prefix(memory_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .unwrap_or(UNIX_EPOCH);
        entries.push((modified, relative, path.to_path_buf()));
    }

    entries.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));

    let mut logs = Vec::new();
    let mut total_bytes: usize = 0;

    for (_modified, name, path) in entries {
        let content = fs::read_to_string(&path)?;
        if content.trim().is_empty() {
            continue;
        }

        let available = max_input_bytes.saturating_sub(total_bytes);
        if available == 0 {
            break;
        }

        let truncated = truncate_utf8_bytes(&content, available);
        total_bytes += truncated.len();
        logs.push(MemoryLog {
            name,
            content: truncated,
        });
    }

    Ok(logs)
}

/// Call the model with the Dreamer prompt and return synthesized files.
pub fn consolidate_memory(
    logs: &[MemoryLog],
    client: &mut impl ApiClient,
) -> Result<DreamerOutput, DreamerError> {
    if logs.is_empty() {
        return Err(DreamerError::NoLogs);
    }

    let user_message = build_user_message(logs);
    let input_bytes: usize = logs.iter().map(|l| l.content.len()).sum();

    let request = ApiRequest {
        system_prompt: vec![DREAMER_SYSTEM_PROMPT.to_string()],
        messages: vec![ConversationMessage {
            role: MessageRole::User,
            blocks: vec![ContentBlock::Text { text: user_message }],
            usage: None,
        }],
    };

    let events = client
        .stream(request)
        .map_err(|e| DreamerError::Api(e.to_string()))?;

    let raw = collect_text_from_events(&events);
    let files = parse_dream_output(&raw)?;
    let markdown = files
        .iter()
        .find(|file| file.path == Path::new(MEMORY_FILENAME))
        .map(|file| file.markdown.clone())
        .ok_or_else(|| DreamerError::InvalidOutput("missing MEMORY.md output".to_string()))?;
    validate_memory_markdown(&markdown)?;

    Ok(DreamerOutput {
        markdown,
        files,
        log_count: logs.len(),
        input_bytes,
    })
}

/// Write dream output files to the memory directory using temp-file writes.
pub fn write_consolidated_memory(
    output: &DreamerOutput,
    memory_dir: &Path,
) -> Result<Vec<PathBuf>, DreamerError> {
    fs::create_dir_all(memory_dir)?;
    let mut written = Vec::new();
    for file in &output.files {
        validate_output_path(&file.path)?;
        if file.markdown.trim().is_empty() {
            return Err(DreamerError::InvalidOutput(format!(
                "{} is empty",
                file.path.display()
            )));
        }
        let dest = memory_dir.join(&file.path);
        atomic_write(&dest, ensure_trailing_newline(&file.markdown).as_bytes())?;
        written.push(dest);
    }
    Ok(written)
}

/// Convenience: collect logs, consolidate, write files, and update dream marker.
pub fn run_dreamer_pass(
    config: &DreamerConfig,
    client: &mut impl ApiClient,
) -> Result<DreamRun, DreamerError> {
    fs::create_dir_all(&config.memory_dir)?;
    let _lock = DreamLock::try_acquire(&config.memory_dir)?;

    let logs = collect_memory_logs(&config.memory_dir, config.max_input_bytes)?;
    let output = consolidate_memory(&logs, client)?;
    let files_written = write_consolidated_memory(&output, &config.memory_dir)?;
    touch_last_dream_marker(&config.memory_dir)?;

    Ok(DreamRun {
        memory_dir: config.memory_dir.clone(),
        files_written,
        log_count: output.log_count,
        input_bytes: output.input_bytes,
    })
}

/// Check auto-dream gates for the current workspace.
pub fn auto_dream_gate(
    memory_dir: &Path,
    cwd: &Path,
    auto_dream_enabled: bool,
    force: bool,
) -> Result<DreamGate, DreamerError> {
    if !auto_dream_enabled && !force {
        return Ok(DreamGate::Disabled);
    }
    if lock_exists(memory_dir) {
        return Ok(DreamGate::Locked);
    }
    if force {
        return Ok(DreamGate::Ready);
    }

    let last_dream = last_dream_time(memory_dir);
    if let Some(last_dream) = last_dream {
        let elapsed = SystemTime::now()
            .duration_since(last_dream)
            .unwrap_or(Duration::ZERO);
        if elapsed < AUTO_DREAM_MIN_INTERVAL {
            return Ok(DreamGate::TooSoon {
                remaining: AUTO_DREAM_MIN_INTERVAL - elapsed,
            });
        }
    }

    let touched = touched_sessions_since(cwd, last_dream)?;
    if touched < AUTO_DREAM_MIN_TOUCHED_SESSIONS {
        return Ok(DreamGate::TooFewSessions {
            touched,
            required: AUTO_DREAM_MIN_TOUCHED_SESSIONS,
        });
    }
    Ok(DreamGate::Ready)
}

/// Run an auto-dream pass only when gates allow it.
pub fn maybe_run_auto_dream(
    config: &DreamerConfig,
    cwd: &Path,
    auto_dream_enabled: bool,
    client: &mut impl ApiClient,
) -> Result<Option<DreamRun>, DreamerError> {
    if auto_dream_gate(&config.memory_dir, cwd, auto_dream_enabled, config.force)?
        != DreamGate::Ready
    {
        return Ok(None);
    }
    run_dreamer_pass(config, client).map(Some)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn append_daily_log_for_time(
    memory_dir: &Path,
    note: &str,
    time: SystemTime,
) -> Result<PathBuf, DreamerError> {
    let note = note.trim();
    if note.is_empty() {
        return Err(DreamerError::InvalidOutput(
            "memory log note is empty".to_string(),
        ));
    }

    let date = utc_date_string(time);
    let (year, month) = (&date[0..4], &date[5..7]);
    let logs_dir = memory_dir.join("logs").join(year).join(month);
    fs::create_dir_all(&logs_dir)?;
    let path = logs_dir.join(format!("{date}.md"));
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "\n- {note}")?;
    Ok(path)
}

fn is_memory_log_candidate(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if matches!(
        name,
        MEMORY_FILENAME | CONSOLIDATED_MEMORY_FILENAME | DREAM_LOCK_FILENAME | LAST_DREAM_FILENAME
    ) {
        return false;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

fn truncate_utf8_bytes(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }
    let mut end = max_bytes;
    while !content.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    content[..end].to_string()
}

fn truncate_for_prompt(content: &str, max_bytes: usize, max_lines: usize) -> String {
    let byte_capped = truncate_utf8_bytes(content, max_bytes);
    let mut lines = byte_capped
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n");
    if content.len() > byte_capped.len() || byte_capped.lines().count() > max_lines {
        lines.push_str("\n\n[truncated]");
    }
    lines
}

fn build_user_message(logs: &[MemoryLog]) -> String {
    let mut buf = String::from(
        "Below are raw memory log files, newest first. Consolidate them into MEMORY.md \
         and optional root-level topic files using the required file block format.\n\n",
    );

    for log in logs {
        buf.push_str("---\n");
        buf.push_str("File: ");
        buf.push_str(&log.name);
        buf.push('\n');
        buf.push_str("---\n");
        buf.push_str(&log.content);
        if !log.content.ends_with('\n') {
            buf.push('\n');
        }
        buf.push('\n');
    }

    buf
}

fn collect_text_from_events(events: &[AssistantEvent]) -> String {
    let mut text = String::new();
    for event in events {
        if let AssistantEvent::TextDelta(delta) = event {
            text.push_str(delta);
        }
    }
    text.trim().to_string()
}

fn parse_dream_output(raw: &str) -> Result<Vec<DreamerFileOutput>, DreamerError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(DreamerError::InvalidOutput(
            "model returned empty text".to_string(),
        ));
    }

    let mut files = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_body = String::new();
    let mut saw_file_marker = false;

    for line in raw.lines() {
        if let Some(path) = parse_file_marker(line) {
            saw_file_marker = true;
            if let Some(path) = current_path.take() {
                files.push(DreamerFileOutput {
                    path,
                    markdown: current_body.trim().to_string(),
                });
                current_body.clear();
            }
            current_path = Some(path);
            continue;
        }
        if line.trim() == "--- END FILE ---" {
            if let Some(path) = current_path.take() {
                files.push(DreamerFileOutput {
                    path,
                    markdown: current_body.trim().to_string(),
                });
                current_body.clear();
            }
            continue;
        }
        if current_path.is_some() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    if let Some(path) = current_path {
        files.push(DreamerFileOutput {
            path,
            markdown: current_body.trim().to_string(),
        });
    }

    if saw_file_marker {
        if files.is_empty() {
            return Err(DreamerError::InvalidOutput(
                "file markers contained no file content".to_string(),
            ));
        }
        for file in &files {
            validate_output_path(&file.path)?;
        }
        return Ok(files);
    }

    Ok(vec![DreamerFileOutput {
        path: PathBuf::from(MEMORY_FILENAME),
        markdown: raw.to_string(),
    }])
}

fn parse_file_marker(line: &str) -> Option<PathBuf> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("--- FILE: ")?;
    let path = rest.strip_suffix(" ---")?.trim();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

fn validate_memory_markdown(markdown: &str) -> Result<(), DreamerError> {
    let trimmed = markdown.trim();
    if trimmed.is_empty() {
        return Err(DreamerError::InvalidOutput(
            "MEMORY.md output is empty".to_string(),
        ));
    }
    if !trimmed
        .lines()
        .any(|line| line.trim_start().starts_with('#'))
    {
        return Err(DreamerError::InvalidOutput(
            "MEMORY.md output has no markdown heading".to_string(),
        ));
    }
    Ok(())
}

fn validate_output_path(path: &Path) -> Result<(), DreamerError> {
    if path.is_absolute() {
        return Err(DreamerError::InvalidOutput(format!(
            "absolute output path is not allowed: {}",
            path.display()
        )));
    }
    if path.components().count() != 1
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(DreamerError::InvalidOutput(format!(
            "output path must be a root-level file: {}",
            path.display()
        )));
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
        return Err(DreamerError::InvalidOutput(format!(
            "output path must be markdown: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_trailing_newline(markdown: &str) -> String {
    if markdown.ends_with('\n') {
        markdown.to_string()
    } else {
        format!("{markdown}\n")
    }
}

fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("memory");
    let tmp_path = parent.join(format!(
        ".{file_name}.tmp-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos()
    ));
    {
        let mut file = File::create(&tmp_path)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn touch_last_dream_marker(memory_dir: &Path) -> io::Result<()> {
    atomic_write(
        &memory_dir.join(LAST_DREAM_FILENAME),
        format!(
            "{}\n",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs()
        )
        .as_bytes(),
    )
}

fn last_dream_time(memory_dir: &Path) -> Option<SystemTime> {
    fs::metadata(memory_dir.join(LAST_DREAM_FILENAME))
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .or_else(|| {
            fs::metadata(memory_dir.join(MEMORY_FILENAME))
                .ok()
                .and_then(|metadata| metadata.modified().ok())
        })
}

fn touched_sessions_since(cwd: &Path, since: Option<SystemTime>) -> Result<usize, DreamerError> {
    let store = SessionStore::from_cwd(cwd)
        .map_err(|error| DreamerError::Io(io::Error::other(error.to_string())))?;
    let sessions = store
        .list_sessions()
        .map_err(|error| DreamerError::Io(io::Error::other(error.to_string())))?;
    let Some(since) = since else {
        return Ok(sessions.len());
    };
    let since_ms = since
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis();
    Ok(sessions
        .into_iter()
        .filter(|session| u128::from(session.updated_at_ms) > since_ms)
        .count())
}

fn lock_exists(memory_dir: &Path) -> bool {
    memory_dir.join(DREAM_LOCK_FILENAME).exists()
}

struct DreamLock {
    path: PathBuf,
}

impl DreamLock {
    fn try_acquire(memory_dir: &Path) -> Result<Self, DreamerError> {
        fs::create_dir_all(memory_dir)?;
        let path = memory_dir.join(DREAM_LOCK_FILENAME);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "pid={}", std::process::id())?;
                Ok(Self { path })
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(DreamerError::Locked),
            Err(error) => Err(DreamerError::Io(error)),
        }
    }
}

impl Drop for DreamLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn utc_date_string(time: SystemTime) -> String {
    let days = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

// Howard Hinnant's civil date conversion, adapted for Unix days.
fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(m <= 2);
    (year as i32, m as u32, d as u32)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{ApiRequest, AssistantEvent, RuntimeError};

    struct StubClient {
        response: String,
    }

    impl ApiClient for StubClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(vec![
                AssistantEvent::TextDelta(self.response.clone()),
                AssistantEvent::MessageStop,
            ])
        }
    }

    #[test]
    fn consolidate_returns_model_text_as_memory_file() {
        let logs = vec![
            MemoryLog {
                name: "logs/2026/04/2026-04-29.md".to_string(),
                content: "- user is a Rust developer".to_string(),
            },
            MemoryLog {
                name: "logs/2026/04/2026-04-28.md".to_string(),
                content: "- project: claw-code CLI in Rust".to_string(),
            },
        ];

        let expected = "# Memory\n\n## User Preferences\n- Rust developer.";
        let mut client = StubClient {
            response: expected.to_string(),
        };

        let result = consolidate_memory(&logs, &mut client).unwrap();

        assert_eq!(result.markdown, expected);
        assert_eq!(result.files[0].path, PathBuf::from(MEMORY_FILENAME));
        assert_eq!(result.log_count, 2);
    }

    #[test]
    fn consolidate_parses_topic_file_blocks() {
        let logs = vec![MemoryLog {
            name: "logs/2026/04/2026-04-29.md".to_string(),
            content: "- keep exact benchmark values".to_string(),
        }];
        let mut client = StubClient {
            response: "--- FILE: MEMORY.md ---\n# Memory\n\n- See benchmark.md.\n--- END FILE ---\n--- FILE: benchmark.md ---\n# Benchmark\n\n- value = 42\n--- END FILE ---".to_string(),
        };

        let result = consolidate_memory(&logs, &mut client).unwrap();

        assert_eq!(result.files.len(), 2);
        assert!(result
            .files
            .iter()
            .any(|file| file.path == PathBuf::from("benchmark.md")));
    }

    #[test]
    fn consolidate_returns_no_logs_error_on_empty_input() {
        let mut client = StubClient {
            response: String::new(),
        };
        let err = consolidate_memory(&[], &mut client).unwrap_err();
        assert!(matches!(err, DreamerError::NoLogs));
    }

    #[test]
    fn collect_memory_logs_is_recursive_and_skips_generated_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::create_dir_all(path.join("logs").join("2026").join("04")).unwrap();

        fs::write(path.join("logs/2026/04/2026-04-29.md"), "new note").unwrap();
        fs::write(path.join("logs/2026/04/2026-04-28.md"), "old note").unwrap();
        fs::write(path.join(MEMORY_FILENAME), "old memory").unwrap();
        fs::write(path.join(CONSOLIDATED_MEMORY_FILENAME), "legacy").unwrap();
        fs::write(path.join(DREAM_LOCK_FILENAME), "locked").unwrap();
        fs::write(path.join("empty.md"), "   \n").unwrap();

        let logs = collect_memory_logs(path, MAX_LOG_INPUT_BYTES).unwrap();
        let names: Vec<_> = logs.iter().map(|l| l.name.as_str()).collect();

        assert_eq!(names.len(), 2);
        assert!(names.contains(&"logs/2026/04/2026-04-29.md"));
        assert!(names.contains(&"logs/2026/04/2026-04-28.md"));
        assert!(!names.contains(&MEMORY_FILENAME));
        assert!(!names.contains(&DREAM_LOCK_FILENAME));
    }

    #[test]
    fn collect_memory_logs_respects_byte_budget_on_utf8_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("a.md"), "é".repeat(10)).unwrap();

        let logs = collect_memory_logs(path, 3).unwrap();

        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].content, "é");
    }

    #[test]
    fn load_memory_prompt_applies_line_and_byte_caps() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(MEMORY_FILENAME),
            (0..500)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let prompt = load_memory_prompt(dir.path()).unwrap().unwrap();

        assert!(prompt.contains("# Persistent Memory"));
        assert!(prompt.contains("[truncated]"));
        assert!(prompt.lines().count() <= MAX_MEMORY_PROMPT_LINES + 5);
    }

    #[test]
    fn write_consolidated_memory_creates_memory_and_topic_files() {
        let dir = tempfile::tempdir().unwrap();
        let output = DreamerOutput {
            markdown: "# Memory\n\n- Durable fact.".to_string(),
            files: vec![
                DreamerFileOutput {
                    path: PathBuf::from(MEMORY_FILENAME),
                    markdown: "# Memory\n\n- Durable fact.".to_string(),
                },
                DreamerFileOutput {
                    path: PathBuf::from("rust.md"),
                    markdown: "# Rust\n\n- Use cargo test.".to_string(),
                },
            ],
            log_count: 1,
            input_bytes: 40,
        };

        let written = write_consolidated_memory(&output, dir.path()).unwrap();

        assert_eq!(written.len(), 2);
        assert!(dir.path().join(MEMORY_FILENAME).exists());
        assert!(dir.path().join("rust.md").exists());
    }

    #[test]
    fn write_consolidated_memory_rejects_empty_or_nested_paths() {
        let dir = tempfile::tempdir().unwrap();
        let output = DreamerOutput {
            markdown: "# Memory".to_string(),
            files: vec![DreamerFileOutput {
                path: PathBuf::from("nested/topic.md"),
                markdown: "# Topic".to_string(),
            }],
            log_count: 1,
            input_bytes: 40,
        };

        let err = write_consolidated_memory(&output, dir.path()).unwrap_err();
        assert!(matches!(err, DreamerError::InvalidOutput(_)));
    }

    #[test]
    fn lock_blocks_concurrent_dreams_and_failed_output_preserves_existing_memory() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(MEMORY_FILENAME), "# Memory\n\n- Existing.").unwrap();
        let _lock = DreamLock::try_acquire(dir.path()).unwrap();
        let mut client = StubClient {
            response: "# Memory\n\n- Replacement.".to_string(),
        };

        let err = run_dreamer_pass(&DreamerConfig::new(dir.path()), &mut client).unwrap_err();

        assert!(matches!(err, DreamerError::Locked));
        assert_eq!(
            fs::read_to_string(dir.path().join(MEMORY_FILENAME)).unwrap(),
            "# Memory\n\n- Existing."
        );
    }

    #[test]
    fn append_daily_log_uses_expected_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = append_daily_log_for_time(
            dir.path(),
            "remember permission preferences",
            UNIX_EPOCH + Duration::from_secs(1_777_420_800),
        )
        .unwrap();

        assert!(path.ends_with("logs/2026/04/2026-04-29.md"));
        assert!(fs::read_to_string(path)
            .unwrap()
            .contains("remember permission preferences"));
    }
}
