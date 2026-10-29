//! `sapphire-agent chat`: the REPL, one-shot and listing client of the agent
//! running on this host, over IPC (served by `sapphire-agent-server`'s
//! `serve::chat_ipc`).

use std::borrow::Cow;
use std::io::{Write, stderr, stdout};

use anyhow::Result;
use reedline::{
    Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline, Signal,
};
use serde_json::{Value, json};

use sapphire_agent_core::{ChatClient, ChatEvent, ToolEvent, Turn};

/// What `sapphire-agent chat` was asked to do.
pub struct ChatOptions {
    /// Public id of a session to resume; a new one otherwise.
    pub session: Option<String>,
    /// List the sessions and exit.
    pub list: bool,
    /// Send one message and exit.
    pub message: Option<String>,
    /// Print the session's history and exit.
    pub history: bool,
    /// Machine-readable output for `list`, `history` and `message`.
    pub json: bool,
}

/// Minimal prompt for reedline: just "> ".
struct SimplePrompt;

impl Prompt for SimplePrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("::: ")
    }
    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({}reverse-search: {}) ",
            prefix, history_search.term
        ))
    }
}

pub async fn run(options: ChatOptions) -> Result<()> {
    let client = ChatClient::connect("chat", sapphire_agent_server::VERSION).await?;

    if options.list {
        return print_sessions(&client.sessions().await?, options.json);
    }

    let opened = client.open(options.session.as_deref()).await?;
    let mut session_id = opened.session_id;

    if options.history {
        return print_history(&client.history(&session_id).await?, options.json, true);
    }

    if let Some(text) = options.message {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            anyhow::bail!("--message requires non-empty text");
        }
        send(&client, &session_id, trimmed, options.json).await?;
        if !options.json {
            println!();
        }
        return Ok(());
    }

    // --json is a no-op in REPL mode (text output is always human-readable).
    println!("sapphire-agent chat  (session: {})", opened.display_id);
    if !opened.is_new {
        println!("[resumed existing session]\n");
        match client.history(&session_id).await {
            Ok(messages) => print_history(&messages, false, false)?,
            Err(e) => eprintln!("[warning: failed to load history: {e:#}]"),
        }
    }
    println!("Commands: /clear  /help  /quit\n");

    let mut line_editor = Reedline::create();
    let prompt = SimplePrompt;

    loop {
        let line = match line_editor.read_line(&prompt) {
            Ok(Signal::Success(buf)) => buf,
            Ok(Signal::CtrlC) | Ok(Signal::CtrlD) => break,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("Error reading input: {e}");
                break;
            }
        };

        let trimmed = line.trim();
        match trimmed {
            "/quit" | "/exit" => break,
            "/help" => {
                println!("  /clear   Start a new session");
                println!("  /quit    Exit");
                continue;
            }
            "/clear" => {
                match client.open(None).await {
                    Ok(opened) => {
                        session_id = opened.session_id;
                        println!("[new session: {}]", opened.display_id);
                    }
                    Err(e) => eprintln!("[error starting new session: {e:#}]"),
                }
                continue;
            }
            "" => continue,
            _ => {}
        }

        if let Err(e) = send(&client, &session_id, trimmed, false).await {
            eprintln!("\n[error: {e:#}]");
        }
        println!();
    }

    Ok(())
}

/// Run one turn and print it: tool calls to stderr as they happen and the
/// reply to stdout, or one JSON object at the end.
async fn send(client: &ChatClient, session_id: &str, content: &str, json_mode: bool) -> Result<()> {
    let mut tools: Vec<Value> = Vec::new();
    let mut on_tool = |event: ToolEvent| {
        if json_mode {
            if event.started {
                tools.push(json!({ "id": event.id, "name": event.name }));
            }
        } else {
            if event.started {
                eprint!("[{}] ", event.name);
            } else {
                eprint!("done ");
            }
            let _ = stderr().flush();
        }
    };
    let reply = client
        .send(session_id, Turn::text(content), |event| {
            if let ChatEvent::Tool(e) = event {
                on_tool(e)
            }
        })
        .await;

    if json_mode {
        let mut out = json!({ "tools": tools });
        match reply {
            Ok(content) => out["content"] = json!(content),
            Err(e) => {
                out["content"] = json!("");
                out["error"] = json!(format!("{e:#}"));
            }
        }
        println!("{}", serde_json::to_string(&out)?);
        return Ok(());
    }

    let content = reply?;
    if !content.is_empty() {
        eprintln!();
        print!("{content}");
        let _ = stdout().flush();
    }
    Ok(())
}

fn print_sessions(sessions: &[Value], json_mode: bool) -> Result<()> {
    if json_mode {
        println!(
            "{}",
            serde_json::to_string(&json!({ "sessions": sessions }))?
        );
        return Ok(());
    }
    if sessions.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }
    println!("{:<10}  {:<30}  Created at", "ID", "Title");
    println!("{}", "-".repeat(64));
    for s in sessions {
        let pub_id = s["public_id"].as_str().unwrap_or("-");
        let title = s["title"].as_str().unwrap_or("(untitled)");
        let created = s["created_at"].as_str().unwrap_or("?");
        println!("{pub_id:<10}  {title:<30}  {created}");
    }
    Ok(())
}

/// Print a session's messages. `standalone` is `--history`; otherwise the
/// dump opens a resumed REPL and is framed as such.
fn print_history(messages: &[Value], json_mode: bool, standalone: bool) -> Result<()> {
    if json_mode {
        println!(
            "{}",
            serde_json::to_string(&json!({ "messages": messages }))?
        );
        return Ok(());
    }
    if messages.is_empty() {
        return Ok(());
    }
    if !standalone {
        println!("──── history ────");
    }
    for msg in messages {
        let role = msg["role"].as_str().unwrap_or("?");
        for part in msg["parts"].as_array().into_iter().flatten() {
            match part["type"].as_str() {
                Some("text") => {
                    let text = part["text"].as_str().unwrap_or("");
                    if text.is_empty() {
                        continue;
                    }
                    match role {
                        "user" => println!("> {text}"),
                        "assistant" => println!("{text}\n"),
                        _ => println!("{text}"),
                    }
                }
                Some("tool_use") => {
                    let name = part["name"].as_str().unwrap_or("?");
                    println!("[tool: {name}]");
                }
                // Tool results are left out to keep the dump readable.
                _ => {}
            }
        }
    }
    if !standalone {
        println!("──── end of history ────\n");
    }
    Ok(())
}
