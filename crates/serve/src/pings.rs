//! The ping deadline's ledger: which pings a connection has written that its client has not
//! answered yet, and when the first of them is overdue.
//!
//! **An answer proves how far a client has read, and nothing else a client sends does.** A
//! Pong echoes its Ping's payload, and a client can only echo a ping once it has read every
//! byte written before it — the stream is in order. A message from the client proves only that
//! it can write. So each ping carries a sequence number, and a Pong that echoes one answers it
//! and every ping before it; a Pong that echoes none — unsolicited, which RFC 6455 allows as a
//! one-way heartbeat, or a repeat — answers nothing. The number is offset by a key drawn per
//! connection, so a client that has stopped reading cannot answer by counting: the payload has
//! to be read to be echoed.
//!
//! **A ping is due within the timeout plus the time the [`SLOWEST_READER`] needs for every byte
//! written before it that the client had not shown it read.** It must read them all to reach
//! the ping, so a client reading a large frame over a slow link answers late and is given the
//! time — the same allowance the write deadline makes per frame, here made for the whole
//! stretch, because the backlog in front of a ping is exactly what delays its answer.
//!
//! **That allowance is fixed when the ping is written.** Pings written while the client is
//! still working through a large frame all wait behind it, so an answer to the first of them
//! says nothing about how long ago the rest could have been read. Re-timing them from that
//! answer would make every one of them overdue the moment the client caught up — the slow
//! reader dropped just as it reached the end. So each keeps its own deadline, and the earliest
//! is the one that counts: a ping written after an answer, owing nothing, can fall due before
//! older ones written behind a backlog.

use std::collections::VecDeque;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::time::Duration;

use axum::extract::ws::Message;
use tokio::time::Instant;

use crate::SLOWEST_READER;

/// The pings a connection has written and its client has not yet answered.
#[derive(Debug)]
pub(crate) struct Pings {
    /// The ping deadline. `None` keeps no ledger at all, so nothing is ever due.
    timeout: Option<Duration>,
    /// Added to every sequence number on the wire, so the payloads cannot be guessed.
    key: u64,
    /// The sequence number the next ping carries.
    next: u64,
    /// What the client has shown it read: the data bytes written before the last ping it
    /// answered.
    read: u64,
    /// Written and unanswered, oldest first.
    waiting: VecDeque<Waiting>,
}

/// One unanswered ping.
#[derive(Debug)]
struct Waiting {
    seq: u64,
    /// The connection's data bytes written before it: what answering it proves was read.
    through: u64,
    /// When it is overdue.
    due: Instant,
    /// The allowance that puts it there.
    allowed: Duration,
}

impl Pings {
    pub(crate) fn new(timeout: Option<Duration>) -> Pings {
        Pings {
            timeout,
            // The standard library's per-process random keys, through the hasher they seed: no
            // dependency for a number that only has to be unguessable from outside.
            key: RandomState::new().build_hasher().finish(),
            next: 0,
            read: 0,
            waiting: VecDeque::new(),
        }
    }

    /// The next ping to write, and the sequence number its answer will echo.
    pub(crate) fn ping(&mut self) -> (u64, Message) {
        let seq = self.next;
        self.next += 1;
        let payload = seq.wrapping_add(self.key).to_be_bytes();
        (seq, Message::Ping(payload.to_vec().into()))
    }

    /// Ping `seq` was written at `at`, when the connection had written `through` bytes of data.
    pub(crate) fn sent(&mut self, seq: u64, at: Instant, through: u64) {
        let Some(timeout) = self.timeout else {
            return;
        };
        let owed = through.saturating_sub(self.read);
        let allowed =
            timeout.saturating_add(Duration::from_secs_f64(owed as f64 / SLOWEST_READER as f64));
        // Saturating, and a deadline past the end of time is never — nothing to wait for, so
        // nothing kept: a timeout of `Duration::MAX` is a natural way to spell "off", and must
        // neither panic every connection nor grow a ledger nobody can reach the end of.
        let Some(due) = at.checked_add(allowed) else {
            return;
        };
        self.waiting.push_back(Waiting {
            seq,
            through,
            due,
            allowed,
        });
    }

    /// A Pong arrived carrying `payload`. It answers the ping it echoes and every one before
    /// it, which the client read first; one that echoes no unanswered ping changes nothing.
    /// Whether it answered anything.
    pub(crate) fn answered(&mut self, payload: &[u8]) -> bool {
        let Ok(echoed) = <[u8; 8]>::try_from(payload).map(u64::from_be_bytes) else {
            return false;
        };
        let seq = echoed.wrapping_sub(self.key);
        let Some(i) = self.waiting.iter().position(|w| w.seq == seq) else {
            return false;
        };
        self.read = self.waiting[i].through;
        self.waiting.drain(..=i);
        true
    }

