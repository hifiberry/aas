//! Pairing a receiver on request: the first call makes it show its PIN and
//! keeps the connection, the second, with the PIN, finishes and keeps the
//! keys. Run in a helper thread, never on a receiver's packet loop.

use super::ap2::hap::{self, Started};
use super::ap2::keys::Pairings;
use super::config::AirplayConfig;
use super::rtsp::{Conn, RtspError};
use crate::api::{Answer, ApiError};
use serde_json::json;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a receiver showing its PIN is waited for.
pub const PIN_WAIT: Duration = Duration::from_secs(120);

/// Drops a held start older than `PIN_WAIT`, closing its connection.
/// Whether it dropped one.
pub fn expire(held: &Mutex<Option<Held>>, now: Instant) -> bool {
    let mut held = held.lock().unwrap();
    let old = held
        .as_ref()
        .is_some_and(|h| now.saturating_duration_since(h.since) > PIN_WAIT);
    if old {
        held.take();
    }
    old
}

/// A pairing started and waiting for its PIN.
pub struct Held {
    conn: Conn,
    started: Started,
    since: Instant,
}

/// One step of pairing the receiver of `config`: without `pin` the start,
/// with it the finish.
pub fn step(
    config: &AirplayConfig,
    pairings: &Pairings,
    held: &Mutex<Option<Held>>,
    pin: Option<&str>,
    now: Instant,
) -> Answer {
    let name = &config.name;
    let headers = [("User-Agent", USER_AGENT.to_string())];
    let Some(pin) = pin else {
        // A new start drops one that waits.
        held.lock().unwrap().take();
        let mut conn = Conn::connect(&config.host, config.port, TIMEOUT).map_err(unreachable)?;
        let started = hap::pin_start(&mut conn, &headers)
            .and_then(|()| hap::setup_start(&mut conn, &headers))
            .map_err(|e| match e {
                RtspError::Status { .. } => refused(),
                RtspError::Pairing(why) => ApiError::unprocessable(why),
                e => unreachable(e),
            })?;
        *held.lock().unwrap() = Some(Held {
            conn,
            started,
            since: now,
        });
        return Ok(json!({"pin": "shown"}));
    };
    let start_first = || ApiError {
        status: 409,
        message: format!("start pairing first: aasctl airplay pair \"{name}\""),
    };
    let Some(Held {
        mut conn,
        started,
        since,
    }) = held.lock().unwrap().take()
    else {
        return Err(start_first());
    };
    if now.saturating_duration_since(since) > PIN_WAIT {
        return Err(start_first());
    }
    let me = pairings.identity();
    let peer = hap::setup_finish(&mut conn, &headers, started, pin, &me).map_err(|e| match e {
        RtspError::WrongPin => ApiError::unprocessable("the receiver did not take the PIN"),
        RtspError::Pairing(why) => ApiError::unprocessable(why),
        e => unreachable(e),
    })?;
    pairings
        .store(name, peer)
        .map_err(|e| ApiError::internal(format!("the pairing cannot be kept: {e}")))?;
    eprintln!("aas: airplay \"{name}\": paired");
    Ok(json!({"paired": true}))
}

/// The User-Agent of pairing requests, as the sessions send it.
const USER_AGENT: &str = "AirPlay/999.0.0";

/// How long the connect and each answer may take.
const TIMEOUT: Duration = Duration::from_secs(5);

fn refused() -> ApiError {
    ApiError::unprocessable(hap::ACCESS_REFUSED)
}

