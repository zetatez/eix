use super::*;

use crate::tui::workspace::{ActiveWorkspace, WorkspaceLayout};

#[test]
fn paste_recognizes_image_paths_and_file_uris() {
    let dir = std::env::temp_dir().join(format!(
        "eix-paste-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let first = dir.join("shot one.png");
    let second = dir.join("two.jpg");
    std::fs::write(&first, b"png").unwrap();
    std::fs::write(&second, b"jpg").unwrap();

    assert_eq!(
        parse_image_paths("shot one.png", &dir),
        Some(vec!["shot one.png".to_string()])
    );
    assert_eq!(
        parse_image_paths("\"shot one.png\" 'two.jpg'", &dir),
        Some(vec!["shot one.png".to_string(), "two.jpg".to_string()])
    );
    let uri = format!("file://{}", first.to_string_lossy().replace(' ', "%20"));
    assert_eq!(
        parse_image_paths(&uri, &dir),
        Some(vec![first.to_string_lossy().into_owned()])
    );
    assert_eq!(parse_image_paths("hello world", &dir), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wrapping_respects_display_width() {
    assert_eq!(wrap_text("abcdef", 3), vec!["abc", "def"]);
    assert_eq!(wrap_text("a\n\nb", 10), vec!["a", "", "b"]);
    assert_eq!(wrap_text("世界", 2), vec!["世", "界"]);
}

#[test]
fn truncate_uses_cell_width() {
    assert_eq!(truncate("abcdef", 4), "abc…");
    assert_eq!(truncate("世界abc", 5), "世界…");
    assert_eq!(truncate("abc", 10), "abc");
}

#[test]
fn fuzzy_matches_subsequences() {
    assert!(fuzzy_score("ses", "session").is_some());
    assert!(fuzzy_score("xyz", "session").is_none());
    assert!(fuzzy_score("", "anything").is_some());
}

#[test]
fn app_starts_in_normal_mode() {
    let mut app = App::new(None);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    assert_eq!(app.input.mode(), Mode::Normal);

    app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &tx);
    assert_eq!(app.input.mode(), Mode::Insert);
}

#[test]
fn startup_attaches_the_session_for_the_launch_directory() {
    let cwd = std::env::current_dir().expect("cwd");
    let mut app = App::new(None);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let other = Session::new("other", "other", "/tmp/eix-other-workspace");
    let here = Session::new("here", "here", cwd);
    app.handle_event(Event::Sessions(vec![other, here]), &tx);

    assert_eq!(app.selected, 1);
    assert!(matches!(rx.try_recv(), Ok(Command::Attach(id)) if id == "here"));
}

#[test]
fn startup_restores_saved_panes_layout_and_focus() {
    let mut app = App::new(None);
    app.restore_workspace(ActiveWorkspace {
        sessions: vec!["a".into(), "b".into()],
        focused: Some("a".into()),
        layout: WorkspaceLayout::Fibonacci,
    });
    assert_eq!(app.layout, PaneLayout::Fibonacci);
    assert_eq!(app.workspace_state().layout, WorkspaceLayout::Fibonacci);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sessions = vec![
        Session::new("a", "a", "/tmp/eix-workspace"),
        Session::new("b", "b", "/tmp/eix-workspace"),
    ];
    app.handle_event(Event::Sessions(sessions.clone()), &tx);
    assert!(matches!(rx.try_recv(), Ok(Command::Attach(id)) if id == "a"));

    app.handle_event(
        Event::Attached(AttachResult {
            session: sessions[0].clone(),
            events: Vec::new(),
            has_more: false,
        }),
        &tx,
    );
    assert!(matches!(rx.try_recv(), Ok(Command::Attach(id)) if id == "b"));

    app.handle_event(
        Event::Attached(AttachResult {
            session: sessions[1].clone(),
            events: Vec::new(),
            has_more: false,
        }),
        &tx,
    );

    assert_eq!(view_order(&app), vec!["a", "b"]);
    assert_eq!(app.focused, 0);
    assert_eq!(app.selected, 0);
}

#[test]
fn startup_falls_back_to_the_latest_workspace_session_for_stale_restore_ids() {
    let cwd = std::env::current_dir().expect("cwd");
    let mut app = App::new(None);
    app.restore_workspace(ActiveWorkspace {
        sessions: vec!["deleted".into()],
        focused: Some("deleted".into()),
        layout: WorkspaceLayout::Tabby,
    });
    assert_eq!(app.layout, PaneLayout::Tabby);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sessions = vec![
        Session::new("newest", "newest", cwd.clone()),
        Session::new("older", "older", cwd),
    ];
    app.handle_event(Event::Sessions(sessions), &tx);

    assert!(matches!(rx.try_recv(), Ok(Command::Attach(id)) if id == "newest"));
    assert!(!matches!(rx.try_recv(), Ok(Command::Create { .. })));
}

#[test]
fn explicit_session_ignores_saved_restore_state() {
    let cwd = std::env::current_dir().expect("cwd");
    let mut app = App::new(Some("explicit".into()));
    app.restore_workspace(ActiveWorkspace {
        sessions: vec!["saved".into()],
        focused: Some("saved".into()),
        layout: WorkspaceLayout::Horizontal,
    });
    assert_eq!(app.layout, PaneLayout::Grid);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sessions = vec![
        Session::new("explicit", "explicit", cwd.clone()),
        Session::new("saved", "saved", cwd),
    ];
    app.handle_event(Event::Sessions(sessions), &tx);

    assert!(matches!(rx.try_recv(), Ok(Command::Attach(id)) if id == "explicit"));
    assert!(app.restore_queue.is_empty());
    assert!(app.restore_focused.is_none());
}

#[test]
fn startup_creates_a_session_when_the_launch_directory_has_none() {
    let mut app = App::new(None);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.handle_event(
        Event::Sessions(vec![Session::new(
            "other",
            "other",
            "/tmp/eix-other-workspace",
        )]),
        &tx,
    );

    assert!(matches!(
        rx.try_recv(),
        Ok(Command::Create {
            worktree: None,
            cwd: None,
            ..
        })
    ));
}

#[test]
fn adjacent_panes_share_one_border() {
    let area = Rect::new(0, 0, 100, 40);
    let two = pane_rects(area, 2);
    assert!(pane_borders(&two, 0).contains(Borders::RIGHT));
    assert!(!pane_borders(&two, 1).contains(Borders::LEFT));

    let stacked = vec![Rect::new(0, 0, 20, 10), Rect::new(0, 10, 20, 10)];
    assert!(!pane_borders(&stacked, 0).contains(Borders::BOTTOM));
    assert!(!pane_borders(&stacked, 1).contains(Borders::BOTTOM));
    assert!(pane_borders(&stacked, 1).contains(Borders::TOP));
}

#[test]
fn timeline_picker_lists_only_user_inputs_and_jumps() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let events = vec![
        SessionEvent::new(
            "s1",
            1,
            EventKind::UserMessage,
            serde_json::json!({"text": "first request"}),
        ),
        SessionEvent::new(
            "s1",
            2,
            EventKind::AssistantMessage,
            serde_json::json!({"text": "first answer"}),
        ),
        SessionEvent::new(
            "s1",
            3,
            EventKind::UserMessage,
            serde_json::json!({"text": "second request"}),
        ),
    ];
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "timeline", "/tmp"),
        events,
    ));
    app.focused = 0;
    let _ = render_text(&mut app, 80, 20);
    let line = app.views[0]
        .user_lines
        .iter()
        .find(|(seq, _)| *seq == 3)
        .map(|(_, line)| *line)
        .expect("user line");

    app.open_picker(PickerKind::Timeline, &tx);
    let picker = app.picker.as_ref().expect("timeline picker");
    assert_eq!(picker.kind, PickerKind::Timeline);
    let values: Vec<&str> = picker
        .items
        .iter()
        .map(|item| item.value.as_str())
        .collect();
    assert_eq!(values, ["3", "1"]);
    assert_eq!(picker.selected, 0);
    assert!(picker.items[0].label.contains(':'));
    assert!(picker.items[0]
        .preview
        .as_deref()
        .is_some_and(|preview| preview.contains("second request")));

    app.picker_enter(PickerKind::Timeline, &tx);
    assert!(app.picker.is_none());
    assert!(!app.views[0].follow);
    assert_eq!(app.views[0].scroll, line.saturating_sub(2));
    assert!(app.status.contains("jumped to e3"));
}

#[test]
fn timeline_picker_is_isolated_per_session() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        vec![SessionEvent::new(
            "s1",
            1,
            EventKind::UserMessage,
            serde_json::json!({"text": "first session"}),
        )],
    ));
    app.views.push(SessionView::new(
        Session::new("s2", "two", "/tmp"),
        vec![
            SessionEvent::new(
                "s2",
                1,
                EventKind::UserMessage,
                serde_json::json!({"text": "second session A"}),
            ),
            SessionEvent::new(
                "s2",
                2,
                EventKind::UserMessage,
                serde_json::json!({"text": "second session B"}),
            ),
        ],
    ));
    app.focused = 1;

    app.open_picker(PickerKind::Timeline, &tx);
    let picker = app.picker.as_ref().expect("timeline picker");
    let previews: Vec<&str> = picker
        .items
        .iter()
        .filter_map(|item| item.preview.as_deref())
        .collect();
    assert_eq!(previews.len(), 2);
    assert!(previews
        .iter()
        .all(|text| text.starts_with("second session")));
    assert!(previews.iter().all(|text| !text.contains("first session")));
}

#[test]
fn leader_h_opens_the_timeline_picker() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let events = vec![SessionEvent::new(
        "s1",
        1,
        EventKind::UserMessage,
        serde_json::json!({"text": "hello"}),
    )];
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "timeline", "/tmp"),
        events,
    ));
    app.focused = 0;

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &tx);
    let picker = app.picker.as_ref().expect("timeline picker");
    assert_eq!(picker.kind, PickerKind::Timeline);
    assert_eq!(picker.items.len(), 1);
}

#[test]
fn leader_g_opens_the_session_picker() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &tx);
    let picker = app.picker.as_ref().expect("session picker");
    assert_eq!(picker.kind, PickerKind::Sessions);
}

#[test]
fn split_panes_render_both_titles() {
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "left pane", "/tmp/left"),
        Vec::new(),
    ));
    app.views.push(SessionView::new(
        Session::new("s2", "right pane", "/tmp/right"),
        Vec::new(),
    ));
    app.connected = true;

    let rendered = render_text(&mut app, 80, 20);
    assert!(rendered.contains("left pane"), "{rendered}");
    assert!(rendered.contains("right pane"), "{rendered}");
}

#[test]
fn focused_pane_border_uses_the_accent_color() {
    let mut app = app_with_panes(&["a", "b"]);
    app.focused = 0;
    let backend = ratatui::backend::TestBackend::new(80, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let left = app.pane_areas[0];
    let right = app.pane_areas[1];
    let focused = &buffer[(left.x, left.y + 2)];
    let inactive = &buffer[(right.x + right.width - 1, right.y + 2)];

    assert_eq!(
        focused.fg,
        composer_focus(app.theme),
        "focused pane border should be brighter"
    );
    assert_eq!(
        focused.symbol(),
        "┊",
        "focused pane border should use the fine dashed line"
    );
    assert_eq!(
        inactive.fg,
        Color::Rgb(48, 52, 62),
        "inactive pane border should stay muted"
    );
    assert_eq!(inactive.symbol(), "│");
}

#[test]
fn pane_layouts_stay_inside_the_area() {
    let area = Rect::new(0, 0, 100, 40);
    for count in 1..=6 {
        let rects = pane_rects(area, count);
        assert_eq!(rects.len(), count);
        for rect in rects {
            assert!(rect.x >= area.x);
            assert!(rect.y >= area.y);
            assert!(rect.x + rect.width <= area.x + area.width);
            assert!(rect.y + rect.height <= area.y + area.height);
        }
    }

    let two = pane_rects(area, 2);
    assert!(two[0].x < two[1].x);
    assert_eq!(two[0].height, area.height);

    let three = pane_rects(area, 3);
    assert_eq!(three[0].height, area.height);
    assert_eq!(three[1].x, three[2].x);
    assert_eq!(three[1].width, three[2].width);
}

#[test]
fn horizontal_layout_gives_the_first_pane_most_height() {
    let area = Rect::new(0, 0, 100, 40);
    let rects = layout_pane_rects(PaneLayout::Horizontal, area, 5);
    assert_eq!(rects.len(), 5);
    assert!(rects[0].height > rects[1].height);
    for rect in &rects {
        assert_eq!(rect.x, area.x);
        assert_eq!(rect.width, area.width);
        assert!(rect.y + rect.height <= area.y + area.height);
    }
    for pair in rects.windows(2) {
        assert_eq!(pair[0].y + pair[0].height, pair[1].y);
    }
}

#[test]
fn horizontal_layout_never_falls_back_to_tabs() {
    let mut app = app_with_panes(&["a", "b", "c", "d", "e"]);
    app.layout = PaneLayout::Horizontal;
    assert_eq!(
        app.effective_layout(Rect::new(0, 0, 80, 16)),
        PaneLayout::Horizontal
    );
    assert_eq!(
        app.effective_layout(Rect::new(0, 0, 80, 40)),
        PaneLayout::Horizontal
    );
}

#[test]
fn horizontal_layout_keeps_every_pane_visible_when_short() {
    for height in [12, 16, 20, 24] {
        let mut app = app_with_panes(&["a", "b", "c", "d", "e"]);
        app.layout = PaneLayout::Horizontal;
        app.focused = 0;
        let rendered = render_text(&mut app, 80, height);
        assert!(
            !rendered.contains("[1 session a]"),
            "horizontal layout fell back to tabs at height {height}:\n{rendered}"
        );
        for id in ["a", "b", "c", "d", "e"] {
            assert!(
                rendered.contains(&format!("session {id}")),
                "pane {id} missing at height {height}:\n{rendered}"
            );
        }
    }
}

#[test]
fn horizontal_layout_shows_head_plus_four_following_panes() {
    let mut app = app_with_panes(&["a", "b", "c", "d", "e", "f", "g", "h"]);
    app.layout = PaneLayout::Horizontal;
    let rendered = render_text(&mut app, 80, 30);
    assert!(!rendered.contains("[1 session a]"), "{rendered}");
    for id in ["a", "b", "c", "d", "e"] {
        assert!(
            rendered.contains(&format!("session {id}")),
            "pane {id} should be visible:\n{rendered}"
        );
    }
    for id in ["f", "g", "h"] {
        assert!(
            !rendered.contains(&format!("session {id}")),
            "pane {id} should be hidden:\n{rendered}"
        );
    }
    assert_eq!(
        app.pane_areas.iter().filter(|rect| rect.height > 0).count(),
        5
    );

    // The first pane keeps the same height as with only five panes.
    let mut five = app_with_panes(&["a", "b", "c", "d", "e"]);
    five.layout = PaneLayout::Horizontal;
    render_text(&mut five, 80, 30);
    assert_eq!(app.pane_areas[0], five.pane_areas[0]);
}

#[test]
fn horizontal_layout_slides_to_a_focused_hidden_pane() {
    let mut app = app_with_panes(&["a", "b", "c", "d", "e", "f", "g", "h"]);
    app.layout = PaneLayout::Horizontal;
    app.focused = 7;
    let rendered = render_text(&mut app, 80, 30);

    // Head plus the four following panes ending at the focused one.
    for id in ["a", "e", "f", "g", "h"] {
        assert!(
            rendered.contains(&format!("session {id}")),
            "pane {id} should be visible:\n{rendered}"
        );
    }
    for id in ["b", "c", "d"] {
        assert!(
            !rendered.contains(&format!("session {id}")),
            "pane {id} should be hidden:\n{rendered}"
        );
    }
    assert!(app.pane_areas[7].height > 0);
    assert_eq!(app.pane_areas[1].height, 0);
}

#[test]
fn fibonacci_layout_spirals_from_the_first_pane() {
    let area = Rect::new(0, 0, 100, 40);
    for count in 1..=7 {
        let rects = layout_pane_rects(PaneLayout::Fibonacci, area, count);
        assert_eq!(rects.len(), count);
        for rect in &rects {
            assert!(rect.x >= area.x);
            assert!(rect.y >= area.y);
            assert!(rect.x + rect.width <= area.x + area.width);
            assert!(rect.y + rect.height <= area.y + area.height);
        }
        if count > 1 {
            assert_eq!(rects[0].height, area.height);
            assert!(rects[0].width > rects[1].width);
            assert_eq!(rects[1].x, rects[0].x + rects[0].width);
            assert_eq!(rects[1].y, area.y);
        }
        if count >= 4 {
            // The spiral turns around the right edge after the top pane.
            assert!(rects[2].x > rects[3].x);
        }
    }
}

fn app_with_panes(ids: &[&str]) -> App {
    let mut app = App::new(None);
    for id in ids {
        app.views.push(SessionView::new(
            Session::new(*id, format!("session {id}"), "/tmp"),
            Vec::new(),
        ));
    }
    app.connected = true;
    app
}

fn view_order(app: &App) -> Vec<&str> {
    app.views
        .iter()
        .map(|view| view.session.id.as_str())
        .collect()
}

fn type_text(app: &mut App, tx: &UnboundedSender<Command>, text: &str) {
    for ch in text.chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), tx);
    }
}

#[test]
fn sending_and_status_changes_keep_the_session_order() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["a", "b", "c"]);
    app.focused = 0;

    let submitted = SubmitResult {
        session: app.views[2].session.clone(),
        turn_id: "turn-c".into(),
    };
    app.submitted(submitted);
    assert_eq!(view_order(&app), vec!["a", "b", "c"]);
    assert_eq!(app.focused, 0);
    assert_eq!(app.focused_session_id().as_deref(), Some("a"));

    let mut idle = app.views[2].session.clone();
    idle.status = SessionStatus::Idle;
    app.notification(
        eix_core::Notification::new("session.updated", serde_json::to_value(idle).unwrap()),
        &tx,
    );
    assert_eq!(view_order(&app), vec!["a", "b", "c"]);
    assert_eq!(app.focused, 0);
}

#[test]
fn session_updates_do_not_replace_the_focused_draft() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["a", "b"]);
    app.focused = 0;
    app.focus = Focus::Input;
    app.input_session = Some("a".to_string());
    app.input.set_text("draft for a");

    let mut ready = app.views[1].session.clone();
    ready.status = SessionStatus::AwaitingApproval;
    app.notification(
        eix_core::Notification::new("session.updated", serde_json::to_value(ready).unwrap()),
        &tx,
    );
    assert_eq!(view_order(&app), vec!["a", "b"]);
    assert_eq!(app.focused_session_id().as_deref(), Some("a"));
    assert_eq!(app.input.text(), "draft for a");

    // The tick-driven draft sync must keep the text attached to pane 1.
    app.tick(&tx);
    assert_eq!(app.focused_session_id().as_deref(), Some("a"));
    assert_eq!(app.input.text(), "draft for a");
}

#[test]
fn jumping_to_a_session_keeps_the_linked_order() {
    let mut app = app_with_panes(&["a", "b", "c", "d"]);
    app.focused = 0;

    assert!(app.jump_to_session("c"));
    assert_eq!(app.focused, 2);
    assert_eq!(app.focused_session_id().as_deref(), Some("c"));
    assert_eq!(view_order(&app), vec!["a", "b", "c", "d"]);

    app.cycle_pane(1);
    assert_eq!(app.focused_session_id().as_deref(), Some("d"));
    app.cycle_pane(1);
    assert_eq!(app.focused_session_id().as_deref(), Some("a"));
    app.cycle_pane(-1);
    assert_eq!(app.focused_session_id().as_deref(), Some("d"));
    assert_eq!(view_order(&app), vec!["a", "b", "c", "d"]);

    assert!(!app.jump_to_session("missing"));
}

