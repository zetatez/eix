//! Rendering for plugin-provided [`UiNode`](eix_core::UiNode) trees.
//!
//! The TUI renders nodes into the existing text overlay instead of a second
//! layout engine. Every button and form submit gets a `/plugins act <action>`
//! hint so the same tree is usable from the keyboard and from scripts.

use super::*;

impl App {
    /// Show a plugin-rerendered surface in the text overlay and remember which
    /// surface is open so `/plugins act` can target it.
    pub(super) fn show_plugin_panel(
        &mut self,
        plugin: &str,
        id: &str,
        surface: &str,
        title: &str,
        result: &eix_core::PluginUiRenderResult,
    ) {
        let body = match (&result.node, result.body.trim()) {
            (Some(node), _) => render_ui_node(node, plugin, id),
            (None, "") => "plugin returned an empty panel".to_string(),
            (None, _) => result.body.clone(),
        };
        let title = if title.trim().is_empty() {
            id.to_string()
        } else {
            title.to_string()
        };
        self.open_text_view(format!("{plugin} · {title}"), &body);
        self.plugin_panel = Some(PluginPanelRef {
            plugin: plugin.to_string(),
            id: id.to_string(),
            surface: surface.to_string(),
            title: title.clone(),
        });
        self.status = format!("{plugin} · {title} · Esc closes");
    }
}

fn render_ui_node(node: &eix_core::UiNode, plugin: &str, id: &str) -> String {
    let mut out = String::new();
    append_node(&mut out, node, plugin, id, 0);
    out.trim_end().to_string()
}

fn append_node(out: &mut String, node: &eix_core::UiNode, plugin: &str, id: &str, indent: usize) {
    let pad = " ".repeat(indent);
    match node {
        eix_core::UiNode::Text { text } | eix_core::UiNode::Markdown { text } => {
            for line in text.lines() {
                out.push_str(&pad);
                out.push_str(line);
                out.push('\n');
            }
            if text.is_empty() {
                out.push('\n');
            }
        }
        eix_core::UiNode::Heading { text, level } => {
            out.push_str(&pad);
            out.push_str(&"#".repeat(*level as usize));
            out.push(' ');
            out.push_str(text);
            out.push('\n');
        }
        eix_core::UiNode::Divider => {
            out.push_str(&pad);
            out.push_str(&"─".repeat(40));
            out.push('\n');
        }
        eix_core::UiNode::List { items } => {
            for item in items {
                out.push_str(&pad);
                out.push_str("• ");
                out.push_str(item);
                out.push('\n');
            }
        }
        eix_core::UiNode::Table { columns, rows } => {
            let mut widths: Vec<usize> = columns
                .iter()
                .map(|column| column.chars().count())
                .collect();
            for row in rows {
                for (index, cell) in row.iter().enumerate() {
                    if let Some(width) = widths.get_mut(index) {
                        *width = (*width).max(cell.chars().count());
                    }
                }
            }
            let render_row = |out: &mut String, row: &[String]| {
                out.push_str(&pad);
                for (index, cell) in row.iter().enumerate() {
                    let width = widths.get(index).copied().unwrap_or(0);
                    out.push_str(cell);
                    out.push_str(&" ".repeat(width.saturating_sub(cell.chars().count()) + 2));
                }
                out.push('\n');
            };
            render_row(out, columns);
            out.push_str(&pad);
            out.push_str(&"─".repeat(widths.iter().sum::<usize>() + 2 * widths.len()));
            out.push('\n');
            for row in rows {
                render_row(out, row);
            }
        }
        eix_core::UiNode::Form {
            title,
            fields,
            submit,
            ..
        } => {
            if !title.is_empty() {
                out.push_str(&pad);
                out.push_str(title);
                out.push('\n');
            }
            for field in fields {
                out.push_str(&pad);
                out.push_str(if field.label.is_empty() {
                    &field.key
                } else {
                    &field.label
                });
                if field.required {
                    out.push_str(" *");
                }
                out.push_str(": ");
                out.push_str(&field_value(field));
                out.push('\n');
                if !field.help.is_empty() {
                    out.push_str(&pad);
                    out.push_str("    ");
                    out.push_str(&field.help);
                    out.push('\n');
                }
            }
            if let Some(action) = submit {
                out.push_str(&pad);
                out.push_str(&format!("[{}]", action.label));
                if !action.key.is_empty() {
                    out.push_str(&format!(" ({})", action.key));
                }
                out.push_str(&format!("  /plugins act {}", action.id));
                out.push('\n');
            }
        }
        eix_core::UiNode::Actions { actions } => {
            for action in actions {
                out.push_str(&pad);
                if action.kind == "danger" {
                    out.push('!');
                }
                out.push_str(&format!("[{}]", action.label));
                if !action.key.is_empty() {
                    out.push_str(&format!(" ({})", action.key));
                }
                out.push_str(&format!(
                    "  /plugins act {}  (plugin {plugin}, surface {id})",
                    action.id
                ));
                out.push('\n');
            }
        }
        eix_core::UiNode::Picker {
            id: picker_id,
            title,
            items,
        } => {
            if !title.is_empty() {
                out.push_str(&pad);
                out.push_str(title);
                out.push('\n');
            }
            for item in items {
                out.push_str(&pad);
                out.push_str("- ");
                out.push_str(&item.title);
                if !item.subtitle.is_empty() {
                    out.push_str(" — ");
                    out.push_str(&item.subtitle);
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push_str(&format!("open with /plugins pick {plugin} {picker_id}"));
            out.push('\n');
        }
        eix_core::UiNode::Group { title, children } => {
            if !title.is_empty() {
                out.push_str(&pad);
                out.push_str(title);
                out.push('\n');
            }
            for child in children {
                append_node(out, child, plugin, id, indent + 2);
            }
        }
    }
}

fn field_value(field: &eix_core::UiField) -> String {
    let value = match &field.value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if field.kind == "select" && !field.options.is_empty() {
        let options = field
            .options
            .iter()
            .map(|option| option.value.clone())
            .collect::<Vec<_>>()
            .join(" | ");
        return format!("{value}  [{options}]");
    }
    if value.is_empty() {
        field.placeholder.clone()
    } else {
        value
    }
}
