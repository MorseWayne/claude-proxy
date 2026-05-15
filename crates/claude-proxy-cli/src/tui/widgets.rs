#![allow(dead_code)]
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState,
        Wrap,
    },
};

use super::app::{Focus, NavItem};
use super::theme;

// ── Layout helpers ──

pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let w = area.width * percent_x / 100;
    let h = area.height * percent_y / 100;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

pub fn centered_rect_fixed(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

// ── Styles ──

pub fn selection_style() -> Style {
    theme::selection_style()
}

pub fn dim_style() -> Style {
    Style::default().fg(theme::FG_DIM)
}

pub fn accent_style() -> Style {
    Style::default().fg(theme::ACCENT)
}

pub fn accent2_style() -> Style {
    Style::default().fg(theme::ACCENT2)
}

pub fn warn_style() -> Style {
    Style::default().fg(theme::WARN)
}

pub fn err_style() -> Style {
    Style::default().fg(theme::ERR)
}

// ── Header (bordered, 3-line height with title + status badges) ──

pub fn render_header(f: &mut Frame, area: Rect, app: &super::app::App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(theme::FG_DIM))
        .style(Style::default().bg(theme::BG_DARK));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let dirty = if app.dirty { " ●" } else { "" };
    let provider_count = app.settings.providers.len();

    // Left: title
    let title_spans = vec![
        Span::styled(
            " ⬡ claude-proxy",
            Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(dirty, Style::default().fg(theme::WARN)),
    ];

    // Right: status badges
    let status_text = format!("  {} providers  ", provider_count,);
    let model_text = format!("  {}  ", app.settings.model.default);

    let right_spans = vec![
        Span::styled(
            status_text,
            Style::default().fg(theme::FG).bg(theme::BG_SURFACE),
        ),
        Span::raw(" "),
        Span::styled(
            model_text,
            Style::default()
                .fg(theme::BG_DARK)
                .bg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    // Render title left-aligned
    f.render_widget(
        Paragraph::new(Line::from(title_spans)).alignment(Alignment::Left),
        inner,
    );

    // Render status badges right-aligned
    f.render_widget(
        Paragraph::new(Line::from(right_spans)).alignment(Alignment::Right),
        inner,
    );
}

// ── Navigation panel (Table-based with icon + text columns) ──

pub fn render_nav(f: &mut Frame, area: Rect, app: &super::app::App) {
    let is_focused = matches!(app.focus, Focus::Nav);

    let rows = NavItem::ALL.iter().map(|item| {
        Row::new(vec![
            Cell::from(format!(" {}", item.icon())),
            Cell::from(item.name()),
        ])
    });

    let table = Table::new(rows, [Constraint::Length(4), Constraint::Min(10)])
        .column_spacing(1)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(theme::pane_border_style(is_focused))
                .title(" Menu ")
                .title_style(if is_focused {
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::FG_DIM)
                })
                .style(Style::default().bg(theme::BG_DARK)),
        )
        .row_highlight_style(selection_style())
        .highlight_symbol(theme::HIGHLIGHT_SYMBOL);

    let mut state = TableState::default();
    state.select(Some(app.nav_idx));
    f.render_stateful_widget(table, area, &mut state);
}

// ── Content container ──

pub fn render_content_frame(f: &mut Frame, area: Rect, app: &super::app::App, title: &str) -> Rect {
    let is_focused = matches!(app.focus, Focus::Content) || matches!(app.focus, Focus::Overlay);

    let block = Block::default()
        .title(format!(" {} ", title))
        .title_style(if is_focused {
            Style::default().fg(theme::FG).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::FG_DIM)
        })
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::pane_border_style(is_focused))
        .style(Style::default().bg(theme::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);
    inner
}

// ── Field row (improved alignment) ──