#[test]
fn ctrl_digits_switch_tabby_tabs_in_insert_mode() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["a", "b", "c"]);
    app.layout = PaneLayout::Tabby;
    app.focus_input();
    app.focused = 0;

    app.handle_key(
        KeyEvent::new(KeyCode::Char('3'), KeyModifiers::CONTROL),
        &tx,
    );

    assert_eq!(app.focused, 2);
    assert_eq!(app.views[app.focused].session.id, "c");
    assert_eq!(app.input.mode(), Mode::Insert);
    assert_eq!(app.status, "tab 3/3 focused");

    // The global shortcut also works while the prompt is in normal mode.
    app.input.enter_normal();
    app.handle_key(
        KeyEvent::new(KeyCode::Char('1'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.focused, 0);
    assert_eq!(app.views[app.focused].session.id, "a");
    assert_eq!(app.input.mode(), Mode::Normal);
    assert_eq!(app.status, "tab 1/3 focused");
}

#[test]
fn ctrl_digits_and_leader_letters_switch_panes_and_layouts() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["a", "b", "c"]);
    app.focus = Focus::Transcript;
    app.focused = 0;

    app.handle_key(
        KeyEvent::new(KeyCode::Char('2'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.focused, 1);

    for (leader, expected) in [
        ('v', PaneLayout::Horizontal),
        ('t', PaneLayout::Tabby),
        ('r', PaneLayout::Fibonacci),
        ('l', PaneLayout::Grid),
    ] {
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(
            KeyEvent::new(KeyCode::Char(leader), KeyModifiers::NONE),
            &tx,
        );
        assert_eq!(app.layout, expected, "leader {leader}");
        let toast = app.toasts.last().expect("layout toast");
        assert_eq!(toast.text, format!("layout: {}", expected.label()));
        assert_eq!(toast.tone, ToastTone::Success);
    }
    assert_eq!(app.status, "layout: grid");
    let rendered = render_text(&mut app, 80, 20);
    assert!(
        rendered
            .lines()
            .nth(1)
            .is_some_and(|line| line.contains("layout: grid")),
        "{rendered}"
    );
}

#[test]
fn every_layout_renders_a_three_pane_session_set() {
    for layout in [
        PaneLayout::Grid,
        PaneLayout::Horizontal,
        PaneLayout::Tabby,
        PaneLayout::Fibonacci,
    ] {
        let mut app = app_with_panes(&["a", "b", "c"]);
        app.layout = layout;
        app.focused = 0;
        let rendered = render_text(&mut app, 80, 30);
        assert!(
            rendered.contains("session a"),
            "{layout:?} lost the first pane:\n{rendered}"
        );
    }
}

#[test]
fn tab_layout_renders_all_tabs_but_only_the_active_pane() {
    let mut app = app_with_panes(&["a", "b", "c"]);
    app.layout = PaneLayout::Tabby;
    app.focused = 1;

    let rendered = render_text(&mut app, 80, 20);
    assert!(rendered.contains("session a"), "{rendered}");
    assert!(rendered.contains("session b"), "{rendered}");
    assert!(rendered.contains("session c"), "{rendered}");
    // The active tab is bracketed; inactive tabs are not separate panes.
    assert!(rendered.contains("[2 session b]"), "{rendered}");

    // Tabs sit directly above the prompt box; the session banner lives in
    // the statusline on the final row.
    let lines: Vec<&str> = rendered.lines().collect();
    let tab_row = lines
        .iter()
        .position(|line| line.contains("[2 session b]"))
        .expect("tab strip");
    let prompt_top = tab_row + 1;
    assert!(lines[prompt_top].contains('╭'), "{rendered}");
    assert!(status_bar_row(&rendered).contains("Build  ·"), "{rendered}");
}

#[test]
fn fibonacci_layout_shows_every_pane_without_a_tab_strip() {
    let mut app = app_with_panes(&["a", "b", "c"]);
    app.layout = PaneLayout::Fibonacci;
    app.focused = 1;

    let rendered = render_text(&mut app, 80, 24);
    assert!(rendered.contains("session a"), "{rendered}");
    assert!(rendered.contains("session b"), "{rendered}");
    assert!(rendered.contains("session c"), "{rendered}");
    assert!(!rendered.contains("[2 session b]"), "{rendered}");
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn render_transcript_at(view: &SessionView, width: usize, tick: u64) -> String {
    let live = view.stream.as_ref().map(|stream| transcript::Live {
        reasoning: &stream.reasoning,
        text: &stream.text,
    });
    transcript::render_with_subagents(
        &view.events,
        live,
        &[],
        &view.folds,
        width,
        Theme::default(),
        tick,
        &view.subagents,
    )
    .lines
    .iter()
    .map(line_text)
    .collect::<Vec<_>>()
    .join("\n")
}

fn render_transcript(view: &SessionView, width: usize) -> String {
    render_transcript_at(view, width, 0)
}

#[test]
fn transcript_renders_tool_cards() {
    let session = Session::new("s1", "tools", "/tmp");
    let events = vec![
        SessionEvent::new(
            "s1",
            1,
            EventKind::ToolCall,
            serde_json::json!({
                "call_id": "c1",
                "tool": "write_file",
                "arguments": {"path": "out.txt", "content": "hello"},
            }),
        ),
        SessionEvent::new(
            "s1",
            2,
            EventKind::ToolResult,
            serde_json::json!({
                "call_id": "c1",
                "tool": "write_file",
                "ok": true,
                "denied": false,
                "output": "wrote 5 bytes to out.txt",
            }),
        ),
    ];
    let mut view = SessionView::new(session, events);

    // File writes stay open by default so their diff/content is visible.
    // The status still sits directly below the summary, not right-aligned.
    let expanded = render_transcript(&view, 60);
    let lines: Vec<&str> = expanded.lines().collect();
    let summary_line = lines
        .iter()
        .position(|line| line.contains("write out.txt (+5 bytes)"))
        .expect("summary line");
    assert!(!lines[summary_line].contains("ok"), "{expanded}");
    assert_eq!(lines[summary_line + 1].trim(), "ok", "{expanded}");
    assert!(lines[summary_line + 1].starts_with("      "), "{expanded}");
    assert!(expanded.contains("wrote 5 bytes to out.txt"));
    assert!(expanded.contains("hello"));
    assert!(!expanded.contains("output"), "{expanded}");
    assert!(!expanded.contains("content"), "{expanded}");

    // The explicit fold still collapses it.
    view.folds.insert(FoldId::Event(1), false);
    let collapsed = render_transcript(&view, 60);
    assert!(!collapsed.contains("wrote 5 bytes to out.txt"));
    assert!(!collapsed.contains("hello"));
}

#[test]
fn edit_and_patch_cards_open_by_default() {
    let session = Session::new("s1", "tools", "/tmp");
    let events = vec![
        SessionEvent::new(
            "s1",
            1,
            EventKind::ToolCall,
            serde_json::json!({
                "call_id": "c1",
                "tool": "edit",
                "arguments": {
                    "path": "src/lib.rs",
                    "oldString": "old edit",
                    "newString": "new edit",
                },
            }),
        ),
        SessionEvent::new(
            "s1",
            2,
            EventKind::PatchProposed,
            serde_json::json!({
                "patch_id": "p1",
                "call_id": "c1",
                "tool": "edit",
                "path": "src/lib.rs",
                "summary": "edit src/lib.rs",
                "diff": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old edit\n+new edit\n",
            }),
        ),
        SessionEvent::new(
            "s1",
            3,
            EventKind::ToolResult,
            serde_json::json!({
                "call_id": "c1",
                "tool": "edit",
                "ok": true,
                "denied": false,
                "output": "edited src/lib.rs",
            }),
        ),
        SessionEvent::new(
            "s1",
            4,
            EventKind::ToolCall,
            serde_json::json!({
                "call_id": "c2",
                "tool": "patch",
                "arguments": {"patchText": "raw patch text"},
            }),
        ),
        SessionEvent::new(
            "s1",
            5,
            EventKind::PatchProposed,
            serde_json::json!({
                "patch_id": "p2",
                "call_id": "c2",
                "tool": "patch",
                "path": "src/one.rs",
                "summary": "patch (2 files)",
                "diff": "--- a/src/one.rs\n+++ b/src/one.rs\n@@ -1 +1 @@\n-old one\n+new one\n--- a/src/two.rs\n+++ b/src/two.rs\n@@ -1 +1 @@\n-old two\n+new two\n",
            }),
        ),
        SessionEvent::new(
            "s1",
            6,
            EventKind::ToolResult,
            serde_json::json!({
                "call_id": "c2",
                "tool": "patch",
                "ok": true,
                "denied": false,
                "output": "applied patch",
            }),
        ),
    ];
    let view = SessionView::new(session, events);
    let rendered = render_transcript(&view, 80);
    assert!(rendered.contains("new edit"), "{rendered}");
    assert!(rendered.contains("new one"), "{rendered}");
    assert!(rendered.contains("new two"), "{rendered}");
    assert!(rendered.contains("── src/one.rs"), "{rendered}");
    assert!(rendered.contains("── src/two.rs"), "{rendered}");
    // The raw edit/patch arguments are not dumped next to the diff.
    assert!(!rendered.contains("oldString"), "{rendered}");
    assert!(!rendered.contains("patchText"), "{rendered}");
}

#[test]
fn auto_applied_write_renders_its_diff() {
    let session = Session::new("s1", "tools", "/tmp");
    let events = vec![
        SessionEvent::new(
            "s1",
            1,
            EventKind::ToolCall,
            serde_json::json!({
                "call_id": "c1",
                "tool": "write_file",
                "arguments": {"path": "out.txt", "content": "new line\n"},
            }),
        ),
        SessionEvent::new(
            "s1",
            2,
            EventKind::PatchApplied,
            serde_json::json!({
                "patch_id": "p1",
                "call_id": "c1",
                "path": "out.txt",
                "applied": true,
                "conflict": false,
                "diff": "--- a/out.txt\n+++ b/out.txt\n@@ -1 +1 @@\n-old line\n+new line\n",
            }),
        ),
        SessionEvent::new(
            "s1",
            3,
            EventKind::ToolResult,
            serde_json::json!({
                "call_id": "c1",
                "tool": "write_file",
                "ok": true,
                "denied": false,
                "output": "wrote 9 bytes to out.txt",
            }),
        ),
    ];
    let view = SessionView::new(session, events);
    let rendered = render_transcript(&view, 80);
    assert!(rendered.contains("old line"), "{rendered}");
    assert!(rendered.contains("new line"), "{rendered}");
    assert!(rendered.contains("── out.txt"), "{rendered}");
    assert!(rendered.contains('│'), "{rendered}");
}

#[test]
fn applied_patch_diff_replaces_the_staged_preview() {
    let session = Session::new("s1", "tools", "/tmp");
    let events = vec![
        SessionEvent::new(
            "s1",
            1,
            EventKind::ToolCall,
            serde_json::json!({
                "call_id": "c1",
                "tool": "write_file",
                "arguments": {"path": "out.rs", "content": "fn raw(){}\n"},
            }),
        ),
        SessionEvent::new(
            "s1",
            2,
            EventKind::PatchProposed,
            serde_json::json!({
                "patch_id": "p1",
                "call_id": "c1",
                "tool": "write_file",
                "path": "out.rs",
                "summary": "write out.rs",
                "diff": "--- a/out.rs\n+++ b/out.rs\n@@ -1 +1 @@\n-fn old(){}\n+fn raw(){}\n",
            }),
        ),
        SessionEvent::new(
            "s1",
            3,
            EventKind::PatchApplied,
            serde_json::json!({
                "patch_id": "p1",
                "call_id": "c1",
                "path": "out.rs",
                "applied": true,
                "conflict": false,
                "diff": "--- a/out.rs\n+++ b/out.rs\n@@ -1 +1,3 @@\n-fn old(){}\n+fn raw() {}\n+\n+// formatted\n",
            }),
        ),
        SessionEvent::new(
            "s1",
            4,
            EventKind::ToolResult,
            serde_json::json!({
                "call_id": "c1",
                "tool": "write_file",
                "ok": true,
                "denied": false,
                "output": "wrote out.rs",
            }),
        ),
    ];
    let view = SessionView::new(session, events);
    let rendered = render_transcript(&view, 80);
    // The applied event carries the final, formatted bytes; the staged
    // preview must not keep showing the pre-format content.
    assert!(rendered.contains("// formatted"), "{rendered}");
    assert!(!rendered.contains("+fn raw(){}"), "{rendered}");
}

fn parent_spawn_events() -> Vec<SessionEvent> {
    vec![SessionEvent::new(
        "parent",
        1,
        EventKind::ToolCall,
        serde_json::json!({
            "call_id": "call-1",
            "tool": "spawn_agent",
            "arguments": {"name": "scout", "prompt": "inspect the parser"},
        }),
    )]
}

#[test]
fn subagent_card_has_a_fixed_live_window_and_folds() {
    let session = Session::new("parent", "parent", "/tmp");
    let mut view = SessionView::new(session, parent_spawn_events());
    view.upsert_subagent("child-1".into(), "call-1".into(), "parent/scout-1".into());
    view.push_subagent_event(
        "child-1",
        SessionEvent::new(
            "child-1",
            1,
            EventKind::UserMessage,
            serde_json::json!({"text": "inspect the parser"}),
        ),
    );
    view.push_subagent_event(
        "child-1",
        SessionEvent::new(
            "child-1",
            2,
            EventKind::ToolCall,
            serde_json::json!({
                "call_id": "c2",
                "tool": "read_file",
                "arguments": {"path": "src/parser.rs"},
            }),
        ),
    );
    view.push_subagent_event(
        "child-1",
        SessionEvent::new(
            "child-1",
            3,
            EventKind::ToolResult,
            serde_json::json!({
                "call_id": "c2",
                "tool": "read_file",
                "ok": true,
                "denied": false,
                "output": "fn parse() {}",
            }),
        ),
    );

    let rendered = render_transcript(&view, 60);
    assert!(
        rendered.contains("↳ parent/scout-1 · running"),
        "{rendered}"
    );
    assert!(rendered.contains("▶ inspect the parser"), "{rendered}");
    assert!(rendered.contains("⚙ read src/parser.rs"), "{rendered}");
    assert!(
        rendered.contains("✓ read_file: fn parse() {}"),
        "{rendered}"
    );
    // Five fixed rows between the window borders.
    assert_eq!(rendered.matches("│ ").count(), 5, "{rendered}");
    assert!(
        rendered.contains('╭') && rendered.contains('╰'),
        "{rendered}"
    );

    view.folds.insert(FoldId::Event(1), false);
    let folded = render_transcript(&view, 60);
    assert!(folded.contains("↳ parent/scout-1 · running"), "{folded}");
    assert!(!folded.contains("read src/parser.rs"), "{folded}");
    assert!(!folded.contains('╭'), "{folded}");
}

#[test]
fn completed_subagent_shows_only_its_result() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("parent", "parent", "/tmp"),
        parent_spawn_events(),
    ));
    app.handle_event(
        Event::Notification(eix_core::Notification::new(
            "agent.spawned",
            serde_json::json!({
                "session_id": "parent",
                "agent_id": "child-1",
                "call_id": "call-1",
                "name": "parent/scout-1",
            }),
        )),
        &tx,
    );
    app.handle_event(
        Event::Notification(eix_core::Notification::new(
            "session.event",
            serde_json::to_value(SessionEvent::new(
                "child-1",
                1,
                EventKind::ToolCall,
                serde_json::json!({
                    "call_id": "c2",
                    "tool": "read_file",
                    "arguments": {"path": "src/parser.rs"},
                }),
            ))
            .unwrap(),
        )),
        &tx,
    );
    let running = render_transcript(&app.views[0], 60);
    assert!(running.contains("⚙ read src/parser.rs"), "{running}");

    app.handle_event(
        Event::Notification(eix_core::Notification::new(
            "agent.completed",
            serde_json::json!({
                "session_id": "parent",
                "agent_id": "child-1",
                "call_id": "call-1",
                "name": "parent/scout-1",
                "ok": true,
                "result": "the parser is fine",
            }),
        )),
        &tx,
    );
    let done = render_transcript(&app.views[0], 60);
    assert!(done.contains("↳ parent/scout-1 · done"), "{done}");
    assert!(done.contains("the parser is fine"), "{done}");
    assert!(!done.contains("read src/parser.rs"), "{done}");
    assert!(!done.contains('╭'), "{done}");
}

#[test]
fn failed_subagent_shows_only_the_error() {
    let session = Session::new("parent", "parent", "/tmp");
    let mut view = SessionView::new(session, parent_spawn_events());
    view.upsert_subagent("child-1".into(), "call-1".into(), "parent/scout-1".into());
    view.push_subagent_event(
        "child-1",
        SessionEvent::new(
            "child-1",
            1,
            EventKind::ToolCall,
            serde_json::json!({
                "call_id": "c2",
                "tool": "read_file",
                "arguments": {"path": "src/parser.rs"},
            }),
        ),
    );
    view.complete_subagent(
        "child-1",
        "call-1",
        "parent/scout-1",
        false,
        None,
        Some("provider exploded".into()),
    );

    let rendered = render_transcript(&view, 60);
    assert!(rendered.contains("↳ parent/scout-1 · failed"), "{rendered}");
    assert!(rendered.contains("provider exploded"), "{rendered}");
    assert!(!rendered.contains("read src/parser.rs"), "{rendered}");
    assert!(!rendered.contains('╭'), "{rendered}");
}

#[test]
fn expanded_command_omits_redundant_labels_and_gutter() {
    let session = Session::new("s1", "tools", "/tmp");
    let events = vec![
        SessionEvent::new(
            "s1",
            1,
            EventKind::ToolCall,
            serde_json::json!({
                "call_id": "c1",
                "tool": "run_command",
                "arguments": {"command": "echo hi"},
            }),
        ),
        SessionEvent::new(
            "s1",
            2,
            EventKind::ToolResult,
            serde_json::json!({
                "call_id": "c1",
                "tool": "run_command",
                "ok": true,
                "denied": false,
                "output": "hi",
            }),
        ),
    ];
    let mut view = SessionView::new(session, events);
    view.folds.insert(FoldId::Event(1), true);
    let expanded = render_transcript(&view, 60);

    // The header already shows `$ echo hi`; the body is only the output.
    assert!(!expanded.contains("input"), "{expanded}");
    assert!(!expanded.contains("output"), "{expanded}");
    assert!(!expanded.contains('│'), "{expanded}");
    let output = expanded
        .lines()
        .find(|line| line.trim() == "hi")
        .expect("command output line");
    assert_eq!(output, "    hi");
}

#[test]
fn long_command_wraps_in_header_without_repeating_in_body() {
    let session = Session::new("s1", "tools", "/tmp");
    let command = format!("echo {}", "x".repeat(120));
    let events = vec![
        SessionEvent::new(
            "s1",
            1,
            EventKind::ToolCall,
            serde_json::json!({
                "call_id": "c1",
                "tool": "run_command",
                "arguments": {"command": command, "cwd": "/tmp"},
            }),
        ),
        SessionEvent::new(
            "s1",
            2,
            EventKind::ToolResult,
            serde_json::json!({
                "call_id": "c1",
                "tool": "run_command",
                "ok": true,
                "denied": false,
                "output": "done",
            }),
        ),
    ];
    let mut view = SessionView::new(session, events);
    view.folds.insert(FoldId::Event(1), true);
    let expanded = render_transcript(&view, 60);
    let lines: Vec<&str> = expanded.lines().collect();

    // The command is no longer elided and its status sits on the line
    // below the wrapped command instead of at the right edge.
    assert!(lines[0].contains("$ echo"), "{expanded}");
    assert!(!lines[0].contains("ok"), "{expanded}");
    assert!(!expanded.contains('…'), "{expanded}");
    let status_line = lines
        .iter()
        .position(|line| line.trim() == "ok")
        .expect("status line");
    assert!(status_line > 0, "{expanded}");
    assert!(
        lines[status_line - 1].starts_with("      ") && lines[status_line - 1].contains('x'),
        "{expanded}"
    );
    // The command wraps onto continuation lines aligned under the summary.
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("      ") && line.contains('x')),
        "{expanded}"
    );
    // No truncated header plus a full repeated command line in the body.
    assert_eq!(expanded.matches("$ echo").count(), 1, "{expanded}");
    assert!(
        !lines.iter().any(|line| line.starts_with("    $ ")),
        "{expanded}"
    );
    // Every byte of the command is shown exactly once.
    assert_eq!(expanded.matches('x').count(), 120, "{expanded}");
}

#[test]
fn transcript_renders_chat_turns_and_thinking() {
    let session = Session::new("s1", "chat", "/tmp");
    let events = vec![
        SessionEvent::new(
            "s1",
            1,
            EventKind::UserMessage,
            serde_json::json!({"text": "hi there"}),
        ),
        SessionEvent::new(
            "s1",
            2,
            EventKind::AssistantMessage,
            serde_json::json!({
                "text": "hello",
                "model": "m",
                "reasoning": "pondering",
                "cancelled": false,
            }),
        ),
    ];
    let view = SessionView::new(session, events);
    let rendered = render_transcript(&view, 40);
    // No role prefixes; user turns sit right and agent turns sit left.
    assert!(!rendered.contains("you>"));
    assert!(!rendered.contains("eix>"));
    let lines: Vec<&str> = rendered.lines().collect();
    let user = lines.iter().find(|line| line.contains("hi there")).unwrap();
    assert!(
        user.starts_with(' ') && user.contains("│ hi there │"),
        "user turn should be a right-aligned bubble: {user:?}"
    );
    assert!(lines.iter().any(|line| line.contains("▾ Thinking")));
    // Stored thinking is expanded by default.
    assert!(rendered.contains("pondering"));
    let mut view = view;
    view.folds.insert(FoldId::Event(2), false);
    assert!(!render_transcript(&view, 40).contains("pondering"));
}

#[test]
fn user_messages_render_in_a_bubble() {
    let session = Session::new("s1", "chat", "/tmp");
    let events = vec![SessionEvent::new(
        "s1",
        1,
        EventKind::UserMessage,
        serde_json::json!({"text": "hi there"}),
    )];
    let view = SessionView::new(session, events);
    let rendered = render_transcript(&view, 40);
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.len(), 3, "{rendered}");
    assert!(rendered.contains("╭"), "{rendered}");
    assert!(rendered.contains("│ hi there │"), "{rendered}");
    assert!(rendered.contains("╰"), "{rendered}");
    assert!(
        lines.iter().all(|line| UnicodeWidthStr::width(*line) == 38),
        "{rendered:?}"
    );
    assert!(!rendered.contains("you>"), "{rendered}");
    assert!(!rendered.contains("eix>"), "{rendered}");
}

#[test]
fn transcript_marks_context_events() {
    let session = Session::new("s1", "ctx", "/tmp");
    let events = vec![
        SessionEvent::new(
            "s1",
            1,
            EventKind::UserMessage,
            serde_json::json!({ "text": "old question" }),
        ),
        SessionEvent::new(
            "s1",
            2,
            EventKind::ContextCompacted,
            serde_json::json!({
                "from_seq": 1,
                "to_seq": 1,
                "messages": 1,
                "tokens": 10,
                "summary": "we talked",
            }),
        ),
        SessionEvent::new("s1", 3, EventKind::ContextCleared, serde_json::json!({})),
    ];
    let view = SessionView::new(session, events);
    let rendered = render_transcript(&view, 60);
    assert!(rendered.contains("context compacted · 1 message(s)"));
    assert!(rendered.contains("we talked"));
    assert!(rendered.contains("context cleared"));
    // Clearing context never removes the transcript history.
    assert!(rendered.contains("old question"));
}

#[test]
fn automatic_compaction_keeps_the_summary_compact() {
    let session = Session::new("s1", "ctx", "/tmp");
    let events = vec![SessionEvent::new(
        "s1",
        1,
        EventKind::ContextCompacted,
        serde_json::json!({
            "from_seq": 1,
            "to_seq": 1,
            "messages": 1,
            "tokens": 10,
            "summary": "internal rolling summary",
            "rolling": true,
        }),
    )];
    let view = SessionView::new(session, events);
    let rendered = render_transcript(&view, 60);
    assert!(rendered.contains("context compacted · 1 message(s)"));
    assert!(!rendered.contains("internal rolling summary"));
}

#[test]
fn transcript_hides_mode_changes() {
    let session = Session::new("s1", "modes", "/tmp");
    let events = vec![
        SessionEvent::new(
            "s1",
            1,
            EventKind::UserMessage,
            serde_json::json!({ "text": "start in plan" }),
        ),
        SessionEvent::new(
            "s1",
            2,
            EventKind::Mode,
            serde_json::json!({
                "from": "plan",
                "to": "build",
                "temporary": false,
            }),
        ),
        SessionEvent::new(
            "s1",
            3,
            EventKind::AssistantMessage,
            serde_json::json!({
                "text": "now building",
                "model": "m",
                "cancelled": false,
            }),
        ),
    ];
    let view = SessionView::new(session, events);
    let rendered = render_transcript(&view, 60);
    assert!(rendered.contains("start in plan"), "{rendered}");
    assert!(rendered.contains("now building"), "{rendered}");
    assert!(!rendered.contains("mode plan"), "{rendered}");
    assert!(!rendered.contains('◆'), "{rendered}");
}

#[test]
fn transcript_cache_refreshes_after_new_events() {
    let session = Session::new("s1", "cache", "/tmp");
    let events = vec![SessionEvent::new(
        "s1",
        1,
        EventKind::UserMessage,
        serde_json::json!({ "text": "first turn" }),
    )];
    let mut app = App::new(None);
    app.views.push(SessionView::new(session, events));
    app.focused = 0;
    assert!(render_text(&mut app, 60, 12).contains("first turn"));

    app.views[0].events.push(SessionEvent::new(
        "s1",
        2,
        EventKind::UserMessage,
        serde_json::json!({ "text": "second turn" }),
    ));
    app.views[0].events_rev += 1;
    assert!(render_text(&mut app, 60, 12).contains("second turn"));
}

#[test]
fn transcript_aligns_user_right_and_agent_left() {
    let session = Session::new("s1", "align", "/tmp");
    let events = vec![
        SessionEvent::new(
            "s1",
            1,
            EventKind::UserMessage,
            serde_json::json!({ "text": "question" }),
        ),
        SessionEvent::new(
            "s1",
            2,
            EventKind::AssistantMessage,
            serde_json::json!({ "text": "answer", "model": "test" }),
        ),
    ];
    let mut app = App::new(None);
    app.views.push(SessionView::new(session, events));
    app.focused = 0;
    render_text(&mut app, 40, 12);

    let lines = &app.views[0].history_cache.as_ref().unwrap().lines;
    let user = lines
        .iter()
        .find(|line| text_line(line).contains("question"))
        .expect("user line");
    let agent = lines
        .iter()
        .find(|line| text_line(line).contains("answer"))
        .expect("agent line");
    let indent = |line: &Line<'static>| {
        let text = text_line(line);
        text.len().saturating_sub(text.trim_start().len())
    };
    assert!(
        indent(user) > indent(agent),
        "user line should sit right of the agent line: {:?} vs {:?}",
        text_line(user),
        text_line(agent)
    );

    let rendered = render_text(&mut app, 40, 20);
    let user_x = rendered
        .lines()
        .find(|line| line.contains("question"))
        .and_then(|line| line.find("question"))
        .expect("rendered user line");
    let agent_x = rendered
        .lines()
        .find(|line| line.contains("answer"))
        .and_then(|line| line.find("answer"))
        .expect("rendered agent line");
    assert!(
        user_x > agent_x,
        "user text should render right of agent text: {rendered}"
    );
}

#[test]
fn live_tail_renders_over_cached_history() {
    let session = Session::new("s1", "cache", "/tmp");
    let events = vec![SessionEvent::new(
        "s1",
        1,
        EventKind::UserMessage,
        serde_json::json!({ "text": "stored history" }),
    )];
    let mut app = App::new(None);
    app.views.push(SessionView::new(session, events));
    app.focused = 0;
    app.views[0].stream = Some(StreamState {
        turn_id: "t1".into(),
        text: "half an answer".into(),
        reasoning: String::new(),
    });
    assert!(render_text(&mut app, 60, 12).contains("half an answer"));

    app.views[0].stream.as_mut().unwrap().text = "full answer".into();
    assert!(render_text(&mut app, 60, 12).contains("full answer"));
}

#[test]
fn cached_history_honors_fold_changes() {
    let session = Session::new("s1", "cache", "/tmp");
    let events = vec![SessionEvent::new(
        "s1",
        2,
        EventKind::AssistantMessage,
        serde_json::json!({
            "text": "answer",
            "model": "test",
            "reasoning": "hidden chain",
        }),
    )];
    let mut app = App::new(None);
    app.views.push(SessionView::new(session, events));
    app.focused = 0;
    let unfolded = render_text(&mut app, 60, 24);
    assert!(unfolded.contains("hidden chain"), "{unfolded:?}");

    app.views[0].folds.insert(FoldId::Event(2), false);
    let folded = render_text(&mut app, 60, 24);
    assert!(folded.contains("Thinking"));
    assert!(!folded.contains("hidden chain"));
}

#[test]
fn cached_history_renders_only_the_visible_window() {
    let session = Session::new("s1", "cache", "/tmp");
    let events: Vec<_> = (0..20)
        .map(|index| {
            SessionEvent::new(
                "s1",
                index + 1,
                EventKind::UserMessage,
                serde_json::json!({ "text": format!("turn {index}") }),
            )
        })
        .collect();
    let mut app = App::new(None);
    app.views.push(SessionView::new(session, events));
    app.focused = 0;
    app.views[0].follow = false;
    app.views[0].scroll = 0;
    let top = render_text(&mut app, 60, 12);
    assert!(top.contains("turn 0"), "{top}");
    assert!(!top.contains("turn 19"), "{top}");

    app.views[0].follow = true;
    let bottom = render_text(&mut app, 60, 12);
    assert!(bottom.contains("turn 19"), "{bottom}");
}

#[test]
fn history_page_prepends_older_events_and_keeps_scroll() {
    let session = Session::new("s1", "paged", "/tmp");
    let recent: Vec<_> = (10..20)
        .map(|index| {
            SessionEvent::new(
                "s1",
                index + 1,
                EventKind::UserMessage,
                serde_json::json!({ "text": format!("turn {index}") }),
            )
        })
        .collect();
    let older: Vec<_> = (0..10)
        .map(|index| {
            SessionEvent::new(
                "s1",
                index + 1,
                EventKind::UserMessage,
                serde_json::json!({ "text": format!("turn {index}") }),
            )
        })
        .collect();
    let mut app = App::new(None);
    app.views.push(SessionView::new(session, recent));
    app.focused = 0;
    app.views[0].history_has_more = true;
    render_text(&mut app, 60, 12);

    app.views[0].follow = false;
    app.views[0].scroll = 4;
    app.history_page("s1".into(), older, false);
    assert_eq!(app.views[0].events.len(), 20);
    assert_eq!(app.views[0].events[0].seq, 1);
    assert_eq!(app.views[0].events_rev, 1);

    render_text(&mut app, 60, 12);
    assert!(
        app.views[0].scroll > 4,
        "viewport should move by the prepended line count"
    );
    assert!(app.views[0]
        .text_lines
        .iter()
        .any(|line| line.contains("turn 0")));
    assert!(!app.views[0].history_has_more);
    assert!(app.take_history_request().is_none());
}

