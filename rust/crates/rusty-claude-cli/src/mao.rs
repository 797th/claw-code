/// MAO Phase 1: Agent Topologies & Task Delegation
///
/// Implements the core parent-child orchestration hierarchy:
///   1. ManagerAgent decomposes the user prompt into typed sub-task briefs (JSON)
///   2. Specialized Subagents (Frontend, Backend, Database, Generic) execute briefs concurrently
///   3. Results aggregate into a unified response returned to the user
///
/// Phase 1 constraint: Subagents do not communicate with each other; they return
/// text/code to the Manager which aggregates. Cross-agent context sharing is Phase 2.

use api::{
    ApiError, InputContentBlock, InputMessage, MessageRequest, OutputContentBlock, ProviderClient,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Agent persona system prompts (Step 1.1)
// ---------------------------------------------------------------------------

const MANAGER_SYSTEM_PROMPT: &str = r#"You are the ManagerAgent in a multi-agent orchestration system.

Your sole responsibility in this turn is to DECOMPOSE the user's task into a list of
independent sub-task briefs. Each brief targets a single specialized subagent.

RULES:
- Output ONLY valid JSON — no prose, no markdown fences, no explanation.
- The JSON must be an array of objects, each with exactly these fields:
    {
      "id": <integer, 1-based>,
      "agent": <"frontend" | "backend" | "database" | "generic">,
      "brief": <clear, self-contained task description as a string>
    }
- Keep briefs isolated: each subagent receives ONLY its own brief.
- Prefer 2–5 sub-tasks. Combine trivially related work into one brief.
- If the task is trivially single-agent, emit a single-element array.

Example output:
[
  {"id":1,"agent":"backend","brief":"Create a REST endpoint POST /api/login that validates email+password against a users table and returns a JWT."},
  {"id":2,"agent":"frontend","brief":"Build a React login form that POSTs to /api/login and stores the returned JWT in localStorage."},
  {"id":3,"agent":"database","brief":"Write the SQL schema for a users table with id, email, password_hash, and created_at columns."}
]"#;

const FRONTEND_SYSTEM_PROMPT: &str = r#"You are the FrontendAgent — a specialist in UI, React, HTML, CSS, and browser APIs.
You receive a focused task brief and must produce concrete, working code or a clear implementation.
Output your code in fenced code blocks. Be precise — the Manager will aggregate your output directly."#;

const BACKEND_SYSTEM_PROMPT: &str = r#"You are the BackendAgent — a specialist in server-side logic, REST/GraphQL APIs, authentication, and Rust/Node.js/Python.
You receive a focused task brief and must produce concrete, working code or a clear implementation.
Output your code in fenced code blocks. Be precise — the Manager will aggregate your output directly."#;

const DATABASE_SYSTEM_PROMPT: &str = r#"You are the DatabaseAgent — a specialist in SQL schema design, migrations, indexing, and query optimization.
You receive a focused task brief and must produce concrete SQL or migration files.
Output your code in fenced code blocks. Be precise — the Manager will aggregate your output directly."#;

const GENERIC_SYSTEM_PROMPT: &str = r#"You are a specialized coding agent. You receive a focused task brief and must produce concrete, working code or a clear implementation plan.
Output your code in fenced code blocks. Be precise — the Manager will aggregate your output directly."#;

fn persona_for_agent(agent: &str) -> &'static str {
    match agent {
        "frontend" => FRONTEND_SYSTEM_PROMPT,
        "backend" => BACKEND_SYSTEM_PROMPT,
        "database" => DATABASE_SYSTEM_PROMPT,
        _ => GENERIC_SYSTEM_PROMPT,
    }
}

// ---------------------------------------------------------------------------
// Sub-task types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTask {
    pub id: u32,
    pub agent: String,
    pub brief: String,
}

#[derive(Debug, Clone)]
pub struct SubTaskResult {
    pub task: SubTask,
    pub output: String,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Orchestration error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum MaoError {
    Api(api::ApiError),
    ParseJson(String),
    NoTasks,
    Tokio(String),
}

impl std::fmt::Display for MaoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api(e) => write!(f, "API error: {e}"),
            Self::ParseJson(msg) => write!(f, "Manager JSON parse error: {msg}"),
            Self::NoTasks => write!(f, "Manager returned no sub-tasks"),
            Self::Tokio(msg) => write!(f, "Async runtime error: {msg}"),
        }
    }
}

impl std::error::Error for MaoError {}

impl From<api::ApiError> for MaoError {
    fn from(e: api::ApiError) -> Self {
        Self::Api(e)
    }
}

// ---------------------------------------------------------------------------
// Step 1.2: Decomposition loop — Manager decomposes the user prompt
// ---------------------------------------------------------------------------