pub fn render_field(
    f: &mut Frame,
    area: Rect,
    label: &str,
    value: &str,
    is_selected: bool,
    is_masked: bool,
) {
    let display = if is_masked {
        mask_value(value)
    } else if value.is_empty() {
        "(none)".to_string()
    } else {
        value.to_string()
    };

    if is_selected {
        // Highlighted row
        let line = Line::from(vec![
            Span::styled(
                format!(" ▸ {:<16}", label),
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} ", display),
                Style::default()
                    .fg(theme::BG_DARK)
                    .bg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
    } else {
        let line = Line::from(vec![
            Span::styled(
                format!("   {:<16}", label),
                Style::default().fg(theme::FG_DIM),
            ),
            Span::styled(format!(" {}", display), Style::default().fg(theme::FG)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }
}

// ── Toggle field ──

pub fn render_toggle(f: &mut Frame, area: Rect, label: &str, value: bool, is_selected: bool) {
    let (mark, mark_color) = if value {
        ("◉ ON ", theme::ACCENT2)
    } else {
        ("○ OFF", theme::FG_DIM)
    };

    if is_selected {
        let line = Line::from(vec![
            Span::styled(
                format!(" ▸ {:<16}", label),
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} ", mark),
                Style::default()
                    .fg(theme::BG_DARK)
                    .bg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
    } else {
        let line = Line::from(vec![
            Span::styled(
                format!("   {:<16}", label),
                Style::default().fg(theme::FG_DIM),
            ),
            Span::styled(format!(" {}", mark), Style::default().fg(mark_color)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }
}

// ── Details (key-value read-only) ──

pub fn render_details(f: &mut Frame, area: Rect, rows: &[(String, String)]) {
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("   {:<16}", k), dim_style()),
                Span::styled(format!(" {}", v), Style::default().fg(theme::ACCENT)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

// ── Footer key hints (two-tone grouped style) ──

pub fn render_footer(
    f: &mut Frame,
    area: Rect,
    nav_hints: &[(&str, &str)],
    act_hints: &[(&str, &str)],
) {
    let nav_key_style = Style::default()
        .fg(theme::FOOTER_NAV_FG)
        .bg(theme::FOOTER_NAV_BG)
        .add_modifier(Modifier::BOLD);
    let nav_desc_style = Style::default()
        .fg(theme::FOOTER_NAV_FG)
        .bg(theme::FOOTER_NAV_BG);
    let act_key_style = Style::default()
        .fg(theme::FOOTER_ACT_FG)
        .bg(theme::FOOTER_ACT_BG)
        .add_modifier(Modifier::BOLD);
    let act_desc_style = Style::default()
        .fg(theme::FOOTER_ACT_FG)
        .bg(theme::FOOTER_ACT_BG);

    let mut spans: Vec<Span> = Vec::new();

    // Navigation group
    for (i, (key, desc)) in nav_hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", nav_desc_style));
        }
        spans.push(Span::styled(format!(" {} ", key), nav_key_style));
        spans.push(Span::styled(format!(" {}", desc), nav_desc_style));
    }
    if !nav_hints.is_empty() {
        spans.push(Span::styled(" ", nav_desc_style));
        // Gap between groups
        spans.push(Span::raw(" "));
    }

    // Action group
    for (i, (key, desc)) in act_hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", act_desc_style));
        }
        spans.push(Span::styled(format!(" {} ", key), act_key_style));
        spans.push(Span::styled(format!(" {}", desc), act_desc_style));
    }
    if !act_hints.is_empty() {
        spans.push(Span::styled(" ", act_desc_style));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── Toast ──

pub fn render_toast(f: &mut Frame, area: Rect, app: &super::app::App) {
    if let Some(ref toast) = app.toast {
        let msg_width = toast.message.len() as u16 + 6;
        let width = msg_width.clamp(24, 60);
        let height = 3;
        let toast_area = Rect::new(
            area.right().saturating_sub(width + 2),
            area.y + 1,
            width,
            height,
        );

        let color = match toast.kind {
            super::app::ToastKind::Info => theme::ACCENT,
            super::app::ToastKind::Success => theme::ACCENT2,
            super::app::ToastKind::Warning => theme::WARN,
            super::app::ToastKind::Error => theme::ERR,
        };

        let icon = match toast.kind {
            super::app::ToastKind::Info => "ℹ ",
            super::app::ToastKind::Success => "✓ ",
            super::app::ToastKind::Warning => "⚠ ",
            super::app::ToastKind::Error => "✗ ",
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(color))
            .style(Style::default().bg(theme::BG_DARK));
        let inner = block.inner(toast_area);
        f.render_widget(Clear, toast_area);
        f.render_widget(block, toast_area);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    icon,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(&toast.message, Style::default().fg(color)),
            ]))
            .centered(),
            inner,
        );
    }
}

// ── Confirm overlay ──

pub fn render_confirm_overlay(
    f: &mut Frame,
    area: Rect,
    title: &str,
    message: &str,
    kind: &super::app::ConfirmKind,
) {
    let dialog = centered_rect_fixed(54, 8, area);
    f.render_widget(Clear, dialog);

    let block = Block::default()
        .title(format!(" {} ", title))
        .title_style(
            Style::default()
                .fg(theme::WARN)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::WARN))
        .style(Style::default().bg(theme::BG_DARK));
    let inner = block.inner(dialog);
    f.render_widget(block, dialog);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(message)
            .style(Style::default().fg(theme::FG))
            .centered(),
        chunks[0],
    );

    let hints = match kind {
        super::app::ConfirmKind::YesNo { .. } => {
            vec![
                Span::styled(
                    " Enter ",
                    Style::default()
                        .fg(theme::BG_DARK)
                        .bg(theme::ACCENT2)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" Yes  ", dim_style()),
                Span::styled(
                    " Esc ",
                    Style::default()
                        .fg(theme::BG_DARK)
                        .bg(theme::ERR)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" No", dim_style()),
            ]
        }
        super::app::ConfirmKind::DirtyQuit => {
            vec![
                Span::styled(
                    " Enter ",
                    Style::default()
                        .fg(theme::BG_DARK)
                        .bg(theme::ACCENT2)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" Save & Quit  ", dim_style()),
                Span::styled(
                    " n ",
                    Style::default()
                        .fg(theme::BG_DARK)
                        .bg(theme::WARN)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" Discard  ", dim_style()),
                Span::styled(
                    " Esc ",
                    Style::default()
                        .fg(theme::BG_DARK)
                        .bg(theme::ERR)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" Cancel", dim_style()),
            ]
        }
        super::app::ConfirmKind::Info => {
            vec![
                Span::styled(
                    " Enter ",
                    Style::default()
                        .fg(theme::BG_DARK)
                        .bg(theme::ACCENT2)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" Close", dim_style()),
            ]
        }
    };

    f.render_widget(Paragraph::new(Line::from(hints)).centered(), chunks[1]);
}

// ── Input overlay ──

pub fn render_input_overlay(f: &mut Frame, area: Rect, overlay: &super::app::InputOverlay) {
    let dialog = centered_rect_fixed(56, 8, area);
    f.render_widget(Clear, dialog);

    let block = Block::default()
        .title(format!(" {} ", overlay.title))
        .title_style(
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::ACCENT))
        .style(Style::default().bg(theme::BG_DARK));
    let inner = block.inner(dialog);
    f.render_widget(block, dialog);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(format!("  {}", overlay.prompt)).style(dim_style()),
        chunks[0],
    );

    // Input field with visible cursor
    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(theme::BG_SURFACE))
        .style(Style::default().bg(theme::BG_SURFACE));
    let input_inner = input_block.inner(chunks[2]);
    f.render_widget(input_block, chunks[2]);

    f.render_widget(
        Paragraph::new(format!(" {}▌", overlay.value)).style(Style::default().fg(theme::ACCENT)),
        input_inner,
    );
}