#[test]
fn history_request_loads_one_page_at_a_time() {
    let session = Session::new("s1", "paged", "/tmp");
    let events: Vec<_> = (0..10)
        .map(|index| {
            SessionEvent::new(
                "s1",
                index + 101,
                EventKind::UserMessage,
                serde_json::json!({ "text": format!("turn {index}") }),
            )
        })
        .collect();
    let mut app = App::new(None);
    app.views.push(SessionView::new(session, events));
    app.focused = 0;
    app.views[0].history_has_more = true;
    app.views[0].follow = false;
    app.views[0].scroll = 0;
    render_text(&mut app, 60, 12);

    let (session_id, before_seq) = app
        .take_history_request()
        .expect("scrolling to the top requests an older page");
    assert_eq!(session_id, "s1");
    assert_eq!(before_seq, 101);
    assert!(app.views[0].history_loading);
    assert!(!app.views[0].history_armed);

    render_text(&mut app, 60, 12);
    assert!(
        app.take_history_request().is_none(),
        "an in-flight page must not be requested twice"
    );

    let older: Vec<_> = (1..=100)
        .map(|seq| {
            SessionEvent::new(
                "s1",
                seq,
                EventKind::UserMessage,
                serde_json::json!({ "text": format!("older {seq}") }),
            )
        })
        .collect();
    app.history_page(session_id, older, false);
    render_text(&mut app, 60, 12);
    assert_eq!(app.views[0].events.len(), 110);
    assert_eq!(app.views[0].events[0].seq, 1);
    assert!(!app.views[0].history_loading);
    assert!(!app.views[0].history_has_more);
    assert!(app.views[0].scroll > 0);
    assert!(app.take_history_request().is_none());
}

#[test]
fn expanded_tool_card_shows_the_full_long_command() {
    let session = Session::new("s1", "tools", "/tmp");
    let command = format!("echo {} ENDMARK", "x".repeat(70));
    let events = vec![SessionEvent::new(
        "s1",
        1,
        EventKind::ToolCall,
        serde_json::json!({
            "call_id": "c1",
            "tool": "run_command",
            "arguments": {"command": command, "cwd": "/tmp"},
        }),
    )];
    let mut view = SessionView::new(session, events);

    let open = render_transcript(&view, 60);
    assert!(open.contains("ENDMARK"), "{open}");
    assert!(open.contains("cwd: /tmp"), "{open}");

    view.folds.insert(FoldId::Event(1), false);
    let collapsed = render_transcript(&view, 60);
    assert!(!collapsed.contains("ENDMARK"), "{collapsed}");
    assert!(collapsed.contains('…'), "{collapsed}");
}

#[test]
fn transcript_streams_live_thinking() {
    let session = Session::new("s1", "chat", "/tmp");
    let mut view = SessionView::new(session, Vec::new());
    view.stream = Some(StreamState {
        turn_id: "t1".into(),
        text: String::new(),
        reasoning: "thinking out loud".into(),
    });
    let rendered = render_transcript(&view, 40);
    assert!(rendered.contains("Thinking…"));
    assert!(rendered.contains("thinking out loud"));
}

#[test]
fn waiting_placeholder_cycles_dots_and_yields_to_streamed_text() {
    let session = Session::new("s1", "chat", "/tmp");
    let mut view = SessionView::new(session, Vec::new());
    view.stream = Some(StreamState {
        turn_id: "t1".into(),
        text: String::new(),
        reasoning: String::new(),
    });

    for (tick, dots) in [(0, "."), (1, "."), (2, ".."), (4, "..."), (6, ".")] {
        let rendered = render_transcript_at(&view, 40, tick);
        assert!(
            rendered.lines().any(|line| line.trim() == dots),
            "tick {tick}: {rendered}"
        );
    }

    view.stream.as_mut().unwrap().text = "answer".into();
    let rendered = render_transcript_at(&view, 40, 4);
    assert!(rendered.contains("answer"), "{rendered}");
    assert!(
        !rendered
            .lines()
            .any(|line| matches!(line.trim(), "." | ".." | "...")),
        "{rendered}"
    );

    let stream = view.stream.as_mut().unwrap();
    stream.text.clear();
    stream.reasoning = "thinking".into();
    let rendered = render_transcript_at(&view, 40, 4);
    assert!(rendered.contains("Thinking…"), "{rendered}");
    assert!(
        !rendered
            .lines()
            .any(|line| matches!(line.trim(), "." | ".." | "...")),
        "{rendered}"
    );
}

#[test]
fn reasoning_delta_accumulates_before_the_answer() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "chat", "/tmp"),
        Vec::new(),
    ));
    for (method, delta) in [("reasoning.delta", "think"), ("message.delta", "answer")] {
        app.handle_event(
            Event::Notification(eix_core::Notification::new(
                method,
                serde_json::json!({
                    "session_id": "s1",
                    "turn_id": "t1",
                    "delta": delta,
                }),
            )),
            &tx,
        );
    }
    let stream = app.views[0].stream.as_ref().expect("stream");
    assert_eq!(stream.reasoning, "think");
    assert_eq!(stream.text, "answer");
}

#[test]
fn retry_notification_clears_the_partial_stream() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "chat", "/tmp"),
        Vec::new(),
    ));
    app.views[0].stream = Some(StreamState {
        turn_id: "t1".into(),
        text: "partial".into(),
        reasoning: "part".into(),
    });

    app.handle_event(
        Event::Notification(eix_core::Notification::new(
            "turn.retrying",
            serde_json::json!({
                "session_id": "s1",
                "turn_id": "t1",
                "attempt": 2,
                "max_attempts": 6,
                "delay_ms": 500,
                "kind": "rate_limited",
                "reason": "provider returned HTTP 429: rate limited",
            }),
        )),
        &tx,
    );

    assert!(app.views[0].stream.is_none());
    assert!(
        app.status.contains("rate limited · retrying 2/6"),
        "{}",
        app.status
    );
    assert!(app.status.contains("0.5s"), "{}", app.status);
}

#[test]
fn clicking_a_fold_toggles_it() {
    let mut app = App::new(None);
    let session = Session::new("s1", "tools", "/tmp");
    let mut view = SessionView::new(session, Vec::new());
    view.slots = vec![Some(transcript::Fold {
        id: FoldId::Event(1),
        open: false,
    })];
    app.views.push(view);
    app.pane_areas = vec![Rect::new(0, 0, 40, 10)];
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

    let click = |kind, row| MouseEvent {
        kind,
        column: 3,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let press = MouseEventKind::Down(MouseButton::Left);
    let release = MouseEventKind::Up(MouseButton::Left);
    // The first inner row is the block header. A click toggles on release
    // so the same gesture can also start a drag selection.
    app.handle_mouse(click(press, 1), &tx);
    app.handle_mouse(click(release, 1), &tx);
    assert_eq!(app.views[0].folds.get(&FoldId::Event(1)), Some(&true));
    app.views[0].slots[0] = Some(transcript::Fold {
        id: FoldId::Event(1),
        open: true,
    });
    app.handle_mouse(click(press, 1), &tx);
    app.handle_mouse(click(release, 1), &tx);
    assert_eq!(app.views[0].folds.get(&FoldId::Event(1)), Some(&false));
}

#[test]
fn clicking_a_subagent_card_opens_the_child_session() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let parent = Session::new("parent", "parent", "/tmp");
    let mut child = Session::new("child-1", "parent/scout-1", "/tmp");
    child.origin = eix_core::SessionOrigin::Agent;
    child.parent_id = Some("parent".into());
    app.sessions = vec![parent.clone(), child.clone()];
    let mut view = SessionView::new(parent, parent_spawn_events());
    view.upsert_subagent("child-1".into(), "call-1".into(), "parent/scout-1".into());
    view.slots = vec![Some(transcript::Fold {
        id: FoldId::Event(1),
        open: true,
    })];
    view.text_lines = vec!["  ▾ ● ↳ parent/scout-1 · running".into()];
    app.views.push(view);
    app.pane_areas = vec![Rect::new(0, 0, 60, 10)];

    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 6, 1), &tx);
    app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 6, 1), &tx);

    assert_eq!(
        app.pending_attach
            .as_ref()
            .map(|pending| pending.session_id.as_str()),
        Some("child-1")
    );
    match rx.try_recv() {
        Ok(Command::Attach(session_id)) => assert_eq!(session_id, "child-1"),
        Ok(_) => panic!("unexpected command"),
        Err(err) => panic!("unexpected command error: {err}"),
    }
}

#[test]
fn subagent_transcript_shows_a_parent_link() {
    let mut app = App::new(None);
    let mut child = Session::new("child-1", "parent/scout-1", "/tmp");
    child.origin = eix_core::SessionOrigin::Agent;
    child.parent_id = Some("parent".into());
    app.views.push(SessionView::new(child, Vec::new()));

    let rendered = render_text(&mut app, 80, 20);
    assert!(
        rendered.contains("← parent agent · click to return"),
        "{rendered}"
    );
}

#[test]
fn clicking_the_parent_link_returns_to_the_parent_agent() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let parent = Session::new("parent", "parent", "/tmp");
    let mut child = Session::new("child-1", "parent/scout-1", "/tmp");
    child.origin = eix_core::SessionOrigin::Agent;
    child.parent_id = Some("parent".into());
    app.sessions = vec![parent.clone(), child.clone()];
    app.views.push(SessionView::new(parent, Vec::new()));
    let mut child_view = SessionView::new(child, Vec::new());
    child_view.text_lines = vec!["  ← parent agent · click to return".into()];
    app.views.push(child_view);
    app.focused = 1;
    app.pane_areas = vec![Rect::new(0, 0, 30, 10), Rect::new(30, 0, 30, 10)];

    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 40, 1), &tx);
    app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 40, 1), &tx);

    assert_eq!(app.focused, 0);
    assert_eq!(app.views.len(), 1, "the child pane should close");
    assert_eq!(app.views[0].session.id, "parent");
    assert_eq!(app.status, "parent agent focused");
}

#[test]
fn returning_to_parent_closes_a_child_pane_before_the_parent() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let parent = Session::new("parent", "parent", "/tmp");
    let mut child = Session::new("child-1", "parent/scout-1", "/tmp");
    child.origin = eix_core::SessionOrigin::Agent;
    child.parent_id = Some("parent".into());
    app.sessions = vec![parent.clone(), child.clone()];
    let mut child_view = SessionView::new(child, Vec::new());
    child_view.text_lines = vec!["  ← parent agent · click to return".into()];
    app.views.push(child_view);
    app.views.push(SessionView::new(parent, Vec::new()));
    app.focused = 0;
    app.pane_areas = vec![Rect::new(0, 0, 30, 10), Rect::new(30, 0, 30, 10)];

    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 10, 1), &tx);
    app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 10, 1), &tx);

    assert_eq!(app.focused, 0);
    assert_eq!(app.views.len(), 1);
    assert_eq!(app.views[0].session.id, "parent");
    assert_eq!(app.status, "parent agent focused");
}

#[test]
fn dragging_the_transcript_copies_the_selected_cells() {
    let mut app = App::new(None);
    let mut view = SessionView::new(Session::new("s1", "tools", "/tmp"), Vec::new());
    view.text_lines = vec!["hello world".into(), "second line".into()];
    app.views.push(view);
    app.pane_areas = vec![Rect::new(0, 0, 40, 10)];
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 1), &tx);
    app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 6, 1), &tx);
    app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 6, 1), &tx);

    assert_eq!(app.take_clipboard().as_deref(), Some("hello"));
    assert_eq!(
        app.toasts.last().map(|toast| toast.text.as_str()),
        Some("Copied 5 chars")
    );
    assert!(app.views[0].selection.is_some());

    let rendered = render_text(&mut app, 40, 12);
    let top_right = rendered.lines().nth(1).unwrap_or_default();
    let column = top_right.find("Copied 5 chars").unwrap_or_default();
    assert!(column > 20, "{rendered}");

    // The copied text also seeds the prompt register, so normal-mode `p`
    // pastes it even without a native clipboard helper.
    assert!(app.input.has_register_text());
    app.focus = Focus::Input;
    app.input.enter_normal();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &tx);
    assert_eq!(app.input.text(), "hello");
}

#[test]
fn toasts_stack_and_expire_independently() {
    let mut app = App::new(None);
    app.push_toast("first".into(), ToastTone::Success, Duration::from_secs(10));
    app.push_toast("second".into(), ToastTone::Success, Duration::from_secs(10));
    app.push_toast("third".into(), ToastTone::Success, Duration::from_secs(10));

    // New messages are visible immediately; they do not wait behind a
    // minimum display time. Newest renders on the top row.
    assert_eq!(
        app.toasts
            .iter()
            .map(|toast| toast.text.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second", "third"]
    );
    let text = render_text(&mut app, 80, 20);
    assert!(
        text.lines()
            .nth(1)
            .is_some_and(|line| line.contains("third")),
        "{text}"
    );
    assert!(
        text.lines()
            .nth(2)
            .is_some_and(|line| line.contains("second")),
        "{text}"
    );
    assert!(
        text.lines()
            .nth(3)
            .is_some_and(|line| line.contains("first")),
        "{text}"
    );

    // Repeating a message refreshes it and moves it back to the top.
    app.push_toast("second".into(), ToastTone::Warning, Duration::from_secs(10));
    assert_eq!(
        app.toasts
            .iter()
            .map(|toast| toast.text.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "third", "second"]
    );
    assert_eq!(
        app.toasts.last().map(|toast| toast.tone),
        Some(ToastTone::Warning)
    );

    // Expiry removes each row on its own schedule.
    let now = Instant::now();
    app.toasts[0].expires_at = now - Duration::from_millis(1);
    app.toasts[1].expires_at = now - Duration::from_millis(1);
    app.expire_toasts(now);
    assert_eq!(
        app.toasts
            .iter()
            .map(|toast| toast.text.as_str())
            .collect::<Vec<_>>(),
        vec!["second"]
    );

    app.toasts[0].expires_at = now - Duration::from_millis(1);
    let text = render_text(&mut app, 80, 20);
    assert!(!text.contains("second"), "{text}");
    assert!(app.toasts.is_empty());
}

#[test]
fn selection_text_spans_lines_and_respects_wide_glyphs() {
    let lines = vec!["hello world".to_string(), "second line".to_string()];
    let selection = TextSelection {
        anchor: (0, 6),
        cursor: (1, 6),
        dragging: false,
        moved: true,
    };
    assert_eq!(selection_text(&lines, &selection), "world\nsecond");

    let wide = vec!["你a".to_string()];
    let selection = TextSelection {
        anchor: (0, 0),
        cursor: (0, 2),
        dragging: false,
        moved: true,
    };
    assert_eq!(selection_text(&wide, &selection), "你");
    let selection = TextSelection {
        anchor: (0, 0),
        cursor: (0, 3),
        dragging: false,
        moved: true,
    };
    assert_eq!(selection_text(&wide, &selection), "你a");
}

#[test]
fn selection_highlight_is_reverse_video_and_keeps_text() {
    let mut lines = vec![Line::from("hello world")];
    let selection = TextSelection {
        anchor: (0, 6),
        cursor: (0, 11),
        dragging: false,
        moved: true,
    };
    highlight_selection(&mut lines, &selection);
    assert_eq!(text_line(&lines[0]), "hello world");
    let reversed: String = lines[0]
        .spans
        .iter()
        .filter(|span| span.style.add_modifier.contains(Modifier::REVERSED))
        .map(|span| span.content.as_ref())
        .collect();
    assert_eq!(reversed, "world");
}

#[test]
fn background_attention_is_marked_and_cleared() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.views.push(SessionView::new(
        Session::new("s2", "two", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;

    app.handle_event(
        Event::Notification(eix_core::Notification::new(
            "session.attention",
            serde_json::json!({"session_id": "s2", "status": "awaiting_approval"}),
        )),
        &tx,
    );
    // Attention is marked in place; the pane order does not change.
    assert_eq!(app.views[0].session.id, "s1");
    assert_eq!(app.views[1].session.id, "s2");
    assert_eq!(app.views[1].session.status, SessionStatus::AwaitingApproval);
    assert_eq!(app.focused, 0);
    assert!(!app.pane_needs_attention(0));
    assert!(app.pane_needs_attention(1));
    assert_eq!(app.attention_count(), 1);

    app.focus_next_attention();
    assert_eq!(app.focused, 1);
    assert!(!app.pane_needs_attention(1));
    assert_eq!(app.attention_count(), 0);
}

#[test]
fn background_attention_rings_only_for_background_panes_when_enabled() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.views.push(SessionView::new(
        Session::new("s2", "two", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;

    let attention = |session: &str| {
        Event::Notification(eix_core::Notification::new(
            "session.attention",
            serde_json::json!({"session_id": session, "status": "awaiting_approval"}),
        ))
    };

    // Off by default.
    app.handle_event(attention("s2"), &tx);
    assert!(!app.take_ring());

    app.bell = true;
    // The ready session is in pane 2, so it is background work and rings
    // exactly once.
    app.handle_event(attention("s2"), &tx);
    assert!(app.take_ring());
    assert!(!app.take_ring());

    // The focused working pane does not ring.
    app.handle_event(attention("s1"), &tx);
    assert!(!app.take_ring());
}

#[test]
fn attach_replays_a_pending_approval() {
    let session = Session::new("s1", "tools", "/tmp");
    let approval = SessionEvent::new(
        "s1",
        1,
        EventKind::ApprovalRequired,
        serde_json::json!({
            "approval_id": "a1",
            "call_id": "c1",
            "tool": "write_file",
            "summary": "write_file out.txt",
            "arguments": {"path": "out.txt", "content": "x"},
            "diff": "--- a/out.txt\n+++ b/out.txt\n@@ -1 +1 @@\n-x\n+y\n",
        }),
    );
    let result = SessionEvent::new(
        "s1",
        2,
        EventKind::ToolResult,
        serde_json::json!({
            "call_id": "c1",
            "tool": "write_file",
            "ok": true,
            "denied": false,
            "output": "done",
        }),
    );

    let pending = SessionView::new(session.clone(), vec![approval.clone()]).pending_approval();
    let pending = pending.expect("restored approval");
    assert_eq!(pending.approval_id, "a1");
    assert!(pending
        .diff
        .as_deref()
        .is_some_and(|diff| diff.contains("+y")));

    let resolved = SessionView::new(session, vec![approval, result]).pending_approval();
    assert!(resolved.is_none());
}

#[test]
fn approval_card_shows_the_full_long_command() {
    let command = format!("echo {} ENDMARK", "x".repeat(220));
    let session = Session::new("s1", "tools", "/tmp");
    let event = SessionEvent::new(
        "s1",
        1,
        EventKind::ApprovalRequired,
        serde_json::json!({
            "approval_id": "a1",
            "call_id": "c1",
            "tool": "run_command",
            "summary": "run_command: echo (truncated)",
            "arguments": {"command": command, "cwd": "/tmp"},
            "scopes": ["once", "session", "user"],
        }),
    );
    let pending = SessionView::new(session.clone(), vec![event])
        .pending_approval()
        .expect("pending command approval");
    assert_eq!(
        pending.summary,
        format!("$ {command}\nscopes: once, session, user")
    );

    let mut app = App::new(None);
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;
    app.focus = Focus::Transcript;
    app.approvals.insert("s1".to_string(), pending);

    let rendered = render_text(&mut app, 72, 30);
    assert!(rendered.contains("$ echo"), "{rendered}");
    assert!(rendered.contains("ENDMARK"), "{rendered}");
    assert!(rendered.contains("Approve"), "{rendered}");
}

#[test]
fn approval_keys_answer_the_focused_pane() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "tools", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus = Focus::Transcript;
    app.approvals.insert(
        "s1".to_string(),
        PendingApproval {
            kind: PendingKind::Tool,
            approval_id: "a1".to_string(),
            call_id: "c1".to_string(),
            tool: "write_file".to_string(),
            summary: "write_file out.txt".to_string(),
            diff: None,
            path: None,
            hunks: Vec::new(),
            selected: Vec::new(),
            cursor: 0,
        },
    );

    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
    match rx.try_recv().expect("approval command") {
        Command::Approval {
            session_id,
            approval_id,
            decision,
            hunks,
            reason,
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(approval_id, "a1");
            assert_eq!(decision, ApprovalDecision::Approve);
            assert!(hunks.is_none());
            assert!(reason.is_none());
        }
        _ => panic!("expected an approval command"),
    }
    // The card stays until the daemon confirms the decision.
    assert!(app.approvals.contains_key("s1"));
}

fn app_with_diff(
    diff: Option<&str>,
) -> (
    App,
    tokio::sync::mpsc::UnboundedSender<Command>,
    tokio::sync::mpsc::UnboundedReceiver<Command>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "tools", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus = Focus::Transcript;
    app.approvals.insert(
        "s1".to_string(),
        PendingApproval {
            kind: PendingKind::Tool,
            approval_id: "a1".to_string(),
            call_id: "c1".to_string(),
            tool: "write_file".to_string(),
            summary: "write_file out.txt".to_string(),
            diff: diff.map(str::to_string),
            path: None,
            hunks: Vec::new(),
            selected: Vec::new(),
            cursor: 0,
        },
    );
    (app, tx, rx)
}

fn hunk(index: usize, header: &str, lines: &[&str]) -> HunkInfo {
    HunkInfo {
        index,
        header: header.to_string(),
        lines: lines.iter().map(|line| line.to_string()).collect(),
    }
}

fn app_with_hunks(hunks: Vec<HunkInfo>) -> App {
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "tools", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus = Focus::Transcript;
    app.approvals.insert(
        "s1".to_string(),
        PendingApproval {
            kind: PendingKind::Tool,
            approval_id: "a1".to_string(),
            call_id: "c1".to_string(),
            tool: "write_file".to_string(),
            summary: "write_file out.txt".to_string(),
            diff: Some("unused".to_string()),
            path: None,
            selected: vec![true; hunks.len()],
            hunks,
            cursor: 0,
        },
    );
    app
}

#[test]
fn approval_reason_is_sent_with_a_denial() {
    let (mut app, tx, mut rx) = app_with_diff(None);

    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE), &tx);
    assert!(app.reason_prompt.is_some());
    for ch in "scope".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &tx);
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.reason_prompt.is_none());

    match rx.try_recv().expect("deny command") {
        Command::Approval {
            decision, reason, ..
        } => {
            assert_eq!(decision, ApprovalDecision::Deny);
            assert_eq!(reason.as_deref(), Some("scope"));
        }
        _ => panic!("expected a denial"),
    }
}

#[test]
fn editing_a_staged_patch_requests_its_content() {
    let (mut app, tx, mut rx) = app_with_diff(Some("@@ -1 +1 @@\n-x\n+y\n"));
    if let Some(approval) = app.approvals.get_mut("s1") {
        approval.kind = PendingKind::Patch;
        approval.path = Some("out.txt".to_string());
    }

    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE), &tx);
    match rx.try_recv().expect("patch.get command") {
        Command::PatchGet {
            session_id,
            patch_id,
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(patch_id, "a1");
        }
        _ => panic!("expected a patch.get command"),
    }
}

#[test]
fn inbox_lists_and_answers_pending_approvals() {
    let (mut app, tx, mut rx) = app_with_diff(None);
    app.sessions.push(Session::new("s1", "tools", "/tmp"));

    app.run_action("approvals", &tx);
    let inbox = app.inbox.as_ref().expect("inbox opened");
    assert_eq!(inbox.items.len(), 1);
    assert_eq!(inbox.items[0].session_name, "tools");

    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
    match rx.try_recv().expect("approval command") {
        Command::Approval {
            session_id,
            decision,
            ..
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(decision, ApprovalDecision::Approve);
        }
        _ => panic!("expected an approval command"),
    }

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(app.inbox.is_none());
}

#[test]
fn diff_review_scrolls_and_closes() {
    let (mut app, tx, _rx) =
        app_with_diff(Some("--- a/out.txt\n+++ b/out.txt\n@@ -1 +1 @@\n-x\n+y\n"));

    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
    assert!(app.diff_view.is_some());

    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &tx);
    assert_eq!(app.diff_view.as_ref().map(|view| view.scroll), Some(1));

    app.handle_transcript_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(app.diff_view.is_none());
}

#[test]
fn diff_review_answers_the_approval() {
    let (mut app, tx, mut rx) = app_with_diff(Some("@@ -1 +1 @@\n-x\n+y\n"));

    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
    assert!(app.diff_view.is_none());
    match rx.try_recv().expect("approval command") {
        Command::Approval {
            session_id,
            approval_id,
            decision,
            hunks,
            reason,
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(approval_id, "a1");
            assert_eq!(decision, ApprovalDecision::Approve);
            assert!(hunks.is_none());
            assert!(reason.is_none());
        }
        _ => panic!("expected an approval command"),
    }

    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);
    match rx.try_recv().expect("deny command") {
        Command::Approval { decision, .. } => {
            assert_eq!(decision, ApprovalDecision::Deny);
        }
        _ => panic!("expected a deny command"),
    }
}

#[test]
fn diff_review_applies_only_selected_hunks() {
    let hunks = vec![
        hunk(0, "@@ -1,2 +1,2 @@", &["@@ -1,2 +1,2 @@", "-a", "+A"]),
        hunk(1, "@@ -8,2 +8,2 @@", &["@@ -8,2 +8,2 @@", "-b", "+B"]),
    ];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_hunks(hunks);
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
    // Move to the second hunk and deselect it.
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE), &tx);
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);

    match rx.try_recv().expect("approval command") {
        Command::Approval {
            decision, hunks, ..
        } => {
            assert_eq!(decision, ApprovalDecision::Approve);
            assert_eq!(hunks, Some(vec![0]));
        }
        _ => panic!("expected an approval command"),
    }
}

