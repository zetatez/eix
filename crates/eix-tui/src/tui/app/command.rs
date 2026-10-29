//! Slash command execution.

use chrono::TimeZone;

use super::*;

impl App {
    /// Prompt commands are typed as the whole message (`/name args`). They never
    /// reach the model.
    pub(super) fn handle_prompt_command(
        &mut self,
        text: &str,
        tx: &UnboundedSender<Command>,
    ) -> bool {
        self.handle_command_text(text, tx, true)
    }

    /// Run a `/command` line. `clear_prompt` is false for the `:` command
    /// line, where the prompt buffer is an unrelated draft that must survive.
    pub(super) fn handle_command_text(
        &mut self,
        text: &str,
        tx: &UnboundedSender<Command>,
        clear_prompt: bool,
    ) -> bool {
        let trimmed = text.trim();
        let (name, args) = match trimmed.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (trimmed, ""),
        };
        let typed = name.trim_start_matches('/');
        if typed.is_empty() {
            return false;
        }
        let spec = self
            .commands
            .iter()
            .find(|spec| spec.name == typed)
            .or_else(|| {
                command_alias(typed)
                    .and_then(|canonical| self.commands.iter().find(|spec| spec.name == canonical))
            })
            .cloned();
        let Some(spec) = spec else {
            self.error = Some(format!("unknown command /{typed} · /help lists them"));
            return true;
        };
        let name = spec.name.as_str();
        if clear_prompt {
            // Commands are not drafts: drop the typed line (and its saved
            // draft) before dispatching. `/init` installs its own prompt
            // afterwards. The `:` command line leaves the prompt draft alone.
            self.input.clear();
            self.last_saved_draft.clear();
            self.reset_history();
            // `/exit`, `/quit` and `/close` save the (now empty) draft
            // themselves.
            if !matches!(name, "exit" | "quit" | "close") {
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::DraftSet {
                        session_id,
                        text: String::new(),
                    });
                }
            }
        }
        match name {
            "exit" | "quit" => {
                if clear_prompt {
                    self.exit_session(tx);
                } else {
                    self.quit_preserving_draft(tx);
                }
                true
            }
            "close" => {
                if clear_prompt {
                    self.close_session(tx);
                } else {
                    self.close_preserving_draft(tx);
                }
                true
            }
            "help" => {
                self.open_help_scoped(None, args);
                true
            }
            "new" => {
                self.open_picker(PickerKind::NewPane, tx);
                true
            }
            "new-worktree" => {
                let name = (!args.is_empty()).then(|| args.to_string());
                self.new_worktree(tx, name);
                true
            }
            "ready" => {
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::Ready { session_id });
                    self.status = "queuing worktree for integration…".into();
                } else {
                    self.error = Some("no session to mark ready".into());
                }
                true
            }
            "integrate" => {
                if let Some(session) = self.status_session() {
                    if let Some(state) = self.integrations.get(&session.id) {
                        let error = state
                            .error
                            .as_deref()
                            .map(|error| format!(" · {}", truncate(error, 80)))
                            .unwrap_or_default();
                        self.status = format!(
                            "integration {} · {}{}",
                            state.status.symbol(),
                            state.status.as_str(),
                            error
                        );
                    } else {
                        self.status = "no integration state yet".into();
                    }
                }
                true
            }
            "files" => {
                self.open_picker(PickerKind::Files, tx);
                true
            }
            "grep" => {
                self.open_grep_picker(args, tx);
                true
            }
            "git" => {
                self.open_picker(PickerKind::Git, tx);
                true
            }
            "processes" => {
                self.open_processes(tx);
                true
            }
            "approvals" => {
                self.open_inbox();
                true
            }
            "cancel" => {
                self.cancel(tx);
                true
            }
            "cd" => {
                self.command_cd(args, tx);
                true
            }
            "clear" => {
                self.command_clear(tx);
                true
            }
            "compact" => {
                self.command_compact(args, tx);
                true
            }
            "rewind" => {
                if args.is_empty() {
                    self.open_picker(PickerKind::Rewind, tx);
                } else if let Some(session_id) = self.focused_session_id() {
                    let mut parts = args.split_whitespace();
                    let target = parts.next().unwrap_or_default();
                    let scope_arg = parts.next();
                    if parts.next().is_some() {
                        self.error = Some("usage: /rewind [n|e<seq>] [both|chat|files]".into());
                        return true;
                    }
                    let scope = match scope_arg.unwrap_or("both") {
                        "both" => Some(eix_core::RewindScope::Both),
                        "chat" | "conversation" => Some(eix_core::RewindScope::Conversation),
                        "files" | "file" => Some(eix_core::RewindScope::Files),
                        other => {
                            self.error = Some(format!(
                                "unknown rewind scope {other:?} · use both, chat or files"
                            ));
                            None
                        }
                    };
                    if let Some(scope) = scope {
                        let turns: Vec<u64> = self
                            .focused_view()
                            .map(|view| {
                                timeline_picker_items(&view.events)
                                    .into_iter()
                                    .filter_map(|item| item.value.parse::<u64>().ok())
                                    .collect()
                            })
                            .unwrap_or_default();
                        let at_seq = if let Some(seq) = target.strip_prefix('e') {
                            seq.parse::<u64>().ok().filter(|seq| turns.contains(seq))
                        } else {
                            target
                                .parse::<usize>()
                                .ok()
                                .filter(|n| *n > 0)
                                .and_then(|n| turns.get(n - 1).copied())
                        };
                        match at_seq {
                            Some(at_seq) => {
                                let _ = tx.send(Command::Rewind {
                                    session_id,
                                    at_seq,
                                    scope,
                                });
                                self.status = format!("rewinding to e{at_seq} · {}", scope.label());
                            }
                            None => {
                                self.error = Some(format!(
                                    "no checkpoint {target:?} · /rewind opens the picker"
                                ));
                            }
                        }
                    }
                }
                true
            }
            "context" => {
                let all = match args {
                    "" => Some(false),
                    "all" | "full" => Some(true),
                    _ => None,
                };
                match (all, self.focused_session_id()) {
                    (Some(all), Some(session_id)) => {
                        let _ = tx.send(Command::ContextInfo { session_id, all });
                        self.status = "context: reading ledger…".into();
                    }
                    (Some(_), None) => self.status = "context: no session".into(),
                    (None, _) => self.error = Some("usage: /context [all]".into()),
                }
                true
            }
            "effort" => {
                if args.is_empty() {
                    self.open_picker(PickerKind::Effort, tx);
                } else if self.set_reasoning(args, tx) {
                    self.status = format!("effort: {args}");
                }
                true
            }
            "btw" => {
                self.command_btw(args, tx);
                true
            }
            "init" => {
                self.command_init();
                true
            }
            "sessions" => {
                self.open_picker(PickerKind::Sessions, tx);
                true
            }
            "projects" => {
                self.open_picker(PickerKind::Projects, tx);
                true
            }
            "model" => {
                if args.is_empty() {
                    self.open_picker(PickerKind::Models, tx);
                } else if let Some(global) = args
                    .strip_prefix("global")
                    .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
                {
                    let model = global.trim();
                    let model =
                        (!model.is_empty() && model != "default").then(|| model.to_string());
                    let _ = tx.send(Command::UseModel { model });
                    self.status = "switching the daemon default model…".into();
                } else if let Some(session_id) = self.focused_session_id() {
                    let model = (args != "default").then(|| args.to_string());
                    let _ = tx.send(Command::SetModel { session_id, model });
                    self.status = format!("switching model to {args}…");
                }
                true
            }
            "model-plan" => {
                if args.is_empty() {
                    self.open_picker(PickerKind::PlanModels, tx);
                } else if let Some(session_id) = self.focused_session_id() {
                    let model = (args != "default").then(|| args.to_string());
                    let _ = tx.send(Command::SetPlanModel { session_id, model });
                    self.status = format!("switching Plan model to {args}…");
                }
                true
            }
            "agents" => {
                if args.is_empty() {
                    self.open_picker(PickerKind::Agents, tx);
                } else if let Some(session_id) = self.focused_session_id() {
                    let agent = (args != "default").then(|| args.to_string());
                    let _ = tx.send(Command::SetAgent { session_id, agent });
                    self.status = format!("switching agent to {args}…");
                }
                true
            }
            "subagents" => {
                if args.is_empty() {
                    self.open_subagents(tx);
                } else if args == "limit" {
                    self.open_picker(PickerKind::Subagents, tx);
                } else {
                    match args.parse::<usize>() {
                        Ok(value) => self.set_subagent_concurrency(value, tx),
                        Err(_) => {
                            self.error = Some(
                                "usage: /subagents [n|limit] (2, 4, 8, 16, 32, 64, 128, or 256)"
                                    .into(),
                            );
                        }
                    }
                }
                true
            }
            "mcp" => {
                self.open_picker(PickerKind::Mcp, tx);
                true
            }
            "plugins" => {
                let mut parts = args.split_whitespace();
                let first = parts.next().unwrap_or_default();
                match first {
                    "" => {
                        let _ = tx.send(Command::ListPlugins { name: None });
                        self.status = "plugins: loading…".into();
                    }
                    "logs" => {
                        let name = parts.next().map(str::to_string);
                        let _ = tx.send(Command::PluginLogs { name });
                        self.status = "plugin logs: loading…".into();
                    }
                    "doctor" | "check" => {
                        let name = parts.next().map(str::to_string);
                        let _ = tx.send(Command::PluginDoctor { name });
                        self.status = "plugin doctor: running…".into();
                    }
                    "restart" | "start" | "stop" | "enable" | "disable" | "uninstall" => {
                        match parts.next() {
                            Some(name) => {
                                let _ = tx.send(Command::PluginAction {
                                    action: first.to_string(),
                                    name: name.to_string(),
                                });
                                self.status = format!("plugin {name}: {first}…");
                            }
                            None => {
                                self.error = Some(format!("usage: /plugins {first} <name>"));
                            }
                        }
                    }
                    "open" => match (parts.next(), parts.next()) {
                        (Some(plugin), Some(id)) => {
                            let surface = parts.next().unwrap_or("panel").to_string();
                            let _ = tx.send(Command::PluginUiRender {
                                plugin: plugin.to_string(),
                                surface,
                                id: id.to_string(),
                            });
                            self.status = "plugin panel: loading…".into();
                        }
                        _ => {
                            self.error =
                                Some("usage: /plugins open <plugin> <id> [surface]".into());
                        }
                    },
                    "action" | "act" => {
                        let (plugin, id) = if first == "act" {
                            match self.plugin_panel.as_ref() {
                                Some(panel) => (panel.plugin.clone(), panel.id.clone()),
                                None => {
                                    self.error =
                                        Some("no plugin panel open; use /plugins open".into());
                                    return true;
                                }
                            }
                        } else {
                            match (parts.next(), parts.next()) {
                                (Some(plugin), Some(id)) => (plugin.to_string(), id.to_string()),
                                _ => {
                                    self.error = Some(
                                        "usage: /plugins action <plugin> <id> <action> [k=v]…"
                                            .into(),
                                    );
                                    return true;
                                }
                            }
                        };
                        let Some(action) = parts.next() else {
                            self.error =
                                Some("usage: /plugins action <plugin> <id> <action> [k=v]…".into());
                            return true;
                        };
                        let mut values = serde_json::Map::new();
                        for assignment in parts {
                            let Some((key, value)) = assignment.split_once('=') else {
                                self.error = Some(format!(
                                    "plugin action values must be key=value (bad {assignment:?})"
                                ));
                                return true;
                            };
                            let value = serde_json::from_str(value)
                                .unwrap_or_else(|_| Value::String(value.to_string()));
                            values.insert(key.to_string(), value);
                        }
                        let _ = tx.send(Command::PluginUiAction {
                            plugin,
                            id,
                            action: action.to_string(),
                            values: Value::Object(values),
                        });
                        self.status = format!("plugin action {action}: sending…");
                    }
                    "pick" => match (parts.next(), parts.next()) {
                        (Some(plugin), Some(id)) => {
                            let query = parts.collect::<Vec<_>>().join(" ");
                            self.open_plugin_picker(plugin, id, id, tx);
                            if !query.is_empty() {
                                if let Some(picker) = &mut self.picker {
                                    picker.query = query;
                                }
                                self.picker_query_due = true;
                            }
                        }
                        _ => {
                            self.error = Some("usage: /plugins pick <plugin> <id> [query]".into());
                        }
                    },
                    "set" => match parts.next() {
                        Some(name) => {
                            let mut values = serde_json::Map::new();
                            for assignment in parts {
                                let Some((key, value)) = assignment.split_once('=') else {
                                    self.error = Some(format!(
                                        "usage: /plugins set <name> <key>=<json>… (bad {assignment:?})"
                                    ));
                                    return true;
                                };
                                let value = serde_json::from_str(value)
                                    .unwrap_or_else(|_| Value::String(value.to_string()));
                                values.insert(key.to_string(), value);
                            }
                            if values.is_empty() {
                                self.error =
                                    Some("usage: /plugins set <name> <key>=<json>…".into());
                            } else {
                                let _ = tx.send(Command::PluginSettingsSet {
                                    name: name.to_string(),
                                    values: Value::Object(values),
                                });
                                self.status = format!("plugin {name}: updating settings…");
                            }
                        }
                        None => {
                            self.error = Some("usage: /plugins set <name> <key>=<json>…".into());
                        }
                    },
                    _ => {
                        let _ = tx.send(Command::ListPlugins {
                            name: Some(first.to_string()),
                        });
                        self.status = "plugins: loading…".into();
                    }
                }
                true
            }
            "reload" => {
                let _ = tx.send(Command::ReloadConfig);
                self.status = "reloading config…".into();
                true
            }
            "math" => {
                let option = args.split_whitespace().next().unwrap_or("status");
                match option {
                    "on" | "off" => {
                        self.math_config.images = option == "on";
                        self.apply_math_mode(false);
                    }
                    "reload" => {
                        self.apply_math_mode(true);
                    }
                    "status" => {
                        let status = super::super::math_image::status();
                        self.status = if super::super::math_image::raw_source() {
                            "math: off (raw TeX)".into()
                        } else if status.enabled {
                            format!("math: images {} @ {} dpi", status.protocol, status.dpi)
                        } else {
                            format!(
                                "math: ASCII fallback ({})",
                                status
                                    .reason
                                    .as_deref()
                                    .unwrap_or("no graphics protocol detected")
                            )
                        };
                        if let Some(error) = &status.last_error {
                            self.status.push_str(&format!(" · last: {error}"));
                        }
                    }
                    other => {
                        self.error = Some(format!(
                            "unknown /math option {other:?} · use on, off, reload or status"
                        ));
                    }
                }
                true
            }
            "colorscheme" => {
                if args.is_empty() {
                    self.open_picker(PickerKind::Themes, tx);
                } else {
                    self.error = Some("usage: /colorscheme (opens the colorscheme picker)".into());
                }
                true
            }
            "timeline" => {
                self.open_picker(PickerKind::Timeline, tx);
                true
            }
            "archive" => {
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::ArchiveSession {
                        session_id,
                        archived: true,
                    });
                    self.status = "archiving…".into();
                }
                true
            }
            "delete" => {
                let Some(session_id) = self.focused_session_id() else {
                    self.status = "no session yet".into();
                    return true;
                };
                let _ = tx.send(Command::DeleteSession { session_id });
                self.status = "deleting…".into();
                true
            }
            "image" => {
                if args == "clear" {
                    self.pending_images.clear();
                    self.status = "images cleared".into();
                } else if args.is_empty() {
                    self.open_picker(PickerKind::Images, tx);
                } else {
                    let cwd = self
                        .focused_view()
                        .map(|view| view.session.cwd.clone())
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
                    let paths = parse_image_paths(args, &cwd)
                        .unwrap_or_else(|| args.split_whitespace().map(str::to_string).collect());
                    for path in paths {
                        if !self.pending_images.contains(&path) {
                            self.pending_images.push(path);
                        }
                    }
                    self.status = format!(
                        "attached {} · {} image(s) for the next prompt",
                        args,
                        self.pending_images.len()
                    );
                }
                true
            }
            "todos" => {
                self.open_picker(PickerKind::Todos, tx);
                true
            }
            "skills" => {
                let cwd = self
                    .focused_view()
                    .map(|view| view.session.cwd.clone())
                    .unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                    });
                if args.is_empty() {
                    self.open_picker(PickerKind::Skills, tx);
                } else {
                    let (name, arguments) = args
                        .split_once(char::is_whitespace)
                        .map(|(name, rest)| (name, rest.trim()))
                        .unwrap_or((args, ""));
                    if let Some(skill) = eix_core::find_skill(&cwd, name, &self.compat) {
                        let mention = if arguments.is_empty() {
                            format!("@{}", skill.name)
                        } else {
                            format!("@{} {}", skill.name, arguments)
                        };
                        self.input.insert_text(&mention);
                        self.input.enter_insert();
                        self.focus = Focus::Input;
                        self.status = format!("inserted @{}", skill.name);
                    } else {
                        self.error = Some(format!("unknown skill {name:?}"));
                    }
                }
                true
            }
            "undo" | "redo" => {
                if let Some(session_id) = self.focused_session_id() {
                    let redo = name == "redo";
                    let _ = tx.send(Command::Undo { session_id, redo });
                    self.status = if redo { "redoing…" } else { "undoing…" }.into();
                }
                true
            }
            "rename" => {
                let Some(session_id) = self.focused_session_id() else {
                    self.status = "no session yet".into();
                    return true;
                };
                let name = args.trim();
                if name.is_empty() {
                    self.error = Some("usage: /rename <name>".into());
                } else {
                    self.status = format!("renaming to {name}…");
                    let _ = tx.send(Command::RenameSession {
                        session_id,
                        name: name.to_string(),
                    });
                }
                true
            }
            "diff" => {
                let Some(session_id) = self.focused_session_id() else {
                    self.status = "no session yet".into();
                    return true;
                };
                let staged = matches!(args, "staged" | "--staged" | "cached");
                if !args.is_empty() && !staged {
                    self.error = Some("usage: /diff [staged]".into());
                    return true;
                }
                self.open_picker(PickerKind::Diff, tx);
                let _ = tx.send(Command::WorkspaceDiff { session_id, staged });
                self.status = if staged {
                    "diff (staged): loading…".into()
                } else {
                    "diff: loading…".into()
                };
                true
            }
            "doctor" => {
                let _ = tx.send(Command::Doctor);
                self.status = "running doctor…".into();
                true
            }
            "goal" => {
                let Some(session_id) = self.focused_session_id() else {
                    self.status = "no session yet".into();
                    return true;
                };
                let (text, done, clear) = match args {
                    "" | "status" => (None, None, false),
                    "clear" | "off" => (None, None, true),
                    "done" | "complete" => (None, Some(true), false),
                    "reopen" | "open" => (None, Some(false), false),
                    text => (Some(text.to_string()), None, false),
                };
                let _ = tx.send(Command::Goal {
                    session_id,
                    text,
                    done,
                    clear,
                });
                self.status = "updating goal…".into();
                true
            }
            "loop" => {
                let Some(session_id) = self.focused_session_id() else {
                    self.status = "no session yet".into();
                    return true;
                };
                if args.is_empty() || args == "list" {
                    let _ = tx.send(Command::ListSchedules {
                        session_id: Some(session_id),
                    });
                    self.status = "loading schedules…".into();
                } else if args == "stop" || args == "clear" {
                    let _ = tx.send(Command::RemoveSchedules {
                        id: None,
                        session_id: Some(session_id),
                        kind: Some("loop".into()),
                    });
                } else {
                    let (interval, prompt) = args
                        .split_once(char::is_whitespace)
                        .map(|(head, rest)| (head, rest.trim()))
                        .unwrap_or((args, ""));
                    let Some(seconds) = parse_duration(interval) else {
                        self.error = Some(
                            "usage: /loop <interval> <prompt> · e.g. /loop 10m check the build"
                                .into(),
                        );
                        return true;
                    };
                    if prompt.is_empty() {
                        self.error = Some("usage: /loop <interval> <prompt>".into());
                        return true;
                    }
                    let _ = tx.send(Command::AddSchedule {
                        session_id,
                        kind: "loop".into(),
                        prompt: prompt.to_string(),
                        interval_secs: seconds,
                        at: None,
                    });
                    self.status = format!("starting a loop every {interval}…");
                }
                true
            }
            "schedule" => {
                let Some(session_id) = self.focused_session_id() else {
                    self.status = "no session yet".into();
                    return true;
                };
                if args.is_empty() || args == "list" {
                    let _ = tx.send(Command::ListSchedules {
                        session_id: Some(session_id),
                    });
                    self.status = "loading schedules…".into();
                } else if let Some(id) = args.strip_prefix("stop ") {
                    let _ = tx.send(Command::RemoveSchedules {
                        id: Some(id.trim().to_string()),
                        session_id: None,
                        kind: None,
                    });
                } else if args == "stop" || args == "clear" {
                    let _ = tx.send(Command::RemoveSchedules {
                        id: None,
                        session_id: Some(session_id),
                        kind: Some("once".into()),
                    });
                } else {
                    let (when, prompt) = args
                        .split_once(char::is_whitespace)
                        .map(|(head, rest)| (head, rest.trim()))
                        .unwrap_or((args, ""));
                    let Some(at) = parse_schedule_at(when) else {
                        self.error = Some(
                            "usage: /schedule <+duration|HH:MM> <prompt> · e.g. /schedule +30m run tests"
                                .into(),
                        );
                        return true;
                    };
                    if prompt.is_empty() {
                        self.error = Some("usage: /schedule <when> <prompt>".into());
                        return true;
                    }
                    let _ = tx.send(Command::AddSchedule {
                        session_id,
                        kind: "once".into(),
                        prompt: prompt.to_string(),
                        interval_secs: 0,
                        at: Some(at),
                    });
                    self.status = format!("scheduling for {when}…");
                }
                true
            }
            "perm" => {
                self.command_mode(args, tx);
                true
            }
            "fork" => {
                if args.is_empty() {
                    self.open_picker(PickerKind::Fork, tx);
                } else if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::Fork {
                        session_id,
                        at_seq: None,
                        name: Some(args.to_string()),
                        branch: false,
                    });
                    self.status = "forking…".into();
                }
                true
            }
            "branch" => {
                let name = (!args.is_empty()).then(|| args.to_string());
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::Fork {
                        session_id,
                        at_seq: None,
                        name,
                        branch: true,
                    });
                    self.status = "branching…".into();
                }
                true
            }
            _ => {
                if spec.action {
                    let Some(session_id) = self.focused_session_id() else {
                        self.status = "no session yet".into();
                        return true;
                    };
                    let _ = tx.send(Command::RunPluginCommand {
                        session_id,
                        name: spec.name.clone(),
                        arguments: args.to_string(),
                    });
                    self.status = format!("/{}: running…", spec.name);
                    return true;
                }
                if let Some(template) = spec.template {
                    let expanded = eix_core::expand_command(&template, args);
                    if expanded.trim().is_empty() {
                        self.error = Some(format!("/{name} produced an empty prompt"));
                        return true;
                    }
                    let Some(session_id) = self.focused_session_id() else {
                        self.status = "no session yet".into();
                        return true;
                    };
                    if let Some(model) = spec.model {
                        let _ = tx.send(Command::SetModel {
                            session_id: session_id.clone(),
                            model: Some(model),
                        });
                    }
                    if let Some(agent) = spec.agent {
                        let _ = tx.send(Command::SetAgent {
                            session_id: session_id.clone(),
                            agent: Some(agent),
                        });
                    }
                    if self.session_busy(&session_id) {
                        self.enqueue(session_id, &expanded, tx);
                    } else {
                        self.send_prompt(&session_id, &expanded, tx, false, Vec::new());
                    }
                    true
                } else {
                    self.error = Some(format!("unknown command /{name} · /help lists them"));
                    true
                }
            }
        }
    }

    pub(super) fn command_cd(&mut self, args: &str, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        if args.is_empty() {
            let cwd = self
                .focused_view()
                .map(|view| view.session.cwd.display().to_string())
                .unwrap_or_default();
            self.status = format!("cwd: {cwd}");
            return;
        }
        self.status = format!("changing directory to {args}…");
        let _ = tx.send(Command::SetCwd {
            session_id,
            path: args.to_string(),
        });
    }

    pub(super) fn command_clear(&mut self, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        if self.session_busy(&session_id) {
            self.error = Some("cancel the running turn before /clear".into());
            return;
        }
        self.status = "clearing session…".into();
        let _ = tx.send(Command::ClearContext { session_id });
    }

    pub(super) fn command_compact(&mut self, args: &str, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        if self.session_busy(&session_id) {
            self.error = Some("cancel the running turn before /compact".into());
            return;
        }
        let notes = (!args.is_empty()).then(|| args.to_string());
        self.status = "summarizing the conversation…".into();
        let _ = tx.send(Command::CompactContext { session_id, notes });
    }

    pub(super) fn command_btw(&mut self, question: &str, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        if question.is_empty() {
            self.status = "usage: /btw <question>".into();
            return;
        }
        self.status = "asking a side question…".into();
        self.btw = Some(BtwView {
            session_id: session_id.clone(),
            btw_id: String::new(),
            question: question.to_string(),
            text: String::new(),
            error: None,
            done: false,
            scroll: 0,
        });
        let _ = tx.send(Command::Btw {
            session_id,
            question: question.to_string(),
        });
    }

    /// `/init` stages a prompt that asks the model to write `AGENTS.md`. The
    /// user reviews it in insert mode before sending.
    pub(super) fn command_init(&mut self) {
        let prompt = "Explore this repository and create a concise AGENTS.md at its root. \
Cover: the project's purpose, the workspace layout, the build/test/lint commands that \
actually work here, code style conventions, and any non-obvious constraints future \
contributors must respect. Read the existing docs and config first. If an AGENTS.md \
already exists, update it in place rather than rewriting it from scratch.";
        self.input.set_text(prompt);
        self.input.enter_insert();
        self.focus = Focus::Input;
        self.reset_history();
        self.status = "review the /init prompt, then press Enter to run it".into();
    }

    pub(super) fn command_mode(&mut self, args: &str, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        if args.is_empty() {
            self.open_picker(PickerKind::Modes, tx);
            return;
        }
        match args.parse::<SessionMode>() {
            Ok(mode) => {
                self.status = format!("mode → {}", mode.label());
                let _ = tx.send(Command::SetMode {
                    session_id,
                    mode,
                    temporary: false,
                });
            }
            Err(()) => {
                self.error = Some("usage: /perm plan | build | auto | full".into());
            }
        }
    }

    pub(super) fn set_subagent_concurrency(&mut self, value: usize, tx: &UnboundedSender<Command>) {
        if !eix_core::subagent_concurrency_supported(value) {
            self.error =
                Some("subagent concurrency must be 2, 4, 8, 16, 32, 64, 128, or 256".into());
            return;
        }
        self.status = format!("setting subagents to {value}…");
        let _ = tx.send(Command::SetSubagentConcurrency { value });
    }
}