// ── Help overlay ──

pub fn render_help_overlay(f: &mut Frame, area: Rect) {
    let dialog = centered_rect(75, 75, area);
    f.render_widget(Clear, dialog);

    let block = Block::default()
        .title(" Help ")
        .title_style(Style::default().fg(theme::FG).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::FG_DIM))
        .style(Style::default().bg(theme::BG_DARK));
    let inner = block.inner(dialog);
    f.render_widget(block, dialog);

    let section_style = accent_style().add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(theme::BG_DARK).bg(theme::ACCENT2);

    let help_text = vec![
        Line::from(Span::styled(" Navigation", section_style)),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ← → / h l  ", key_style),
            Span::raw("  Switch between nav and content"),
        ]),
        Line::from(vec![
            Span::styled("  ↑ ↓ / k j  ", key_style),
            Span::raw("  Move selection up/down"),
        ]),
        Line::from(""),
        Line::from(Span::styled(" Actions", section_style)),
        Line::from(""),
        Line::from(vec![
            Span::styled("  e / Enter   ", key_style),
            Span::raw("  Edit selected field / Enter page"),
        ]),
        Line::from(vec![
            Span::styled("  Space       ", key_style),
            Span::raw("  Toggle boolean values"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+S      ", key_style),
            Span::raw("  Save configuration"),
        ]),
        Line::from(""),
        Line::from(Span::styled(" Providers Page", section_style)),
        Line::from(""),
        Line::from(vec![
            Span::styled("  a           ", key_style),
            Span::raw("  Add new provider"),
        ]),
        Line::from(vec![
            Span::styled("  d           ", key_style),
            Span::raw("  Delete provider"),
        ]),
        Line::from(vec![
            Span::styled("  o           ", key_style),
            Span::raw("  Authenticate / re-authenticate OAuth provider"),
        ]),
        Line::from(vec![
            Span::styled("  t           ", key_style),
            Span::raw("  Test provider connectivity/auth"),
        ]),
        Line::from(""),
        Line::from(Span::styled(" General", section_style)),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Esc         ", key_style),
            Span::raw("  Go back / Cancel / Quit"),
        ]),
        Line::from(vec![
            Span::styled("  q           ", key_style),
            Span::raw("  Quit"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+C      ", key_style),
            Span::raw("  Force quit (no confirm)"),
        ]),
        Line::from(vec![
            Span::styled("  ?           ", key_style),
            Span::raw("  Show this help"),
        ]),
    ];

    f.render_widget(Paragraph::new(help_text).wrap(Wrap { trim: false }), inner);
}