#[test]
fn diff_review_approves_everything_when_all_hunks_are_selected() {
    let hunks = vec![
        hunk(0, "@@ -1 +1 @@", &["@@ -1 +1 @@", "-a", "+A"]),
        hunk(1, "@@ -8 +8 @@", &["@@ -8 +8 @@", "-b", "+B"]),
    ];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_hunks(hunks);
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
    match rx.try_recv().expect("approval command") {
        Command::Approval { hunks, .. } => assert!(hunks.is_none()),
        _ => panic!("expected an approval command"),
    }
}

#[test]
fn diff_review_refuses_to_apply_with_no_hunks_selected() {
    let hunks = vec![
        hunk(0, "@@ -1 +1 @@", &["@@ -1 +1 @@", "-a", "+A"]),
        hunk(1, "@@ -8 +8 @@", &["@@ -8 +8 @@", "-b", "+B"]),
    ];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_hunks(hunks);
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
    // `a` deselects everything because everything starts selected.
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &tx);
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
    assert!(rx.try_recv().is_err());
    assert!(app.status.contains("no hunks selected"));
    assert!(app.diff_view.is_some());
}

#[test]
fn diff_review_needs_a_diff() {
    let (mut app, tx, _rx) = app_with_diff(None);
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
    assert!(app.diff_view.is_none());
    assert!(app.status.contains("no diff"));
}

#[test]
fn write_file_card_uses_the_whole_diff_budget() {
    let mut diff = String::from("--- a/out.txt\n+++ b/out.txt\n@@ -1,15 +1,15 @@\n");
    for index in 0..15 {
        diff.push_str(&format!("-old_{index}\n+new_{index}\n"));
    }
    let (mut app, _tx, _rx) = app_with_diff(Some(&diff));
    let rendered = render_text(&mut app, 80, 40);
    // The card no longer stops at eight rows for file changes.
    assert!(rendered.contains("old_14"), "{rendered}");
    assert!(rendered.contains("new_14"), "{rendered}");
    assert!(rendered.contains('│'), "{rendered}");
}

fn process_info(id: &str, status: ProcessStatus) -> ProcessInfo {
    ProcessInfo {
        id: id.into(),
        session_id: "s1".into(),
        command: "npm run dev".into(),
        cwd: "/tmp".into(),
        pid: 42,
        status,
        exit_code: None,
        started_ms: 0,
        ended_ms: None,
        bytes: 0,
        truncated: false,
        log: "/tmp/x.log".into(),
    }
}

#[test]
fn process_view_keeps_the_selection_across_refreshes() {
    let mut view = ProcessView::new("s1".into());
    view.set_list(ProcessList {
        processes: vec![
            process_info("aaaaaaaa-1", ProcessStatus::Running),
            process_info("bbbbbbbb-2", ProcessStatus::Exited),
        ],
    });
    view.cursor = 1;
    // A newer process appears at the front; the cursor follows the id.
    view.set_list(ProcessList {
        processes: vec![
            process_info("cccccccc-3", ProcessStatus::Running),
            process_info("aaaaaaaa-1", ProcessStatus::Running),
            process_info("bbbbbbbb-2", ProcessStatus::Exited),
        ],
    });
    assert_eq!(
        view.selected().map(|process| process.id.as_str()),
        Some("bbbbbbbb-2")
    );
    // Shrinking the list clamps the cursor.
    view.set_list(ProcessList {
        processes: vec![process_info("aaaaaaaa-1", ProcessStatus::Running)],
    });
    assert_eq!(view.cursor, 0);
}

#[test]
fn process_view_only_appends_output_for_the_shown_process() {
    let mut view = ProcessView::new("s1".into());
    view.set_tail(ProcessOutput {
        process_id: "aaaaaaaa-1".into(),
        text: "one\n".into(),
        bytes: 4,
        truncated: false,
        status: ProcessStatus::Running,
        exit_code: None,
    });
    view.append("bbbbbbbb-2", "ignored");
    assert_eq!(view.tail, "one\n");
    view.append("aaaaaaaa-1", "two\n");
    assert_eq!(view.tail, "one\ntwo\n");

    // Exits are reflected in the list.
    view.set_list(ProcessList {
        processes: vec![process_info("aaaaaaaa-1", ProcessStatus::Running)],
    });
    view.mark_exited("aaaaaaaa-1", ProcessStatus::Killed, None);
    assert_eq!(view.selected().unwrap().status, ProcessStatus::Killed);
}

#[test]
fn process_dialog_uses_the_shared_overlay_size() {
    let mut app = app_with_panes(&["s1"]);
    app.process_view = Some(ProcessView::new("s1".into()));
    let text = render_text(&mut app, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    let rect = search_dialog_rect(area);
    let top = text
        .lines()
        .nth(rect.y as usize)
        .expect("process dialog top border");
    assert_eq!(
        top.chars().position(|ch| ch == '┌'),
        Some(rect.x as usize),
        "{text}"
    );
    assert!(top.contains("processes · s1"), "{text}");
}

#[test]
fn opening_processes_sends_a_list_request_and_closes_with_q() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &tx);
    assert!(app.process_view.is_some());
    assert!(rx.try_recv().is_ok());

    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &tx);
    assert!(app.process_view.is_none());
    assert!(!app.should_quit, "q closes the overlay, not the app");
}

#[test]
fn forked_session_opens_a_new_pane() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let mut fork = Session::new("fork-1", "one~2", "/tmp");
    fork.parent_id = Some("s1".into());
    fork.forked_from_seq = Some(12);
    app.handle_event(Event::Forked(fork), &tx);
    assert_eq!(
        app.pending_attach
            .as_ref()
            .map(|pending| pending.session_id.as_str()),
        Some("fork-1")
    );
    assert!(app.sessions.iter().any(|session| session.id == "fork-1"));
    match rx.try_recv().expect("attach command") {
        Command::Attach(id) => assert_eq!(id, "fork-1"),
        _ => panic!("expected an attach command"),
    }
    assert!(app.status.contains("workspace") || app.status.contains("files"));
}

#[test]
fn slash_and_question_search_the_current_transcript_vim_style() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.views[0].text_lines = ["zero", "one", "alpha here", "three", "alpha again"]
        .into_iter()
        .map(str::to_string)
        .collect();
    app.focus_input();
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "alpha");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    let search = app.transcript_search.as_ref().expect("transcript search");
    assert_eq!(search.query, "alpha");
    assert_eq!(search.matches, vec![2, 4]);
    assert_eq!(search.current, 0);
    assert_eq!(app.views[0].scroll, 0);
    assert!(app.status.contains("/alpha · 1/2"));

    // n / N cycle matches without leaving normal mode.
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);
    assert_eq!(app.transcript_search.as_ref().unwrap().current, 1);
    assert_eq!(app.views[0].scroll, 2);
    app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE), &tx);
    assert_eq!(app.transcript_search.as_ref().unwrap().current, 0);

    // `?` searches backward from the current viewport.
    app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT), &tx);
    type_text(&mut app, &tx, "alpha");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    let search = app.transcript_search.as_ref().unwrap();
    assert_eq!(search.current, 1);
    assert!(app.status.contains("?alpha · 2/2"));

    // Esc leaves search mode and removes the highlight state.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(app.transcript_search.is_none());
    assert_eq!(app.status, "search cleared");
}

#[test]
fn command_line_shows_a_dynamic_filtered_command_menu() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    assert!(!app.cmdline_menu_open(), "the menu must wait for Tab");
    assert!(!app.cmdline_matches().is_empty());

    type_text(&mut app, &tx, "mod");
    assert!(!app.cmdline_menu_open(), "typing alone must not open it");
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
    assert!(app.cmdline_menu_open(), "Tab enables the command menu");
    let names: Vec<&str> = app
        .cmdline_matches()
        .into_iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert!(names.contains(&"model"), "{names:?}");
    assert!(names.iter().all(|name| name.starts_with("mod")));

    // Arguments hide the command menu.
    type_text(&mut app, &tx, " provider/model");
    assert!(!app.cmdline_menu_open());

    // `/` and `?` command lines are transcript searches, not command menus.
    app.cmdline = None;
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE), &tx);
    assert!(!app.cmdline_menu_open());
    assert!(app.cmdline_matches().is_empty());
}

#[test]
fn command_line_menu_navigates_with_ctrl_keys() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "re");
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
    assert_eq!(
        app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
        Some("ready")
    );
    let names: Vec<&str> = app
        .cmdline_matches()
        .into_iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert_eq!(names, vec!["ready", "reload", "redo"]);
    assert_eq!(app.cmdline_menu_cursor, None);

    app.handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.cmdline_menu_cursor, Some(1));
    assert_eq!(
        app.selected_cmdline_match().map(|spec| spec.name.as_str()),
        Some("reload")
    );
    assert_eq!(
        app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
        Some("ready"),
        "moving the menu cursor must not rewrite the typed prefix"
    );

    app.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.cmdline_menu_cursor, Some(0));
    app.handle_key(
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.cmdline_menu_cursor, Some(1));
    app.handle_key(
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.cmdline_menu_cursor, Some(0));

    // Terminals may report Ctrl-J as Ctrl-Enter; both move the menu.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL), &tx);
    assert_eq!(app.cmdline_menu_cursor, Some(1));
    // Enter accepts the highlighted row, not the completed first match.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.cmdline.is_none());
}

#[test]
fn normal_search_keys_open_the_command_line() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    for (focus, key, prompt) in [
        (
            Focus::Input,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            '/',
        ),
        (
            Focus::Input,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
            '?',
        ),
        (
            Focus::Transcript,
            KeyEvent::new(KeyCode::Char('#'), KeyModifiers::SHIFT),
            '#',
        ),
    ] {
        let mut app = app_with_panes(&["s1"]);
        app.focus = focus;
        if focus == Focus::Input {
            app.input.enter_normal();
        }
        app.handle_key(key, &tx);
        let cmdline = app.cmdline.as_ref().expect("command line");
        assert_eq!(cmdline.prompt, prompt);
        assert!(cmdline.text.is_empty());
    }
}

#[test]
fn picker_kinds_map_to_daemon_providers() {
    assert_eq!(PickerKind::Files.data_kind(), Some(eix_core::KIND_FILES));
    assert_eq!(PickerKind::Grep.data_kind(), Some(eix_core::KIND_GREP));
    assert_eq!(PickerKind::Git.data_kind(), Some(eix_core::KIND_GIT));
    assert!(PickerKind::Sessions.data_kind().is_none());
    assert!(PickerKind::Commands.data_kind().is_none());
    assert!(PickerKind::Themes.data_kind().is_none());
    assert!(!PickerKind::Files.title().is_empty());
    assert!(!PickerKind::Commands.hint().is_empty());
}

#[test]
fn command_palette_ids_are_unique() {
    let items = command_items(&builtin_commands());
    assert!(items.len() >= 10);
    let mut ids: Vec<&str> = items.iter().map(|item| item.value.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), items.len());
    assert!(items.iter().all(|item| !item.label.is_empty()));
    assert!(items.iter().any(|item| item.value == "quit"));
}

#[test]
fn file_picker_inserts_at_path_into_the_native_prompt() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.picker = Some(Picker {
        kind: PickerKind::Files,
        query: String::new(),
        selected: 0,
        items: vec![PickerItem::new("src/main.rs"), PickerItem::new("README.md")],
        loading: false,
        preview: PickerPreview::default(),
        preview_request: None,
    });
    app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.picker.is_none());
    assert_eq!(app.input.text(), "@src/main.rs");
    assert!(matches!(app.focus, Focus::Input));
}

#[test]
fn grep_picker_searches_live_and_inserts_at_path_line() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.open_grep_picker("needle", &tx);
    let picker = app.picker.as_ref().expect("grep picker");
    assert_eq!(picker.kind, PickerKind::Grep);
    assert_eq!(picker.query, "needle");
    assert!(app.picker_query_due);

    app.flush_picker_query(&tx);
    match rx.try_recv().expect("grep query") {
        Command::PickerData {
            session_id,
            kind,
            query,
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(kind, eix_core::KIND_GREP);
            assert_eq!(query.as_deref(), Some("needle"));
        }
        _ => panic!("expected a picker command"),
    }

    app.handle_event(
        Event::PickerData {
            kind: eix_core::KIND_GREP.into(),
            items: vec![PickerItem::new("src/main.rs:3")
                .with_label("src/main.rs:3")
                .with_detail("let needle = 1;")],
            query: Some("needle".into()),
        },
        &tx,
    );
    assert_eq!(app.picker.as_ref().unwrap().items.len(), 1);

    // The next keystroke clears the old rows; a response for the old
    // query must not repopulate them.
    app.handle_picker_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &tx);
    assert!(app.picker.as_ref().unwrap().items.is_empty());
    app.handle_event(
        Event::PickerData {
            kind: eix_core::KIND_GREP.into(),
            items: vec![PickerItem::new("src/main.rs:3")],
            query: Some("needle".into()),
        },
        &tx,
    );
    assert!(app.picker.as_ref().unwrap().items.is_empty());

    app.flush_picker_query(&tx);
    match rx.try_recv().expect("updated grep query") {
        Command::PickerData { query, .. } => {
            assert_eq!(query.as_deref(), Some("needlex"));
        }
        _ => panic!("expected a picker command"),
    }
    app.handle_event(
        Event::PickerData {
            kind: eix_core::KIND_GREP.into(),
            items: vec![PickerItem::new("src/main.rs:3")],
            query: Some("needlex".into()),
        },
        &tx,
    );
    app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.picker.is_none());
    assert_eq!(app.input.text(), "@src/main.rs:3");
}

#[test]
fn grep_command_opens_with_an_initial_query() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    assert!(app.handle_command_text("/grep TODO", &tx, true));
    let picker = app.picker.as_ref().expect("grep picker");
    assert_eq!(picker.kind, PickerKind::Grep);
    assert_eq!(picker.query, "TODO");
    assert!(picker.loading);
    assert!(app.picker_query_due);
}

#[test]
fn file_picker_preview_requests_the_selected_file() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.picker = Some(Picker {
        kind: PickerKind::Files,
        query: String::new(),
        selected: 0,
        items: vec![PickerItem::new("src/a.rs"), PickerItem::new("src/b.rs")],
        loading: false,
        preview: PickerPreview::default(),
        preview_request: None,
    });

    let filtered = app.picker_filtered();
    app.sync_picker_preview(&filtered, 0);
    let (kind, session_id, value) = app.take_picker_preview_request().unwrap();
    assert_eq!(kind, eix_core::KIND_FILES);
    assert_eq!(session_id.as_deref(), Some("s1"));
    assert_eq!(value, "src/a.rs");

    app.picker.as_mut().unwrap().selected = 1;
    let filtered = app.picker_filtered();
    app.sync_picker_preview(&filtered, 1);
    let (_, _, value) = app.take_picker_preview_request().unwrap();
    assert_eq!(value, "src/b.rs");

    // A response for the previous row must not overwrite the new preview.
    app.handle_event(
        Event::PickerPreview {
            kind: eix_core::KIND_FILES.into(),
            value: "src/a.rs".into(),
            text: Some("old contents".into()),
            error: None,
        },
        &tx,
    );
    assert!(app.picker.as_ref().unwrap().preview.text.is_none());
    app.handle_event(
        Event::PickerPreview {
            kind: eix_core::KIND_FILES.into(),
            value: "src/b.rs".into(),
            text: Some("fn b() {}".into()),
            error: None,
        },
        &tx,
    );
    assert_eq!(
        app.picker_preview_lines(&filtered),
        vec!["fn b() {}".to_string()]
    );
}

#[test]
fn file_picker_draws_the_lazy_preview_on_the_right() {
    let mut app = app_with_panes(&["s1"]);
    app.picker = Some(Picker {
        kind: PickerKind::Files,
        query: String::new(),
        selected: 0,
        items: vec![PickerItem::new("src/a.rs")],
        loading: false,
        preview: PickerPreview {
            key: Some("src/a.rs".into()),
            text: Some("   1 │ fn a() {}".into()),
            error: None,
            loading: false,
        },
        preview_request: None,
    });

    let rendered = render_text(&mut app, 100, 24);
    assert!(rendered.contains("fn a() {}"), "{rendered}");
}

#[test]
fn session_picker_preview_requests_the_selected_session_history() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.sessions = vec![
        Session::new("s1", "one", "/tmp"),
        Session::new("s2", "two", "/tmp"),
    ];
    app.picker = Some(Picker {
        kind: PickerKind::NewPane,
        query: String::new(),
        selected: 0,
        items: Vec::new(),
        loading: false,
        preview: PickerPreview::default(),
        preview_request: None,
    });

    let filtered = app.picker_filtered();
    app.sync_picker_preview(&filtered, 0);
    let (kind, session_id, value) = app.take_picker_preview_request().unwrap();
    assert_eq!(kind, eix_core::KIND_SESSION_PREVIEW);
    assert!(session_id.is_none());
    assert_eq!(value, "s1");

    app.handle_event(
        Event::PickerPreview {
            kind: eix_core::KIND_SESSION_PREVIEW.into(),
            value: "s1".into(),
            text: Some("you:\n  hello".into()),
            error: None,
        },
        &tx,
    );
    assert_eq!(
        app.picker_preview_lines(&filtered),
        vec!["you:".to_string(), "  hello".to_string()]
    );
}

#[test]
fn session_picker_hides_subagents_and_marks_worktrees_before_ids() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let mut worktree = Session::new("s2", "worktree", "/tmp/wt");
    worktree.worktree = Some(std::path::PathBuf::from("/tmp/wt"));
    let mut child = Session::new("s3", "child", "/tmp");
    child.origin = eix_core::SessionOrigin::Agent;
    child.parent_id = Some("s1".into());
    app.sessions = vec![Session::new("s1", "normal", "/tmp"), worktree, child];

    app.open_picker(PickerKind::NewPane, &tx);
    let filtered = app.picker_filtered();
    assert_eq!(filtered, vec![0, 1]);
    assert_eq!(app.picker.as_ref().unwrap().selected, 0);

    // Searching goes through the same agent filter.
    app.picker.as_mut().unwrap().query = "child".into();
    assert!(app.filtered_sessions().is_empty());
    app.picker.as_mut().unwrap().query = "worktree".into();
    assert_eq!(app.filtered_sessions(), vec![1]);
    app.picker.as_mut().unwrap().query.clear();

    let rendered = render_text(&mut app, 120, 30);
    assert!(!rendered.contains("child"), "{rendered}");
    assert!(!rendered.contains('↳'), "{rendered}");
    let row = rendered
        .lines()
        .find(|line| line.contains("worktree"))
        .unwrap_or_else(|| panic!("worktree row missing: {rendered}"));
    let name = row.find("worktree").expect("name");
    let marker = row.find('⑂').expect("worktree marker");
    let id = row.find("s2").expect("session id");
    assert!(name < marker && marker < id, "{row:?}");
}

#[test]
fn command_palette_enter_runs_the_command() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.open_picker(PickerKind::Commands, &tx);
    let help = app
        .picker
        .as_ref()
        .unwrap()
        .items
        .iter()
        .position(|item| item.value == "help")
        .expect("help command");
    app.picker.as_mut().unwrap().selected = help;
    app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.help);
    assert!(app.picker.is_none());
    assert!(app.help_query.is_empty());
    assert_eq!(app.help_selected, 0);
}

#[test]
fn colorscheme_command_opens_picker_and_escape_restores() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.theme_config.preset = Some("github-dark".into());
    app.theme = Theme::from_config(&app.theme_config);
    let saved = app.theme;

    assert!(app.handle_prompt_command("/colorscheme", &tx));
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Themes)
    ));
    assert!(app.theme_picker_backup.is_some());

    app.handle_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
    assert_ne!(app.theme.preset, Some("github-dark"));
    app.handle_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(app.picker.is_none());
    assert_eq!(app.theme, saved);
    assert_eq!(app.theme_config.preset.as_deref(), Some("github-dark"));
    assert!(app.theme_picker_backup.is_none());

    // Colorscheme arguments no longer switch directly; the picker is the only path.
    assert!(app.handle_prompt_command("/colorscheme nord", &tx));
    assert!(app.picker.is_none());
    assert!(app
        .error
        .as_deref()
        .is_some_and(|error| error.contains("usage: /colorscheme")));
}

#[test]
fn theme_alias_is_not_available() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    assert!(app.handle_prompt_command("/theme", &tx));
    assert!(app.picker.is_none());
    assert!(app
        .error
        .as_deref()
        .is_some_and(|error| error.contains("unknown command /theme")));
}

#[test]
fn theme_background_paints_panes_and_dialogs() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.theme_config.preset = Some("nord".into());
    app.theme = Theme::from_config(&app.theme_config);
    let session = Session::new("s1", "theme background", "/home/shiyi/git/eix");
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;

    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let nord_bg = Color::Rgb(46, 52, 64);
    assert_eq!(terminal.backend().buffer()[(2, 2)].bg, nord_bg);
    // The status bar's empty middle must use the theme background too, not
    // whatever the terminal painted there previously.
    for x in 30..50 {
        assert_eq!(
            terminal.backend().buffer()[(x, 22)].bg,
            nord_bg,
            "status bar background at x={x}"
        );
    }

    // Previewing the first built-in preset must repaint the pane and the
    // picker dialog, not leave either on the terminal's default background.
    app.theme_config = eix_core::ThemeConfig::default();
    app.theme = Theme::default();
    app.open_picker(PickerKind::Themes, &tx);
    app.handle_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
    assert_eq!(app.theme.preset, Some("github-dark"));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let preview_bg = Color::Rgb(13, 17, 23);
    assert_eq!(terminal.backend().buffer()[(2, 2)].bg, preview_bg);
    // The centered picker's border and body should use the previewed
    // palette, not the terminal's default background.
    assert_eq!(terminal.backend().buffer()[(8, 4)].bg, preview_bg);
    assert_eq!(terminal.backend().buffer()[(10, 7)].bg, preview_bg);
    assert_eq!(terminal.backend().buffer()[(40, 22)].bg, preview_bg);
}

#[test]
fn theme_picker_enter_commits_the_preview() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.open_picker(PickerKind::Themes, &tx);
    app.handle_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
    assert_eq!(app.theme.preset, Some("github-dark"));

    app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.picker.is_none());
    assert!(app.theme_picker_backup.is_none());
    assert_eq!(app.theme_config.preset.as_deref(), Some("github-dark"));
    assert_eq!(app.theme.preset, Some("github-dark"));
    assert!(app.status.contains("colorscheme → github-dark"));
}

#[test]
fn help_overlay_filters_and_selects_key_entries() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.open_help();

    for ch in "approval".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &tx);
    }
    assert!(app.help);
    assert_eq!(app.help_query, "approval");
    let count = help_entry_count(&app.help_query);
    assert!(count >= 2, "{count}");

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
    assert_eq!(app.help_selected, 1);
    app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &tx);
    assert_eq!(app.help_selected, count - 1);

    // Plain q/j/k are search input, not shortcuts.
    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &tx);
    assert!(app.help);
    assert_eq!(app.help_query, "approvalq");

    app.handle_key(
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(app.help_query.is_empty());
    assert_eq!(app.help_selected, 0);

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(!app.help);
}

#[test]
fn help_accepts_ctrl_j_k_and_ctrl_n_p() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.open_help();

    for (key, expected) in [('j', 1), ('k', 0), ('n', 1), ('p', 0)] {
        app.handle_key(
            KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.help_selected, expected, "Ctrl-{key}");
    }
}

#[test]
fn help_search_renders_the_selected_match() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.open_help();
    for ch in "approvals inbox".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &tx);
    }

    let text = render_text(&mut app, 120, 40);
    assert!(text.contains("/approvals"), "{text}");
    assert!(text.contains("pending-approval inbox"), "{text}");
    assert!(text.contains("› /approvals"), "{text}");
}

#[test]
fn help_key_descriptions_align() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.open_help();
    for ch in "layout".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &tx);
    }

    let text = render_text(&mut app, 120, 40);
    let long = text
        .lines()
        .find(|line| line.contains("Space v"))
        .expect("long key row");
    let short = text
        .lines()
        .find(|line| line.contains("Space l"))
        .expect("short key row");
    let column = |line: &str, needle: &str| {
        let index = line.find(needle).expect("description");
        UnicodeWidthStr::width(&line[..index])
    };
    assert_eq!(
        column(long, "horizontal"),
        column(short, "cycle grid"),
        "{text}"
    );
}

#[test]
fn command_palette_keeps_the_prompt_draft() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.focus_input();
    app.input.set_text("keep this draft");
    app.open_picker(PickerKind::Commands, &tx);
    let help = app
        .picker
        .as_ref()
        .unwrap()
        .items
        .iter()
        .position(|item| item.value == "help")
        .expect("help command");
    app.picker.as_mut().unwrap().selected = help;

    app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.help);
    assert_eq!(app.input.text(), "keep this draft");
}

#[test]
fn command_palette_aligns_descriptions() {
    let commands = vec![
        builtin("a", "", "first description"),
        builtin("long-command", "[arg]", "second description"),
    ];
    let mut app = App::new(None);
    app.picker = Some(Picker {
        kind: PickerKind::Commands,
        query: String::new(),
        selected: 0,
        items: command_items(&commands),
        loading: false,
        preview: PickerPreview::default(),
        preview_request: None,
    });

    let rendered = render_text(&mut app, 60, 20);
    let column = |needle: &str| {
        rendered
            .lines()
            .find_map(|line| {
                line.find(needle)
                    .map(|index| UnicodeWidthStr::width(&line[..index]))
            })
            .expect("command description")
    };
    assert_eq!(
        column("first description"),
        column("second description"),
        "{rendered}"
    );
}