/// Parse `30s`, `10m`, `2h`, `1h30m`, `1d`, or a bare number of seconds.
pub(super) fn parse_duration(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut total = 0u64;
    let mut number = 0u64;
    let mut digits = false;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            number = number
                .saturating_mul(10)
                .saturating_add(u64::from(ch as u8 - b'0'));
            digits = true;
            continue;
        }
        let scale = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            _ => return None,
        };
        if !digits {
            return None;
        }
        total = total.saturating_add(number.saturating_mul(scale));
        number = 0;
        digits = false;
    }
    if digits {
        total = total.saturating_add(number);
    }
    (total > 0).then_some(total)
}

/// Parse a one-shot time: `+30m` (relative), `30m` (relative) or `HH:MM`
/// (today, or tomorrow when that time already passed).
pub(super) fn parse_schedule_at(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('+') {
        let seconds = parse_duration(rest)?;
        return Some(eix_core::now_ms() + seconds as i64 * 1_000);
    }
    if let Some((hour, minute)) = text.split_once(':') {
        let hour: u32 = hour.parse().ok()?;
        let minute: u32 = minute.parse().ok()?;
        if hour > 23 || minute > 59 {
            return None;
        }
        let now = chrono::Local::now();
        let naive = now.date_naive().and_hms_opt(hour, minute, 0)?;
        let mut at = chrono::Local.from_local_datetime(&naive).single()?;
        if at <= now {
            at += chrono::Duration::days(1);
        }
        return Some(at.timestamp_millis());
    }
    let seconds = parse_duration(text)?;
    Some(eix_core::now_ms() + seconds as i64 * 1_000)
}

impl App {
    /// Re-probe the terminal with the current `[math]` settings and drop the
    /// rendered transcripts so every visible pane picks up the new mode.
    fn apply_math_mode(&mut self, reload_config: bool) {
        if reload_config {
            if let Ok(loaded) = eix_core::Config::load(self.config_path.as_deref()) {
                self.math_config = loaded.math;
            }
        }
        let enabled = super::super::math_image::reload(&self.math_config);
        for view in &mut self.views {
            view.history_cache = None;
            view.tail_key = None;
            view.text_lines_valid = false;
        }
        self.force_clear = true;
        let status = super::super::math_image::status();
        self.status = if super::super::math_image::raw_source() {
            "math: off (raw TeX)".into()
        } else if enabled {
            format!("math: images {} @ {} dpi", status.protocol, status.dpi)
        } else {
            format!(
                "math: ASCII fallback ({})",
                status
                    .reason
                    .as_deref()
                    .unwrap_or("no graphics protocol detected")
            )
        };
    }
}