    /// When the first unanswered ping falls overdue, and the allowance it was given — or `None`
    /// when nothing is waiting, there is no deadline, or every deadline is too far off to name.
    pub(crate) fn due(&self) -> Option<(Instant, Duration)> {
        self.waiting
            .iter()
            .map(|w| (w.due, w.allowed))
            .min_by_key(|&(at, _)| at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(message: &Message) -> Vec<u8> {
        match message {
            Message::Ping(bytes) => bytes.to_vec(),
            other => panic!("not a ping: {other:?}"),
        }
    }

    #[test]
    fn a_ping_is_due_after_the_timeout_and_the_time_a_slow_link_needs_for_what_came_before_it() {
        let timeout = Duration::from_secs(30);
        let mut pings = Pings::new(Some(timeout));
        let start = Instant::now();

        // Nothing written, nothing due.
        assert_eq!(pings.due(), None);

        // A ping behind one second's worth of data at the slowest reader: one second more.
        let (seq, _) = pings.ping();
        pings.sent(seq, start, SLOWEST_READER);
        assert_eq!(
            pings.due(),
            Some((
                start + timeout + Duration::from_secs(1),
                timeout + Duration::from_secs(1)
            ))
        );
    }

    #[test]
    fn an_answer_proves_what_was_read_and_only_the_bytes_after_it_are_owed() {
        let timeout = Duration::from_secs(30);
        let mut pings = Pings::new(Some(timeout));
        let start = Instant::now();

        let (first, ping) = pings.ping();
        pings.sent(first, start, 10 * SLOWEST_READER);
        pings.answered(&payload(&ping));
        assert_eq!(pings.due(), None, "an answered ping is not waited on");

        // Ten seconds' worth was read before the first answer; only what came after it is owed.
        let (second, _) = pings.ping();
        let later = start + Duration::from_secs(25);
        pings.sent(second, later, 12 * SLOWEST_READER);
        assert_eq!(
            pings.due(),
            Some((
                later + timeout + Duration::from_secs(2),
                timeout + Duration::from_secs(2)
            ))
        );
    }

    #[test]
    fn a_ping_written_behind_a_backlog_keeps_its_time_when_an_earlier_one_is_answered() {
        // Two pings written behind the same large frame, a tenth of a second apart, then the
        // client reaches the first and answers it. The second is right behind it in the stream
        // and owes nothing more — but it was written long ago, and re-timing it from the bytes
        // after the answer would make it overdue on the spot.
        let timeout = Duration::from_secs(30);
        let mut pings = Pings::new(Some(timeout));
        let start = Instant::now();
        let frame = 100 * SLOWEST_READER;
        let backlog = timeout + Duration::from_secs(100);

        let (first, ping) = pings.ping();
        pings.sent(first, start, frame);
        let (second, _) = pings.ping();
        let then = start + Duration::from_millis(100);
        pings.sent(second, then, frame);
        pings.answered(&payload(&ping));
        assert_eq!(pings.due(), Some((then + backlog, backlog)));

        // A ping written after the answer owes nothing, and falls due first.
        let (third, _) = pings.ping();
        let caught_up = start + Duration::from_secs(5);
        pings.sent(third, caught_up, frame);
        assert_eq!(pings.due(), Some((caught_up + timeout, timeout)));
    }

    #[test]
    fn an_answer_to_a_later_ping_answers_the_earlier_ones() {
        // RFC 6455 lets a client answer only the most recent of several pings it read at once.
        let mut pings = Pings::new(Some(Duration::from_secs(30)));
        let start = Instant::now();
        let mut last = Vec::new();
        for i in 0..3 {
            let (seq, ping) = pings.ping();
            pings.sent(seq, start + Duration::from_secs(i), 100 * i);
            last = payload(&ping);
        }
        pings.answered(&last);
        assert_eq!(pings.due(), None, "the third answer covers all three");
    }

    #[test]
    fn a_pong_that_echoes_no_unanswered_ping_answers_nothing() {
        // An unsolicited Pong — a one-way heartbeat, which the RFC allows — or a repeat says
        // nothing about what the client has read, so a client that has stopped reading cannot
        // keep its connection by sending them.
        let mut pings = Pings::new(Some(Duration::from_secs(30)));
        let start = Instant::now();
        let (seq, ping) = pings.ping();
        pings.sent(seq, start, 0);
        let due = pings.due();

        // The next ping's payload, never written: an echo of it proves nothing either.
        let (_, unsent) = pings.ping();
        for forged in [&[][..], b"keepalive", &seq.to_be_bytes(), &payload(&unsent)] {
            assert!(!pings.answered(forged), "{forged:?} answered a ping");
        }
        assert_eq!(
            pings.due(),
            due,
            "an echo of nothing we sent moved the deadline"
        );

        assert!(pings.answered(&payload(&ping)));
        assert!(!pings.answered(&payload(&ping)), "a repeat answered again");
        assert_eq!(pings.due(), None);
    }

    #[test]
    fn a_client_cannot_answer_by_counting() {
        // A client that has stopped reading knows the protocol and can count, so a payload that
        // is just the sequence number could be echoed without being read. Two ledgers' first
        // pings differ, and neither is the bare number.
        let (mut one, mut two) = (Pings::new(None), Pings::new(None));
        let (first, ping) = one.ping();
        let (_, other) = two.ping();
        assert_ne!(payload(&ping), payload(&other));
        assert_ne!(payload(&ping), first.to_be_bytes().to_vec());
    }

    #[test]
    fn with_no_deadline_nothing_is_kept_and_nothing_is_ever_due() {
        let mut pings = Pings::new(None);
        for i in 0..1_000 {
            let (seq, _) = pings.ping();
            pings.sent(seq, Instant::now(), i);
        }
        assert!(pings.waiting.is_empty(), "a ledger nobody reads grew");
        assert_eq!(pings.due(), None);
    }

    #[test]
    fn a_deadline_too_far_off_to_name_is_never_rather_than_a_panic() {
        let mut pings = Pings::new(Some(Duration::MAX));
        for i in 0..1_000 {
            let (seq, _) = pings.ping();
            pings.sent(seq, Instant::now(), u64::MAX - i);
        }
        assert_eq!(pings.due(), None);
        assert!(
            pings.waiting.is_empty(),
            "kept pings nobody could ever be late for"
        );
    }
}
