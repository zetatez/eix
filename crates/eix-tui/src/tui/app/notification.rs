//! Mapping daemon notifications onto TUI state.

use super::*;

impl App {
    pub(super) fn notification(
        &mut self,
        notification: eix_core::Notification,
        tx: &UnboundedSender<Command>,
    ) {
        match notification.method.as_str() {
            "plugin.ui.open" | "plugin.ui.update" => {
                let params = &notification.params;
                let plugin = params
                    .get("plugin")
                    .and_then(Value::as_str)
                    .unwrap_or("plugin");
                let title = params
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or(plugin);
                let body = params
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.open_text_view(format!("{plugin} · {title}"), body);
                self.status = format!("{plugin} · {title}");
            }
            "plugin.ui.close" => {
                let plugin = notification
                    .params
                    .get("plugin")
                    .and_then(Value::as_str)
                    .unwrap_or("plugin");
                if self
                    .text_view
                    .as_ref()
                    .is_some_and(|view| view.title.starts_with(&format!("{plugin} · ")))
                {
                    self.text_view = None;
                    self.status = format!("{plugin} closed its panel");
                }
            }
            "plugin.notify" => {
                let plugin = notification
                    .params
                    .get("plugin")
                    .and_then(Value::as_str)
                    .unwrap_or("plugin");
                let level = notification
                    .params
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("info");
                let message = notification
                    .params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let tone = if level == "error" || level == "warn" || level == "warning" {
                    ToastTone::Warning
                } else {
                    ToastTone::Success
                };
                self.status = format!("{plugin}: {message}");
                self.push_toast(self.status.clone(), tone, ATTENTION_TOAST_DISPLAY);
            }
            "integration.updated" => {
                if let Ok(state) = serde_json::from_value::<IntegrationState>(notification.params) {
                    let session_id = state.session_id.clone();
                    let status = state.status;
                    self.integrations.insert(session_id.clone(), state);
                    if status != IntegrationStatus::Merged
                        && self
                            .status_session()
                            .is_some_and(|session| session.id == session_id)
                    {
                        self.status = format!("integration · {}", status.as_str());
                    }
                }
            }
            "integration.attention" => {
                let session_id = notification
                    .params
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = notification
                    .params
                    .get("session_name")
                    .and_then(Value::as_str)
                    .unwrap_or("session")
                    .to_string();
                let error = notification
                    .params
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("integration needs attention")
                    .to_string();
                self.status = format!("integration failed · {}", truncate(&error, 100));
                self.last_seen_status.clone_from(&self.status);
                self.push_toast(
                    format!("{name} · integration failed"),
                    ToastTone::Warning,
                    ATTENTION_TOAST_DISPLAY,
                );
                if self
                    .status_session()
                    .is_none_or(|session| session.id != session_id)
                    && self.bell
                {
                    self.ring = true;
                }
            }
            "session.updated" => {
                if let Ok(session) = serde_json::from_value::<Session>(notification.params) {
                    let id = session.id.clone();
                    let index = self.views.iter().position(|view| view.session.id == id);
                    if let Some(index) = index {
                        self.views[index].session = session.clone();
                        // A queued prompt can start the next turn immediately;
                        // the old "finished" hint no longer applies.
                        if session.status.is_busy() {
                            self.views[index].unread = false;
                        }
                        if index != self.focused && self.pane_needs_attention(index) {
                            self.status = format!("{} needs attention", session.name);
                        }
                    }
                    self.upsert_session(session, false);
                }
            }
            "turn.started" => {
                // The daemon queue starts the next turn without a submit RPC,
                // so adopt its id here; otherwise the stream stays keyed on
                // the previous turn and drops every delta.
                if let Ok(started) = serde_json::from_value::<TurnRef>(notification.params) {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == started.session_id)
                    {
                        if view
                            .stream
                            .as_ref()
                            .is_none_or(|stream| stream.turn_id != started.turn_id)
                        {
                            view.stream = Some(StreamState::new(started.turn_id));
                        }
                        view.unread = false;
                    }
                    if self.status.starts_with("queued") {
                        self.status = "running".into();
                    }
                }
            }
            "prompt.queued" => {
                // The submit response also sets a queued status; a later
                // position from another enqueue may arrive while it is shown.
                if let Ok(queued) = serde_json::from_value::<PromptQueued>(notification.params) {
                    let focused = self
                        .views
                        .iter()
                        .position(|view| view.session.id == queued.session_id)
                        .is_some_and(|index| index == self.focused);
                    if focused && self.status.starts_with("queued") {
                        self.status = format!(
                            "queued · position {} · waiting for the running turn",
                            queued.position.max(1)
                        );
                    }
                }
            }
            "prompt.failed" => {
                // A queued prompt can fail to start (e.g. the session entered
                // integration). Without this the pane would keep showing the
                // queued status forever.
                if let Ok(failed) = serde_json::from_value::<PromptFailed>(notification.params) {
                    let message = if failed.error.is_empty() {
                        "queued prompt failed to start".to_string()
                    } else {
                        failed.error
                    };
                    match self
                        .views
                        .iter()
                        .position(|view| view.session.id == failed.session_id)
                    {
                        Some(index) if index != self.focused => {
                            self.views[index].unread = true;
                        }
                        Some(_) => {
                            self.error = Some(message);
                            self.status = "error".into();
                        }
                        None => {
                            self.push_toast(message, ToastTone::Warning, ATTENTION_TOAST_DISPLAY)
                        }
                    }
                }
            }
            "prompt.cancelled" => {
                // Cancelling a session drops its queue on the daemon side;
                // clear the status here so the pane does not wait forever.
                if let Ok(cancelled) =
                    serde_json::from_value::<PromptCancelled>(notification.params)
                {
                    let focused = self
                        .views
                        .iter()
                        .position(|view| view.session.id == cancelled.session_id)
                        .is_some_and(|index| index == self.focused);
                    if focused && self.status.starts_with("queued") {
                        self.status = if cancelled.dropped > 1 {
                            format!("cancelled · {} queued prompts dropped", cancelled.dropped)
                        } else {
                            "cancelled · queued prompt dropped".into()
                        };
                    }
                }
            }
            "session.attention" => {
                if let Ok(params) = serde_json::from_value::<AttentionParams>(notification.params) {
                    let mut alert = false;
                    if let Some((index, view)) = self
                        .views
                        .iter_mut()
                        .enumerate()
                        .find(|(_, view)| view.session.id == params.session_id)
                    {
                        view.session.status = params.status;
                        if index != self.focused {
                            self.status = format!(
                                "{} · {} in pane {}",
                                view.session.name,
                                params.status.as_str(),
                                index + 1
                            );
                            alert = true;
                        }
                    }
                    if alert {
                        self.last_seen_status.clone_from(&self.status);
                        self.push_toast(
                            self.status.clone(),
                            ToastTone::Warning,
                            ATTENTION_TOAST_DISPLAY,
                        );
                        if self.bell {
                            self.ring = true;
                        }
                    }
                }
            }
            "session.event" => {
                if let Ok(event) = serde_json::from_value::<SessionEvent>(notification.params) {
                    let id = event.session_id.clone();
                    // Any persisted event means the session was used, even if
                    // it is not currently open in a pane.
                    self.created_sessions.remove(&id);
                    // Child sessions are not always open as panes. Feed their
                    // events into the parent's subagent card even when the
                    // child has no view of its own.
                    if let Some(index) = self
                        .views
                        .iter()
                        .position(|view| view.subagents.contains_key(&id))
                    {
                        self.views[index].push_subagent_event(&id, event.clone());
                    }
                    let spawn_result = (event.kind == EventKind::ToolResult)
                        .then(|| {
                            serde_json::from_value::<ToolResultPayload>(event.payload.clone()).ok()
                        })
                        .flatten()
                        .filter(|payload| payload.tool == "spawn_agent");
                    let relevant = matches!(
                        event.kind,
                        EventKind::ToolResult
                            | EventKind::ApprovalRequired
                            | EventKind::PatchProposed
                            | EventKind::PatchApplied
                            | EventKind::PatchRejected
                            | EventKind::PatchConflict
                    );
                    let relevant_input = matches!(
                        event.kind,
                        EventKind::InputRequired | EventKind::InputAnswered
                    );
                    let Some(view) = self.views.iter_mut().find(|view| view.session.id == id)
                    else {
                        return;
                    };
                    match event.kind {
                        EventKind::AssistantMessage => {
                            view.stream = None;
                            if let Ok(payload) = serde_json::from_value::<AssistantMessagePayload>(
                                event.payload.clone(),
                            ) {
                                if payload.cancelled {
                                    self.status = "cancelled".into();
                                }
                            }
                        }
                        EventKind::Error => {
                            if let Some(message) =
                                event.payload.get("message").and_then(Value::as_str)
                            {
                                self.error = Some(message.to_string());
                                self.status = "error".into();
                            }
                        }
                        _ => {}
                    }
                    view.events.push(event);
                    view.events_rev = view.events_rev.wrapping_add(1);
                    if let Some(result) = spawn_result {
                        view.finish_subagent_by_call(&result.call_id, result.ok, &result.output);
                    }
                    if relevant {
                        self.reload_pending(&id);
                    }
                    if relevant_input {
                        self.reload_input(&id);
                    }
                }
            }
            "agent.spawned" => {
                if let Ok(params) =
                    serde_json::from_value::<AgentSpawnedParams>(notification.params)
                {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == params.session_id)
                    {
                        view.upsert_subagent(params.agent_id, params.call_id, params.name.clone());
                        self.status = format!("sub-agent {} started", params.name);
                    }
                }
            }
            "agent.completed" => {
                if let Ok(params) =
                    serde_json::from_value::<AgentCompletedParams>(notification.params)
                {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == params.session_id)
                    {
                        view.complete_subagent(
                            &params.agent_id,
                            &params.call_id,
                            &params.name,
                            params.ok,
                            params.result,
                            params.error,
                        );
                        self.status = if params.ok {
                            format!("sub-agent {} finished", params.name)
                        } else {
                            format!("sub-agent {} failed", params.name)
                        };
                    }
                }
            }
            "message.delta" | "reasoning.delta" => {
                let reasoning = notification.method == "reasoning.delta";
                if let Ok(delta) = serde_json::from_value::<DeltaParams>(notification.params) {
                    // Children usually have no pane of their own, but their
                    // thinking belongs in the parent's card. Fall through so
                    // an open child pane still updates its own stream.
                    if let Some(index) = self
                        .views
                        .iter()
                        .position(|view| view.subagents.contains_key(&delta.session_id))
                    {
                        self.views[index].push_subagent_delta(
                            &delta.session_id,
                            &delta.turn_id,
                            reasoning,
                            &delta.delta,
                        );
                    }
                    let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == delta.session_id)
                    else {
                        return;
                    };
                    if self.status.starts_with("retrying ") {
                        self.status = "running".into();
                    }
                    match &mut view.stream {
                        Some(stream) if stream.turn_id == delta.turn_id => {
                            let target = if reasoning {
                                &mut stream.reasoning
                            } else {
                                &mut stream.text
                            };
                            target.push_str(&delta.delta);
                        }
                        Some(_) => {
                            // Deltas from a superseded turn; ignore them.
                        }
                        None => {
                            let mut stream = StreamState::new(delta.turn_id);
                            if reasoning {
                                stream.reasoning.push_str(&delta.delta);
                            } else {
                                stream.text.push_str(&delta.delta);
                            }
                            view.stream = Some(stream);
                        }
                    }
                }
            }
            "message.reset" => {
                if let Ok(reset) = serde_json::from_value::<TurnRef>(notification.params) {
                    if let Some(index) = self
                        .views
                        .iter()
                        .position(|view| view.subagents.contains_key(&reset.session_id))
                    {
                        self.views[index]
                            .clear_subagent_reasoning(&reset.session_id, &reset.turn_id);
                    }
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == reset.session_id)
                    {
                        if view
                            .stream
                            .as_ref()
                            .is_some_and(|stream| stream.turn_id == reset.turn_id)
                        {
                            view.stream = None;
                        }
                    }
                }
            }
            "turn.retrying" => {
                if let Ok(retry) = serde_json::from_value::<TurnRetrying>(notification.params) {
                    if let Some(index) = self
                        .views
                        .iter()
                        .position(|view| view.subagents.contains_key(&retry.session_id))
                    {
                        self.views[index]
                            .clear_subagent_reasoning(&retry.session_id, &retry.turn_id);
                    }
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == retry.session_id)
                    {
                        if view
                            .stream
                            .as_ref()
                            .is_some_and(|stream| stream.turn_id == retry.turn_id)
                        {
                            view.stream = None;
                        }
                    }
                    let delay = retry.delay_ms as f64 / 1000.0;
                    let reason = truncate(&retry.reason, 80);
                    let kind = match retry.kind.as_str() {
                        "rate_limited" => "rate limited · ",
                        "empty_response" => "empty response · ",
                        "transient" => "upstream error · ",
                        _ => "",
                    };
                    self.error = None;
                    self.status = if reason.is_empty() {
                        format!(
                            "{kind}retrying {}/{} in {delay:.1}s",
                            retry.attempt, retry.max_attempts
                        )
                    } else {
                        format!(
                            "{kind}retrying {}/{} in {delay:.1}s: {reason}",
                            retry.attempt, retry.max_attempts
                        )
                    };
                }
            }
            "tool.approval_required" => {
                if let Ok(params) = serde_json::from_value::<ApprovalParams>(notification.params) {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == params.session_id)
                    {
                        view.follow = true;
                    }
                    if self
                        .views
                        .get(self.focused)
                        .is_some_and(|view| view.session.id == params.session_id)
                    {
                        // The turn is blocked on the card; let j/k/Enter drive
                        // it without first leaving the prompt.
                        self.focus = Focus::Transcript;
                    }
                    self.status = format!("approval needed: {}", params.summary);
                    let mut summary = if matches!(params.tool.as_str(), "exec_command") {
                        params
                            .arguments
                            .get("cmd")
                            .or_else(|| params.arguments.get("command"))
                            .and_then(serde_json::Value::as_str)
                            .map(|command| format!("$ {command}"))
                            .unwrap_or(params.summary)
                    } else {
                        params.summary
                    };
                    let risk = params.risk.to_ascii_uppercase();
                    if !risk.is_empty() {
                        summary = format!("[{risk}] {summary}");
                    }
                    for reason in &params.reasons {
                        summary.push_str(&format!("\n{reason}"));
                    }
                    if !params.risk_categories.is_empty() {
                        summary.push_str(&format!("\nrisk: {}", params.risk_categories.join(", ")));
                    }
                    if !params.sandbox.is_empty() {
                        summary.push_str(&format!("\nsandbox: {}", params.sandbox));
                    }
                    if let Some(rule) = params.rule.as_deref() {
                        summary.push_str(&format!("\nrule: {rule} ({})", params.rule_source));
                    }
                    if !params.scopes.is_empty() {
                        summary.push_str(&format!("\nscopes: {}", params.scopes.join(", ")));
                    }
                    if !params.allow_always {
                        summary.push_str(NO_ALWAYS_MARKER);
                    }
                    self.approval_rev = self.approval_rev.wrapping_add(1);
                    self.approvals.insert(
                        params.session_id,
                        PendingApproval {
                            kind: PendingKind::Tool,
                            approval_id: params.approval_id,
                            call_id: params.call_id,
                            tool: params.tool,
                            summary,
                            selected: vec![true; params.hunks.len()],
                            path: params
                                .arguments
                                .get("path")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                            diff: params.diff,
                            hunks: params.hunks,
                            cursor: 0,
                        },
                    );
                }
            }
            "input.required" => {
                if let Ok(params) = serde_json::from_value::<InputParams>(notification.params) {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == params.session_id)
                    {
                        view.follow = true;
                    }
                    if self
                        .views
                        .get(self.focused)
                        .is_some_and(|view| view.session.id == params.session_id)
                    {
                        self.focus = Focus::Transcript;
                    }
                    self.status = params
                        .questions
                        .first()
                        .map(|question| format!("question: {}", truncate(&question.header, 40)))
                        .unwrap_or_else(|| "question".to_string());
                    self.inputs.insert(
                        params.session_id,
                        PendingInput::new(params.input_id, params.call_id, params.questions),
                    );
                }
            }
            "patch.proposed" => {
                if let Ok(params) =
                    serde_json::from_value::<PatchProposedParams>(notification.params)
                {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == params.session_id)
                    {
                        view.follow = true;
                    }
                    self.status = format!("patch staged: {}", params.path);
                    self.reload_pending(&params.session_id);
                }
            }
            "model.updated" => {
                if let Some(model) = notification.params["model"].as_str() {
                    if let Some(hello) = self.hello.as_mut() {
                        hello.model = model.to_string();
                    }
                    let message = format!("default model → {model}");
                    self.status = message.clone();
                    self.push_toast(message, ToastTone::Success, RELOAD_TOAST_DISPLAY);
                }
            }
            "config.reloaded" => {
                if let Some(value) = notification.params["max_parallel_subagents"]
                    .as_u64()
                    .map(|value| value as usize)
                {
                    self.subagent_concurrency = value;
                }
                self.status = "config reloaded".into();
            }
            "config.error" => {
                let error = notification.params["error"]
                    .as_str()
                    .unwrap_or("invalid config");
                self.error = Some(format!("config: {error}"));
                self.status = "config error".into();
            }
            "patch.applied" | "patch.rejected" | "patch.conflict" => {
                if let Ok(params) =
                    serde_json::from_value::<PatchOutcomeParams>(notification.params)
                {
                    let message = match notification.method.as_str() {
                        "patch.applied" => format!("patch applied: {}", params.path),
                        "patch.rejected" => format!("patch rejected: {}", params.path),
                        _ => format!("patch conflict: {}", params.path),
                    };
                    self.status = message;
                    self.reload_pending(&params.session_id);
                    self.close_diff_view_for(&params.session_id);
                }
            }
            "tool.completed" => {
                if let Ok(params) = serde_json::from_value::<ToolCompleted>(notification.params) {
                    let matches_completed =
                        self.approvals
                            .get(&params.session_id)
                            .is_some_and(|pending| {
                                pending.kind == PendingKind::Tool
                                    && (params.call_id.is_empty()
                                        || pending.call_id == params.call_id)
                            });
                    if matches_completed {
                        self.reload_pending(&params.session_id);
                        self.close_diff_view_for(&params.session_id);
                    }
                    if self.inputs.get(&params.session_id).is_some_and(|pending| {
                        params.call_id.is_empty() || pending.call_id == params.call_id
                    }) {
                        self.inputs.remove(&params.session_id);
                    }
                }
            }
            "turn.completed" => {
                if let Ok(done) = serde_json::from_value::<TurnCompleted>(notification.params) {
                    // A blocking approval belongs to the finished turn; a staged
                    // patch survives until the user decides.
                    if self
                        .approvals
                        .get(&done.session_id)
                        .is_some_and(|pending| pending.kind == PendingKind::Tool)
                    {
                        self.reload_pending(&done.session_id);
                        self.close_diff_view_for(&done.session_id);
                    }
                    // A question cannot outlive its turn.
                    self.inputs.remove(&done.session_id);
                    let Some(index) = self
                        .views
                        .iter()
                        .position(|view| view.session.id == done.session_id)
                    else {
                        return;
                    };
                    if let Some(stream) = &self.views[index].stream {
                        if stream.turn_id != done.turn_id {
                            return;
                        }
                    }
                    // A completed turn in a background pane stays marked until
                    // the user focuses that pane; a cancelled or failed turn is
                    // covered by the attention marker instead.
                    let finished = done.error.is_none() && !done.cancelled;
                    let background = index != self.focused;
                    self.views[index].stream = None;
                    if let Some(error) = done.error {
                        self.error = Some(error);
                        self.status = "error".into();
                        // The prompt was cleared on submit; restore the draft so
                        // a provider failure does not lose it.
                        self.restore_pending_draft(&done.session_id);
                    } else if done.cancelled {
                        self.status = "cancelled".into();
                    } else {
                        self.status = turn_summary(&done);
                        self.clear_pending_draft(&done.session_id);
                    }
                    if finished && background {
                        self.views[index].unread = true;
                    }
                }
            }
            "process.started" => {
                if let Ok(info) = serde_json::from_value::<ProcessInfo>(notification.params) {
                    if self
                        .views
                        .iter()
                        .any(|view| view.session.id == info.session_id)
                    {
                        self.status =
                            format!("process {} started: {}", short_id(&info.id), info.command);
                    }
                    if let Some(view) = &self.process_view {
                        if view.session_id == info.session_id {
                            let session_id = view.session_id.clone();
                            let _ = tx.send(Command::ProcessList { session_id });
                        }
                    }
                }
            }
            "process.output" => {
                if let Ok(params) =
                    serde_json::from_value::<ProcessChunkParams>(notification.params)
                {
                    if let Some(view) = &mut self.process_view {
                        if view.session_id == params.session_id {
                            view.append(&params.process_id, &params.chunk);
                        }
                    }
                }
            }
            "process.exited" => {
                if let Ok(params) =
                    serde_json::from_value::<ProcessExitedParams>(notification.params)
                {
                    let status = params.status;
                    if self
                        .views
                        .iter()
                        .any(|view| view.session.id == params.session_id)
                    {
                        self.status = format!(
                            "process {} {}{}",
                            short_id(&params.process_id),
                            status.as_str(),
                            params
                                .exit_code
                                .map(|code| format!(" ({code})"))
                                .unwrap_or_default()
                        );
                    }
                    if let Some(view) = &mut self.process_view {
                        if view.session_id == params.session_id {
                            view.mark_exited(&params.process_id, status, params.exit_code);
                            if view.selected().map(|process| process.id.as_str())
                                == Some(params.process_id.as_str())
                            {
                                let _ = tx.send(Command::ProcessOutput {
                                    process_id: params.process_id,
                                });
                            }
                        }
                    }
                }
            }
            "btw.delta" => {
                if let Ok(params) = serde_json::from_value::<BtwDeltaParams>(notification.params) {
                    if let Some(btw) = self.btw.as_mut() {
                        if btw.session_id == params.session_id
                            && (btw.btw_id.is_empty() || btw.btw_id == params.btw_id)
                        {
                            btw.text.push_str(&params.delta);
                        }
                    }
                }
            }
            "btw.completed" => {
                if let Ok(params) =
                    serde_json::from_value::<BtwCompletedParams>(notification.params)
                {
                    if let Some(btw) = self.btw.as_mut() {
                        if btw.session_id == params.session_id
                            && (btw.btw_id.is_empty() || btw.btw_id == params.btw_id)
                        {
                            if !params.text.is_empty() {
                                btw.text = params.text;
                            }
                            btw.done = true;
                            self.status = match params.error.as_deref() {
                                Some(error) => {
                                    btw.error = Some(error.to_string());
                                    "side question failed".into()
                                }
                                None => "side question answered · Esc to close".into(),
                            };
                        }
                    }
                }
            }
            "stream.lagged" => {
                if let Some(skipped) = notification.params.get("skipped") {
                    self.status = format!("dropped {skipped} notifications");
                }
            }
            _ => {}
        }
    }
}
