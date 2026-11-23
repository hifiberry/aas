//! The streams announced over SAP: fed by the SAP thread, read by the API
//! and the input.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use super::sap::SapMessage;
use super::sdp;

/// Assumed announcement interval until a second announcement is seen.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);
/// A stream is forgotten after this many intervals without an announcement.
const EXPIRY_INTERVALS: u32 = 10;
const MAX_EXPIRY: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone)]
pub struct Listed {
    pub session: sdp::Session,
    /// Announced by us: the SDP origin or the SAP source is our address.
    pub own: bool,
    pub last_seen: Instant,
    /// Measured time between the last two announcements, at least 30 s.
    pub interval: Duration,
}

impl Listed {
    fn expires_at(&self) -> Instant {
        self.last_seen + (self.interval * EXPIRY_INTERVALS).min(MAX_EXPIRY)
    }
}

pub struct Directory {
    own_origin: Option<Ipv4Addr>,
    streams: HashMap<(Ipv4Addr, u16), Listed>,
    /// Counts the changes `message` and `expire` report.
    changes: u64,
}

impl Directory {
    pub fn new(own_origin: Option<Ipv4Addr>) -> Directory {
        Directory {
            own_origin,
            streams: HashMap::new(),
            changes: 0,
        }
    }

    /// How many times the list has changed, so a reader can tell whether
    /// it has since it last looked.
    pub fn changes(&self) -> u64 {
        self.changes
    }

    /// Apply a SAP message. True when the visible list changed: a stream
    /// appeared, changed version or went away. A repeat only refreshes
    /// `last_seen`. An announcement whose SDP does not parse is ignored.
    pub fn message(&mut self, msg: SapMessage, now: Instant) -> bool {
        let changed = self.apply(msg, now);
        self.changes += u64::from(changed);
        changed
    }

    fn apply(&mut self, msg: SapMessage, now: Instant) -> bool {
        match msg {
            SapMessage::Delete { hash, source } => self.streams.remove(&(source, hash)).is_some(),
            SapMessage::Announce { hash, source, sdp } => {
                let Ok(session) = sdp::parse(&sdp) else {
                    return false;
                };
                let own = self
                    .own_origin
                    .is_some_and(|a| a == session.origin || a == source);
                match self.streams.get_mut(&(source, hash)) {
                    Some(listed) => {
                        listed.interval = now
                            .saturating_duration_since(listed.last_seen)
                            .max(DEFAULT_INTERVAL);
                        listed.last_seen = now;
                        listed.own = own;
                        if listed.session == session {
                            false
                        } else {
                            listed.session = session;
                            true
                        }
                    }
                    None => {
                        self.streams.insert(
                            (source, hash),
                            Listed {
                                session,
                                own,
                                last_seen: now,
                                interval: DEFAULT_INTERVAL,
                            },
                        );
                        true
                    }
                }
            }
        }
    }

