//! Heartbeat: periodic background tasks.
//!
//! Two parallel loops:
//!
//! 1. **Day-boundary loop**: at every `day_boundary_hour:00:00` local time,
//!    optionally generates a daily log and compacts MEMORY.md.
//! 2. **Cron loop**: scans `<workspace>/heartbeat/*.md` and fires `prompt`
//!    triggers on the agent according to each task's cron schedule.
//!
//! When the workspace is synced with other hosts, each runs on one host
//! only (`placement`): the logs and the compaction on the primary device,
//! each cron task on the host that can deliver it.

use crate::agent::Agent;
use crate::config::{Config, DigestConfig};
use crate::heartbeat_config::{HeartbeatVoiceTarget, load_heartbeat_dir, next_due};
use crate::memory_compaction::compact_memory;
use crate::periodic_log::{
    catchup_missing_daily_digests, catchup_pending_daily_logs, catchup_pending_monthly_logs,
    catchup_pending_weekly_logs, catchup_pending_yearly_logs, generate_daily_log,
    generate_monthly_log, generate_weekly_log, generate_yearly_log,
};
use crate::provider::Provider;
use crate::provider::registry::ProviderRegistry;
use crate::session::SessionStore;
use chrono::{Datelike, Duration, Local, NaiveTime, Timelike, Weekday};
use sapphire_framework::workspace::WorkspaceState;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tracing::{info, warn};

/// How often the catchup loop scans for missing daily logs.
const CATCHUP_INTERVAL: StdDuration = StdDuration::from_secs(60 * 60);

/// A heartbeat task that's due to fire on this tick.
struct DueTask {
    name: String,
    body: String,
    /// Per-task chat room override; falls through to `default_room_id`.
    room_id: Option<String>,
    /// Voice satellite target, when this task should be delivered as
    /// TTS audio instead of (or in addition to, on failure) chat.
    voice: Option<HeartbeatVoiceTarget>,
    /// The device that runs it, when the task names one.
    device: Option<String>,
}

pub struct Heartbeat {
    pub workspace_dir: PathBuf,
    pub ws_state: Arc<WorkspaceState>,
    pub day_boundary_hour: u8,
    pub daily_log_enabled: bool,
    pub memory_compaction_enabled: bool,
    pub digest_cfg: DigestConfig,
    pub session_store: Arc<SessionStore>,
    /// Provider registry — every background LLM call resolves through
    /// `registry.background_provider_for_namespace(&config, &ns)` so each
    /// namespace can pick its own primary (and optional fallback) instead
    /// of all sharing one provider.
    pub registry: Arc<ProviderRegistry>,
    /// The channel agent. `None` on a host with no chat channel: the logs
    /// and voice-only tasks still run there, chat delivery does not.
    pub agent: Option<Arc<Agent>>,
    pub default_room_id: Option<String>,
    /// Which host runs what, once the workspace is synced.
    pub placement: Arc<crate::placement::Placement>,
    /// The rooms this host's channels serve, for placing cron tasks.
    pub local_rooms: std::collections::HashMap<String, String>,
    /// Whether one of this host's channels listens in every room.
    pub local_catch_all: bool,
    /// Config snapshot used to enumerate memory namespaces and resolve
    /// per-room namespace assignment for periodic-log catch-up.
    pub config: Config,
    /// Shared API server state, when voice is configured. Cron tasks
    /// targeting a `voice:` satellite go through `serve.rs`'s push
    /// helper; absent (no voice providers, or chat-only build) the
    /// voice path is skipped and `room_id` is used as the only target.
    pub serve_state: Option<Arc<crate::serve::ServeState>>,
}

impl Heartbeat {
    /// Resolve which namespace a session belongs to. Matrix/Discord rooms
    /// follow `Config::namespace_for_room`. Device-default sessions
    /// (channel=`"device-default"`) and cross-device sessions
    /// (channel=`"rpc"`) persist their resolved namespace in the JSONL
    /// meta — trust that first, falling back to the default namespace
    /// for old files that predate the field.
    fn namespace_for_session(&self, meta: &crate::session::SessionMeta) -> String {
        if meta.channel == "device-default" || meta.channel == "rpc" || meta.channel == "acp" {
            meta.namespace
                .clone()
                .unwrap_or_else(|| crate::config::DEFAULT_NAMESPACE_NAME.to_string())
        } else {
            self.config.namespace_for_room(&meta.room_id).to_string()
        }
    }

    /// Background provider for `namespace`. Honours the namespace's
    /// `background_profile`, falling back through the global background
    /// profile to plain Anthropic.
    fn provider_for_namespace(&self, namespace: &str) -> Arc<dyn Provider> {
        self.registry
            .background_provider_for_namespace(&self.config, namespace)
    }
}

