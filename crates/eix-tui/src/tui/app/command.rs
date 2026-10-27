//! Slash command execution.

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
            "keys" => {
                self.open_keys_help(args);
                true
            }
            "new" => {
                let name = (!args.is_empty()).then(|| args.to_string());
                self.new_session(tx, name);
                true
            }
            "new-pane" => {
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
            "attention" => {
                self.focus_next_attention();
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
            "btw" => {
                self.command_btw(args, tx);
                true
            }
            "init" => {
                self.command_init();
                true
            }
            "queue" => {
                self.command_queue(args);
                true
            }
            "sessions" => {
                self.open_picker(PickerKind::Sessions, tx);
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
                    self.open_picker(PickerKind::Subagents, tx);
                } else {
                    match args.parse::<usize>() {
                        Ok(value) => self.set_subagent_concurrency(value, tx),
                        Err(_) => {
                            self.error = Some("usage: /subagents [2|4|8|16|32|64|128|256]".into());
                        }
                    }
                }
                true
            }
            "mcp" => {
                self.open_picker(PickerKind::Mcp, tx);
                true
            }
            "reload" => {
                let _ = tx.send(Command::ReloadConfig);
                self.status = "reloading config…".into();
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
                if self.pending_delete.as_deref() == Some(session_id.as_str()) {
                    self.pending_delete = None;
                    let _ = tx.send(Command::DeleteSession { session_id });
                    self.status = "deleting…".into();
                } else {
                    self.pending_delete = Some(session_id);
                    self.status = "run /delete again to delete this session permanently".into();
                }
                true
            }
            "image" => {
                if args == "clear" {
                    self.pending_images.clear();
                    self.status = "images cleared".into();
                } else if args.is_empty() {
                    self.status = if self.pending_images.is_empty() {
                        "no images attached".into()
                    } else {
                        format!("attached: {}", self.pending_images.join(", "))
                    };
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
                        self.input
                            .set_text(&eix_core::expand_command(&skill.body, arguments));
                        self.input.enter_insert();
                        self.focus = Focus::Input;
                        self.status = format!("inserted skill {}", skill.name);
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
            "perm" => {
                self.command_mode(args, tx);
                true
            }
            "fork" => {
                let name = (!args.is_empty()).then(|| args.to_string());
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::Fork {
                        session_id,
                        at_seq: None,
                        name,
                    });
                    self.status = "forking…".into();
                }
                true
            }
            _ => {
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
        self.status = "clearing context…".into();
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

    pub(super) fn command_queue(&mut self, args: &str) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        let mut parts = args.split_whitespace();
        match parts.next() {
            Some("clear") => {
                let dropped = self
                    .queues
                    .remove(&session_id)
                    .map_or(0, |queue| queue.len());
                self.status = format!("cleared {dropped} queued prompt(s)");
                return;
            }
            Some("rm" | "drop") => {
                let index = match parts.next().and_then(|value| value.parse::<usize>().ok()) {
                    Some(index) if index > 0 => index - 1,
                    _ => {
                        self.error = Some("usage: /queue rm <1-based index>".into());
                        return;
                    }
                };
                let removed = self
                    .queues
                    .get_mut(&session_id)
                    .and_then(|queue| (index < queue.len()).then(|| queue.remove(index)).flatten());
                self.status = match removed {
                    Some(prompt) => format!("dropped queued prompt: {}", truncate(&prompt, 60)),
                    None => "no queued prompt at that index".into(),
                };
                return;
            }
            Some("move") => {
                let from = parts.next().and_then(|value| value.parse::<usize>().ok());
                let to = parts.next().and_then(|value| value.parse::<usize>().ok());
                let (Some(from), Some(to)) = (from, to) else {
                    self.error = Some("usage: /queue move <from> <to>".into());
                    return;
                };
                if from == 0 || to == 0 {
                    self.error = Some("queue indices are 1-based".into());
                    return;
                }
                let moved = self.queues.get_mut(&session_id).and_then(|queue| {
                    if from > queue.len() || to > queue.len() {
                        return None;
                    }
                    let prompt = queue.remove(from - 1)?;
                    queue.insert(to - 1, prompt);
                    Some(())
                });
                self.status = if moved.is_some() {
                    format!("moved queued prompt {from} → {to}")
                } else {
                    "queue index out of range".into()
                };
                return;
            }
            _ => {}
        }
        let Some(queue) = self.queues.get(&session_id) else {
            self.status = "no queued prompts".into();
            return;
        };
        if queue.is_empty() {
            self.status = "no queued prompts".into();
            return;
        }
        let preview: Vec<String> = queue
            .iter()
            .take(3)
            .enumerate()
            .map(|(index, prompt)| format!("{}: {}", index + 1, truncate(prompt, 40)))
            .collect();
        self.status = format!(
            "{} queued · {} · /queue rm N or /queue move N M",
            queue.len(),
            preview.join(" · ")
        );
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
                self.error = Some("usage: /perm plan | build | auto | bypass".into());
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
