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

use crate::anim::FRAME;
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

/// Draws at most once per [`DEBOUNCE`] for file and herdr changes, at once for keys and
/// frames, and otherwise sleeps until the next message, tick or frame. Frames run only while
/// something on screen moves.
fn event_loop(terminal: &mut DefaultTerminal, mut app: App, rx: &Receiver<Msg>) -> Result<()> {
    let mut list = ListState::default();
    let mut dirty = true;
    let mut urgent = true;
    let mut last_draw = Instant::now();
    let mut ticks = Deadline::new();
    let mut frames = Deadline::new();
    loop {
        let now = Instant::now();
        if ticks.due(now, app.tick_interval()) {
            app.tick();
            // From the end of the tick, so one slowed by a hung herdr call is not followed
            // straight away by the next.
            ticks.fired(Instant::now());
            dirty = true;
        }
        let animating = app.animating();
        if animating && frames.due(now, FRAME) {
            app.advance_frame();
            frames.fired(now);
            dirty = true;
            urgent = true;
        }
        if fade_ended(last_draw, app.fade_until(), now) {
            dirty = true;
            urgent = true;
        }
        if dirty && (urgent || now.duration_since(last_draw) >= DEBOUNCE) {
            terminal.draw(|f| ui::draw(f, &app.view(), &mut list))?;
            last_draw = Instant::now();
            dirty = false;
            urgent = false;
        }
        let now = Instant::now();
        let wait = next_wait(
            now,
            ticks.next(app.tick_interval(), now),
            frames.next(FRAME, now),
            animating,
            dirty,
            last_draw,
            app.fade_until(),
        );
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

/// How long to sleep before the next tick, frame, debounced draw or fade end. A frame
/// deadline counts only while animating and a fade end only while ahead, so one left in the
/// past by an idle spell cannot spin the loop.
fn next_wait(
    now: Instant,
    tick_at: Instant,
    frame_at: Instant,
    animating: bool,
    dirty: bool,
    last_draw: Instant,
    fade_until: Option<Instant>,
) -> Duration {
    let mut at = tick_at;
    if animating {
        at = at.min(frame_at);
    }
    if dirty {
        at = at.min(last_draw + DEBOUNCE);
    }
    if let Some(fade) = fade_until.filter(|&t| t > now) {
        at = at.min(fade);
    }
    at.saturating_duration_since(now)
}

/// A flash or highlight ran out since the last draw, which still shows it.
fn fade_ended(last_draw: Instant, fade_until: Option<Instant>, now: Instant) -> bool {
    fade_until.is_some_and(|t| last_draw < t && t <= now)
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

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_wait_ignores_a_stale_frame_deadline_when_nothing_animates() {
        let now = Instant::now() + ms(10_000);
        let wait = next_wait(now, now + ms(1000), now - ms(5000), false, false, now, None);
        assert_eq!(wait, ms(1000));
    }

    #[test]
    fn the_wait_is_zero_only_when_something_is_due() {
        let now = Instant::now() + ms(10_000);
        assert_eq!(
            next_wait(now, now, now + ms(50), true, false, now, None),
            ms(0)
        );
        assert_eq!(
            next_wait(now, now + ms(1000), now, true, false, now, None),
            ms(0)
        );
        let drew = now - ms(30);
        assert_eq!(
            next_wait(now, now + ms(1000), now - ms(500), false, true, drew, None),
            ms(70)
        );
        assert_eq!(
            next_wait(now, now + ms(1000), now + ms(50), false, false, drew, None),
            ms(1000)
        );
    }

    #[test]
    fn the_wait_caps_at_the_next_frame_while_animating() {
        let now = Instant::now() + ms(10_000);
        let mut frames = Deadline::new();
        frames.fired(now - ms(25));
        let frame_at = frames.next(FRAME, now);
        let wait = next_wait(now, now + ms(30_000), frame_at, true, false, now, None);
        assert_eq!(wait, FRAME - ms(25));
    }

    #[test]
    fn without_frames_a_fade_wakes_the_loop_once_at_its_end() {
        let now = Instant::now() + ms(10_000);
        let tick = now + ms(1000);
        let stale = now - ms(5000);
        let until = Some(now + ms(300));
        assert_eq!(
            next_wait(now, tick, stale, false, false, now, until),
            ms(300)
        );
        let past = Some(now - ms(10));
        assert_eq!(
            next_wait(now, tick, stale, false, false, now, past),
            ms(1000)
        );
        assert!(fade_ended(now - ms(50), past, now));
        assert!(!fade_ended(now, past, now));
        assert!(!fade_ended(now - ms(50), until, now));
        assert!(!fade_ended(now - ms(50), None, now));
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