impl Heartbeat {
    /// Spawn the day-boundary, cron, and catchup loops as independent tasks.
    pub fn spawn(self) {
        let me = Arc::new(self);
        let a = Arc::clone(&me);
        tokio::spawn(async move { a.run_day_boundary().await });
        let b = Arc::clone(&me);
        tokio::spawn(async move { b.run_cron().await });
        if me.daily_log_enabled {
            let c = Arc::clone(&me);
            tokio::spawn(async move { c.run_catchup().await });
        }
    }

    /// Periodically scan for past dates that have session messages but no
    /// daily log file, and generate any that are missing. Recovers from
    /// transient failures during the day-boundary fire (e.g. network blip
    /// during the LLM call) without waiting for the next process restart.
    async fn run_catchup(self: Arc<Self>) {
        let mut tick = tokio::time::interval(CATCHUP_INTERVAL);
        tick.tick().await; // skip immediate fire — startup catchup already ran in main
        loop {
            tick.tick().await;
            if !self.placement.runs_background().await {
                continue;
            }
            let today_local =
                crate::session::local_date_for_timestamp(Local::now(), self.day_boundary_hour);
            let mut total: usize = 0;
            for ns in self.config.all_memory_namespaces() {
                let provider = self.provider_for_namespace(&ns);
                let ns_for_predicate = ns.clone();
                let me = Arc::clone(&self);
                let predicate = move |meta: &crate::session::SessionMeta| -> bool {
                    me.namespace_for_session(meta) == ns_for_predicate
                };
                total += catchup_pending_daily_logs(
                    &self.session_store,
                    self.serve_state.as_ref().map(|s| &*s.acp_session_store),
                    provider.as_ref(),
                    &self.ws_state,
                    &self.workspace_dir,
                    &ns,
                    self.day_boundary_hour,
                    &predicate,
                )
                .await;
                total += catchup_missing_daily_digests(
                    provider.as_ref(),
                    &self.ws_state,
                    &self.workspace_dir,
                    &ns,
                )
                .await;
                if self.digest_cfg.weekly_enabled {
                    total += catchup_pending_weekly_logs(
                        provider.as_ref(),
                        &self.ws_state,
                        &self.workspace_dir,
                        &ns,
                        today_local,
                    )
                    .await;
                }
                if self.digest_cfg.monthly_enabled {
                    total += catchup_pending_monthly_logs(
                        provider.as_ref(),
                        &self.ws_state,
                        &self.workspace_dir,
                        &ns,
                        today_local,
                    )
                    .await;
                }
                if self.digest_cfg.yearly_enabled {
                    total += catchup_pending_yearly_logs(
                        provider.as_ref(),
                        &self.ws_state,
                        &self.workspace_dir,
                        &ns,
                        today_local,
                    )
                    .await;
                }
            }
            if total > 0
                && let Some(agent) = &self.agent
            {
                agent.invalidate_system_prompts().await;
            }
        }
    }