// ── Picker overlay ──

pub fn render_picker_overlay(f: &mut Frame, area: Rect, overlay: &super::app::PickerOverlay) {
    let height = (overlay.items.len() as u16 + 5).min(20);
    let dialog = centered_rect_fixed(58, height, area);
    f.render_widget(Clear, dialog);

    let block = Block::default()
        .title(format!(" {} ", overlay.title))
        .title_style(
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::ACCENT))
        .style(Style::default().bg(theme::BG_DARK));
    let inner = block.inner(dialog);
    f.render_widget(block, dialog);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    f.render_widget(
        Paragraph::new("  ↑↓ navigate  Enter select  Esc cancel").style(dim_style()),
        chunks[0],
    );

    let items: Vec<ListItem> = overlay
        .items
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let label = if i == overlay.selected {
                format!(" ▸ {}", name)
            } else {
                format!("   {}", name)
            };
            ListItem::new(label)
        })
        .collect();

    let max_idx = overlay.items.len().saturating_sub(1);
    let selected = overlay.selected.min(max_idx);
    f.render_stateful_widget(
        List::new(items).highlight_style(selection_style()),
        chunks[1],
        &mut ratatui::widgets::ListState::default().with_selected(Some(selected)),
    );
}

// ── Loading overlay ──

pub fn render_loading_overlay(f: &mut Frame, area: Rect, overlay: &super::app::LoadingOverlay) {
    let dialog = centered_rect_fixed(46, 6, area);
    f.render_widget(Clear, dialog);

    let spinners = ["◜", "◝", "◞", "◟"];
    let spinner = spinners[(overlay.spinner_tick % 4) as usize];

    let block = Block::default()
        .title(format!(" {} {} ", spinner, overlay.title))
        .title_style(
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::ACCENT))
        .style(Style::default().bg(theme::BG_DARK));
    let inner = block.inner(dialog);
    f.render_widget(block, dialog);

    f.render_widget(
        Paragraph::new(overlay.message.as_str())
            .style(dim_style())
            .centered(),
        inner,
    );
}