#[test]
fn picker_detail_columns_align_for_every_kind() {
    let kinds = [
        PickerKind::Files,
        PickerKind::Grep,
        PickerKind::Git,
        PickerKind::Commands,
        PickerKind::Models,
        PickerKind::Agents,
        PickerKind::Subagents,
        PickerKind::Modes,
        PickerKind::Timeline,
        PickerKind::Skills,
        PickerKind::Themes,
        PickerKind::Mcp,
        PickerKind::Todos,
    ];
    for kind in kinds {
        let mut app = App::new(None);
        app.picker = Some(Picker {
            kind,
            query: String::new(),
            selected: 0,
            items: vec![
                PickerItem::new("a").with_label("a").with_detail("first"),
                PickerItem::new("b")
                    .with_label("long-label")
                    .with_detail("second"),
                PickerItem::new("c")
                    .with_label("a-very-long-label-that-exceeds-the-cap")
                    .with_detail("third"),
            ],
            loading: false,
            preview: PickerPreview::default(),
            preview_request: None,
        });

        let rendered = render_text(&mut app, 60, 20);
        let column = |needle: &str| {
            rendered
                .lines()
                .find_map(|line| {
                    line.find(needle)
                        .map(|index| UnicodeWidthStr::width(&line[..index]))
                })
                .unwrap_or_else(|| panic!("{kind:?}: missing {needle}\n{rendered}"))
        };
        let first = column("first");
        assert_eq!(first, column("second"), "{kind:?}\n{rendered}");
        assert_eq!(first, column("third"), "{kind:?}\n{rendered}");
    }
}

#[test]
fn session_picker_metadata_columns_align() {
    let mut app = App::new(None);
    app.sessions = vec![
        Session::new("aaaaaaaa1", "a", "/tmp/one"),
        Session::new("bbbbbbbb2", "a much longer session name", "/tmp/two"),
    ];
    app.picker = Some(Picker {
        kind: PickerKind::Sessions,
        query: String::new(),
        selected: 0,
        items: Vec::new(),
        loading: false,
        preview: PickerPreview::default(),
        preview_request: None,
    });

    let rendered = render_text(&mut app, 80, 20);
    let column = |needle: &str| {
        rendered
            .lines()
            .find_map(|line| {
                line.find(needle)
                    .map(|index| UnicodeWidthStr::width(&line[..index]))
            })
            .unwrap_or_else(|| panic!("missing {needle}\n{rendered}"))
    };
    assert_eq!(column("aaaaaaaa"), column("bbbbbbbb"), "{rendered}");
    assert_eq!(column("/tmp/one"), column("/tmp/two"), "{rendered}");
}

#[test]
fn command_palette_includes_user_commands() {
    let mut commands = builtin_commands();
    commands.push(CommandSpec {
        name: "deploy".into(),
        args: "[env]".into(),
        description: "custom deployment command".into(),
        template: Some("deploy {{args}}".into()),
        agent: None,
        model: None,
    });
    let items = command_items(&commands);
    assert!(items.iter().any(|item| {
        item.value == "deploy"
            && item.label == "/deploy [env]"
            && item.detail.as_deref() == Some("custom deployment command")
    }));
}

#[test]
fn plugin_commands_join_the_menu_without_shadowing_builtins() {
    let cwd = std::env::current_dir().unwrap();
    let commands = merge_commands(
        &cwd,
        &eix_core::CompatConfig::default(),
        &[
            eix_core::PluginCommandInfo {
                plugin: "acme".into(),
                name: "plugin/acme/review".into(),
                args: "<path>".into(),
                description: "review from a plugin".into(),
                prompt: "Review $ARGUMENTS".into(),
            },
            eix_core::PluginCommandInfo {
                plugin: "acme".into(),
                name: "help".into(),
                args: String::new(),
                description: "must not shadow the built-in".into(),
                prompt: "shadowed".into(),
            },
        ],
    );
    let review = commands
        .iter()
        .find(|command| command.name == "plugin/acme/review")
        .expect("plugin command");
    assert_eq!(review.template.as_deref(), Some("Review $ARGUMENTS"));
    let help = commands
        .iter()
        .find(|command| command.name == "help")
        .expect("built-in help");
    assert_ne!(help.template.as_deref(), Some("shadowed"));
}

#[test]
fn command_palette_filters_across_label_value_and_detail() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.open_picker(PickerKind::Commands, &tx);
    for c in "git worktrees".chars() {
        app.handle_picker_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &tx);
    }
    let filtered = app.picker_filtered();
    assert_eq!(filtered.len(), 1);
    assert_eq!(app.picker.as_ref().unwrap().items[filtered[0]].value, "git");
}

#[test]
fn data_picker_requests_its_provider() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.open_picker(PickerKind::Git, &tx);
    assert!(app.picker.as_ref().unwrap().loading);
    match rx.try_recv().expect("picker command") {
        Command::PickerData {
            session_id,
            kind,
            query,
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(kind, eix_core::KIND_GIT);
            assert!(query.is_none());
        }
        _ => panic!("expected a picker command"),
    }
}

#[test]
fn data_picker_without_a_session_stays_closed() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.open_picker(PickerKind::Files, &tx);
    assert!(app.picker.is_none());
    assert!(rx.try_recv().is_err());
}

#[test]
fn banner_helpers_are_compact() {
    assert_eq!(
        short_model("deepseek/deepseek-v4.1-flash"),
        "deepseek-v4.1-flash"
    );
    assert_eq!(short_model("gpt-4o"), "gpt-4o");
    assert_eq!(compact_path("/tmp/x"), "/tmp/x");
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        let home = home.to_string_lossy();
        assert_eq!(compact_path(&format!("{home}/git/eix")), "~/git/eix");
    }
}

#[test]
fn prompt_height_grows_for_long_drafts() {
    let mut input = Editor::new();
    // Short drafts keep the minimum 3 editor rows + 2 borders.
    assert_eq!(prompt_input_height(40, 80, &input), 5);
    assert_eq!(prompt_input_height(100, 80, &input), 5);

    // A long command grows the box so wrapped rows stay visible, but the
    // cap is 12 rows total and short terminals still win.
    input.set_text(&"x".repeat(1_000));
    assert_eq!(prompt_input_height(40, 80, &input), 12);
    assert_eq!(prompt_input_height(12, 80, &input), 7);
    assert_eq!(prompt_input_height(8, 80, &input), 4);
}

#[test]
fn esc_keeps_focus_in_the_prompt_until_a_double_press() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.focus = Focus::Input;
    app.input.enter_insert();

    // Leaving insert mode does not arm the gesture.
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(matches!(app.focus, Focus::Input));
    assert_eq!(app.input.mode(), Mode::Normal);
    assert!(app.esc_at.is_none());

    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(matches!(app.focus, Focus::Input));
    assert!(app.esc_at.is_some());

    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(matches!(app.focus, Focus::Transcript));
    assert!(app.esc_at.is_none());
}

#[test]
fn double_escape_after_insert_cancels_a_busy_session() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let mut session = Session::new("s1", "one", "/tmp");
    session.status = SessionStatus::Running;
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;
    app.focus = Focus::Input;
    app.input.enter_insert();

    // One Esc leaves insert mode; the next two within the window cancel.
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert_eq!(app.input.mode(), Mode::Normal);
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    match rx.try_recv().expect("cancel command") {
        Command::Cancel(session_id) => assert_eq!(session_id, "s1"),
        _ => panic!("expected a cancel command"),
    }
}

#[test]
fn double_escape_in_transcript_and_after_cmdline_cancels() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let mut session = Session::new("s1", "one", "/tmp");
    session.status = SessionStatus::Running;
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;
    app.focus = Focus::Transcript;

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    assert!(app.cmdline.is_some());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(app.cmdline.is_none());
    assert!(matches!(app.focus, Focus::Transcript));

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    match rx.try_recv().expect("cancel command") {
        Command::Cancel(session_id) => assert_eq!(session_id, "s1"),
        _ => panic!("expected a cancel command"),
    }
}

#[test]
fn ctrl_p_in_prompt_normal_mode_opens_the_palette() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.focus_input();

    // Insert mode still types a literal colon.
    app.handle_input_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    assert!(app.picker.is_none());
    assert!(app.cmdline.is_none());
    assert_eq!(app.input.text(), ":");

    // In normal mode Ctrl-P opens the interactive command palette instead
    // of editing the draft, matching the transcript binding.
    app.input.clear();
    app.input.enter_normal();
    app.handle_key(
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Commands)
    ));

    // A bare colon in normal mode opens the Vim command line, not the
    // palette, and leaves the draft untouched.
    app.picker = None;
    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    assert!(app.picker.is_none());
    assert!(app.cmdline.is_some());
    assert!(app.input.is_empty());
}

#[test]
fn colon_command_line_preserves_the_prompt_draft() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.set_text("draft");
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    assert!(app.cmdline.is_some());
    type_text(&mut app, &tx, "help");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(app.help);
    assert!(app.cmdline.is_none());
    assert_eq!(app.input.text(), "draft");
}

#[test]
fn command_line_colorscheme_opens_the_picker() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.set_text("draft");
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "colorscheme");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(app.cmdline.is_none());
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Themes)
    ));
    assert!(app.theme_picker_backup.is_some());
    assert_eq!(app.input.text(), "draft");
}

#[test]
fn command_line_bang_runs_a_shell_command() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "!printf hi");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(app.cmdline.is_none());
    match rx.try_recv().expect("shell command") {
        Command::RunShell {
            session_id,
            command,
            insert_output,
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(command, "printf hi");
            assert!(!insert_output);
        }
        _ => panic!("unexpected command"),
    }
}

#[test]
fn command_line_read_shell_inserts_output_into_the_prompt() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "r!printf hi");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(app.cmdline.is_none());
    app.handle_event(
        Event::ShellRan {
            ok: true,
            output: "hi\n".into(),
            insert_output: true,
        },
        &tx,
    );
    assert_eq!(app.input.text(), "hi");
    assert!(matches!(app.focus, Focus::Input));
    assert!(app.status.contains("read 1 line"));
}

#[test]
fn command_line_esc_cancels_without_running_the_command() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.set_text("draft");
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "quit");
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

    assert!(!app.should_quit);
    assert!(app.cmdline.is_none());
    assert_eq!(app.input.text(), "draft");
}

#[test]
fn command_line_edits_at_the_cursor() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "hep");
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "l");
    assert_eq!(
        app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
        Some("help")
    );
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(app.help);
}

#[test]
fn command_line_unknown_commands_keep_the_draft() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.set_text("draft");
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "nope");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(app
        .error
        .as_deref()
        .is_some_and(|error| error.contains("unknown command")));
    assert!(app.cmdline.is_none());
    assert_eq!(app.input.text(), "draft");
}

#[test]
fn command_line_history_browses_previous_entries() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.set_text("draft");
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "w");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert_eq!(app.status, "draft saved");
    assert_eq!(app.input.text(), "draft");

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    assert_eq!(
        app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
        Some("")
    );
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
    assert_eq!(
        app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
        Some("w")
    );
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
    assert_eq!(
        app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
        Some("")
    );

    // With the command menu closed, Ctrl-P/N keep browsing history.
    app.handle_key(
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(
        app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
        Some("w")
    );
    app.handle_key(
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(
        app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
        Some("")
    );
}

#[test]
fn command_line_tab_completes_command_names() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "hel");
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
    assert_eq!(
        app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
        Some("help")
    );
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.help);
}

#[test]
fn command_line_esc_dismisses_menu_before_closing() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "mod");
    assert!(!app.cmdline_menu_open());
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
    assert!(app.cmdline_menu_open());
    let text = app
        .cmdline
        .as_ref()
        .map(|cmdline| cmdline.text.clone())
        .expect("command line");

    // The first Esc closes only the popup and keeps the typed command.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(!app.cmdline_menu_open());
    assert!(!app.cmdline_menu_armed);
    assert_eq!(
        app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
        Some(text.as_str())
    );

    // A second Esc closes the command line itself.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(app.cmdline.is_none());
    assert_eq!(app.input.mode(), Mode::Normal);
}

#[test]
fn command_line_q_closes_the_pane_and_saves_the_draft() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1", "s2"]);
    app.focus_input();
    app.input.set_text("keep me");
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "q");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert_eq!(view_order(&app), vec!["s2"]);
    assert!(!app.should_clear_active_workspace());
    assert_eq!(app.input.mode(), Mode::Normal);
    let mut saved = None;
    while let Ok(command) = rx.try_recv() {
        if let Command::DraftSet { session_id, text } = command {
            if session_id == "s1" {
                saved = Some(text);
            }
        }
    }
    assert_eq!(saved.as_deref(), Some("keep me"));
}

#[test]
fn command_line_w_saves_and_returns_to_normal_mode() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.set_text("keep me");
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "w");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert_eq!(app.input.mode(), Mode::Normal);
    assert_eq!(app.input.text(), "keep me");
    let mut saved = None;
    while let Ok(command) = rx.try_recv() {
        if let Command::DraftSet { session_id, text } = command {
            if session_id == "s1" {
                saved = Some(text);
            }
        }
    }
    assert_eq!(saved.as_deref(), Some("keep me"));
}

#[test]
fn command_line_qa_quits_after_saving_the_draft() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.set_text("keep me");
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "qa");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(app.should_quit);
    assert!(app.should_clear_active_workspace());
    let mut saved = None;
    while let Ok(command) = rx.try_recv() {
        if let Command::DraftSet { session_id, text } = command {
            if session_id == "s1" {
                saved = Some(text);
            }
        }
    }
    assert_eq!(saved.as_deref(), Some("keep me"));
}

#[test]
fn command_line_is_visible_by_default() {
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.enter_normal();

    let text = render_text(&mut app, 80, 20);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.last().unwrap().trim().is_empty(), "{text}");
    assert!(!lines.last().unwrap().contains(':'), "{text}");
    assert!(
        status_bar_row(&text).trim_start().starts_with("N · Build"),
        "{text}"
    );
}

#[test]
fn command_line_renders_below_the_status_bar() {
    let mut app = app_with_panes(&["s1"]);
    app.focus_input();
    app.input.enter_normal();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    type_text(&mut app, &tx, "model");

    let text = render_text(&mut app, 80, 20);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.last().unwrap().trim_end(), ":model");
    assert!(
        status_bar_row(&text).trim_start().starts_with("C · Build"),
        "{text}"
    );

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    let text = render_text(&mut app, 80, 20);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.last().unwrap().trim().is_empty(), "{text}");
    assert!(
        status_bar_row(&text).trim_start().starts_with("N · Build"),
        "{text}"
    );
    assert!(!lines.last().unwrap().contains(":model"), "{text}");
}

#[test]
fn colon_from_the_transcript_opens_the_command_line() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus = Focus::Transcript;
    app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
    assert!(app.cmdline.is_some());
}

#[test]
fn ctrl_c_and_ctrl_d_quit_from_any_mode() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    for key in ['c', 'd'] {
        let mut app = App::new(None);
        app.focus_input();
        assert_eq!(app.input.mode(), Mode::Insert);
        app.handle_key(
            KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(app.should_quit, "Ctrl-{key} should quit from insert mode");
        assert!(!app.should_clear_active_workspace());

        let mut app = App::new(None);
        app.focus = Focus::Transcript;
        app.handle_key(
            KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(
            app.should_quit,
            "Ctrl-{key} should quit from the transcript"
        );
        assert!(!app.should_clear_active_workspace());
    }
}

#[test]
fn other_keys_break_the_double_esc_gesture() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.focus = Focus::Input;

    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    app.handle_input_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &tx);
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(matches!(app.focus, Focus::Input));
}

#[test]
fn transcript_leader_matches_the_prompt_leader() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.views.push(SessionView::new(
        Session::new("s2", "two", "/tmp"),
        Vec::new(),
    ));
    app.focus = Focus::Transcript;

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);
    assert!(app.picker.is_none());
    match rx.try_recv().expect("create command") {
        Command::Create { worktree, cwd, .. } => {
            assert_eq!(worktree, Some(false));
            assert_eq!(cwd.as_deref().and_then(|path| path.to_str()), Some("/tmp"));
        }
        _ => panic!("expected a create command"),
    }

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &tx);
    assert!(app.picker.is_none(), "leader+a is not the command palette");
    assert_eq!(app.status, "no pending approvals");

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT), &tx);
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Git)
    ));

    app.picker = None;
    app.handle_key(
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Commands)
    ));

    app.picker = None;
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE), &tx);
    assert!(!app.help, "leader+? no longer opens help");
    app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE), &tx);
    assert!(app.cmdline.is_some(), "bare ? opens the command line");
    assert!(!app.help, "? no longer opens help");
    app.cmdline = None;
    app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), &tx);
    assert!(app.help, "F1 still opens help");
}

#[test]
fn leader_hint_disappears_on_the_next_key() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus = Focus::Transcript;
    app.status = "previous status".into();

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    assert!(app.pending_leader);
    assert!(app.status.starts_with("leader:"));

    // An unknown leader key still dismisses the hint and restores the
    // status that was visible before `<Space>`.
    app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), &tx);
    assert!(!app.pending_leader);
    assert_eq!(app.status, "previous status");
    let text = render_text(&mut app, 80, 20);
    assert!(!text.contains("leader:"), "{text}");

    // A working leader action also replaces the hint rather than leaving
    // it on screen.
    app.focus = Focus::Transcript;
    app.status = "previous status".into();
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), &tx);
    assert_eq!(app.layout, PaneLayout::Horizontal);
    let text = render_text(&mut app, 80, 20);
    assert!(!text.contains("leader:"), "{text}");
}

#[test]
fn transcript_pane_requires_leader_for_pane_actions_except_q() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus = Focus::Transcript;

    // Bare letters are inert: no pane action, no accidental quit. `q` is
    // the one exception and is checked below.
    for ch in ['o', 'p', 'n', 'N', 'c', 'w', 'b', 'x', 'a'] {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &tx);
        assert!(app.picker.is_none(), "bare {ch} opened a picker");
        assert!(app.process_view.is_none(), "bare {ch} opened processes");
        assert!(!app.should_quit, "bare {ch} quit the app");
    }
    assert!(!app.creating, "bare n created a session");
    assert!(rx.try_recv().is_err(), "bare keys sent a command");

    // `q` closes the focused pane, except when it is the last one.
    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &tx);
    assert_eq!(app.views.len(), 1, "bare q closed the last pane");
    assert!(!app.should_quit, "bare q quit the app");
    assert_eq!(app.status, "cannot close the last pane");
    let _ = rx.try_recv();

    // `i` is the vim-style exception: it focuses the prompt in insert mode.
    app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &tx);
    assert!(matches!(app.focus, Focus::Input));
    assert_eq!(app.input.mode(), Mode::Insert);
    app.focus = Focus::Transcript;

    // leader+s opens the content picker instead of switching sessions.
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &tx);
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Grep)
    ));
    app.picker = None;

    // The leader retains the action.
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &tx);
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::NewPane)
    ));

    app.picker = None;
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &tx);
    assert!(app.process_view.is_some());

    // Ctrl-P opens the command palette from the transcript.
    app.process_view = None;
    app.handle_key(
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Commands)
    ));

    // Focus still lives behind the leader.
    app.picker = None;
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), &tx);
    assert_eq!(app.layout, PaneLayout::Horizontal);
    assert!(matches!(app.focus, Focus::Transcript));

    // Shift+Z quits from normal mode; leader+Q no longer does.
    app.focus = Focus::Transcript;
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::NONE), &tx);
    assert!(!app.should_quit);
    app.handle_key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT), &tx);
    assert!(app.should_quit);
}

#[test]
fn shift_z_quits_from_prompt_normal_mode() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus_input();
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT), &tx);
    assert!(app.should_quit);
}

#[test]
fn new_worktree_moves_the_focused_session_and_warns_when_it_already_has_one() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;

    app.run_action("new-worktree", &tx);
    match rx.try_recv().expect("worktree command") {
        Command::CreateWorktree { session_id, name } => {
            assert_eq!(session_id, "s1");
            assert!(name.is_none());
        }
        _ => panic!("expected a worktree command"),
    }

    let mut moved = app.views[0].session.clone();
    moved.worktree = Some("/tmp/.worktrees/one".into());
    app.handle_event(Event::Updated(moved), &tx);

    app.run_action("new-worktree", &tx);
    assert!(
        rx.try_recv().is_err(),
        "an existing worktree must not create another one"
    );
    assert_eq!(app.status, "this session already has a worktree");
}

#[test]
fn new_session_reuses_the_focused_worktree_without_creating_one() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let mut session = Session::new("s1", "one", "/tmp/repo/.worktrees/feat/src");
    session.worktree = Some("/tmp/repo/.worktrees/feat".into());
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;

    assert!(app.handle_command_text("/new feature", &tx, false));
    match rx.try_recv().expect("create command") {
        Command::Create {
            worktree,
            cwd,
            name,
            ..
        } => {
            assert_eq!(worktree, Some(false), "must never create a second worktree");
            assert_eq!(
                cwd,
                Some(std::path::PathBuf::from("/tmp/repo/.worktrees/feat/src"))
            );
            assert_eq!(name.as_deref(), Some("feature"));
        }
        _ => panic!("expected a create command"),
    }
    assert!(!app.commands.iter().any(|spec| spec.name == "new-shared"));
    assert!(app.handle_command_text("/new-shared", &tx, false));
    assert!(app
        .error
        .as_deref()
        .is_some_and(|error| error.contains("unknown command /new-shared")));
}

#[test]
fn leader_n_creates_a_session_and_leader_q_does_not_close_a_pane() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.views.push(SessionView::new(
        Session::new("s2", "two", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus_input();
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);
    assert!(app.picker.is_none());
    match rx.try_recv().expect("create command") {
        Command::Create { worktree, cwd, .. } => {
            assert_eq!(worktree, Some(false));
            assert_eq!(cwd.as_deref().and_then(|path| path.to_str()), Some("/tmp"));
        }
        _ => panic!("expected a create command"),
    }

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &tx);
    assert_eq!(app.views.len(), 2, "leader+q no longer closes a pane");
}

#[test]
fn leader_n_keeps_the_prompt_in_normal_mode_after_attaching() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus_input();
    app.input.enter_normal();
    app.input_session = Some("s1".to_string());

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);

    let created = Session::new("s2", "two", "/tmp");
    app.handle_event(Event::Created(created.clone()), &tx);
    app.handle_event(
        Event::Attached(AttachResult {
            session: created,
            events: Vec::new(),
            has_more: false,
        }),
        &tx,
    );
    app.tick(&tx);

    assert_eq!(app.focused, 1);
    assert!(matches!(app.focus, Focus::Input));
    assert_eq!(app.input.mode(), Mode::Normal);
}

#[test]
fn q_closes_the_focused_pane_and_never_quits_on_the_last() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.views.push(SessionView::new(
        Session::new("s2", "two", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus = Focus::Input;
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &tx);
    assert_eq!(app.views.len(), 1);
    assert_eq!(app.views[0].session.id, "s2");
    assert!(!app.should_quit);

    // The transcript accepts the same key, but the last pane stays open.
    app.focus = Focus::Transcript;
    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &tx);
    assert_eq!(app.views.len(), 1);
    assert!(!app.should_quit);
    assert_eq!(app.status, "cannot close the last pane");
}

#[test]
fn leader_c_closes_the_focused_pane() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.views.push(SessionView::new(
        Session::new("s2", "two", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus = Focus::Transcript;

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &tx);

    assert_eq!(app.views.len(), 1);
}

#[test]
fn prompt_normal_mode_matches_transcript_for_tui_actions() {
    fn pane_app() -> App {
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.views.push(SessionView::new(
            Session::new("s2", "two", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.keybinds = Keybinds::from_config(&std::collections::BTreeMap::from([(
            "help".to_string(),
            "F2".to_string(),
        )]));
        app
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Snapshot {
        help: bool,
        processes: bool,
        picker: Option<u8>,
        panes: usize,
        focused: usize,
        scroll: Option<(usize, bool)>,
        leader: bool,
    }

    fn snapshot(app: &App) -> Snapshot {
        Snapshot {
            help: app.help,
            processes: app.process_view.is_some(),
            picker: app.picker.as_ref().map(|picker| picker.kind as u8),
            panes: app.views.len(),
            focused: app.focused,
            scroll: app.focused_view().map(|view| (view.scroll, view.follow)),
            leader: app.pending_leader,
        }
    }

    let scenarios = vec![
        vec![KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT)],
        vec![KeyEvent::new(KeyCode::Char('#'), KeyModifiers::SHIFT)],
        vec![KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)],
        vec![KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)],
        vec![KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)],
        vec![
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        ],
        vec![KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)],
        vec![
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        ],
        vec![
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        ],
        vec![
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        ],
        vec![
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
        ],
        vec![
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        ],
    ];

    for keys in scenarios {
        let (pane_tx, _pane_rx) = tokio::sync::mpsc::unbounded_channel();
        let (prompt_tx, _prompt_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut pane = pane_app();
        pane.focus = Focus::Transcript;
        let mut prompt = pane_app();
        prompt.focus_input();
        prompt.input.enter_normal();

        for key in keys.clone() {
            pane.handle_key(key, &pane_tx);
            prompt.handle_key(key, &prompt_tx);
        }

        assert_eq!(
            snapshot(&pane),
            snapshot(&prompt),
            "transcript and prompt normal mode disagreed on {keys:?}"
        );
    }
}

#[test]
fn config_remaps_global_and_leader_actions() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    for id in ["s1", "s2"] {
        app.views
            .push(SessionView::new(Session::new(id, id, "/tmp"), Vec::new()));
    }
    app.focused = 0;
    app.focus = Focus::Transcript;
    app.keybinds = Keybinds::from_config(&std::collections::BTreeMap::from([
        ("sessions".to_string(), "C-t".to_string()),
        ("new_session".to_string(), "leader N".to_string()),
        ("help".to_string(), "F2".to_string()),
        ("close_pane".to_string(), "C-q".to_string()),
        ("quit".to_string(), "C-x".to_string()),
    ]));

    // Ctrl-G is no longer a default, and the sessions override removes the
    // leader-g default too. Ctrl-T wins over reasoning because overrides
    // are matched first.
    app.handle_key(
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(app.picker.is_none());
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &tx);
    assert!(app.picker.is_none());
    app.handle_key(
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Sessions)
    ));
    app.picker = None;

    // Leader actions can be remapped independently of the prefix.
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT), &tx);
    assert!(matches!(rx.try_recv(), Ok(Command::Create { .. })));

    // F1 is replaced by F2.
    app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE), &tx);
    assert!(app.help);
    app.help = false;
    app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), &tx);
    assert!(!app.help);

    // q and Z are inert after their actions move elsewhere.
    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &tx);
    assert_eq!(app.views.len(), 2);
    app.handle_key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT), &tx);
    assert!(!app.should_quit);
    app.handle_key(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(!app.should_quit);

    app.handle_key(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.views.len(), 1);
    app.handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(app.should_quit);
}