    async fn run_day_boundary(self: Arc<Self>) {
        loop {
            let sleep_dur = self.time_until_next_boundary();
            info!(
                "Heartbeat: next day-boundary in {:.0}s (boundary hour: {}:00 local)",
                sleep_dur.as_secs_f64(),
                self.day_boundary_hour
            );
            tokio::time::sleep(sleep_dur).await;

            let yesterday = crate::session::local_date_for_timestamp(
                Local::now() - Duration::seconds(1),
                self.day_boundary_hour,
            );

            // The logs and the compaction write the synced workspace, so
            // they run on one host; the subagent prune below is this
            // host's own cache and runs everywhere.
            let background = self.placement.runs_background().await;

            if self.daily_log_enabled && background {
                let today = yesterday + Duration::days(1);
                let mut any_generated = false;
                for ns in self.config.all_memory_namespaces() {
                    let provider = self.provider_for_namespace(&ns);
                    info!("Heartbeat: generating daily log for {yesterday} in '{ns}'");
                    let ns_for_predicate = ns.clone();
                    let me = Arc::clone(&self);
                    let predicate = move |meta: &crate::session::SessionMeta| -> bool {
                        me.namespace_for_session(meta) == ns_for_predicate
                    };
                    match generate_daily_log(
                        &self.session_store,
                        self.serve_state.as_ref().map(|s| &*s.acp_session_store),
                        provider.as_ref(),
                        &self.ws_state,
                        &self.workspace_dir,
                        &ns,
                        yesterday,
                        self.day_boundary_hour,
                        &predicate,
                    )
                    .await
                    {
                        Ok(true) => any_generated = true,
                        Ok(false) => {}
                        Err(e) => warn!(
                            "Heartbeat: failed to generate daily log for {yesterday} in '{ns}': {e:#}"
                        ),
                    }

                    // Weekly: today is Monday → last ISO week closed yesterday.
                    if self.digest_cfg.weekly_enabled && today.weekday() == Weekday::Mon {
                        let iso = yesterday.iso_week();
                        info!(
                            "Heartbeat: generating weekly log for {}-W{:02} in '{ns}'",
                            iso.year(),
                            iso.week()
                        );
                        match generate_weekly_log(
                            provider.as_ref(),
                            &self.ws_state,
                            &self.workspace_dir,
                            &ns,
                            iso.year(),
                            iso.week(),
                        )
                        .await
                        {
                            Ok(true) => any_generated = true,
                            Ok(false) => {}
                            Err(e) => {
                                warn!("Heartbeat: failed to generate weekly log in '{ns}': {e:#}")
                            }
                        }
                    }

                    // Monthly: today is day 1 → last calendar month ended yesterday.
                    if self.digest_cfg.monthly_enabled && today.day() == 1 {
                        let (year, month) = (yesterday.year(), yesterday.month());
                        info!(
                            "Heartbeat: generating monthly log for {year:04}-{month:02} in '{ns}'"
                        );
                        match generate_monthly_log(
                            provider.as_ref(),
                            &self.ws_state,
                            &self.workspace_dir,
                            &ns,
                            year,
                            month,
                        )
                        .await
                        {
                            Ok(true) => any_generated = true,
                            Ok(false) => {}
                            Err(e) => {
                                warn!("Heartbeat: failed to generate monthly log in '{ns}': {e:#}")
                            }
                        }
                    }

                    // Yearly: today is Jan 1 → last calendar year ended yesterday.
                    if self.digest_cfg.yearly_enabled && today.day() == 1 && today.month() == 1 {
                        let year = yesterday.year();
                        info!("Heartbeat: generating yearly log for {year:04} in '{ns}'");
                        match generate_yearly_log(
                            provider.as_ref(),
                            &self.ws_state,
                            &self.workspace_dir,
                            &ns,
                            year,
                        )
                        .await
                        {
                            Ok(true) => any_generated = true,
                            Ok(false) => {}
                            Err(e) => {
                                warn!("Heartbeat: failed to generate yearly log in '{ns}': {e:#}")
                            }
                        }
                    }
                }

                if any_generated && let Some(agent) = &self.agent {
                    agent.invalidate_system_prompts().await;
                }
            }

            // Subagent child conversations are pruned once per
            // day-boundary tick too — not gated on `daily_log_enabled`,
            // since they have no permanent-record equivalent to hand
            // off to; a conversation just goes stale on its own clock.
            if let Some(cache) = self
                .serve_state
                .as_ref()
                .and_then(|s| s.subagent_cache.as_ref())
            {
                let cutoff = chrono::Utc::now()
                    - Duration::days(self.config.subagent_cache.retain_days.into());
                let removed = cache.prune_before(cutoff);
                if removed > 0 {
                    info!("Pruned {removed} stale subagent child conversation(s)");
                }
            }

            if self.memory_compaction_enabled && background {
                for ns in self.config.all_memory_namespaces() {
                    let provider = self.provider_for_namespace(&ns);
                    info!("Heartbeat: compacting MEMORY.md in '{ns}'");
                    compact_memory(provider.as_ref(), &self.workspace_dir, &ns).await;
                }
            }
        }
    }

    async fn run_cron(self: Arc<Self>) {
        let dir = self.workspace_dir.join("heartbeat");
        loop {
            // Re-read the directory every iteration so edits take effect.
            let tasks = load_heartbeat_dir(&dir);
            let enabled: Vec<_> = tasks.into_iter().filter(|t| t.meta.enabled).collect();

            let now = Local::now();
            let (next_at, due_tasks): (chrono::DateTime<Local>, Vec<DueTask>) =
                match next_due(&enabled, now) {
                    None => {
                        // No tasks (or none with valid schedules); poll the
                        // directory periodically in case files are added.
                        tokio::time::sleep(StdDuration::from_secs(60)).await;
                        continue;
                    }
                    Some((at, due)) => {
                        let extracted = due
                            .into_iter()
                            .map(|t| DueTask {
                                name: t.name.clone(),
                                body: t.body.clone(),
                                room_id: t.meta.room_id.clone(),
                                voice: t.meta.voice.clone(),
                                device: t.meta.device.clone(),
                            })
                            .collect();
                        (at, extracted)
                    }
                };

            let wait = (next_at - now)
                .to_std()
                .unwrap_or(StdDuration::from_secs(0));
            info!(
                "Heartbeat cron: next fire in {:.0}s ({} task(s))",
                wait.as_secs_f64(),
                due_tasks.len()
            );
            tokio::time::sleep(wait).await;

            for task in due_tasks {
                if self.runs_here(&task).await {
                    self.fire_task(task).await;
                }
            }
        }
    }

