mod action;
mod anim;
mod app;
mod children;
mod crew;
mod feed;
mod herdr;
mod live;
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
    let mut ticks = Deadline::new();
    loop {
        let now = Instant::now();
        if ticks.due(now, app.tick_interval()) {
            app.tick();
            // From the end of the tick, so one slowed by a hung herdr call is not followed
            // straight away by the next.
            ticks.fired(Instant::now());
            dirty = true;
        }
        if dirty && (urgent || now.duration_since(last_draw) >= DEBOUNCE) {
            terminal.draw(|f| ui::draw(f, &app.view(), &mut list))?;
            last_draw = Instant::now();
            dirty = false;
            urgent = false;
        }
        let now = Instant::now();
        let mut wait = ticks
            .next(app.tick_interval(), now)
            .saturating_duration_since(now);
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

/// A tick deadline that messages arriving in between do not push back. It keeps when it
/// last fired rather than when it fires next, so a shorter interval takes effect at once.
#[derive(Debug, Default)]
struct Deadline {
    last: Option<Instant>,
}

impl Deadline {
    fn new() -> Self {
        Deadline::default()
    }

    fn next(&self, interval: Duration, now: Instant) -> Instant {
        self.last.map_or(now, |last| last + interval)
    }

    fn due(&self, now: Instant, interval: Duration) -> bool {
        now >= self.next(interval, now)
    }

    fn fired(&mut self, at: Instant) {
        self.last = Some(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fire(d: &mut Deadline, now: Instant, interval: Duration) -> bool {
        let due = d.due(now, interval);
        if due {
            d.fired(now);
        }
        due
    }

    #[test]
    fn steady_messages_do_not_postpone_the_tick() {
        let start = Instant::now();
        let second = Duration::from_secs(1);
        let mut d = Deadline::new();
        assert!(fire(&mut d, start, second));
        let fired: Vec<bool> = (1..=12)
            .map(|i| fire(&mut d, start + Duration::from_millis(100 * i), second))
            .collect();
        assert_eq!(fired.iter().filter(|f| **f).count(), 1);
        assert!(fired[9]);
        assert_eq!(d.next(second, start), start + 2 * second);
    }

    #[test]
    fn a_shrinking_interval_fires_within_the_new_one() {
        let start = Instant::now();
        let mut d = Deadline::new();
        assert!(fire(&mut d, start, Duration::from_secs(30)));
        let second = Duration::from_secs(1);
        assert!(!d.due(start + Duration::from_millis(500), second));
        assert!(d.due(start + second, second));
    }

    #[test]
    fn a_slow_tick_still_leaves_a_full_interval() {
        let start = Instant::now();
        let second = Duration::from_secs(1);
        let mut d = Deadline::new();
        assert!(d.due(start, second));
        let finished = start + 2 * second;
        d.fired(finished);
        assert!(!d.due(finished + Duration::from_millis(500), second));
        assert_eq!(d.next(second, finished), finished + second);
    }
}