#[test]
fn config_leader_prefix_replaces_space() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus = Focus::Transcript;
    app.keybinds = Keybinds::from_config(&std::collections::BTreeMap::from([
        ("leader".to_string(), "C-b".to_string()),
        ("new_session".to_string(), "leader n".to_string()),
    ]));

    // Space is no longer a leader; the old chord must stay inert.
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    assert!(!app.pending_leader);
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);
    assert!(rx.try_recv().is_err());

    // Ctrl-B is now the leader, even though it is also a default page-up
    // binding; the leader prefix has priority.
    app.handle_key(
        KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(app.pending_leader);
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);
    assert!(matches!(rx.try_recv(), Ok(Command::Create { .. })));
}

#[test]
fn config_bare_letter_is_normal_only() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus = Focus::Transcript;
    app.keybinds = Keybinds::from_config(&std::collections::BTreeMap::from([(
        "sessions".to_string(),
        "o".to_string(),
    )]));

    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &tx);
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Sessions)
    ));
    app.picker = None;

    // In insert mode the bare letter types instead of opening the picker.
    app.focus_input();
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &tx);
    assert_eq!(app.input.text(), "o");
    assert!(app.picker.is_none());
}

#[test]
fn prompt_normal_mode_ctrl_b_f_page_the_transcript() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let events: Vec<_> = (1..=30)
        .map(|seq| {
            SessionEvent::new(
                "s1",
                seq,
                EventKind::UserMessage,
                serde_json::json!({ "text": format!("turn {seq}") }),
            )
        })
        .collect();
    let mut app = App::new(None);
    app.views
        .push(SessionView::new(Session::new("s1", "one", "/tmp"), events));
    app.focused = 0;
    app.connected = true;
    app.focus_input();
    app.input.enter_normal();
    let _ = render_text(&mut app, 80, 12);
    let max = app.views[0]
        .text_lines
        .len()
        .saturating_sub(app.pane_areas[0].height.saturating_sub(1) as usize);
    app.views[0].follow = true;

    // Already at the tail: paging down keeps following.
    app.handle_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.views[0].scroll, max);
    assert!(app.views[0].follow);

    // Paging up pauses follow, and paging back down resumes it.
    app.handle_key(
        KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.views[0].scroll, max.saturating_sub(10));
    assert!(!app.views[0].follow);
    app.handle_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.views[0].scroll, max);
    assert!(app.views[0].follow);
}

#[test]
fn scrolling_near_the_tail_resumes_following() {
    let events: Vec<_> = (1..=30)
        .map(|seq| {
            SessionEvent::new(
                "s1",
                seq,
                EventKind::UserMessage,
                serde_json::json!({ "text": format!("turn {seq}") }),
            )
        })
        .collect();
    let mut app = App::new(None);
    app.views
        .push(SessionView::new(Session::new("s1", "one", "/tmp"), events));
    app.focused = 0;
    app.connected = true;
    let _ = render_text(&mut app, 80, 12);
    let max = app.views[0]
        .text_lines
        .len()
        .saturating_sub(app.pane_areas[0].height.saturating_sub(1) as usize);

    // A wheel-up from the tail pauses exactly one line above it.
    app.views[0].follow = true;
    app.views[0].scroll = 0;
    app.scroll_focused(-1);
    assert_eq!(app.views[0].scroll, max.saturating_sub(1));
    assert!(!app.views[0].follow);

    // One line down is still paused; two lines from the tail re-attaches.
    app.views[0].scroll = max.saturating_sub(4);
    app.views[0].follow = false;
    app.scroll_focused(1);
    assert_eq!(app.views[0].scroll, max.saturating_sub(3));
    assert!(!app.views[0].follow);
    app.scroll_focused(1);
    assert_eq!(app.views[0].scroll, max);
    assert!(app.views[0].follow);
}

#[test]
fn scrolling_up_an_empty_transcript_keeps_following() {
    let mut app = app_with_panes(&["s1"]);
    app.focused = 0;
    app.views[0].follow = true;
    app.scroll_focused(-3);
    assert_eq!(app.views[0].scroll, 0);
    assert!(app.views[0].follow);
}

#[test]
fn ctrl_b_f_page_dialog_overlays() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let control = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);

    // Help filter results.
    let mut app = App::new(None);
    app.open_help();
    let help_last = help_entry_count("").saturating_sub(1);
    app.handle_key(control('f'), &tx);
    assert_eq!(app.help_selected, 8.min(help_last));
    app.handle_key(control('b'), &tx);
    assert_eq!(app.help_selected, 0);
    app.help = false;

    // Picker rows.
    app.sessions = (0..15)
        .map(|i| Session::new(format!("s{i}"), format!("s{i}"), "/tmp"))
        .collect();
    app.open_picker(PickerKind::Sessions, &tx);
    assert_eq!(app.picker.as_ref().unwrap().selected, 0);
    app.handle_key(control('f'), &tx);
    assert_eq!(app.picker.as_ref().unwrap().selected, 10);
    app.handle_key(control('b'), &tx);
    assert_eq!(app.picker.as_ref().unwrap().selected, 0);
    app.picker = None;

    // Cross-session approval inbox.
    app.inbox = Some(InboxView {
        items: (0..15)
            .map(|i| InboxItem {
                session_id: format!("s{i}"),
                session_name: format!("s{i}"),
                tool: "run_command".into(),
                summary: format!("item {i}"),
                kind: PendingKind::Tool,
            })
            .collect(),
        cursor: 0,
    });
    app.handle_key(control('f'), &tx);
    assert_eq!(app.inbox.as_ref().unwrap().cursor, 10);
    app.handle_key(control('b'), &tx);
    assert_eq!(app.inbox.as_ref().unwrap().cursor, 0);
    app.inbox = None;

    // Background process output.
    let mut processes = ProcessView::new("s1".into());
    processes.processes = (0..15)
        .map(|i| ProcessInfo {
            id: format!("p{i}"),
            session_id: "s1".into(),
            command: format!("cmd {i}"),
            cwd: "/tmp".into(),
            pid: i as u32,
            status: ProcessStatus::Running,
            exit_code: None,
            started_ms: 0,
            ended_ms: None,
            bytes: 0,
            truncated: false,
            log: String::new(),
        })
        .collect();
    processes.scroll = 100;
    app.process_view = Some(processes);
    app.handle_key(control('f'), &tx);
    assert_eq!(app.process_view.as_ref().unwrap().scroll, 80);
    app.handle_key(control('b'), &tx);
    assert_eq!(app.process_view.as_ref().unwrap().scroll, 100);
    app.process_view = None;

    // `/btw` side-question overlay.
    app.btw = Some(BtwView {
        session_id: "s1".into(),
        btw_id: "b1".into(),
        question: "why?".into(),
        text: "answer".into(),
        error: None,
        done: true,
        scroll: 0,
    });
    app.handle_key(control('f'), &tx);
    assert_eq!(app.btw.as_ref().unwrap().scroll, 8);
    app.handle_key(control('b'), &tx);
    assert_eq!(app.btw.as_ref().unwrap().scroll, 0);

    // Diff review.
    let mut diff_app = app_with_hunks(
        (0..20)
            .map(|i| hunk(i, "@@ -1 +1 @@", &["@@ -1 +1 @@", "-a", "+A"]))
            .collect(),
    );
    diff_app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
    assert!(diff_app.diff_view.is_some());
    diff_app.handle_key(control('f'), &tx);
    assert_eq!(diff_app.diff_view.as_ref().unwrap().scroll, 10);
    diff_app.handle_key(control('b'), &tx);
    assert_eq!(diff_app.diff_view.as_ref().unwrap().scroll, 0);
}

#[test]
fn shifted_angle_brackets_cycle_panes() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    for (id, name) in [("s1", "one"), ("s2", "two"), ("s3", "three")] {
        app.views
            .push(SessionView::new(Session::new(id, name, "/tmp"), Vec::new()));
    }
    app.focused = 0;
    app.focus = Focus::Transcript;

    for expected in [1, 2, 0] {
        app.handle_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT), &tx);
        assert_eq!(app.focused, expected);
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::SHIFT), &tx);
    assert_eq!(app.focused, 2);

    // The prompt's normal mode uses the same bindings; insert mode does not.
    app.focus_input();
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT), &tx);
    assert_eq!(app.focused, 0);
    app.handle_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::SHIFT), &tx);
    assert_eq!(app.focused, 2);
}

#[test]
fn double_ctrl_w_cycles_panes_and_wraps() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let ctrl_w = || KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);

    for focus in [Focus::Transcript, Focus::Input] {
        let mut app = app_with_panes(&["s1", "s2", "s3"]);
        app.focused = 0;
        if focus == Focus::Input {
            app.focus_input();
            app.input.enter_normal();
        } else {
            app.focus = Focus::Transcript;
        }

        // A single Ctrl-W only arms the gesture.
        app.handle_key(ctrl_w(), &tx);
        assert_eq!(app.focused, 0);
        for expected in [1, 2, 0] {
            app.handle_key(ctrl_w(), &tx);
            assert_eq!(app.focused, expected);
            if expected != 0 {
                app.handle_key(ctrl_w(), &tx);
            }
        }
        assert!(app.focus == focus, "pane focus changed");
    }

    // A stale first press does not pair with a much later one.
    let mut app = app_with_panes(&["s1", "s2"]);
    app.focused = 0;
    app.focus = Focus::Transcript;
    app.ctrl_w_at = Some(Instant::now() - CTRL_W_WINDOW);
    app.handle_key(ctrl_w(), &tx);
    assert_eq!(app.focused, 0);
    app.handle_key(ctrl_w(), &tx);
    assert_eq!(app.focused, 1);

    // Another key between the two presses also breaks the gesture.
    app.focused = 0;
    app.ctrl_w_at = None;
    app.handle_key(ctrl_w(), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &tx);
    app.handle_key(ctrl_w(), &tx);
    assert_eq!(app.focused, 0);
}

#[test]
fn normal_mode_enter_promotes_the_focused_session() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    for focus in [Focus::Transcript, Focus::Input] {
        let mut app = app_with_panes(&["a", "b", "c"]);
        app.focused = 2;
        if focus == Focus::Input {
            app.focus_input();
            app.input.enter_normal();
        } else {
            app.focus = Focus::Transcript;
        }

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert_eq!(view_order(&app), vec!["c", "a", "b"]);
        assert_eq!(app.focused, 0);
        assert_eq!(app.focused_session_id().as_deref(), Some("c"));
        assert!(app.focus == focus, "Enter changed the focus");
        assert_eq!(app.status, "session c promoted to first");
        if focus == Focus::Input {
            assert_eq!(app.input.mode(), Mode::Normal);
        }
    }
}

#[test]
fn ctrl_w_still_deletes_a_word_in_insert_mode() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1", "s2"]);
    app.focused = 0;
    app.focus_input();
    type_text(&mut app, &tx, "one two");

    app.handle_key(
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.focused, 0);
    assert_eq!(app.input.text(), "one ");
}

#[test]
fn leader_o_opens_the_new_pane_picker() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.focus_input();
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &tx);
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::NewPane)
    ));
}

#[test]
fn picker_accepts_ctrl_j_and_ctrl_k() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.sessions = vec![
        Session::new("s1", "one", "/tmp"),
        Session::new("s2", "two", "/tmp"),
        Session::new("s3", "three", "/tmp"),
    ];
    app.open_picker(PickerKind::Sessions, &tx);
    assert_eq!(app.picker.as_ref().unwrap().selected, 0);

    app.handle_picker_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.picker.as_ref().unwrap().selected, 1);
    app.handle_picker_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.picker.as_ref().unwrap().selected, 0);
}

#[test]
fn pane_title_omits_read_only_badge() {
    let mut app = app_with_panes(&["plan-pane"]);
    app.focused = 0;
    app.views[0].session.mode = SessionMode::Plan;
    let rendered = render_text(&mut app, 80, 20);
    let title = rendered.lines().next().expect("pane title");
    assert!(title.contains("plan-pane"), "{title}");
    assert!(!title.contains(" ro"), "{title}");
}

#[test]
fn status_bar_keeps_every_session_mode_aligned() {
    fn left(app: &App) -> String {
        app.status_bar_left()
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    let mut app = App::new(None);
    app.connected = true;
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.views[0].session.model = Some("llmgate/deepseek/deepseek-v4.1-flash#high".into());
    app.models.push(ModelInfo {
        id: "llmgate/deepseek/deepseek-v4.1-flash".into(),
        api_model: Some("deepseek/deepseek-v4.1-flash".into()),
        name: "deepseek/deepseek-v4.1-flash".into(),
        provider: "llmgate".into(),
        default: false,
        variants: vec!["high".into()],
        input_cost: None,
        output_cost: None,
        context_tokens: None,
    });

    app.views[0].session.mode = SessionMode::Build;
    let reference = left(&app);
    assert_eq!(
        reference,
        " N · Build  · ⠶ · deepseek-v4.1-flash · llmgate · high"
    );
    let first_dot = reference.find('·').expect("separator");

    // Every label is padded to Bypass's width, so neither the total line
    // length nor the first separator moves when the mode changes.
    for mode in [SessionMode::Plan, SessionMode::Auto, SessionMode::Bypass] {
        app.views[0].session.mode = mode;
        let line = left(&app);
        assert_eq!(line.len(), reference.len(), "{mode:?}: {line}");
        assert_eq!(line.find('·'), Some(first_dot), "{mode:?}: {line}");
        assert_eq!(
            app.status_bar_left().spans[2].content.chars().count(),
            SESSION_MODE_WIDTH,
            "{mode:?}"
        );
    }
}

#[test]
fn status_bar_colors_every_session_mode_differently() {
    let mut app = App::new(None);
    app.connected = true;
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;

    let mut colors = Vec::new();
    for mode in [
        SessionMode::Plan,
        SessionMode::Build,
        SessionMode::Auto,
        SessionMode::Bypass,
    ] {
        app.views[0].session.mode = mode;
        let line = app.status_bar_left();
        let badge = &line.spans[2];
        assert_eq!(badge.content.trim(), mode.label());
        colors.push(badge.style.fg.expect("mode badge color"));
    }
    for (index, color) in colors.iter().enumerate() {
        for other in &colors[index + 1..] {
            assert_ne!(color, other, "{colors:?}");
        }
    }
}

#[test]
fn badge_text_contrasts_with_light_and_dark_accents() {
    assert_eq!(
        badge_text_color(Color::Rgb(250, 250, 250)),
        COMPOSER_BADGE_TEXT
    );
    assert_eq!(
        badge_text_color(Color::Rgb(3, 47, 98)),
        COMPOSER_BADGE_TEXT_LIGHT
    );
    assert_eq!(badge_text_color(Color::Cyan), COMPOSER_BADGE_TEXT);
}

#[test]
fn session_mode_colors_stay_distinct_and_readable_in_every_theme() {
    let mut themes = vec![Theme::default()];
    themes.extend(THEME_PRESETS.iter().map(|preset| {
        Theme::from_config(&eix_core::ThemeConfig {
            preset: Some(preset.name.to_string()),
            ..Default::default()
        })
    }));

    for theme in themes {
        let colors: Vec<Color> = [
            SessionMode::Plan,
            SessionMode::Build,
            SessionMode::Auto,
            SessionMode::Bypass,
        ]
        .into_iter()
        .map(|mode| session_mode_color(mode, theme))
        .collect();
        for (index, color) in colors.iter().enumerate() {
            for other in &colors[index + 1..] {
                assert_ne!(
                    color, other,
                    "mode colors collide for preset {:?}: {colors:?}",
                    theme.preset
                );
            }
        }
        if let Some(Color::Rgb(red, green, blue)) = theme.background {
            let background = relative_luminance(red, green, blue);
            for color in &colors {
                let Color::Rgb(red, green, blue) = *color else {
                    continue;
                };
                assert!(
                    contrast_ratio(relative_luminance(red, green, blue), background)
                        >= MIN_ACCENT_CONTRAST,
                    "mode color {color:?} is too faint on preset {:?}",
                    theme.preset
                );
            }
        }
    }
}

#[test]
fn prompt_header_uses_icon_for_idle_and_animated_running() {
    let mut app = App::new(None);
    assert_eq!(app.status_glyph(SessionStatus::Idle), "⠶");
    assert_eq!(app.status_glyph(SessionStatus::Running), "◰");
    app.tick = 1;
    assert_eq!(app.status_glyph(SessionStatus::Running), "◳");
    app.tick = 2;
    assert_eq!(app.status_glyph(SessionStatus::Running), "◲");
    app.tick = 3;
    assert_eq!(app.status_glyph(SessionStatus::Running), "◱");
    app.tick = 4;
    assert_eq!(app.status_glyph(SessionStatus::Running), "◰");
    assert_eq!(
        app.status_glyph(SessionStatus::AwaitingApproval),
        "⚠ awaiting_approval"
    );
}

#[test]
fn running_pane_icon_rotates_and_uses_status_color() {
    let mut app = App::new(None);
    let mut session = Session::new("s1", "one", "/tmp");
    session.status = SessionStatus::Running;
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;
    app.connected = true;
    app.tick = 0;

    let backend = ratatui::backend::TestBackend::new(80, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let icons: Vec<_> = buffer
        .content
        .iter()
        .filter(|cell| cell.symbol() == "◰")
        .collect();
    assert!(!icons.is_empty(), "running icon missing");
    assert!(
        icons
            .iter()
            .all(|cell| cell.fg == composer_success(app.theme)),
        "running icon should be green"
    );

    app.tick = 1;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        buffer.content.iter().any(|cell| cell.symbol() == "◳"),
        "running icon did not advance"
    );

    app.views[0].session.status = SessionStatus::Idle;
    app.tick = 0;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let icons: Vec<_> = buffer
        .content
        .iter()
        .filter(|cell| cell.symbol() == "⠶")
        .collect();
    assert!(!icons.is_empty(), "idle icon missing");
    assert!(
        icons
            .iter()
            .all(|cell| cell.fg == composer_muted(app.theme)),
        "idle icon should be gray"
    );
}

#[test]
fn status_bar_shows_git_branch_without_worktree_badge() {
    let mut app = App::new(None);
    let cwd = std::path::PathBuf::from("/tmp/eix-repo");
    let mut session = Session::new("s1", "one", cwd.clone());
    session.worktree = Some(cwd.join(".worktrees/feat"));
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;
    app.git_cwd = Some(cwd);
    app.git_branch = Some("eix/feat".into());
    app.git_checked = Some(Instant::now());

    let workspace = app.status_bar_workspace().expect("workspace");
    let workspace = line_text(&workspace);
    assert!(workspace.contains("eix-repo"), "{workspace}");
    assert!(workspace.contains("eix/feat"), "{workspace}");
    assert!(
        app.status_bar_middle().is_none(),
        "a worktree badge should not occupy the status line"
    );

    // A cached branch for another cwd must not leak into this session.
    app.git_cwd = Some(std::path::PathBuf::from("/tmp/other"));
    let workspace = app.status_bar_workspace().expect("workspace");
    let workspace = line_text(&workspace);
    assert!(!workspace.contains("eix/feat"), "{workspace}");
}

#[test]
fn mode_feedback_stays_out_of_toasts() {
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;

    app.status = "mode: bypass".into();
    let text = render_text(&mut app, 80, 20);
    let bar = status_bar_row(&text);
    assert!(!bar.contains("mode: bypass"), "{bar}");
    assert!(
        !text
            .lines()
            .nth(1)
            .is_some_and(|line| line.contains("mode: bypass")),
        "{text}"
    );
    assert!(app.status_bar_middle().is_none());
    assert!(app.toasts.is_empty());

    app.integrations.insert(
        "s1".into(),
        IntegrationState {
            session_id: "s1".into(),
            status: IntegrationStatus::Merged,
            ..IntegrationState::default()
        },
    );
    app.status = "integration · merged".into();
    let text = render_text(&mut app, 80, 20);
    assert!(!text.contains("integration · merged"), "{text}");
    assert!(app.status_bar_middle().is_none());
    assert!(app.toasts.is_empty());
}

#[test]
fn command_feedback_stays_out_of_toasts() {
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    for status in [
        "patch applied: go/main.go",
        "patch staged: go/main.go",
        "process abcd1234 started: go test",
        "process abcd1234 exited (0)",
        "running shell command…",
        "reading shell output…",
        "approval sent",
        "answer sent",
        "answered: Option A",
        "draft saved",
        "config reloaded",
        "no changes",
    ] {
        app.status = status.into();
        let text = render_text(&mut app, 80, 20);
        assert!(
            !text
                .lines()
                .nth(1)
                .is_some_and(|line| line.contains(status)),
            "{status}: {text}"
        );
        let bar = status_bar_row(&text);
        assert!(!bar.contains(status), "{status}: {bar}");
        assert!(app.status_bar_middle().is_none(), "{status}");
        assert!(app.toasts.is_empty(), "{status}");
    }
}

#[test]
fn config_reload_finishes_with_a_top_right_toast() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);

    app.handle_event(
        Event::ConfigReloaded {
            paths: vec!["/tmp/global.yaml".into(), "/tmp/project.yaml".into()],
            max_parallel_subagents: 8,
        },
        &tx,
    );

    assert_eq!(app.status, "config reloaded (2 layer(s))");
    assert_eq!(app.subagent_concurrency, 8);
    let toast = app.toasts.last().expect("reload toast");
    assert_eq!(toast.text, "config reloaded (2 layer(s))");
    assert_eq!(toast.tone, ToastTone::Success);

    let text = render_text(&mut app, 80, 20);
    assert!(
        text.lines()
            .nth(1)
            .is_some_and(|line| line.contains("config reloaded (2 layer(s))")),
        "{text}"
    );
}

#[test]
fn config_reload_refreshes_user_commands() {
    let dir = std::env::temp_dir().join(format!(
        "eix-commands-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let commands_dir = dir.join(".eix/cmds");
    std::fs::create_dir_all(&commands_dir).unwrap();
    std::fs::write(commands_dir.join("live.md"), "first\n").unwrap();

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", dir.clone()),
        Vec::new(),
    ));
    app.focused = 0;
    app.set_compat(eix_core::CompatConfig::default());

    let template = |app: &App, name: &str| {
        app.commands
            .iter()
            .find(|spec| spec.name == name)
            .and_then(|spec| spec.template.clone())
    };
    assert_eq!(template(&app, "live").as_deref(), Some("first"));

    std::fs::write(commands_dir.join("live.md"), "second\n").unwrap();
    std::fs::write(commands_dir.join("new.md"), "new\n").unwrap();
    app.handle_event(
        Event::ConfigReloaded {
            paths: Vec::new(),
            max_parallel_subagents: 4,
        },
        &tx,
    );

    assert_eq!(template(&app, "live").as_deref(), Some("second"));
    assert_eq!(template(&app, "new").as_deref(), Some("new"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prompt_normal_mode_tab_cycles_modes() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus_input();
    app.status = "ready".into();
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
    assert_eq!(app.views[0].session.mode, SessionMode::Auto);
    // The mode lives in the status bar, not in the transient app status.
    assert_eq!(app.status, "ready");
    match rx.try_recv().expect("set auto mode") {
        Command::SetMode {
            session_id,
            mode,
            temporary,
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(mode, SessionMode::Auto);
            assert!(!temporary);
        }
        _ => panic!("unexpected command"),
    }

    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
    assert_eq!(app.views[0].session.mode, SessionMode::Bypass);
    assert_eq!(app.status, "ready");
    match rx.try_recv().expect("set bypass mode") {
        Command::SetMode {
            session_id, mode, ..
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(mode, SessionMode::Bypass);
        }
        _ => panic!("unexpected command"),
    }
}

#[test]
fn prompt_normal_mode_shift_tab_cycles_backward() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus_input();
    app.status = "ready".into();
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

    // Build → Plan, then Plan → Bypass. The reverse walk stays available
    // from the prompt without a round-trip through the transcript.
    for (key, expected) in [
        (
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            SessionMode::Plan,
        ),
        (
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
            SessionMode::Bypass,
        ),
    ] {
        app.handle_input_key(key, &tx);
        assert_eq!(app.views[0].session.mode, expected);
        assert_eq!(app.status, "ready");
        match rx.try_recv().expect("set mode") {
            Command::SetMode {
                session_id, mode, ..
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(mode, expected);
            }
            _ => panic!("unexpected command"),
        }
    }
}

#[test]
fn switching_a_manual_mode_to_build_restores_write_access() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let mut session = Session::new("s1", "one", "/tmp");
    session.mode = SessionMode::Plan;
    session.agent = Some("reviewer".into());
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;

    app.toggle_mode(&tx);

    assert_eq!(app.views[0].session.mode, SessionMode::Build);
    // The custom agent stays selected; mode is independent of the agent.
    assert_eq!(app.views[0].session.agent.as_deref(), Some("reviewer"));
    match rx.try_recv().expect("set build mode") {
        Command::SetMode { mode, .. } => assert_eq!(mode, SessionMode::Build),
        _ => panic!("unexpected command"),
    }
}