// ── OAuth overlay ──

pub fn render_oauth_overlay(
    f: &mut Frame,
    area: Rect,
    overlay: &super::app::OAuthOverlay,
) {
    use super::app::OAuthStep;

    let width = 54;
    let height = 9;
    let dialog = centered_rect_fixed(width, height, area);
    f.render_widget(Clear, dialog);

    let spinners = ["◜", "◝", "◞", "◟"];
    let spinner = spinners[(overlay.spinner_tick % 4) as usize];

    let title = match &overlay.step {
        OAuthStep::Requesting => format!(" {spinner} GitHub Copilot Auth "),
        OAuthStep::Polling => format!(" {spinner} GitHub Copilot Auth "),
        OAuthStep::ShowCode { .. } => format!(" {spinner} GitHub Copilot Auth "),
        OAuthStep::Success => " ✓ GitHub Copilot Auth ".to_string(),
        OAuthStep::Failed(_) => " ✗ GitHub Copilot Auth ".to_string(),
    };

    let title_fg = match &overlay.step {
        OAuthStep::Failed(_) => theme::ERR,
        OAuthStep::Success => theme::FG,
        _ => theme::ACCENT,
    };

    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(title_fg)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if matches!(overlay.step, OAuthStep::Failed(_)) {
            theme::ERR
        } else {
            theme::ACCENT
        }))
        .style(Style::default().bg(theme::BG_DARK));
    let inner = block.inner(dialog);
    f.render_widget(block, dialog);

    let lines: Vec<Line> = match &overlay.step {
        OAuthStep::Requesting => vec![
            Line::from(""),
            Line::from(Span::styled("  Requesting device code...", dim_style())),
        ],
        OAuthStep::ShowCode { url, code } => vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  Open: {url}"),
                Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!("  Enter code: {code}"),
                Style::default()
                    .fg(theme::FG)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Press c to copy code, u to copy URL",
                dim_style(),
            )),
            Line::from(Span::styled("  Waiting for authorization...", dim_style())),
        ],
        OAuthStep::Polling => vec![
            Line::from(""),
            Line::from(Span::styled("  Waiting for authorization...", dim_style())),
        ],
        OAuthStep::Success => vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Copilot authenticated successfully!",
                Style::default().fg(theme::ACCENT),
            )),
        ],
        OAuthStep::Failed(err) => vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  Error: {err}"),
                Style::default().fg(theme::ERR),
            )),
            Line::from(""),
            Line::from(Span::styled("  Press Esc to dismiss", dim_style())),
        ],
    };

    f.render_widget(
        Paragraph::new(lines).centered(),
        inner,
    );
}

// ── Helpers ──

pub fn mask_value(s: &str) -> String {
    if s.len() <= 8 {
        "***".to_string()
    } else {
        format!("{}...{}", &s[..4], &s[s.len() - 4..])
    }
}

pub fn field_rows(area: Rect, count: usize) -> Vec<Rect> {
    let constraints: Vec<Constraint> = (0..count)
        .map(|_| Constraint::Length(2))
        .chain(std::iter::once(Constraint::Min(0)))
        .collect();
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
        .to_vec()
}