    /// Drop streams not heard for 10 intervals (at most 3600 s). True when
    /// any went.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.streams.len();
        self.streams.retain(|_, l| l.expires_at() > now);
        let changed = self.streams.len() != before;
        self.changes += u64::from(changed);
        changed
    }

    /// The most recently seen stream with this name; callers check `own`.
    pub fn by_name(&self, name: &str) -> Option<&Listed> {
        self.streams
            .iter()
            .filter(|(_, l)| l.session.name == name)
            .max_by_key(|(k, l)| (l.last_seen, **k))
            .map(|(_, l)| l)
    }

    pub fn all(&self) -> impl Iterator<Item = &Listed> {
        self.streams.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aes67::sdp::DANTE_SDP;

    const SRC: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);
    const OTHER: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 20);

    fn sdp_text(name: &str, version: u64, origin: Ipv4Addr) -> String {
        DANTE_SDP
            .replace(
                "o=- 1311738121 1311738121 IN IP4 192.0.2.10",
                &format!("o=- 1311738121 {version} IN IP4 {origin}"),
            )
            .replace("s=AU-U22-000001 : 1", &format!("s={name}"))
    }

    fn announce(hash: u16, source: Ipv4Addr, name: &str, version: u64) -> SapMessage {
        SapMessage::Announce {
            hash,
            source,
            sdp: sdp_text(name, version, source),
        }
    }

    #[test]
    fn an_announcement_is_listed_and_updated_in_place() {
        let t = Instant::now();
        let mut d = Directory::new(None);
        assert!(d.message(announce(1, SRC, "one", 1), t));
        // An identical repeat is not a change.
        assert!(!d.message(announce(1, SRC, "one", 1), t + Duration::from_secs(30)));
        // A new version replaces the entry.
        assert!(d.message(announce(1, SRC, "renamed", 2), t + Duration::from_secs(60)));
        assert_eq!(d.all().count(), 1);
        assert!(d.by_name("one").is_none());
        let l = d.by_name("renamed").unwrap();
        assert_eq!(l.session.version, 2);
        assert_eq!(l.interval, Duration::from_secs(30));
        assert!(!l.own);
    }

    #[test]
    fn an_unparsable_announcement_is_ignored() {
        let mut d = Directory::new(None);
        let msg = SapMessage::Announce {
            hash: 1,
            source: SRC,
            sdp: "garbage".into(),
        };
        assert!(!d.message(msg, Instant::now()));
        assert_eq!(d.all().count(), 0);
    }

    #[test]
    fn changes_count_what_message_and_expire_report() {
        let t = Instant::now();
        let mut d = Directory::new(None);
        d.message(announce(1, SRC, "one", 1), t);
        d.message(announce(1, SRC, "one", 1), t);
        assert_eq!(d.changes(), 1);
        d.expire(t + Duration::from_secs(300));
        assert_eq!(d.changes(), 2);
    }

    #[test]
    fn a_delete_removes_it() {
        let t = Instant::now();
        let mut d = Directory::new(None);
        d.message(announce(1, SRC, "one", 1), t);
        assert!(!d.message(
            SapMessage::Delete {
                hash: 2,
                source: SRC
            },
            t
        ));
        assert!(!d.message(
            SapMessage::Delete {
                hash: 1,
                source: OTHER
            },
            t
        ));
        assert!(d.message(
            SapMessage::Delete {
                hash: 1,
                source: SRC
            },
            t
        ));
        assert_eq!(d.all().count(), 0);
    }

    #[test]
    fn ten_intervals_without_one_expires_it() {
        let t = Instant::now();
        let s = Duration::from_secs;
        let mut d = Directory::new(None);
        // Default interval 30 s: gone after 300 s.
        d.message(announce(1, SRC, "one", 1), t);
        assert!(!d.expire(t + s(299)));
        assert!(d.expire(t + s(300)));
        // A measured interval below 30 s is floored at 30 s.
        d.message(announce(1, SRC, "one", 1), t);
        d.message(announce(1, SRC, "one", 1), t + s(10));
        assert!(!d.expire(t + s(10 + 299)));
        assert!(d.expire(t + s(10 + 300)));
        // Measured interval 60 s: gone after 600 s.
        d.message(announce(1, SRC, "one", 1), t);
        d.message(announce(1, SRC, "one", 1), t + s(60));
        assert!(!d.expire(t + s(60 + 599)));
        assert!(d.expire(t + s(60 + 600)));
        // Measured interval 1000 s: capped at 3600 s.
        d.message(announce(1, SRC, "one", 1), t);
        d.message(announce(1, SRC, "one", 1), t + s(1000));
        assert!(!d.expire(t + s(1000 + 3599)));
        assert!(d.expire(t + s(1000 + 3600)));
    }

    #[test]
    fn a_quick_second_announcement_does_not_shorten_the_expiry() {
        let t = Instant::now();
        let s = Duration::from_secs;
        let mut d = Directory::new(None);
        d.message(announce(1, SRC, "one", 1), t);
        // A version change 1 s later, then one at the same instant.
        d.message(announce(1, SRC, "one", 2), t + s(1));
        d.message(announce(1, SRC, "one", 3), t + s(1));
        assert!(!d.expire(t + s(1 + 11)));
        assert!(!d.expire(t + s(1 + 299)));
        assert!(d.expire(t + s(1 + 300)));
    }

    #[test]
    fn our_own_announcement_is_marked_and_not_selectable() {
        let t = Instant::now();
        let mut d = Directory::new(Some(SRC));
        // By SDP origin, from another SAP source.
        let by_origin = SapMessage::Announce {
            hash: 1,
            source: OTHER,
            sdp: sdp_text("mine", 1, SRC),
        };
        d.message(by_origin, t);
        assert!(d.by_name("mine").unwrap().own);
        // By SAP source.
        let by_source = SapMessage::Announce {
            hash: 2,
            source: SRC,
            sdp: sdp_text("also mine", 1, OTHER),
        };
        d.message(by_source, t);
        assert!(d.by_name("also mine").unwrap().own);
        d.message(announce(3, OTHER, "theirs", 1), t);
        assert!(!d.by_name("theirs").unwrap().own);
    }

    #[test]
    fn two_streams_with_one_name() {
        let t = Instant::now();
        let mut d = Directory::new(None);
        d.message(announce(1, SRC, "same", 1), t);
        d.message(announce(1, OTHER, "same", 1), t + Duration::from_secs(5));
        assert_eq!(d.all().count(), 2);
        assert!(d.by_name("same").unwrap().session.origin == OTHER);
        // Equal last_seen: the higher (source, hash) wins, every time.
        d.message(announce(2, SRC, "tie", 1), t);
        d.message(announce(3, SRC, "tie", 1), t);
        assert_eq!(d.by_name("tie").unwrap().session.origin, SRC);
        assert_eq!(d.all().filter(|l| l.session.name == "tie").count(), 2);
    }
}
