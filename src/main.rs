mod action;
mod app;
mod children;
mod crew;
mod feed;
mod herdr;
mod log;
mod model;
mod profile;
mod transcript;
mod ui;
mod watch;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::widgets::ListState;

use crate::app::{App, Effect, Msg};
use crate::herdr::Client;
use crate::watch::Watches;

/// herdr raises no event when a title, the context use or a session id changes.
const REFRESH: Duration = Duration::from_secs(10);
const DEBOUNCE: Duration = Duration::from_millis(100);

fn main() -> Result<()> {
    let client = Client::from_env()
        .context("HERDR_SOCKET_PATH is not set: run godview inside a herdr pane")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => run(client),
        ["--action", "open"] => action::open(&client),
        _ => bail!("usage: godview [--action open]"),
    }
}

fn run(client: Client) -> Result<()> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let projects = PathBuf::from(home).join(".claude/projects");
    let (tx, rx) = mpsc::channel();
    let feed = {
        let tx = tx.clone();
        feed::spawn(client.clone(), REFRESH, move |m| {
            tx.send(Msg::Feed(m)).is_ok()
        })
    };
    let watches = {
        let tx = tx.clone();
        Watches::new(move |c| {
            let _ = tx.send(Msg::Watch(c));
        })?
    };
    thread::spawn(move || {
        while let Ok(ev) = event::read() {
            let msg = match ev {
                Event::Key(k) if k.kind == KeyEventKind::Press => Msg::Key(k),
                Event::Resize(..) => Msg::Resize,
                _ => continue,
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
    let app = App::new(client, feed, watches, projects);
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, app, &rx);
    ratatui::restore();
    result
}

/// Draws at most once per [`DEBOUNCE`] for file and herdr changes and at once for keys, and
/// otherwise sleeps until the next message or tick.
fn event_loop(terminal: &mut DefaultTerminal, mut app: App, rx: &Receiver<Msg>) -> Result<()> {
    let mut list = ListState::default();
    let mut dirty = true;
    let mut urgent = true;
    let mut last_draw = Instant::now();
    let mut ticks = Deadline::new(Instant::now());
    loop {
        let now = Instant::now();
        if ticks.due(now, app.tick_interval()) {
            app.tick();
            dirty = true;
        }
        if dirty && (urgent || now.duration_since(last_draw) >= DEBOUNCE) {
            terminal.draw(|f| ui::draw(f, &app.view(), &mut list))?;
            last_draw = Instant::now();
            dirty = false;
            urgent = false;
        }
        let mut wait = ticks.left(Instant::now());
        if dirty {
            wait = wait.min(DEBOUNCE.saturating_sub(last_draw.elapsed()));
        }
        let first = match rx.recv_timeout(wait) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        for msg in std::iter::once(first).chain(rx.try_iter()) {
            let key = matches!(msg, Msg::Key(_) | Msg::Resize);
            match app.handle(msg) {
                Effect::Quit => return Ok(()),
                Effect::Redraw => {
                    dirty = true;
                    urgent |= key;
                }
                Effect::None => {}
            }
        }
    }
}

/// A tick deadline that messages arriving in between do not push back.
#[derive(Debug)]
struct Deadline {
    next: Instant,
}

impl Deadline {
    fn new(now: Instant) -> Self {
        Deadline { next: now }
    }

    /// `true` once the deadline passed, and the next one is then `interval` away.
    fn due(&mut self, now: Instant, interval: Duration) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + interval;
        true
    }

    fn left(&self, now: Instant) -> Duration {
        self.next.saturating_duration_since(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_messages_do_not_postpone_the_tick() {
        let start = Instant::now();
        let second = Duration::from_secs(1);
        let mut d = Deadline::new(start);
        assert!(d.due(start, second));
        let fired: Vec<bool> = (1..=12)
            .map(|i| d.due(start + Duration::from_millis(100 * i), second))
            .collect();
        assert_eq!(fired.iter().filter(|f| **f).count(), 1);
        assert!(fired[9]);
        assert_eq!(d.left(start + second), second);
    }
}