/// Send prompt to the ManagerAgent and return parsed SubTasks.
/// The Manager is given up to `max_refinement_cycles` inference turns to
/// produce valid JSON; on each failed parse it is asked to correct itself.
async fn decompose_prompt(
    client: &ProviderClient,
    model: &str,
    user_prompt: &str,
    max_refinement_cycles: usize,
) -> Result<Vec<SubTask>, MaoError> {
    let max_tokens = 2048u32;
    let mut messages: Vec<InputMessage> = vec![InputMessage::user_text(user_prompt)];

    for cycle in 0..=max_refinement_cycles {
        let request = MessageRequest {
            model: model.to_string(),
            max_tokens,
            messages: messages.clone(),
            system: Some(MANAGER_SYSTEM_PROMPT.to_string()),
            ..Default::default()
        };
        let response = client.send_message(&request).await?;

        // Extract text from first content block
        let raw_text = response
            .content
            .iter()
            .find_map(|block| {
                if let OutputContentBlock::Text { text } = block {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        // Try to parse JSON from the response
        let trimmed = raw_text.trim();
        // Strip markdown code fence if present
        let json_str = if trimmed.starts_with("```") {
            trimmed
                .lines()
                .skip(1)
                .take_while(|l| !l.starts_with("```"))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            trimmed.to_string()
        };

        match serde_json::from_str::<Vec<SubTask>>(&json_str) {
            Ok(tasks) if tasks.is_empty() => {
                return Err(MaoError::NoTasks);
            }
            Ok(tasks) => {
                return Ok(tasks);
            }
            Err(parse_err) => {
                if cycle == max_refinement_cycles {
                    return Err(MaoError::ParseJson(format!(
                        "After {max_refinement_cycles} refinement cycle(s), Manager still produced invalid JSON.\nLast parse error: {parse_err}\nLast response:\n{raw_text}"
                    )));
                }
                // Inject correction request and continue the refinement loop
                messages.push(InputMessage {
                    role: "assistant".to_string(),
                    content: vec![InputContentBlock::Text { text: raw_text }],
                });
                messages.push(InputMessage::user_text(format!(
                    "Your output could not be parsed as JSON: {parse_err}.\n\
                     Respond with ONLY a valid JSON array of sub-task objects and nothing else."
                )));
            }
        }
    }

    Err(MaoError::ParseJson("Decomposition loop exhausted".into()))
}

// ---------------------------------------------------------------------------
// Step 1.3: Subagent spawning & aggregation
// ---------------------------------------------------------------------------

/// Execute a single sub-task against a specialized subagent model.
async fn run_subagent(
    client: Arc<ProviderClient>,
    model: String,
    task: SubTask,
) -> SubTaskResult {
    let system_prompt = persona_for_agent(&task.agent).to_string();
    let request = MessageRequest {
        model: model.clone(),
        max_tokens: 4096,
        messages: vec![InputMessage::user_text(task.brief.clone())],
        system: Some(system_prompt),
        ..Default::default()
    };

    match client.send_message(&request).await {
        Ok(response) => {
            let output = response
                .content
                .iter()
                .filter_map(|block| {
                    if let OutputContentBlock::Text { text } = block {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            SubTaskResult {
                task,
                output,
                error: None,
            }
        }
        Err(e) => SubTaskResult {
            task,
            output: String::new(),
            error: Some(e.to_string()),
        },
    }
}

/// Spawn all subagents concurrently and collect their results.
async fn spawn_subagents(
    client: Arc<ProviderClient>,
    model: &str,
    tasks: Vec<SubTask>,
) -> Vec<SubTaskResult> {
    let handles: Vec<_> = tasks
        .into_iter()
        .map(|task| {
            let client = Arc::clone(&client);
            let model = model.to_string();
            tokio::spawn(run_subagent(client, model, task))
        })
        .collect();

    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(e) => {
                // JoinError — task panicked; record it but continue
                eprintln!("[mao] subagent task panicked: {e}");
            }
        }
    }
    // Sort by task id so output is deterministic
    results.sort_by_key(|r| r.task.id);
    results
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Phase 1 orchestration: decompose → spawn subagents → aggregate.
///
/// `manager_model`  — high-reasoning model for decomposition (e.g. openai/gpt-oss-120b)
/// `worker_model`   — model for subagents (may be same or a cheaper model)
/// `user_prompt`    — raw user request
pub fn run_orchestrate(
    manager_model: &str,
    worker_model: &str,
    user_prompt: &str,
) -> Result<OrchestrationOutput, MaoError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| MaoError::Tokio(e.to_string()))?;

    rt.block_on(async {
        let client = Arc::new(
            ProviderClient::from_model(manager_model).map_err(MaoError::Api)?,
        );

        // ── Step 1.2: Manager decomposes the prompt ──────────────────────
        eprintln!("[mao] Manager decomposing prompt with model {manager_model}…");
        let tasks = decompose_prompt(&client, manager_model, user_prompt, 2).await?;
        eprintln!("[mao] Manager produced {} sub-task(s)", tasks.len());

        // ── Step 1.3: Spawn subagents concurrently ───────────────────────
        let worker_client = if worker_model == manager_model {
            Arc::clone(&client)
        } else {
            Arc::new(
                ProviderClient::from_model(worker_model).map_err(MaoError::Api)?,
            )
        };

        let task_count = tasks.len();
        eprintln!("[mao] Spawning {task_count} subagent(s) with model {worker_model}…");
        let results = spawn_subagents(worker_client, worker_model, tasks).await;

        Ok(OrchestrationOutput { results })
    })
}

/// Aggregated output from a Phase 1 orchestration run.
pub struct OrchestrationOutput {
    pub results: Vec<SubTaskResult>,
}

impl OrchestrationOutput {
    /// Render results as human-readable text for the CLI.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        for r in &self.results {
            out.push_str(&format!(
                "\n## Task {} — {} agent\n\n**Brief:** {}\n\n",
                r.task.id,
                capitalize(&r.task.agent),
                r.task.brief,
            ));
            if let Some(err) = &r.error {
                out.push_str(&format!("**Error:** {err}\n"));
            } else {
                out.push_str(&r.output);
                out.push('\n');
            }
            out.push_str("---\n");
        }
        out
    }

    /// Render results as JSON for `--output-format json`.
    pub fn render_json(&self) -> String {
        let value = serde_json::json!({
            "type": "orchestration_result",
            "phase": 1,
            "task_count": self.results.len(),
            "tasks": self.results.iter().map(|r| serde_json::json!({
                "id": r.task.id,
                "agent": r.task.agent,
                "brief": r.task.brief,
                "output": r.output,
                "error": r.error,
            })).collect::<Vec<_>>(),
        });
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}