#[test]
fn tab_and_shift_tab_cycle_modes_but_never_switch_sessions() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1", "s2"]);
    app.focus = Focus::Transcript;

    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
    assert_eq!(app.views[0].session.mode, SessionMode::Auto);
    match rx.try_recv().expect("set auto mode") {
        Command::SetMode {
            session_id, mode, ..
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(mode, SessionMode::Auto);
        }
        _ => panic!("unexpected command"),
    }

    app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &tx);
    assert_eq!(app.focused, 0, "Shift-Tab never switches sessions");
    assert_eq!(app.views[0].session.mode, SessionMode::Build);
    match rx.try_recv().expect("set build mode") {
        Command::SetMode {
            session_id, mode, ..
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(mode, SessionMode::Build);
        }
        _ => panic!("unexpected command"),
    }
}

#[test]
fn prompt_normal_mode_ctrl_j_and_k_browse_submitted_history() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let events = vec![
        SessionEvent::new(
            "s1",
            1,
            EventKind::UserMessage,
            serde_json::json!({ "text": "first" }),
        ),
        SessionEvent::new(
            "s1",
            2,
            EventKind::UserMessage,
            serde_json::json!({ "text": "second" }),
        ),
    ];
    app.views
        .push(SessionView::new(Session::new("s1", "one", "/tmp"), events));
    app.focused = 0;
    app.focus_input();
    app.input.set_text("draft");
    app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

    // Ctrl-K recalls newest first; the live draft is restored by Ctrl-J.
    app.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.input.text(), "second");
    app.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.input.text(), "first");
    app.handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.input.text(), "second");
    app.handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.input.text(), "draft");
    assert!(app.history_index.is_none());
}

#[test]
fn leader_actions_dispatch_like_palette_commands() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);

    app.run_action("help", &tx);
    assert!(app.help);

    app.help = false;
    app.run_action("sessions", &tx);
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Sessions)
    ));

    app.picker = None;
    app.run_action("not-an-action", &tx);
    assert!(app.picker.is_none());
}

fn input_app() -> (
    App,
    UnboundedSender<Command>,
    tokio::sync::mpsc::UnboundedReceiver<Command>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "ask", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus = Focus::Transcript;
    app.inputs.insert(
        "s1".to_string(),
        PendingInput {
            input_id: "i1".to_string(),
            call_id: "c1".to_string(),
            question: "Which database?".to_string(),
            options: vec![
                InputOption {
                    label: "SQLite".to_string(),
                    description: Some("embedded".to_string()),
                },
                InputOption {
                    label: "Postgres".to_string(),
                    description: None,
                },
                InputOption {
                    label: "MySQL".to_string(),
                    description: None,
                },
            ],
            cursor: 0,
        },
    );
    (app, tx, rx)
}

#[test]
fn input_keys_pick_options_and_enter_answers() {
    let (mut app, tx, mut rx) = input_app();
    let key = |code, modifiers| KeyEvent::new(code, modifiers);

    app.handle_transcript_key(key(KeyCode::Char('j'), KeyModifiers::NONE), &tx);
    assert_eq!(app.focused_input().expect("pending").cursor, 1);
    app.handle_transcript_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL), &tx);
    assert_eq!(app.focused_input().expect("pending").cursor, 0);
    app.handle_transcript_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL), &tx);
    assert_eq!(app.focused_input().expect("pending").cursor, 1);

    app.handle_transcript_key(key(KeyCode::Enter, KeyModifiers::NONE), &tx);
    match rx.try_recv().expect("input command") {
        Command::Input {
            session_id,
            input_id,
            option_index,
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(input_id, "i1");
            assert_eq!(option_index, 1);
        }
        _ => panic!("expected an input command"),
    }
}

#[test]
fn number_keys_answer_without_moving_the_cursor() {
    let (mut app, tx, mut rx) = input_app();
    app.handle_transcript_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE), &tx);
    match rx.try_recv().expect("input command") {
        Command::Input { option_index, .. } => assert_eq!(option_index, 2),
        _ => panic!("expected an input command"),
    }
}

#[test]
fn question_card_uses_markdown_checkboxes() {
    let (mut app, _, _rx) = input_app();
    let text = render_text(&mut app, 80, 30);
    assert!(text.contains("- [x] SQLite"), "{text}");
    assert!(text.contains("- [ ] Postgres"), "{text}");
    assert!(text.contains("- [ ] MySQL"), "{text}");

    app.move_input_cursor(1);
    let text = render_text(&mut app, 80, 30);
    assert!(text.contains("- [ ] SQLite"), "{text}");
    assert!(text.contains("- [x] Postgres"), "{text}");
}

#[test]
fn card_ctrl_navigation_works_with_prompt_focus_in_tabby() {
    let (mut app, tx, mut rx) = input_app();
    app.layout = PaneLayout::Tabby;
    app.focus = Focus::Input;
    app.input.enter_insert();

    app.handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.focused_input().expect("pending").cursor, 1);

    app.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.focused_input().expect("pending").cursor, 0);

    app.handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &tx,
    );
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    match rx.try_recv().expect("input command") {
        Command::Input { option_index, .. } => assert_eq!(option_index, 1),
        _ => panic!("expected an input command"),
    }
}

#[test]
fn approval_card_uses_markdown_checkboxes() {
    let (mut app, _, _rx) = input_app();
    app.inputs.clear();
    app.approvals.insert(
        "s1".to_string(),
        PendingApproval {
            kind: PendingKind::Tool,
            approval_id: "a1".to_string(),
            call_id: "c1".to_string(),
            tool: "run_command".to_string(),
            summary: "run_command: ls".to_string(),
            diff: None,
            path: None,
            hunks: Vec::new(),
            selected: Vec::new(),
            cursor: 0,
        },
    );

    let text = render_text(&mut app, 80, 30);
    assert!(text.contains("- [x] Approve"), "{text}");
    assert!(text.contains("- [ ] Always allow"), "{text}");

    app.move_approval_cursor(1);
    let text = render_text(&mut app, 80, 30);
    assert!(text.contains("- [ ] Approve"), "{text}");
    assert!(text.contains("- [x] Always allow"), "{text}");
}

#[test]
fn approval_ctrl_navigation_works_with_prompt_focus() {
    let (mut app, tx, mut rx) = input_app();
    app.inputs.clear();
    app.layout = PaneLayout::Tabby;
    app.focus = Focus::Input;
    app.approvals.insert(
        "s1".to_string(),
        PendingApproval {
            kind: PendingKind::Tool,
            approval_id: "a1".to_string(),
            call_id: "c1".to_string(),
            tool: "run_command".to_string(),
            summary: "run_command: ls".to_string(),
            diff: None,
            path: None,
            hunks: Vec::new(),
            selected: Vec::new(),
            cursor: 0,
        },
    );

    app.handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.focused_approval().expect("approval").cursor, 1);

    app.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.focused_approval().expect("approval").cursor, 0);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    match rx.try_recv().expect("approval command") {
        Command::Approval { decision, .. } => {
            assert_eq!(decision, ApprovalDecision::Approve)
        }
        _ => panic!("expected an approval command"),
    }
}

#[test]
fn pending_input_replays_from_events() {
    let events = vec![SessionEvent::new(
        "s1",
        1,
        EventKind::InputRequired,
        eix_core::payload(&InputRequiredPayload {
            input_id: "i1".to_string(),
            call_id: "c1".to_string(),
            question: "Which?".to_string(),
            options: vec![
                InputOption {
                    label: "A".to_string(),
                    description: None,
                },
                InputOption {
                    label: "B".to_string(),
                    description: None,
                },
            ],
        }),
    )];
    let view = SessionView::new(Session::new("s1", "ask", "/tmp"), events);
    let pending = view.pending_input().expect("pending question");
    assert_eq!(pending.question, "Which?");
    assert_eq!(pending.options.len(), 2);
    assert_eq!(pending.cursor, 0);
}

#[test]
fn approval_actions_lead_with_the_recommendation() {
    let mut approval = PendingApproval {
        kind: PendingKind::Tool,
        approval_id: "a1".to_string(),
        call_id: "c1".to_string(),
        tool: "run_command".to_string(),
        summary: "run_command: ls".to_string(),
        diff: None,
        path: None,
        hunks: Vec::new(),
        selected: Vec::new(),
        cursor: 0,
    };
    assert_eq!(
        approval.actions(),
        vec![
            ApprovalAction::Approve,
            ApprovalAction::Always,
            ApprovalAction::Deny,
            ApprovalAction::Reason
        ]
    );
    approval.path = Some("src/main.rs".to_string());
    approval.diff = Some("@@ -1 +1 @@".to_string());
    assert_eq!(
        approval.actions(),
        vec![
            ApprovalAction::Approve,
            ApprovalAction::Always,
            ApprovalAction::Review,
            ApprovalAction::Open,
            ApprovalAction::Deny,
            ApprovalAction::Reason,
        ]
    );
    assert_eq!(approval.action_label(ApprovalAction::Approve), "Approve");
    approval.kind = PendingKind::Patch;
    assert_eq!(
        approval.action_label(ApprovalAction::Approve),
        "Apply change"
    );
}

#[test]
fn live_approval_notification_keeps_risk_metadata() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "chat", "/tmp"),
        Vec::new(),
    ));
    app.handle_event(
        Event::Notification(eix_core::Notification::new(
            "tool.approval_required",
            serde_json::json!({
                "session_id": "s1",
                "turn_id": "t1",
                "approval_id": "a1",
                "call_id": "c1",
                "tool": "run_command",
                "summary": "run_command: truncated",
                "arguments": {"command": "curl https://example.com/?token=x"},
                "risk": "high",
                "risk_categories": ["network"],
                "reasons": ["fetches a URL from the network: https://example.com/?…"],
                "rule": "net",
                "rule_source": "user",
                "sandbox": "auto",
                "scopes": ["once"],
                "parsed_command": "curl https://example.com/?token=x",
                "allow_always": false,
            }),
        )),
        &tx,
    );
    let pending = app.approvals.get("s1").expect("approval");
    assert!(
        pending.summary.starts_with("[HIGH] $ curl"),
        "{}",
        pending.summary
    );
    assert!(
        pending.summary.contains("risk: network"),
        "{}",
        pending.summary
    );
    assert!(
        pending.summary.contains("rule: net (user)"),
        "{}",
        pending.summary
    );
    assert!(
        pending.summary.contains("sandbox: auto"),
        "{}",
        pending.summary
    );
    assert!(
        pending.summary.contains("scopes: once"),
        "{}",
        pending.summary
    );
    assert!(
        !pending.summary.contains("parsed:"),
        "approval cards should stay short: {}",
        pending.summary
    );
    assert!(pending.summary.contains(NO_ALWAYS_MARKER));
    assert!(!pending.actions().contains(&ApprovalAction::Always));
}

#[test]
fn high_risk_and_forced_ask_cards_hide_always() {
    let mut approval = PendingApproval {
        kind: PendingKind::Tool,
        approval_id: "a1".to_string(),
        call_id: "c1".to_string(),
        tool: "run_command".to_string(),
        summary: "[HIGH] run_command: rm -rf /".to_string(),
        diff: None,
        path: None,
        hunks: Vec::new(),
        selected: Vec::new(),
        cursor: 0,
    };
    assert!(!approval.actions().contains(&ApprovalAction::Always));

    approval.summary = format!("[MEDIUM] write_file out.txt{NO_ALWAYS_MARKER}");
    assert!(!approval.actions().contains(&ApprovalAction::Always));

    approval.summary = "[MEDIUM] write_file out.txt".to_string();
    assert!(approval.actions().contains(&ApprovalAction::Always));
}

#[test]
fn enter_approves_the_recommended_action() {
    let (mut app, tx, mut rx) = input_app();
    app.inputs.clear();
    app.approvals.insert(
        "s1".to_string(),
        PendingApproval {
            kind: PendingKind::Tool,
            approval_id: "a1".to_string(),
            call_id: "c1".to_string(),
            tool: "run_command".to_string(),
            summary: "run_command: ls".to_string(),
            diff: None,
            path: None,
            hunks: Vec::new(),
            selected: Vec::new(),
            cursor: 0,
        },
    );
    app.handle_transcript_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    match rx.try_recv().expect("approval command") {
        Command::Approval { decision, .. } => {
            assert_eq!(decision, ApprovalDecision::Approve)
        }
        _ => panic!("expected an approval command"),
    }
}

fn session_pane(id: &str, name: &str) -> SessionView {
    SessionView::new(Session::new(id, name, "/tmp"), Vec::new())
}

#[test]
fn cleanup_deletes_a_created_session_that_was_never_used() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.handle_event(Event::Created(Session::new("s1", "one", "/tmp")), &tx);
    while rx.try_recv().is_ok() {}

    app.cleanup_empty_sessions(&tx);
    match rx.try_recv().expect("delete command") {
        Command::DeleteSessionIfEmpty { session_id } => assert_eq!(session_id, "s1"),
        _ => panic!("expected a conditional delete"),
    }
}

#[test]
fn cleanup_keeps_a_created_session_after_an_event_or_draft() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.handle_event(Event::Created(Session::new("s1", "one", "/tmp")), &tx);
    while rx.try_recv().is_ok() {}

    let event = SessionEvent {
        session_id: "s1".into(),
        seq: 1,
        ts: 0,
        kind: EventKind::UserMessage,
        payload: serde_json::json!({"text": "hi"}),
    };
    app.handle_event(
        Event::Notification(eix_core::Notification::new("session.event", event)),
        &tx,
    );
    app.cleanup_empty_sessions(&tx);
    assert!(rx.try_recv().is_err(), "a used session must be kept");

    // A draft in the prompt is also input.
    app.handle_event(Event::Created(Session::new("s2", "two", "/tmp")), &tx);
    while rx.try_recv().is_ok() {}
    app.input_session = Some("s2".into());
    app.input.set_text("unsent draft");
    app.cleanup_empty_sessions(&tx);
    assert!(rx.try_recv().is_err(), "a draft must keep the session");
}

#[test]
fn slash_exit_quits_without_closing_session_panes() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.views.push(session_pane("s2", "two"));
    app.focused = 0;
    app.focus = Focus::Input;
    app.input.set_text("/exit");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(app.should_quit, "exit quits the whole TUI");
    assert_eq!(app.views.len(), 2, "exit leaves session panes open");
    assert_eq!(app.views[0].session.id, "s1");
    assert!(app.input.is_empty(), "the command line is cleared");
    match rx.try_recv().expect("draft save") {
        Command::DraftSet { session_id, text } => {
            assert_eq!(session_id, "s1");
            assert!(text.is_empty(), "the command is not kept as a draft");
        }
        _ => panic!("expected a draft save"),
    }
}

#[test]
fn slash_quit_quits_with_multiple_panes_open() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.views.push(session_pane("s2", "two"));
    app.focused = 1;
    app.focus = Focus::Input;
    app.input.set_text("/quit");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(app.should_quit, "quit quits the whole TUI");
    assert_eq!(app.views.len(), 2, "quit leaves session panes open");
}

#[test]
fn slash_close_saves_and_closes_the_focused_pane() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.views.push(session_pane("s2", "two"));
    app.focused = 0;
    app.focus = Focus::Input;
    app.created_sessions.insert("s1".into());
    app.input.set_text("/close");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(!app.should_quit, "another pane is still open");
    assert_eq!(app.views.len(), 1);
    assert_eq!(app.views[0].session.id, "s2");
    assert!(app.input.is_empty(), "the command line is cleared");
    match rx.try_recv().expect("draft save") {
        Command::DraftSet { session_id, text } => {
            assert_eq!(session_id, "s1");
            assert!(text.is_empty(), "the command is not kept as a draft");
        }
        _ => panic!("expected a draft save"),
    }
    match rx.try_recv().expect("conditional delete") {
        Command::DeleteSessionIfEmpty { session_id } => assert_eq!(session_id, "s1"),
        _ => panic!("expected a conditional delete"),
    }
}

#[test]
fn closing_a_created_pane_queues_a_conditional_delete() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.views.push(session_pane("s2", "two"));
    app.focused = 0;
    app.created_sessions.insert("s1".into());

    app.run_action("close-pane", &tx);

    assert_eq!(app.views.len(), 1);
    assert_eq!(app.views[0].session.id, "s2");
    match rx.try_recv().expect("draft save") {
        Command::DraftSet { session_id, .. } => assert_eq!(session_id, "s1"),
        _ => panic!("expected a draft save"),
    }
    match rx.try_recv().expect("conditional delete") {
        Command::DeleteSessionIfEmpty { session_id } => assert_eq!(session_id, "s1"),
        _ => panic!("expected a conditional delete"),
    }
}

#[test]
fn slash_close_on_the_last_pane_quits_the_client() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.focused = 0;
    app.focus = Focus::Input;
    app.input.set_text("/close");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(app.should_quit, "closing the last pane quits the TUI");
    assert!(app.should_clear_active_workspace());
    assert_eq!(app.views.len(), 1);
    assert!(app.status.contains("quitting"));
    match rx.try_recv().expect("draft save") {
        Command::DraftSet { session_id, .. } => assert_eq!(session_id, "s1"),
        _ => panic!("expected a draft save"),
    }
}

#[test]
fn command_palette_mirrors_the_slash_command_menu() {
    let commands = builtin_commands();
    let items = command_items(&commands);
    assert_eq!(items.len(), commands.len());
    for (item, spec) in items.iter().zip(commands.iter()) {
        assert_eq!(item.value, spec.name);
        assert_eq!(item.label, command_label(spec, "/"));
        assert_eq!(item.detail.as_deref(), Some(spec.description.as_str()));
    }
}

#[test]
fn slash_help_and_keys_open_filtered_interactive_references() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);

    assert!(app.handle_prompt_command("/help approval", &tx));
    assert!(app.help);
    assert_eq!(app.help_scope, None);
    assert_eq!(app.help_query, "approval");
    assert!(help_entry_count_in(&app.help_query, app.help_scope) > 0);

    app.help = false;
    assert!(app.handle_prompt_command("/keys space", &tx));
    assert!(app.help);
    assert_eq!(app.help_scope, Some("keys"));
    assert_eq!(app.help_query, "space");
    let rows = help_rows_in(&app.help_query, app.help_scope);
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|row| !matches!(
        row,
        HelpRow::Section(title) if *title != "keys"
    )));

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(!app.help);
    assert_eq!(app.help_scope, None);
}

#[test]
fn mcp_and_todos_commands_open_searchable_pickers() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.focused = 0;

    assert!(app.handle_prompt_command("/mcp", &tx));
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Mcp)
    ));
    app.handle_event(
        Event::Mcp(vec![eix_core::McpServerInfo {
            name: "files".into(),
            enabled: true,
            command: "npx server".into(),
            tools: 2,
            resources: 0,
            prompts: 0,
            error: None,
        }]),
        &tx,
    );
    assert_eq!(app.picker.as_ref().unwrap().items.len(), 1);
    assert!(!app.picker.as_ref().unwrap().loading);

    app.picker = None;
    assert!(app.handle_prompt_command("/todos", &tx));
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Todos)
    ));
    app.handle_event(
        Event::Todos(vec![eix_core::TodoItem {
            content: "write tests".into(),
            status: "in_progress".into(),
        }]),
        &tx,
    );
    assert_eq!(app.picker.as_ref().unwrap().items.len(), 1);
    assert!(!app.picker.as_ref().unwrap().loading);
}

#[test]
fn model_command_opens_the_picker_and_preselects_the_current_model() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let mut pane = session_pane("s1", "one");
    pane.session.model = Some("router/gpt-5".into());
    app.views.push(pane);
    app.focused = 0;

    assert!(app.handle_prompt_command("/model", &tx));
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Models)
    ));
    assert!(app.picker.as_ref().unwrap().loading);

    app.handle_event(
        Event::Models {
            models: vec![
                eix_core::ModelInfo {
                    id: "openai/gpt-4o".into(),
                    api_model: Some("gpt-4o".into()),
                    name: "gpt-4o".into(),
                    provider: "openai".into(),
                    default: false,
                    variants: Vec::new(),
                    input_cost: None,
                    output_cost: None,
                    context_tokens: None,
                },
                eix_core::ModelInfo {
                    id: "router/gpt-5".into(),
                    api_model: Some("gpt-5".into()),
                    name: "GPT-5".into(),
                    provider: "Router".into(),
                    default: false,
                    variants: Vec::new(),
                    input_cost: None,
                    output_cost: None,
                    context_tokens: None,
                },
            ],
            warnings: vec!["local: HTTP 401".into()],
        },
        &tx,
    );
    let picker = app.picker.as_ref().expect("model picker");
    assert!(!picker.loading);
    assert_eq!(picker.items[picker.selected].value, "router/gpt-5");
    assert_eq!(picker.items[0].label, "@default");
    assert_eq!(picker.items[0].detail.as_deref(), Some("daemon default"));
    assert_eq!(picker.items[1].label, "gpt-4o");
    assert_eq!(picker.items[1].value, "openai/gpt-4o");
    assert_eq!(picker.items[1].detail.as_deref(), Some("openai"));
    assert_eq!(picker.items[2].label, "gpt-5");
    assert_eq!(picker.items[2].value, "router/gpt-5");
    assert_eq!(picker.items[2].detail.as_deref(), Some("Router"));
    assert!(app.status.contains("local: HTTP 401"));
}

#[test]
fn ctrl_slash_opens_the_model_picker_from_both_focus_modes() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

    let mut transcript = App::new(None);
    transcript.focus = Focus::Transcript;
    transcript.handle_key(
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(matches!(
        transcript.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Models)
    ));
    assert!(transcript.picker.as_ref().unwrap().loading);

    let mut prompt = App::new(None);
    prompt.focus_input();
    prompt.handle_key(
        KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(matches!(
        prompt.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Models)
    ));
    assert!(prompt.picker.as_ref().unwrap().loading);

    let mut legacy = App::new(None);
    legacy.focus = Focus::Transcript;
    legacy.handle_key(
        KeyEvent::new(KeyCode::Char('7'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(matches!(
        legacy.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Models)
    ));
}

#[test]
fn slash_quit_on_the_last_pane_quits_after_saving() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.focused = 0;
    app.focus = Focus::Input;
    app.input.set_text("/quit");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(app.should_quit, "the last pane quits the app");
    assert_eq!(app.views.len(), 1);
    match rx.try_recv().expect("draft save") {
        Command::DraftSet { session_id, text } => {
            assert_eq!(session_id, "s1");
            assert!(text.is_empty());
        }
        _ => panic!("expected a draft save"),
    }
}

#[test]
fn submitting_while_busy_queues_and_flushes_when_idle() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.focused = 0;
    app.focus = Focus::Input;
    app.views[0].session.status = SessionStatus::Running;
    app.input.set_text("next prompt");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert_eq!(app.queue_len("s1"), 1);
    assert!(app.input.is_empty());
    let mut submitted = false;
    while let Ok(command) = rx.try_recv() {
        if matches!(command, Command::Submit { .. }) {
            submitted = true;
        }
    }
    assert!(!submitted, "busy sessions must not receive a submit");

    app.views[0].session.status = SessionStatus::Idle;
    app.tick(&tx);
    let mut text = None;
    while let Ok(command) = rx.try_recv() {
        if let Command::Submit { text: value, .. } = command {
            text = Some(value);
        }
    }
    assert_eq!(text.as_deref(), Some("next prompt"));
    assert!(app.submitting);
    assert_eq!(app.queue_len("s1"), 0);
}

#[test]
fn a_busy_race_requeues_the_inflight_prompt() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.focused = 0;
    app.views[0].session.status = SessionStatus::Running;
    app.input.set_text("queued");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    app.views[0].session.status = SessionStatus::Idle;
    app.tick(&tx);
    while rx.try_recv().is_ok() {}

    app.handle_event(
        Event::Error {
            context: "submit".into(),
            message: "session s1 is already running".into(),
        },
        &tx,
    );
    assert!(!app.submitting);
    assert_eq!(app.queue_len("s1"), 1);

    app.tick(&tx);
    let mut text = None;
    while let Ok(command) = rx.try_recv() {
        if let Command::Submit { text: value, .. } = command {
            text = Some(value);
        }
    }
    assert_eq!(text.as_deref(), Some("queued"));
}

#[test]
fn a_stale_busy_error_queues_the_prompt() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.focused = 0;
    app.focus = Focus::Input;
    app.input.set_text("retry me");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    while rx.try_recv().is_ok() {}

    // The TUI thought the session was idle, but the daemon was busy.
    app.handle_event(
        Event::Error {
            context: "submit".into(),
            message: "busy: session s1 is already running".into(),
        },
        &tx,
    );
    assert_eq!(app.queue_len("s1"), 1);
    assert!(app.input.is_empty());
    assert!(app.error.is_none());
    assert!(!app.submitting);
}

#[test]
fn leader_x_keeps_the_prompt_in_normal_mode() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.focused = 0;
    app.views[0].session.status = SessionStatus::Running;
    app.focus = Focus::Input;
    app.input.enter_normal();

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &tx);

    assert_eq!(app.input.mode(), Mode::Normal);
    assert!(matches!(rx.try_recv(), Ok(Command::Cancel(id)) if id == "s1"));

    // A delayed draft load must not push the prompt back into insert mode.
    app.input_session = Some("s1".to_string());
    app.handle_event(
        Event::Draft {
            session_id: "s1".to_string(),
            text: "restored draft".to_string(),
        },
        &tx,
    );
    assert_eq!(app.input.mode(), Mode::Normal);
    assert_eq!(app.input.text(), "restored draft");
}