fn unreachable(e: RtspError) -> ApiError {
    ApiError {
        status: 502,
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::ap2::fake_hap::FakeAccessory;
    use crate::airplay::config::{Connect, Protocol};
    use crate::airplay::fake::{Answer as Reply, FakeReceiver};
    use std::sync::Arc;

    fn config(port: u16) -> AirplayConfig {
        AirplayConfig {
            name: "test".into(),
            host: "127.0.0.1".into(),
            port,
            protocol: Protocol::AirPlay2,
            connect: Connect::Signal,
            threshold_dbfs: -50.0,
            idle_s: 1,
            latency_ms: 1500,
            volume_db: None,
            enabled: true,
        }
    }

    fn receiver(acc: &Arc<FakeAccessory>, connections: usize) -> FakeReceiver {
        let acc = acc.clone();
        FakeReceiver::serving(connections, move |_| {
            move |seen: &crate::airplay::fake::Seen| {
                acc.answer(seen).unwrap_or(Reply::Status(200, Vec::new()))
            }
        })
    }

    fn status(answer: Answer) -> (u16, String) {
        let e = answer.unwrap_err();
        (e.status, e.message)
    }

    #[test]
    fn start_then_pin_pairs_and_stores_the_keys() {
        let acc = FakeAccessory::new("1234");
        let r = receiver(&acc, 1);
        let (pairings, held, now) = (Pairings::new(), Mutex::new(None), Instant::now());
        let c = config(r.port);
        assert_eq!(
            step(&c, &pairings, &held, None, now).unwrap(),
            json!({"pin": "shown"})
        );
        assert_eq!(
            step(&c, &pairings, &held, Some("1234"), now).unwrap(),
            json!({"paired": true})
        );
        assert_eq!(pairings.peer("test"), Some(acc.peer()));
        assert!(held.lock().unwrap().is_none());
    }

    #[test]
    fn a_pin_without_a_start_is_409() {
        let (pairings, held) = (Pairings::new(), Mutex::new(None));
        let answer = step(&config(9), &pairings, &held, Some("1234"), Instant::now());
        assert_eq!(
            status(answer),
            (
                409,
                "start pairing first: aasctl airplay pair \"test\"".into()
            )
        );
    }

    #[test]
    fn a_pin_after_two_minutes_is_409() {
        let acc = FakeAccessory::new("1234");
        let r = receiver(&acc, 1);
        let (pairings, held, now) = (Pairings::new(), Mutex::new(None), Instant::now());
        let c = config(r.port);
        step(&c, &pairings, &held, None, now).unwrap();
        let late = now + PIN_WAIT + Duration::from_secs(1);
        assert_eq!(
            status(step(&c, &pairings, &held, Some("1234"), late)).0,
            409
        );
        assert_eq!(pairings.peer("test"), None);
    }

    #[test]
    fn a_wrong_pin_stores_nothing_and_a_new_start_works() {
        let acc = FakeAccessory::new("1234");
        let r = receiver(&acc, 2);
        let (pairings, held, now) = (Pairings::new(), Mutex::new(None), Instant::now());
        let c = config(r.port);
        step(&c, &pairings, &held, None, now).unwrap();
        assert_eq!(
            status(step(&c, &pairings, &held, Some("0000"), now)),
            (422, "the receiver did not take the PIN".into())
        );
        assert_eq!(pairings.peer("test"), None);
        assert_eq!(
            step(&c, &pairings, &held, None, now).unwrap(),
            json!({"pin": "shown"})
        );
    }

    #[test]
    fn a_receiver_that_refuses_the_pin_is_422() {
        let r = FakeReceiver::start(|_| Reply::Status(403, Vec::new()));
        let (pairings, held) = (Pairings::new(), Mutex::new(None));
        assert_eq!(
            status(step(
                &config(r.port),
                &pairings,
                &held,
                None,
                Instant::now()
            )),
            (
                422,
                "the receiver refused pairing: allow AirPlay access for everyone on the network"
                    .into()
            )
        );
    }

    #[test]
    fn a_busy_receiver_says_to_try_again() {
        let acc = FakeAccessory::new("1234");
        acc.busy_once();
        let r = receiver(&acc, 1);
        let (pairings, held) = (Pairings::new(), Mutex::new(None));
        assert_eq!(
            status(step(
                &config(r.port),
                &pairings,
                &held,
                None,
                Instant::now()
            )),
            (422, "the receiver is busy: try again in a moment".into())
        );
    }

    #[test]
    fn a_held_start_is_dropped_after_two_minutes() {
        let acc = FakeAccessory::new("1234");
        let r = receiver(&acc, 1);
        let (pairings, held, now) = (Pairings::new(), Mutex::new(None), Instant::now());
        step(&config(r.port), &pairings, &held, None, now).unwrap();
        assert!(!expire(&held, now + PIN_WAIT));
        assert!(held.lock().unwrap().is_some());
        assert!(expire(&held, now + PIN_WAIT + Duration::from_secs(1)));
        assert!(held.lock().unwrap().is_none());
        // The connection is closed with it: the receiver's side ends.
        r.finish();
    }

    #[test]
    fn an_unreachable_receiver_is_502() {
        let (pairings, held) = (Pairings::new(), Mutex::new(None));
        let answer = step(&config(9), &pairings, &held, None, Instant::now());
        assert_eq!(status(answer).0, 502);
    }
}
