use std::io::{BufRead, BufReader};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::herdr::{Client, Snapshot, error_code};

/// The session events that can change what godview shows; each one triggers a fresh snapshot.
/// Output events are left out, so output bursts cost nothing.
pub const EVENTS: &[&str] = &[
    "tab.focused",
    "workspace.focused",
    "tab.moved",
    "pane.moved",
    "workspace.moved",
    "pane.created",
    "pane.closed",
    "pane.agent_detected",
];

#[derive(Debug)]
pub enum FeedMsg {
    Snapshot(Snapshot),
    Lost(String),
}

#[derive(Debug)]
enum FeedIn {
    Line(u64, String),
    Closed(u64),
    Refresh,
}

/// Asks the feed for a fresh snapshot, for changes herdr raises no event for.
#[derive(Debug, Clone)]
pub struct Feed {
    tx: Sender<FeedIn>,
}

impl Feed {
    pub fn refresh(&self) {
        let _ = self.tx.send(FeedIn::Refresh);
    }
}

/// Runs the herdr connection on its own thread: a snapshot on every subscribed event, on
/// `events_lost`, on [`Feed::refresh`] and every `period`, reconnecting on a backoff. It stops
/// once `out` returns false.
pub fn spawn(
    client: Client,
    period: Duration,
    mut out: impl FnMut(FeedMsg) -> bool + Send + 'static,
) -> Feed {
    let (tx, rx) = mpsc::channel();
    let feed = Feed { tx: tx.clone() };
    thread::spawn(move || {
        let mut conn = Connection {
            client: &client,
            tx: &tx,
            rx: &rx,
            out: &mut out,
            tag: 0,
        };
        let mut backoff = BACKOFF_START;
        loop {
            let started = Instant::now();
            let reason = match conn.run(period) {
                Outcome::Gone => return,
                Outcome::Lost(reason) => reason,
            };
            if started.elapsed() > HEALTHY {
                backoff = BACKOFF_START;
            }
            if !(conn.out)(FeedMsg::Lost(reason)) {
                return;
            }
            thread::sleep(backoff);
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    });
    feed
}

const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// A connection that stayed up this long was healthy, so its loss starts the backoff over.
const HEALTHY: Duration = Duration::from_secs(60);
/// How many refused subscribes in a row follow panes closing under them.
const SUBSCRIBE_TRIES: u32 = 3;

enum Outcome {
    Lost(String),
    Gone,
}

impl From<anyhow::Error> for Outcome {
    fn from(e: anyhow::Error) -> Self {
        Outcome::Lost(format!("{e:#}"))
    }
}

/// A live subscription; dropping it closes the socket, which ends its reader thread.
struct Live {
    tag: u64,
    stream: UnixStream,
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

struct Connection<'a, F> {
    client: &'a Client,
    tx: &'a Sender<FeedIn>,
    rx: &'a Receiver<FeedIn>,
    out: &'a mut F,
    tag: u64,
}

impl<F: FnMut(FeedMsg) -> bool> Connection<'_, F> {
    fn run(&mut self, period: Duration) -> Outcome {
        match self.session(period) {
            Ok(never) => match never {},
            Err(outcome) => outcome,
        }
    }

    fn session(&mut self, period: Duration) -> Result<std::convert::Infallible, Outcome> {
        let mut panes = self.client.snapshot()?.agent_panes();
        let mut live = self.subscribe(&mut panes)?;
        // Read again, so a change between the first read and the subscription is not missed.
        panes = self.report()?;
        loop {
            let mut refresh = false;
            let mut batch = Vec::new();
            match self.rx.recv_timeout(period) {
                Ok(msg) => batch.push(msg),
                Err(RecvTimeoutError::Timeout) => refresh = true,
                Err(RecvTimeoutError::Disconnected) => return Err(Outcome::Gone),
            }
            batch.extend(self.rx.try_iter());
            for msg in batch {
                match msg {
                    FeedIn::Line(tag, line) if tag == live.tag => refresh |= wants_snapshot(&line)?,
                    FeedIn::Closed(tag) if tag == live.tag => {
                        return Err(Outcome::Lost("herdr closed the subscription".into()));
                    }
                    FeedIn::Refresh => refresh = true,
                    FeedIn::Line(..) | FeedIn::Closed(_) => {}
                }
            }
            if !refresh {
                continue;
            }
            let now = self.report()?;
            if now != panes {
                panes = now;
                // The old one closes only once the new one is live, so no event falls between.
                let next = self.subscribe(&mut panes)?;
                live = next;
            }
        }
    }

    /// Takes a snapshot and hands it out, returning its agent panes.
    fn report(&mut self) -> Result<Vec<String>, Outcome> {
        let snapshot = self.client.snapshot()?;
        let panes = snapshot.agent_panes();
        if !(self.out)(FeedMsg::Snapshot(snapshot)) {
            return Err(Outcome::Gone);
        }
        Ok(panes)
    }

    fn subscribe(&mut self, panes: &mut Vec<String>) -> Result<Live, Outcome> {
        let mut tries = 0;
        loop {
            match self.client.subscribe(&subscriptions(panes)) {
                Ok((stream, reader)) => {
                    self.tag += 1;
                    read_lines(self.tag, reader, self.tx.clone());
                    return Ok(Live {
                        tag: self.tag,
                        stream,
                    });
                }
                // herdr refuses a status subscription for a pane that just closed.
                Err(e) if error_code(&e).is_some() && tries < SUBSCRIBE_TRIES => {
                    tries += 1;
                    *panes = self.report()?;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

fn subscriptions(panes: &[String]) -> Vec<Value> {
    EVENTS
        .iter()
        .map(|e| json!({"type": e}))
        .chain(
            panes
                .iter()
                .map(|p| json!({"type": "pane.agent_status_changed", "pane_id": p})),
        )
        .collect()
}

/// `Ok(true)` for an event or `events_lost`; any other error ends the connection.
fn wants_snapshot(line: &str) -> Result<bool, Outcome> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Ok(false);
    };
    match value["error"]["code"].as_str() {
        Some("events_lost") => Ok(true),
        Some(code) => Err(Outcome::Lost(format!("subscription error: {code}"))),
        None => Ok(value.get("event").is_some()),
    }
}

fn read_lines(tag: u64, reader: BufReader<UnixStream>, tx: Sender<FeedIn>) {
    thread::spawn(move || {
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if tx.send(FeedIn::Line(tag, line)).is_err() {
                return;
            }
        }
        let _ = tx.send(FeedIn::Closed(tag));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::*;
    use serde_json::{Value, json};
    use std::io::Read;
    use std::time::Instant;

    const WAIT: Duration = Duration::from_secs(5);

    fn start(fake: &FakeHerdr, period: Duration) -> (Feed, Receiver<FeedMsg>) {
        let (tx, rx) = mpsc::channel();
        let feed = spawn(Client::new(&fake.path), period, move |m| tx.send(m).is_ok());
        (feed, rx)
    }

    fn next_snapshot(rx: &Receiver<FeedMsg>) -> Snapshot {
        loop {
            match rx.recv_timeout(WAIT).expect("no feed message") {
                FeedMsg::Snapshot(s) => return s,
                FeedMsg::Lost(_) => {}
            }
        }
    }

    fn next_subscribe(fake: &FakeHerdr) -> Vec<Value> {
        let deadline = Instant::now() + WAIT;
        loop {
            let req = fake
                .requests
                .recv_timeout(deadline - Instant::now())
                .expect("no subscribe");
            if req["method"] == "events.subscribe" {
                return req["params"]["subscriptions"].as_array().unwrap().clone();
            }
        }
    }

    fn status_panes(subs: &[Value]) -> Vec<String> {
        subs.iter()
            .filter(|s| s["type"] == "pane.agent_status_changed")
            .map(|s| s["pane_id"].as_str().unwrap().to_string())
            .collect()
    }

    fn wait_for(mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !cond() {
            assert!(Instant::now() < deadline, "condition never held");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn subscribes_to_session_events_and_each_agent_pane() {
        let fake = FakeHerdr::start(snapshot(vec![agent("w1:p1", Some("a"), "idle", "s1")]));
        let (_feed, rx) = start(&fake, Duration::from_secs(60));
        assert_eq!(next_snapshot(&rx).agents.len(), 1);
        let subs = next_subscribe(&fake);
        for e in EVENTS {
            assert!(subs.iter().any(|s| s["type"] == *e), "missing {e}");
        }
        assert_eq!(status_panes(&subs), vec!["w1:p1"]);
    }

    #[test]
    fn a_new_pane_opens_a_new_subscription_before_closing_the_old() {
        let fake = FakeHerdr::start(snapshot(vec![agent("w1:p1", Some("a"), "idle", "s1")]));
        let (_feed, rx) = start(&fake, Duration::from_secs(60));
        next_snapshot(&rx);
        next_subscribe(&fake);
        wait_for(|| fake.subscriptions.lock().unwrap().len() == 1);
        let mut first = fake.subscriptions.lock().unwrap()[0].1.try_clone().unwrap();

        fake.set_snapshot(snapshot(vec![
            agent("w1:p1", Some("a"), "idle", "s1"),
            agent("w1:p2", Some("b"), "working", "s2"),
        ]));
        fake.push(json!({"event": "pane.created", "data": {"pane_id": "w1:p2"}}));
        wait_for(|| next_snapshot(&rx).agents.len() == 2);
        assert_eq!(status_panes(&next_subscribe(&fake)), vec!["w1:p1", "w1:p2"]);

        first.set_read_timeout(Some(WAIT)).unwrap();
        let mut buf = [0u8; 64];
        assert_eq!(
            first.read(&mut buf).unwrap(),
            0,
            "old subscription stays open"
        );
    }

    #[test]
    fn events_lost_takes_a_fresh_snapshot() {
        let fake = FakeHerdr::start(snapshot(vec![agent("w1:p1", Some("a"), "idle", "s1")]));
        let (_feed, rx) = start(&fake, Duration::from_secs(60));
        next_snapshot(&rx);
        wait_for(|| fake.subscriptions.lock().unwrap().len() == 1);
        fake.set_snapshot(snapshot(vec![agent("w1:p1", Some("a"), "working", "s1")]));
        fake.push(json!({"id": "godview:subscribe", "error": {"code": "events_lost"}}));
        wait_for(|| next_snapshot(&rx).agents[0].status == crate::herdr::Status::Working);
    }

    #[test]
    fn refresh_and_the_timer_see_a_session_change_after_clear() {
        let fake = FakeHerdr::start(snapshot(vec![agent("w1:p1", Some("a"), "idle", "s1")]));
        let (feed, rx) = start(&fake, Duration::from_millis(200));
        next_snapshot(&rx);
        fake.set_snapshot(snapshot(vec![agent("w1:p1", Some("a"), "idle", "s2")]));
        wait_for(|| next_snapshot(&rx).agents[0].session_id.as_deref() == Some("s2"));

        fake.set_snapshot(snapshot(vec![agent("w1:p1", Some("a"), "idle", "s3")]));
        feed.refresh();
        wait_for(|| next_snapshot(&rx).agents[0].session_id.as_deref() == Some("s3"));
    }

    #[test]
    fn a_dropped_connection_reports_lost_and_reconnects() {
        let fake = FakeHerdr::start(snapshot(vec![agent("w1:p1", Some("a"), "idle", "s1")]));
        let (_feed, rx) = start(&fake, Duration::from_secs(60));
        next_snapshot(&rx);
        wait_for(|| fake.subscriptions.lock().unwrap().len() == 1);
        fake.drop_subscriptions();
        match rx.recv_timeout(WAIT).unwrap() {
            FeedMsg::Lost(_) => {}
            other => panic!("expected Lost, got {other:?}"),
        }
        next_snapshot(&rx);
        wait_for(|| fake.subscriptions.lock().unwrap().len() == 1);
    }
}
