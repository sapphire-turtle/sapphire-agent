//! Timer & Pomodoro tools.
//!
//! Single-slot, in-memory. Setting any new timer cancels the previous
//! one. Origin (chat room / voice device) is captured from the
//! `crate::timer::TIMER_ORIGIN_TL` task_local set by the agent/serve
//! tool-execution loops; tools called outside a scoped turn return an
//! error rather than guessing.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde_json::json;

use crate::config::TimerPreset;
use crate::provider::ToolSpec;
use crate::timer::{TimerKind, TimerManager, TimerSnapshot, current_origin, find_preset};
use crate::tools::{Tool, ToolKind};

/// Format a snapshot as a single-line human-readable string for the LLM.
fn format_snapshot(s: &TimerSnapshot) -> String {
    let remaining = (s.fires_at - chrono::Local::now()).num_seconds().max(0);
    let mins = remaining / 60;
    let secs = remaining % 60;
    match &s.kind {
        TimerKind::Single => format!(
            "Active timer '{}' fires at {} (~{}m{:02}s remaining).",
            s.label,
            s.fires_at.format("%H:%M:%S"),
            mins,
            secs
        ),
        TimerKind::Preset {
            name,
            step_index,
            total_steps,
            cycle,
            total_cycles,
        } => format!(
            "Active preset '{}' (cycle {}/{}, step {}/{}): waiting on '{}', fires at {} (~{}m{:02}s remaining).",
            name,
            cycle,
            total_cycles,
            step_index + 1,
            total_steps,
            s.label,
            s.fires_at.format("%H:%M:%S"),
            mins,
            secs
        ),
    }
}

// ---------------------------------------------------------------------------
// timer_set
// ---------------------------------------------------------------------------

pub struct TimerSetTool {
    manager: Arc<TimerManager>,
    spec: ToolSpec,
}

