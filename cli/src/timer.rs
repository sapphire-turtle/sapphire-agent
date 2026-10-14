//! Single-slot, in-memory timer & Pomodoro manager.
//!
//! The agent supports exactly one active timer per process. Setting a new
//! timer aborts the previous one (intentional — voice/chat UX makes
//! multiple parallel timers identified by an ID impractical to manage by
//! voice). When a timer fires, dispatch routes back to where it was
//! originally set: chat timer → `Agent::trigger`, voice timer →
//! `serve::push_voice_text_to_subscriber`. No cross-channel fallback.
//!
//! Origin (`TimerOrigin`) is captured at tool-call time via a
//! `tokio::task_local`. The agent and serve tool loops both wrap each
//! `tools.execute(...)` invocation with `scope_timer_origin(...)` so the
//! timer tool reads the originating room or voice device at the moment
//! it is invoked.

use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::TimerPreset;

/// Where the timer was set from. Single dispatch target, no fallback.
#[derive(Debug, Clone)]
pub enum TimerOrigin {
    /// Chat channel (Matrix / Discord). Fire path:
    /// `Agent::trigger(name, prompt, room_id)`.
    Chat { room_id: String },
    /// Voice satellite subscribed via `voice/subscribe`. Fire path:
    /// `serve::push_voice_text_to_subscriber(state, device_id, ...)`.
    Voice { device_id: String },
}

tokio::task_local! {
    static TIMER_ORIGIN_TL: TimerOrigin;
}

/// Run `fut` with `TIMER_ORIGIN_TL` set to `origin`. Called by the
/// agent / serve tool-execution loops per turn so the timer tool can
/// read where the originating message came from.
pub fn scope_timer_origin<F: std::future::Future>(
    origin: TimerOrigin,
    fut: F,
) -> impl std::future::Future<Output = F::Output> {
    TIMER_ORIGIN_TL.scope(origin, fut)
}

/// Read the active origin, when called inside `scope_timer_origin`.
/// Returns `None` from a turn that wasn't scoped (e.g. background catch-up).
pub fn current_origin() -> Option<TimerOrigin> {
    TIMER_ORIGIN_TL.try_with(|o| o.clone()).ok()
}

/// Snapshot returned by `timer_status` and `timer_cancel`. Describes
/// the in-flight timer enough for the LLM to phrase a status reply.
#[derive(Debug, Clone)]
pub struct TimerSnapshot {
    pub label: String,
    pub fires_at: DateTime<Local>,
    pub kind: TimerKind,
}

#[derive(Debug, Clone)]
pub enum TimerKind {
    Single,
    Preset {
        name: String,
        step_index: usize,
        total_steps: usize,
        cycle: u32,
        total_cycles: u32,
    },
}

struct ActiveTimer {
    handle: JoinHandle<()>,
    snapshot: TimerSnapshot,
}

/// Single-slot timer manager. Setting a new timer aborts the previous
/// one. Lives behind an `Arc` so the tools and the spawned fire tasks
/// can both reach it.
pub struct TimerManager {
    inner: Mutex<Option<ActiveTimer>>,
    agent: tokio::sync::OnceCell<Weak<crate::agent::Agent>>,
    serve_state: tokio::sync::OnceCell<Weak<crate::serve::ServeState>>,
}