    /// Whether this host is the one that runs `task` (`placement`).
    async fn runs_here(&self, task: &DueTask) -> bool {
        use crate::placement::{
            HeartbeatTarget, HostView, Role, Runner, heartbeat_runner, read_claims,
        };
        let role = self.placement.role().await;
        let voice_here = match (&task.voice, &self.serve_state) {
            (Some(v), Some(state)) => state
                .voice_subscribers
                .lock()
                .await
                .contains_key(&v.device_id),
            _ => false,
        };
        let claims = match role {
            Role::Alone => Vec::new(),
            Role::Member { .. } => read_claims(&self.workspace_dir),
        };
        let target = HeartbeatTarget {
            device: task.device.clone(),
            room_id: task.room_id.clone(),
            voice: task.voice.as_ref().map(|v| v.device_id.clone()),
        };
        let view = HostView {
            role: &role,
            local_rooms: &self.local_rooms,
            local_catch_all: self.local_catch_all,
            default_room: self.default_room_id.as_deref(),
            claims: &claims,
            voice_here,
        };
        match heartbeat_runner(&target, &view) {
            Runner::Here => true,
            Runner::Elsewhere => {
                info!("Heartbeat cron: task {} runs on another host", task.name);
                false
            }
            Runner::Nobody(why) => {
                warn!("Heartbeat cron: no host runs task {}: {why}", task.name);
                false
            }
        }
    }

    /// Dispatch a single fired task to the right channel — voice push
    /// when configured and a satellite is online, with a chat-room
    /// fallback when the satellite is offline (per issue #83 4(b)).
    async fn fire_task(self: &Arc<Self>, task: DueTask) {
        let DueTask {
            name,
            body,
            room_id,
            voice,
            ..
        } = task;

        // Voice path: only attempted when a `voice:` target was set on
        // the task AND the server has voice providers configured.
        if let (Some(voice), Some(serve_state)) = (voice, self.serve_state.as_ref()) {
            info!(
                "Heartbeat cron: firing voice task {name} -> device={}",
                voice.device_id
            );
            let prompt = format!("[Heartbeat: {name}]\n\n{body}");
            match crate::serve::push_voice_text_to_subscriber(
                Arc::clone(serve_state),
                voice.device_id.clone(),
                Some(name.clone()),
                prompt,
            )
            .await
            {
                Ok(()) => return,
                Err(crate::serve::VoicePushError::Offline) => {
                    if room_id.is_some() || self.default_room_id.is_some() {
                        warn!(
                            "Heartbeat cron: voice satellite offline (device={}); falling back to chat",
                            voice.device_id
                        );
                    } else {
                        warn!(
                            "Heartbeat cron: voice satellite offline (device={}); no chat fallback configured, dropping",
                            voice.device_id
                        );
                        return;
                    }
                }
                Err(crate::serve::VoicePushError::NoVoice) => {
                    warn!(
                        "Heartbeat cron: voice push unavailable (no STT/TTS providers); falling back to chat for {name}"
                    );
                }
                Err(crate::serve::VoicePushError::NotConfigured) => {
                    warn!(
                        "Heartbeat cron: subscribed satellite's room_profile has no voice_pipeline; falling back to chat for {name}"
                    );
                }
                Err(crate::serve::VoicePushError::Other(msg)) => {
                    warn!(
                        "Heartbeat cron: voice push failed for {name}: {msg}; falling back to chat"
                    );
                }
            }
        }

        // Chat path: original behaviour. Used both when no voice target
        // is set and when the voice push fell through.
        let room = room_id.or_else(|| self.default_room_id.clone());
        match room {
            Some(room) => {
                info!("Heartbeat cron: firing task {name} -> {room}");
                let Some(agent) = &self.agent else {
                    warn!(
                        "Heartbeat cron: task {name} needs a chat channel, and this host has none"
                    );
                    return;
                };
                if let Err(e) = agent.trigger(&name, &body, &room).await {
                    warn!("Heartbeat cron: task {name} failed: {e:#}");
                }
            }
            None => {
                warn!("Heartbeat cron: task {name} has no room_id and no default; skipping");
            }
        }
    }

    /// Compute how long to sleep until the next day-boundary time.
    fn time_until_next_boundary(&self) -> StdDuration {
        let now = Local::now();
        let boundary =
            NaiveTime::from_hms_opt(self.day_boundary_hour as u32, 0, 0).expect("valid hour 0–23");

        let now_time = now.time();
        let secs_until = if now_time < boundary {
            (boundary - now_time).num_seconds()
        } else {
            let secs_today = now_time.num_seconds_from_midnight() as i64;
            let boundary_secs = boundary.num_seconds_from_midnight() as i64;
            86_400 - secs_today + boundary_secs
        };

        StdDuration::from_secs(secs_until.max(1) as u64)
    }
}