impl TimerSetTool {
    pub fn new(manager: Arc<TimerManager>) -> Self {
        Self {
            manager,
            spec: ToolSpec {
                name: "timer_set".into(),
                description: "Set a single-shot in-memory timer. The timer lives ONLY in the \
                    agent process — the user has no visual indicator, no notification, no way to \
                    see it. When you set, replace, or cancel a timer you MUST tell the user in \
                    your text reply (label + when it will fire), otherwise they have no idea \
                    anything happened. When the timer fires, the agent is re-invoked with a \
                    heartbeat — again, ALWAYS reply with a short text message to actually deliver \
                    the notification (tool-only replies are silent to the user). The process \
                    supports ONE active timer at a time; calling this with another timer already \
                    active cancels the previous one. Use 'timer_preset' for Pomodoro-style \
                    multi-step routines."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "minutes": {
                            "type": "number",
                            "description": "Duration in minutes (fractional allowed, e.g. 0.5 = 30 seconds).",
                            "exclusiveMinimum": 0
                        },
                        "label": {
                            "type": "string",
                            "description": "Short label describing what this timer is for (e.g. 'tea steep', 'meeting in 5'). Surfaced in the fire message.",
                            "default": "timer"
                        }
                    },
                    "required": ["minutes"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for TimerSetTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let minutes = input["minutes"]
            .as_f64()
            .context("'minutes' must be a number")?;
        let label = input["label"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| "timer".to_string());
        let origin = current_origin()
            .ok_or_else(|| anyhow!("timer_set can only be called from a chat or voice turn"))?;
        let snap = self
            .manager
            .set_single(minutes, label.clone(), origin)
            .await?;
        Ok(format!(
            "Timer set: '{}' will fire at {} (in {:.1} min).",
            label,
            snap.fires_at.format("%H:%M:%S"),
            minutes
        ))
    }
}

// ---------------------------------------------------------------------------
// timer_preset
// ---------------------------------------------------------------------------

pub struct TimerPresetTool {
    manager: Arc<TimerManager>,
    presets: Vec<TimerPreset>,
    spec: ToolSpec,
}

impl TimerPresetTool {
    pub fn new(manager: Arc<TimerManager>, presets: Vec<TimerPreset>) -> Self {
        let names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
        let description = format!(
            "Start a configured timer preset (Pomodoro-style work/break cycles). The preset's \
             ordered steps fire one by one AUTOMATICALLY — once started, do NOT call timer_set or \
             timer_preset again to advance steps; the next step is already scheduled. Cancels any \
             active timer. The preset lives ONLY in the agent process — the user has no visible \
             indicator. Always confirm in your text reply (which preset, how many cycles, when \
             the first step fires); and when each step fires you'll be re-invoked via heartbeat, \
             so ALWAYS reply with a short text message to actually notify the user. Configured \
             presets: {names:?}. Use 'timer_set' for simple one-shot timers."
        );
        Self {
            manager,
            presets,
            spec: ToolSpec {
                name: "timer_preset".into(),
                description: description.into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Preset name (case-insensitive). Must match one of the configured [[timer.preset]] entries."
                        },
                        "cycles": {
                            "type": "integer",
                            "description": "Override the preset's default cycle count. Optional.",
                            "minimum": 1
                        }
                    },
                    "required": ["name"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for TimerPresetTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, input: &serde_json::Value) -> Result<String> {
        let name = input["name"].as_str().context("missing 'name'")?;
        let cycles_override = input["cycles"].as_u64().map(|n| n as u32);
        let preset = find_preset(&self.presets, name)?.clone();
        let origin = current_origin()
            .ok_or_else(|| anyhow!("timer_preset can only be called from a chat or voice turn"))?;
        let snap = self
            .manager
            .set_preset(preset.clone(), cycles_override, origin)
            .await?;
        let total_cycles = cycles_override.unwrap_or(preset.cycles).max(1);
        let total_min: f64 =
            preset.steps.iter().map(|s| s.minutes).sum::<f64>() * total_cycles as f64;
        Ok(format!(
            "Preset '{}' started: {} cycle(s) of {} step(s) (~{:.1} min total). First step '{}' fires at {}.",
            preset.name,
            total_cycles,
            preset.steps.len(),
            total_min,
            snap.label,
            snap.fires_at.format("%H:%M:%S")
        ))
    }
}

// ---------------------------------------------------------------------------
// timer_cancel
// ---------------------------------------------------------------------------

pub struct TimerCancelTool {
    manager: Arc<TimerManager>,
    spec: ToolSpec,
}

impl TimerCancelTool {
    pub fn new(manager: Arc<TimerManager>) -> Self {
        Self {
            manager,
            spec: ToolSpec {
                name: "timer_cancel".into(),
                description: "Cancel the active timer (single-shot or preset). No-op if no \
                    timer is running. The user can't see the timer state, so confirm the \
                    cancellation in your text reply."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for TimerCancelTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Delete
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, _input: &serde_json::Value) -> Result<String> {
        match self.manager.cancel().await {
            Some(snap) => Ok(format!("Cancelled timer: '{}'.", snap.label)),
            None => Ok("No timer was active.".to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// timer_status
// ---------------------------------------------------------------------------

pub struct TimerStatusTool {
    manager: Arc<TimerManager>,
    spec: ToolSpec,
}

impl TimerStatusTool {
    pub fn new(manager: Arc<TimerManager>) -> Self {
        Self {
            manager,
            spec: ToolSpec {
                name: "timer_status".into(),
                description: "Report the currently active timer (label, kind, time remaining), \
                    or that none is set. The tool result is for your eyes only — relay it to the \
                    user in your text reply, since they have no other way to see timer state."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for TimerStatusTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Search
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, _input: &serde_json::Value) -> Result<String> {
        match self.manager.current().await {
            Some(snap) => Ok(format_snapshot(&snap)),
            None => Ok("No timer is currently set.".to_string()),
        }
    }
}
