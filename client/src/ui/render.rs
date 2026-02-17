use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::model::UiModel;

pub fn draw(f: &mut Frame, model: &UiModel) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Min(5),    // body
            Constraint::Length(3), // input
            Constraint::Length(1), // status
        ])
        .split(f.area());

    draw_title(f, root[0], model);
    draw_body(f, root[1], model);
    draw_input(f, root[2], model);
    draw_status(f, root[3], model);
}

fn draw_title(f: &mut Frame, area: Rect, m: &UiModel) {
    let flags = format!(
        "conn:{} auth:{} ch:{} nick:{} ptt:{}",
        if m.connected { "Y" } else { "N" },
        if m.authed { "Y" } else { "N" },
        m.channel_name,
        m.nick,
        if m.ptt_active { "ON" } else { "OFF" }
    );

    let p = Paragraph::new(Line::from(vec![
        Span::styled(&m.title, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::raw(flags),
    ]));
    f.render_widget(p, area);
}

fn draw_body(f: &mut Frame, area: Rect, m: &UiModel) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(22), Constraint::Min(10)])
        .split(area);

    // Left: channels
    let items: Vec<ListItem> = m
        .channels
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let prefix = if i == m.selected_channel { "> " } else { "  " };
            ListItem::new(Line::from(vec![Span::raw(prefix), Span::raw(name.clone())]))
        })
        .collect();

    let channels = List::new(items).block(Block::default().borders(Borders::ALL).title("Channels"));
    f.render_widget(channels, cols[0]);

    // Right: log/chat
    let log_lines: Vec<Line> = m.log.iter().rev().take(200).rev().map(|s| Line::raw(s.clone())).collect();
    let log = Paragraph::new(log_lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title("Log"));
    f.render_widget(log, cols[1]);
}

fn draw_input(f: &mut Frame, area: Rect, m: &UiModel) {
    let b = Block::default().borders(Borders::ALL).title("Input");
    let p = Paragraph::new(m.input.clone()).block(b);
    f.render_widget(p, area);
}

fn draw_status(f: &mut Frame, area: Rect, m: &UiModel) {
    let p = Paragraph::new(m.status_line.clone());
    f.render_widget(p, area);
}
