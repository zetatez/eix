//! crossterm KeyEvent -> Neovim `nvim_input` notation.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub fn encode(key: KeyEvent) -> Option<String> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let out = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                match c {
                    '[' => "<C-[>".to_string(),
                    '\\' => "<C-\\>".to_string(),
                    ']' => "<C-]>".to_string(),
                    '^' => "<C-^>".to_string(),
                    '_' => "<C-_>".to_string(),
                    '@' => "<C-@>".to_string(),
                    _ => format!("<C-{}>", c.to_ascii_lowercase()),
                }
            } else if alt {
                format!("<M-{}>", escape(&c.to_string()))
            } else {
                escape(&c.to_string())
            }
        }
        KeyCode::Enter => "<CR>".to_string(),
        KeyCode::Esc => "<Esc>".to_string(),
        KeyCode::Tab => {
            if shift {
                "<S-Tab>".to_string()
            } else {
                "<Tab>".to_string()
            }
        }
        KeyCode::BackTab => "<S-Tab>".to_string(),
        KeyCode::Backspace => "<BS>".to_string(),
        KeyCode::Delete => "<Del>".to_string(),
        KeyCode::Insert => "<Insert>".to_string(),
        KeyCode::Home => "<Home>".to_string(),
        KeyCode::End => "<End>".to_string(),
        KeyCode::PageUp => "<PageUp>".to_string(),
        KeyCode::PageDown => "<PageDown>".to_string(),
        KeyCode::Up => "<Up>".to_string(),
        KeyCode::Down => "<Down>".to_string(),
        KeyCode::Left => "<Left>".to_string(),
        KeyCode::Right => "<Right>".to_string(),
        KeyCode::F(n) => format!("<F{n}>"),
        _ => return None,
    };

    Some(out)
}

/// `<` starts an nvim key notation, so a literal `<` must be sent as `<LT>`.
fn escape(s: &str) -> String {
    if s.contains('<') {
        s.replace('<', "<LT>")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn plain_lt_is_escaped() {
        assert_eq!(
            encode(key(KeyCode::Char('<'), KeyModifiers::NONE)).unwrap(),
            "<LT>"
        );
    }

    #[test]
    fn enter_is_cr() {
        assert_eq!(
            encode(key(KeyCode::Enter, KeyModifiers::NONE)).unwrap(),
            "<CR>"
        );
    }

    #[test]
    fn ctrl_j_stays_ctrl_j() {
        assert_eq!(
            encode(key(KeyCode::Char('j'), KeyModifiers::CONTROL)).unwrap(),
            "<C-j>"
        );
    }
}