impl TimerManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(None),
            agent: tokio::sync::OnceCell::new(),
            serve_state: tokio::sync::OnceCell::new(),
        })
    }

    /// Install the agent ref. Called from `main` after the agent is
    /// built. Weak to keep the timer/tools graph acyclic.
    pub fn set_agent(&self, agent: Weak<crate::agent::Agent>) {
        let _ = self.agent.set(agent);
    }

    /// Install the serve-state ref. Called from `main` after
    /// `ServeState` is built.
    pub fn set_serve_state(&self, state: Weak<crate::serve::ServeState>) {
        let _ = self.serve_state.set(state);
    }

    /// Replace the active timer (if any) with a single-shot timer.
    /// Returns the new snapshot. Origin determines fire dispatch.
    pub async fn set_single(
        self: &Arc<Self>,
        minutes: f64,
        label: String,
        origin: TimerOrigin,
    ) -> Result<TimerSnapshot> {
        if !minutes.is_finite() || minutes <= 0.0 {
            anyhow::bail!("minutes must be a positive finite number (got {minutes})");
        }
        let duration = Duration::from_secs_f64(minutes * 60.0);
        let fires_at = Local::now()
            + chrono::Duration::from_std(duration).context("timer duration overflow")?;
        let snapshot = TimerSnapshot {
            label: label.clone(),
            fires_at,
            kind: TimerKind::Single,
        };
        let me = Arc::clone(self);
        let origin_for_fire = origin.clone();
        let label_for_fire = label.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            let prompt = format!(
                "[Timer] '{label}' ({minutes:.1} min) elapsed. Tell the user the timer is up. Keep it short.",
                label = label_for_fire,
                minutes = minutes,
            );
            me.dispatch_fire(&label_for_fire, &prompt, &origin_for_fire);
            // Clear our own slot if we're still the active timer.
            let mut slot = me.inner.lock().await;
            if let Some(active) = slot.as_ref()
                && matches!(active.snapshot.kind, TimerKind::Single)
                && active.snapshot.label == label_for_fire
            {
                *slot = None;
            }
        });
        self.replace_active(ActiveTimer {
            handle,
            snapshot: snapshot.clone(),
        })
        .await;
        Ok(snapshot)
    }

    /// Replace the active timer with a preset run (e.g. Pomodoro).
    /// Each step fires its own message; on the final step a wrap-up
    /// message is sent.
    pub async fn set_preset(
        self: &Arc<Self>,
        preset: TimerPreset,
        cycles_override: Option<u32>,
        origin: TimerOrigin,
    ) -> Result<TimerSnapshot> {
        let cycles = cycles_override.unwrap_or(preset.cycles).max(1);
        if preset.steps.is_empty() {
            anyhow::bail!("preset '{}' has no steps", preset.name);
        }
        for step in &preset.steps {
            if !step.minutes.is_finite() || step.minutes <= 0.0 {
                anyhow::bail!(
                    "preset '{}' step '{}' has invalid minutes: {}",
                    preset.name,
                    step.label,
                    step.minutes
                );
            }
        }
        let total_steps = preset.steps.len();
        let first = &preset.steps[0];
        let first_dur = Duration::from_secs_f64(first.minutes * 60.0);
        let fires_at = Local::now()
            + chrono::Duration::from_std(first_dur).context("preset duration overflow")?;
        let snapshot = TimerSnapshot {
            label: first.label.clone(),
            fires_at,
            kind: TimerKind::Preset {
                name: preset.name.clone(),
                step_index: 0,
                total_steps,
                cycle: 1,
                total_cycles: cycles,
            },
        };

        let me = Arc::clone(self);
        let preset_for_task = preset.clone();
        let origin_for_task = origin.clone();
        let handle = tokio::spawn(async move {
            me.run_preset_loop(preset_for_task, cycles, origin_for_task)
                .await;
        });
        self.replace_active(ActiveTimer {
            handle,
            snapshot: snapshot.clone(),
        })
        .await;
        Ok(snapshot)
    }

    /// Drive the preset state machine. Sleeps for each step, fires
    /// the per-step message, advances `snapshot`. On final completion
    /// clears the slot.
    async fn run_preset_loop(
        self: Arc<Self>,
        preset: TimerPreset,
        cycles: u32,
        origin: TimerOrigin,
    ) {
        let total_steps = preset.steps.len();
        for cycle in 1..=cycles {
            for (i, step) in preset.steps.iter().enumerate() {
                let duration = Duration::from_secs_f64(step.minutes * 60.0);
                // Update snapshot to reflect what we're currently waiting on.
                {
                    let mut slot = self.inner.lock().await;
                    if let Some(active) = slot.as_mut() {
                        active.snapshot.label = step.label.clone();
                        active.snapshot.fires_at = Local::now()
                            + chrono::Duration::from_std(duration)
                                .unwrap_or(chrono::Duration::zero());
                        if let TimerKind::Preset {
                            step_index,
                            cycle: c,
                            ..
                        } = &mut active.snapshot.kind
                        {
                            *step_index = i;
                            *c = cycle;
                        }
                    } else {
                        // Slot was cleared (cancelled); abort the loop.
                        return;
                    }
                }

                tokio::time::sleep(duration).await;

                let is_last_step = cycle == cycles && i + 1 == total_steps;
                let next_label = if is_last_step {
                    None
                } else if i + 1 < total_steps {
                    Some(preset.steps[i + 1].label.clone())
                } else {
                    Some(preset.steps[0].label.clone())
                };

                let prompt = if is_last_step {
                    format!(
                        "[Timer: {preset_name}] All {cycles} cycle(s) complete. The final '{label}' step ({minutes:.1} min) just ended. Tell the user the full Pomodoro is finished. Keep it short.",
                        preset_name = preset.name,
                        cycles = cycles,
                        label = step.label,
                        minutes = step.minutes,
                    )
                } else {
                    format!(
                        "[Timer: {preset_name}] Step {cur}/{total} of cycle {cycle}/{cycles}: '{label}' ({minutes:.1} min) ended. Next step '{next}' is already auto-scheduled by the preset — do NOT call timer_set / timer_preset. Just tell the user to switch. Keep it short.",
                        preset_name = preset.name,
                        cur = i + 1,
                        total = total_steps,
                        cycle = cycle,
                        cycles = cycles,
                        label = step.label,
                        minutes = step.minutes,
                        next = next_label.as_deref().unwrap_or(""),
                    )
                };
                self.dispatch_fire(&step.label, &prompt, &origin);
            }
        }
        // Clear slot at the end if still ours.
        let mut slot = self.inner.lock().await;
        if let Some(active) = slot.as_ref()
            && matches!(active.snapshot.kind, TimerKind::Preset { ref name, .. } if name == &preset.name)
        {
            *slot = None;
        }
    }

    /// Send the fire message to the right channel. Logs and drops on
    /// failure — no fallback to the other channel.
    ///
    /// The dispatch is detached via `tokio::spawn` so the trigger does not
    /// run inside the calling timer task. If we awaited it here, an AI
    /// `timer_set`/`timer_preset` invocation during the trigger would call
    /// `replace_active` → `prev.handle.abort()` on the very task we're
    /// running inside, killing the AI's tool-call loop mid-flight (the
    /// `tool_use` then has no matching `tool_result` and the next turn
    /// fails with Anthropic 400 `tool_use ids were found without
    /// tool_result blocks immediately after`).
    fn dispatch_fire(&self, task_name: &str, prompt: &str, origin: &TimerOrigin) {
        match origin {
            TimerOrigin::Chat { room_id } => match self.agent.get().and_then(Weak::upgrade) {
                Some(agent) => {
                    let task_name = task_name.to_string();
                    let prompt = prompt.to_string();
                    let room_id = room_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = agent.trigger(&task_name, &prompt, &room_id).await {
                            warn!("Timer fire (chat) failed for {task_name} -> {room_id}: {e:#}");
                        }
                    });
                }
                None => warn!(
                    "Timer fire (chat) skipped for {task_name}: agent not wired into TimerManager"
                ),
            },
            TimerOrigin::Voice { device_id } => {
                match self.serve_state.get().and_then(Weak::upgrade) {
                    Some(state) => {
                        let task_name = task_name.to_string();
                        let prompt = prompt.to_string();
                        let device_id = device_id.clone();
                        tokio::spawn(async move {
                            match crate::serve::push_voice_text_to_subscriber(
                                state,
                                device_id.clone(),
                                Some(task_name.clone()),
                                prompt,
                            )
                            .await
                            {
                                Ok(()) => {}
                                Err(e) => {
                                    let reason = match e {
                                        crate::serve::VoicePushError::Offline => {
                                            "offline".to_string()
                                        }
                                        crate::serve::VoicePushError::NoVoice => {
                                            "no voice providers".to_string()
                                        }
                                        crate::serve::VoicePushError::NotConfigured => {
                                            "voice_pipeline not configured for room_profile"
                                                .to_string()
                                        }
                                        crate::serve::VoicePushError::Other(msg) => msg,
                                    };
                                    warn!(
                                        "Timer fire (voice) failed for {task_name} -> device={device_id}: {reason}"
                                    );
                                }
                            }
                        });
                    }
                    None => warn!(
                        "Timer fire (voice) skipped for {task_name}: serve_state not wired into TimerManager"
                    ),
                }
            }
        }
    }

    /// Atomically swap in a new active timer; abort the prior one if any.
    async fn replace_active(&self, new: ActiveTimer) {
        let mut slot = self.inner.lock().await;
        if let Some(prev) = slot.take() {
            info!("Timer: replacing previous timer '{}'", prev.snapshot.label);
            prev.handle.abort();
        }
        *slot = Some(new);
    }

    /// Cancel the active timer (if any) and return its snapshot.
    pub async fn cancel(&self) -> Option<TimerSnapshot> {
        let mut slot = self.inner.lock().await;
        slot.take().map(|t| {
            t.handle.abort();
            t.snapshot
        })
    }

    /// Read-only snapshot of the active timer.
    pub async fn current(&self) -> Option<TimerSnapshot> {
        self.inner.lock().await.as_ref().map(|t| t.snapshot.clone())
    }
}

/// Look up a preset by case-insensitive name match.
pub fn find_preset<'a>(presets: &'a [TimerPreset], name: &str) -> Result<&'a TimerPreset> {
    presets
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let known: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
            anyhow!("unknown timer preset '{name}'. Known presets: {known:?}")
        })
}