#[test]
fn double_esc_in_normal_mode_interrupts_a_running_turn() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.focused = 0;
    app.views[0].session.status = SessionStatus::Running;
    app.focus_input();
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    app.handle_input_key(esc, &tx); // insert -> normal
    app.handle_input_key(esc, &tx); // arm
    app.handle_input_key(esc, &tx); // interrupt
    let mut cancelled = None;
    while let Ok(command) = rx.try_recv() {
        if let Command::Cancel(id) = command {
            cancelled = Some(id);
        }
    }
    assert_eq!(cancelled.as_deref(), Some("s1"));
    assert!(matches!(app.focus, Focus::Input));
}

#[test]
fn slash_commands_dispatch_requests_and_clear_the_prompt() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.focused = 0;
    app.focus = Focus::Input;

    app.input.set_text("/clear");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.input.is_empty());
    let mut cleared = false;
    while let Ok(command) = rx.try_recv() {
        if let Command::ClearContext { session_id } = command {
            assert_eq!(session_id, "s1");
            cleared = true;
        }
    }
    assert!(cleared);

    app.input.set_text("/cd /tmp");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    let mut cwd = None;
    while let Ok(command) = rx.try_recv() {
        if let Command::SetCwd { path, .. } = command {
            cwd = Some(path);
        }
    }
    assert_eq!(cwd.as_deref(), Some("/tmp"));

    app.input.set_text("/compact tighten it");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    let mut notes = None;
    while let Ok(command) = rx.try_recv() {
        if let Command::CompactContext { notes: value, .. } = command {
            notes = value;
        }
    }
    assert_eq!(notes.as_deref(), Some("tighten it"));

    app.input.set_text("/btw why is the sky blue?");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    let mut question = None;
    while let Ok(command) = rx.try_recv() {
        if let Command::Btw {
            question: value, ..
        } = command
        {
            question = Some(value);
        }
    }
    assert_eq!(question.as_deref(), Some("why is the sky blue?"));
    assert!(app.btw.is_some());
    app.btw = None;

    app.input.set_text("/help");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.help);

    app.input.set_text("/init");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.input.text().contains("AGENTS.md"));
    let mut submitted = false;
    while let Ok(command) = rx.try_recv() {
        if matches!(command, Command::Submit { .. }) {
            submitted = true;
        }
    }
    assert!(!submitted, "/init only stages a prompt");
}

#[test]
fn command_menu_filters_completes_and_runs() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.focus_input();
    app.input.set_text("/cl");
    assert!(app.command_menu_open());
    let names: Vec<&str> = app
        .command_matches()
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert!(names.contains(&"clear"), "{names:?}");
    assert!(names.iter().all(|name| name.starts_with("cl")), "{names:?}");
    app.complete_command();
    assert_eq!(app.input.text(), "/clear");

    app.input.set_text("/hel");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.help, "Enter runs the highlighted command");
}

#[test]
fn subagents_command_and_picker_adjust_the_runtime_limit() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);

    assert!(app.handle_prompt_command("/subagents 8", &tx));
    assert!(app.error.is_none());
    assert!(app.status.contains('8'));
    match rx.try_recv().expect("set command") {
        Command::SetSubagentConcurrency { value } => assert_eq!(value, 8),
        _ => panic!("expected a subagent concurrency command"),
    }

    app.handle_event(Event::SubagentConcurrency { value: 8 }, &tx);
    assert_eq!(app.subagent_concurrency, 8);
    assert!(app.status.contains("runtime"));

    app.open_picker(PickerKind::Subagents, &tx);
    let selected_value = {
        let picker = app.picker.as_ref().expect("subagent picker");
        assert_eq!(picker.kind, PickerKind::Subagents);
        picker.items[picker.selected].value.clone()
    };
    assert_eq!(selected_value, "8");
    let values = {
        let picker = app.picker.as_ref().expect("subagent picker");
        picker
            .items
            .iter()
            .map(|item| item.value.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(values, ["2", "4", "8", "16", "32", "64", "128", "256"]);

    let next = {
        let picker = app.picker.as_ref().expect("subagent picker");
        picker
            .items
            .iter()
            .position(|item| item.value == "16")
            .expect("16 row")
    };
    app.picker.as_mut().expect("picker").selected = next;
    app.picker_enter(PickerKind::Subagents, &tx);
    assert!(app.picker.is_none());
    match rx.try_recv().expect("picker command") {
        Command::SetSubagentConcurrency { value } => assert_eq!(value, 16),
        _ => panic!("expected a subagent concurrency command"),
    }

    for invalid in ["0", "1", "3", "6", "12", "24", "48", "100", "512"] {
        assert!(app.handle_prompt_command(&format!("/subagents {invalid}"), &tx));
        assert!(
            app.error
                .as_deref()
                .is_some_and(|err| err.contains("2, 4, 8, 16, 32, 64, 128, or 256")),
            "{invalid}"
        );
    }
    assert!(
        rx.try_recv().is_err(),
        "invalid values must not reach the daemon"
    );
}

#[test]
fn perm_command_opens_a_searchable_picker() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "chat", "/tmp"),
        Vec::new(),
    ));

    assert!(app.handle_prompt_command("/perm", &tx));
    let values = {
        let picker = app.picker.as_ref().expect("mode picker");
        assert_eq!(picker.kind, PickerKind::Modes);
        picker
            .items
            .iter()
            .map(|item| item.value.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(values, ["plan", "build", "auto", "bypass"]);
    let current = app
        .picker
        .as_ref()
        .expect("picker")
        .items
        .iter()
        .find(|item| item.value == "build")
        .expect("build row");
    assert!(current.label.contains("(current)"), "{:?}", current.label);

    app.picker.as_mut().expect("picker").query = "bypass".into();
    app.picker.as_mut().expect("picker").selected = 0;
    app.picker_enter(PickerKind::Modes, &tx);
    assert!(app.picker.is_none());
    let mut selected = None;
    while let Ok(command) = rx.try_recv() {
        if let Command::SetMode { mode, .. } = command {
            selected = Some(mode);
        }
    }
    assert_eq!(selected, Some(SessionMode::Bypass));
}

#[test]
fn mode_alias_does_not_compete_with_model_in_the_command_menu() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "chat", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus_input();

    // The old `/mode` spelling resolves straight to `/perm`, so Enter and
    // Tab can no longer land on `/model` just because it shares a prefix.
    app.input.set_text("/mode");
    let names: Vec<&str> = app
        .command_matches()
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert_eq!(names, ["perm"]);
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Modes)
    ));

    // The canonical command itself still opens the same picker.
    app.picker = None;
    app.focus_input();
    app.input.set_text("/perm");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Modes)
    ));

    // And `/model` remains unambiguous.
    app.picker = None;
    app.focus_input();
    app.input.set_text("/model");
    let names: Vec<&str> = app
        .command_matches()
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert_eq!(names, ["model"]);
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(matches!(
        app.picker.as_ref().map(|picker| picker.kind),
        Some(PickerKind::Models)
    ));
}

#[test]
fn tab_and_shift_tab_cycle_every_session_mode() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus = Focus::Transcript;

    // The default is Build; Tab walks forward through the full ring.
    assert_eq!(app.views[0].session.mode, SessionMode::Build);
    for expected in [
        SessionMode::Auto,
        SessionMode::Bypass,
        SessionMode::Plan,
        SessionMode::Build,
    ] {
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert_eq!(app.views[0].session.mode, expected);
        match rx.try_recv().expect("set mode") {
            Command::SetMode { mode, .. } => assert_eq!(mode, expected),
            _ => panic!("unexpected command"),
        }
    }

    // Shift-Tab mirrors the ring: Build → Plan → Bypass → Auto → Build.
    // Terminals encode it as either BackTab or Shift+Tab; both must work.
    for (step, expected) in [
        SessionMode::Plan,
        SessionMode::Bypass,
        SessionMode::Auto,
        SessionMode::Build,
    ]
    .into_iter()
    .enumerate()
    {
        let key = if step % 2 == 0 {
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)
        } else {
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)
        };
        app.handle_key(key, &tx);
        assert_eq!(app.views[0].session.mode, expected);
        match rx.try_recv().expect("set mode") {
            Command::SetMode { mode, .. } => assert_eq!(mode, expected),
            _ => panic!("unexpected command"),
        }
    }
}

#[test]
fn leader_m_still_opens_the_full_mode_picker() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app_with_panes(&["s1"]);
    app.focus = Focus::Transcript;

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE), &tx);
    let picker = app.picker.as_ref().expect("mode picker");
    assert_eq!(picker.kind, PickerKind::Modes);
    let values: Vec<&str> = picker
        .items
        .iter()
        .map(|item| item.value.as_str())
        .collect();
    assert_eq!(values, ["plan", "build", "auto", "bypass"]);
    assert!(
        rx.try_recv().is_err(),
        "opening the picker must not change the mode"
    );
    assert!(app.status.starts_with("mode: "), "{}", app.status);
}

#[test]
fn old_permission_command_is_gone() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "chat", "/tmp"),
        Vec::new(),
    ));

    assert!(app.handle_prompt_command("/permission", &tx));
    assert!(app.picker.is_none());
    assert!(app
        .error
        .as_deref()
        .is_some_and(|error| error.contains("unknown command /permission")));
}

fn render_text(app: &mut App, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// The status bar sits directly above the always-visible command line.
fn status_bar_row(rendered: &str) -> &str {
    let mut lines = rendered.lines().rev();
    lines.next().expect("command line row");
    lines.next().expect("status bar row")
}

#[test]
fn prompt_grows_to_show_a_long_command() {
    let mut app = App::new(None);
    app.focus = Focus::Input;
    app.input
        .set_text(&format!("echo {} ENDMARK", "x".repeat(300)));

    let rendered = render_text(&mut app, 80, 40);
    assert!(rendered.contains("echo"), "{rendered}");
    assert!(rendered.contains("ENDMARK"), "{rendered}");
}

#[test]
fn prompt_border_stays_below_the_draft_inside_the_box() {
    let mut app = App::new(None);
    let mut session = Session::new("s1", "one", "/tmp");
    session.model = Some("llmgate/deepseek/deepseek-v4.1-flash#high".into());
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;
    app.focus = Focus::Input;
    app.connected = true;
    app.git_cwd = Some(std::path::PathBuf::from("/tmp"));
    app.git_branch = Some("main".into());
    app.models.push(ModelInfo {
        id: "llmgate/deepseek/deepseek-v4.1-flash".into(),
        api_model: Some("deepseek/deepseek-v4.1-flash".into()),
        name: "deepseek/deepseek-v4.1-flash".into(),
        provider: "llmgate".into(),
        default: false,
        variants: vec!["high".into()],
        input_cost: None,
        output_cost: None,
        context_tokens: None,
    });
    app.input.set_text("hello footer");

    let rendered = render_text(&mut app, 80, 20);
    let lines: Vec<&str> = rendered.lines().collect();
    let text_row = lines
        .iter()
        .position(|line| line.contains("hello footer"))
        .expect("draft");
    let bottom_row = lines
        .iter()
        .rposition(|line| line.contains('╰'))
        .expect("prompt bottom");
    assert!(!lines[bottom_row - 1].contains("Build"), "{rendered}");
    assert!(!lines[bottom_row - 1].contains("deepseek"), "{rendered}");
    let status_row = status_bar_row(&rendered);
    assert!(status_row.contains(" I · Build  ·"), "{rendered}");
    assert!(status_row.contains("deepseek-v4.1-flash"), "{rendered}");
    assert!(status_row.contains("llmgate"), "{rendered}");
    assert!(status_row.contains("· high"), "{rendered}");
    assert!(status_row.contains("· main"), "{rendered}");
    let top_row = lines
        .iter()
        .position(|line| line.contains('╭'))
        .expect("prompt top border");
    assert!(!lines[top_row].contains("main"), "{rendered}");
    assert!(
        !lines[top_row.saturating_sub(1)].contains('└'),
        "the transcript should not add a second border above the prompt:\n{rendered}"
    );
    assert!(text_row < bottom_row, "{rendered}");
}

#[test]
fn status_bar_keeps_workspace_metadata_when_narrow() {
    let mut app = App::new(None);
    let mut session = Session::new("s1", "one", "/tmp");
    session.model = Some("provider/a-very-long-model-name#high".into());
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;
    app.connected = true;
    app.git_cwd = Some(std::path::PathBuf::from("/tmp"));
    app.git_branch = Some("main".into());

    let rendered = render_text(&mut app, 40, 20);
    let bar = status_bar_row(&rendered);
    assert!(bar.contains("/tmp · main"), "{bar}");
    assert!(bar.contains("Build  ·"), "{bar}");
    assert!(UnicodeWidthStr::width(bar) <= 40, "{bar}");
}

#[test]
fn ctrl_t_cycles_reasoning_strength() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let mut session = Session::new("s1", "one", "/tmp");
    session.model = Some("deepseek/deepseek-v4.1-flash".into());
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;
    app.focus = Focus::Input;
    app.input.enter_insert();
    app.connected = true;

    // No suffix means the default medium, so the first step is high.
    assert_eq!(app.model_header_parts().2.as_deref(), Some("medium"));
    app.status = "ready".into();
    for (expected, variant) in [
        ("deepseek/deepseek-v4.1-flash#high", "high"),
        ("deepseek/deepseek-v4.1-flash#low", "low"),
        ("deepseek/deepseek-v4.1-flash", "medium"),
        ("deepseek/deepseek-v4.1-flash#high", "high"),
        ("deepseek/deepseek-v4.1-flash#low", "low"),
    ] {
        app.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.views[0].session.model.as_deref(), Some(expected));
        assert_eq!(app.model_header_parts().2.as_deref(), Some(variant));
        assert_eq!(
            app.status, "ready",
            "Ctrl-T must not duplicate the reasoning label"
        );
        assert!(app.input.text().is_empty());
        match rx.try_recv().expect("set model command") {
            Command::SetModel { model, .. } => {
                assert_eq!(model.as_deref(), Some(expected));
            }
            _ => panic!("unexpected command"),
        }
    }
}

#[test]
fn prompt_is_a_minimal_styled_panel_with_command_menu() {
    let mut app = App::new(None);
    let mut session = Session::new("s1", "prompt polish", "/home/shiyi/git/eix");
    session.model = Some("openai/gpt-5".into());
    session.status = SessionStatus::Running;
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;
    app.focus = Focus::Input;
    app.connected = true;

    let empty = render_text(&mut app, 80, 20);
    assert!(empty.contains("Build  · ◰"), "{empty}");
    assert!(!empty.contains("eixd"), "no footer brand: {empty}");
    assert!(!empty.contains("/cmds"), "no footer hints: {empty}");
    assert!(empty.contains("1 ◰ prompt polish"), "{empty}");
    assert!(
        empty.contains('╭'),
        "the composer keeps its border: {empty}"
    );
    assert!(
        empty.contains('╮'),
        "the composer keeps its border: {empty}"
    );
    assert!(!empty.contains("❯"), "{empty}");
    assert!(empty.contains(" N "), "{empty}");
    assert!(!empty.contains("INSERT"), "{empty}");
    let lines: Vec<&str> = empty.lines().collect();
    let top_border = lines
        .iter()
        .position(|line| line.contains('╭'))
        .expect("prompt top border");
    let prompt_bottom = lines
        .iter()
        .rposition(|line| line.contains('╰'))
        .expect("prompt bottom");
    let status_bar = status_bar_row(&empty);
    assert!(!lines[top_border].contains("~/git/eix"), "{empty}");
    assert!(!lines[prompt_bottom - 1].contains(" N "), "{empty}");
    assert!(!lines[prompt_bottom - 1].contains("Build"), "{empty}");
    assert!(status_bar.contains(" N · Build  ·"), "{empty}");
    assert!(status_bar.contains("~/git/eix"), "{empty}");
    assert!(!lines[prompt_bottom - 1].contains("gpt-5"), "{empty}");
    assert!(status_bar.contains("gpt-5"), "{empty}");
    assert!(status_bar.contains("openai"), "{empty}");
    assert!(status_bar.contains("medium"), "{empty}");
    assert!(!empty.contains("Message eix…"), "{empty}");
    assert!(!empty.contains("C-j newline"), "{empty}");
    assert!(!empty.contains("Esc normal"), "{empty}");

    app.input.set_text("/cl");
    let menu = render_text(&mut app, 80, 20);
    assert!(menu.contains("/ commands"), "{menu}");
    assert!(menu.contains("❯ /clear"), "{menu}");
    assert!(!menu.contains("Tab complete"), "{menu}");
}

#[test]
fn status_feedback_uses_top_right_toasts() {
    let mut app = App::new(None);
    let mut session = Session::new("s1", "status test", "/tmp");
    session.model = Some("openai/gpt-5".into());
    app.views.push(SessionView::new(session, Vec::new()));
    app.focused = 0;
    app.focus = Focus::Input;
    app.connected = true;

    // Lifecycle chatter, progress, popup contents and successful command
    // feedback are already visible elsewhere; none of them become toasts.
    for hidden in [
        "attached",
        "connected to eixd 0.1.0",
        "idle",
        "running",
        "sending…",
        "sending queued prompt…",
        "retrying 2/5",
        "model: loading provider catalogs…",
        "approvals · Enter opens · y/n answer · Esc closes",
        "layout: grid",
        "mode: bypass",
        "pane closed",
        "no tasks",
        "no MCP servers configured (see [mcp.servers])",
        "no branch matched",
        "3 model(s)",
        "inserted skill foo",
        "colorscheme preview · Enter apply · Esc cancel",
        "no images attached",
    ] {
        app.status = hidden.into();
        let text = render_text(&mut app, 80, 20);
        let bar = status_bar_row(&text);
        assert!(!bar.contains(hidden), "{hidden}: {bar}");
        assert!(
            !text
                .lines()
                .nth(1)
                .is_some_and(|line| line.contains(hidden)),
            "{hidden}: {text}"
        );
        assert!(app.toasts.is_empty(), "{hidden}");
    }
    assert!(!status_bar_row(&render_text(&mut app, 80, 20)).contains("status test"));

    app.status = "retrying 2/5".into();
    let text = render_text(&mut app, 80, 20);
    let bar = status_bar_row(&text);
    assert!(bar.contains("Build  ·"), "{bar}");
    assert!(!bar.contains("retrying 2/5"), "{bar}");
    assert!(bar.contains("gpt-5"), "{bar}");

    // The primary group is ordered Vim mode → agent mode → model →
    // provider → reasoning, and cwd stays pinned to the right edge.
    assert!(bar.trim_start().starts_with("N · Build  ·"), "{bar}");
    let vim_mode = bar.find('N').unwrap();
    let build = bar.find("Build").unwrap();
    let model = bar.find("gpt-5").unwrap();
    let provider = bar.find("openai").unwrap();
    let reasoning = bar.find("medium").unwrap();
    assert!(
        vim_mode < build && build < model && model < provider && provider < reasoning,
        "{bar}"
    );
    assert!(bar.trim_end().ends_with("/tmp"), "{bar}");
    assert!(app.status_bar_middle().is_none());

    // A failure with no other visible surface is worth one warning toast.
    app.status = "cannot close the last pane".into();
    let text = render_text(&mut app, 80, 20);
    assert!(
        text.lines()
            .nth(1)
            .is_some_and(|line| line.contains("cannot close the last pane")),
        "{text}"
    );
    assert!(
        !status_bar_row(&text).contains("cannot close the last pane"),
        "{text}"
    );
    assert_eq!(
        app.toasts.last().map(|toast| toast.tone),
        Some(ToastTone::Warning)
    );

    // The mode badge follows the active context, not the editor buffer.
    app.focus = Focus::Transcript;
    let text = render_text(&mut app, 80, 20);
    let bar = status_bar_row(&text);
    assert!(bar.trim_start().starts_with("N · Build  ·"), "{text}");
}

#[test]
fn warning_status_filter_is_narrow() {
    for status in [
        "cannot close the last pane",
        "no session",
        "no session to search",
        "no previous search",
        "no matches",
        "no earlier prompts in this session",
        "no pending approvals",
        "no queued prompts",
        "no queued prompt at that index",
        "no diff to review",
        "no hunks selected; press a to select all",
        "this approval has no file to open",
        "no integration state yet",
        "reasoning: no model",
        "that session is no longer listed",
        "session 3 is not open",
        "process is not running",
        "approval was stale; the daemon is still waiting",
        "editor: permission denied",
        "usage: :r!<shell command>",
        "dropped 3 notifications",
        "wait for the running turn before sending images",
        "patch conflict: go/main.go",
        "queue index out of range",
    ] {
        assert!(status_needs_warning(status), "{status}");
    }

    for status in [
        "sending…",
        "upstream error · retrying 2/5 in 1.5s: 500",
        "rate limited · retrying 1/5 in 4.0s",
        "model: loading provider catalogs…",
        "approvals · Enter opens · y/n answer · Esc closes",
        "layout: grid",
        "mode → Build",
        "pane closed",
        "session created",
        "draft saved",
        "no tasks",
        "no MCP servers configured (see [mcp.servers])",
        "no branch matched",
        "no images attached",
        "no changes",
        "3 model(s)",
        "inserted skill foo",
        "colorscheme preview · Enter apply · Esc cancel",
        "patch applied: go/main.go",
        "process abcd1234 exited (0)",
        "sub-agent finished",
        "session is already open",
    ] {
        assert!(!status_needs_warning(status), "{status}");
    }
}

#[test]
fn visible_error_dialog_suppresses_duplicate_warning() {
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.error = Some("provider returned 500".into());
    app.status = "config error".into();

    assert!(app.status_warning_is_redundant());
    let text = render_text(&mut app, 80, 20);
    assert!(text.contains("provider returned 500"), "{text}");
    assert!(app.toasts.is_empty());
}

#[test]
fn open_picker_suppresses_status_warnings() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(SessionView::new(
        Session::new("s1", "one", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.open_picker(PickerKind::Commands, &tx);
    app.status = "mcp: error — connection refused".into();

    assert!(app.status_warning_is_redundant());
    let _text = render_text(&mut app, 80, 20);
    assert!(app.toasts.is_empty());
}

#[test]
fn attention_alerts_render_top_right_without_moving_the_statusline() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    let mut session = Session::new("s1", "status test", "/tmp");
    session.model = Some("openai/gpt-5".into());
    app.views.push(SessionView::new(session, Vec::new()));
    app.views.push(SessionView::new(
        Session::new("s2", "worker", "/tmp"),
        Vec::new(),
    ));
    app.focused = 0;
    app.focus = Focus::Input;
    app.connected = true;
    app.status = "retrying 2/5".into();

    app.handle_event(
        Event::Notification(eix_core::Notification::new(
            "session.attention",
            serde_json::json!({"session_id": "s2", "status": "awaiting_approval"}),
        )),
        &tx,
    );

    let text = render_text(&mut app, 80, 20);
    assert!(
        text.lines()
            .nth(1)
            .is_some_and(|line| line.contains("worker · awaiting_approval in pane 2")),
        "{text}"
    );
    let bar = status_bar_row(&text);
    assert!(!bar.contains("worker · awaiting_approval"), "{bar}");
    assert!(!bar.contains("retrying 2/5"), "{bar}");
    assert!(bar.contains("gpt-5"), "{bar}");
    assert_eq!(
        app.toasts.last().map(|toast| toast.tone),
        Some(ToastTone::Warning)
    );

    // Expiry removes the notice without replaying the alert or moving the
    // persistent statusline metadata.
    app.toasts.last_mut().expect("attention toast").expires_at =
        Instant::now() - Duration::from_millis(1);
    let text = render_text(&mut app, 80, 20);
    assert!(!text.contains("worker · awaiting_approval"), "{text}");
    let bar = status_bar_row(&text);
    assert!(bar.contains("gpt-5"), "{bar}");
}

#[test]
fn default_theme_keeps_the_prompt_transparent() {
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.focused = 0;
    app.focus = Focus::Input;
    app.connected = true;
    assert!(app.theme.background.is_none());
    assert_eq!(composer_background(app.theme), Color::Reset);
    assert_eq!(dialog_background(app.theme), Color::Reset);

    let backend = ratatui::backend::TestBackend::new(80, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            assert_ne!(
                buffer[(x, y)].bg,
                Color::Rgb(30, 33, 42),
                "prompt kept an opaque panel at ({x}, {y})"
            );
        }
    }
}

#[test]
fn prompt_errors_render_in_a_dialog_not_the_input_header() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.focused = 0;
    app.focus = Focus::Input;
    app.connected = true;
    app.error = Some("provider returned 500".into());

    let text = render_text(&mut app, 80, 20);
    assert!(text.contains("Build  ·"), "{text}");
    assert!(!text.contains("error: provider returned 500"), "{text}");
    assert!(text.contains(" error "), "{text}");
    assert!(text.contains("provider returned 500"), "{text}");

    // The dialog is modal; typing does not reach the editor.
    let mode = app.input.mode();
    app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &tx);
    assert!(app.error.is_some());
    assert_eq!(app.input.mode(), mode);

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(app.error.is_none());
    let text = render_text(&mut app, 80, 20);
    assert!(!text.contains("provider returned 500"), "{text}");
}

#[test]
fn a_message_mentioning_slash_exit_is_still_sent() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(None);
    app.views.push(session_pane("s1", "one"));
    app.focused = 0;
    app.focus = Focus::Input;
    app.input.set_text("please run /exit later");
    app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

    assert!(!app.should_quit);
    assert_eq!(app.views.len(), 1);
    match rx.try_recv().expect("submit") {
        Command::Submit { text, .. } => assert_eq!(text, "please run /exit later"),
        _ => panic!("expected a submit"),
    }
}
