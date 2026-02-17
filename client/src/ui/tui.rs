use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, time::Duration};
use tokio::sync::mpsc;

use super::{
    input::handle_key,
    model::{UiEvent, UiIntent, UiModel},
    render::draw,
};

pub struct Tui {
    pub tx_intent: mpsc::Sender<UiIntent>,
    pub rx_event: mpsc::Receiver<UiEvent>,
}

impl Tui {
    pub fn new(tx_intent: mpsc::Sender<UiIntent>, rx_event: mpsc::Receiver<UiEvent>) -> Self {
        Self { tx_intent, rx_event }
    }

    /// Runs the UI loop on the current thread (spawn it with tokio::task::spawn_blocking if needed).
    pub fn run_blocking(mut self, mut model: UiModel) -> Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;

        let tick = Duration::from_millis(33);

        loop {
            // Drain app->ui events
            while let Ok(ev) = self.rx_event.try_recv() {
                apply_event(&mut model, ev);
            }

            terminal.draw(|f| draw(f, &model))?;

            // Input poll
            if event::poll(tick)? {
                if let Event::Key(key) = event::read()? {
                    // Only act on press (ignore repeats/releases unless you want momentary PTT)
                    if key.kind == KeyEventKind::Press {
                        if let Some(intent) = handle_key(&mut model, key) {
                            // Best-effort; if app is overloaded, UI keeps running
                            let _ = self.tx_intent.try_send(intent.clone());
                            if matches!(intent, UiIntent::Quit) {
                                break;
                            }
                        }
                    }
                }
            }
        }

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;
        Ok(())
    }
}

fn apply_event(model: &mut UiModel, ev: UiEvent) {
    match ev {
        UiEvent::SetConnected(v) => model.connected = v,
        UiEvent::SetAuthed(v) => model.authed = v,
        UiEvent::SetChannelName(s) => model.channel_name = s,
        UiEvent::AppendLog(s) => model.push_log(s),
        UiEvent::SetStatus(s) => model.status_line = s,
        UiEvent::SetChannels(ch) => model.channels = ch,
    }
}
