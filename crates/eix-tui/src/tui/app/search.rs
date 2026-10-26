//! Transcript search.

use super::*;

impl App {
    /// Search the focused session transcript, Vim style. `/` starts forward,
    /// `?` and `#` start backward. An empty query repeats the last search.
    pub(super) fn search_transcript(&mut self, query: &str, forward: bool) {
        let query = query.trim();
        let query = if query.is_empty() {
            self.transcript_search
                .as_ref()
                .map(|search| search.query.clone())
                .unwrap_or_default()
        } else {
            query.to_string()
        };
        if query.is_empty() {
            self.status = "no previous search".into();
            return;
        }
        let Some(view) = self.focused_view() else {
            self.status = "no session to search".into();
            return;
        };
        let session_id = view.session.id.clone();
        let needle = query.to_lowercase();
        let matches: Vec<usize> = view
            .text_lines
            .iter()
            .enumerate()
            .filter_map(|(line, text)| text.to_lowercase().contains(&needle).then_some(line))
            .collect();
        if matches.is_empty() {
            self.transcript_search = Some(TranscriptSearch {
                session_id,
                query,
                matches,
                current: 0,
                forward,
            });
            self.status = "no matches".into();
            return;
        }
        let start = view.scroll;
        let current = if forward {
            matches.iter().position(|line| *line >= start).unwrap_or(0)
        } else {
            matches
                .iter()
                .rposition(|line| *line < start)
                .unwrap_or(matches.len() - 1)
        };
        let line = matches[current];
        let total = matches.len();
        let query = query.clone();
        self.transcript_search = Some(TranscriptSearch {
            session_id,
            query: query.clone(),
            matches,
            current,
            forward,
        });
        if let Some(view) = self.focused_view_mut() {
            view.follow = false;
            view.scroll = line.saturating_sub(2);
        }
        self.status = format!(
            "{}{} · {}/{}",
            if forward { "/" } else { "?" },
            query,
            current + 1,
            total
        );
    }

    /// Move to the next/previous match from the current in-session search.
    pub(super) fn search_transcript_next(&mut self, reverse: bool) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            return;
        };
        let Some(search) = self.transcript_search.as_mut() else {
            return;
        };
        if search.session_id != session_id || search.matches.is_empty() {
            return;
        }
        let len = search.matches.len();
        search.current = if reverse {
            (search.current + len - 1) % len
        } else {
            (search.current + 1) % len
        };
        let line = search.matches[search.current];
        let current = search.current + 1;
        let query = search.query.clone();
        let forward = search.forward;
        if let Some(view) = self.focused_view_mut() {
            view.follow = false;
            view.scroll = line.saturating_sub(2);
        }
        self.status = format!(
            "{}{} · {}/{}",
            if forward { "/" } else { "?" },
            query,
            current,
            len
        );
    }
}
