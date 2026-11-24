//! What the Memories page shows of the palace as a whole: the entrance and each
//! room's memories. Each answer is kept here, per slot, with the key of the
//! palace state it was fetched for. The page draws from these slots, never from
//! the engine's read history, which keeps only the newest few answers.
//!
//! A slot is asked for when its key differs from the palace's current key (or
//! its answer is older than a minute, while shown). Requests go out in batches:
//! at most one batch every REFRESH_SPACING_MS, and only when no request of
//! ours is still in flight, so a batch never evicts a pending answer of ours.
//! A request that does not answer within SLOT_TIMEOUT_MS counts as failed; a
//! request whose answer was evicted before it arrived is asked for again.

use connecto_core::memory::{MemoryCard, ProjectMemoryView, ReadAnswer, ReadState, Room};

use super::browse::BriefKey;

/// Least time between two batches of requests.
pub(crate) const REFRESH_SPACING_MS: u64 = 2_000;
/// How long a request may wait for its answer before it counts as failed.
pub(crate) const SLOT_TIMEOUT_MS: u64 = 12_000;
/// How long an answer is kept as current while the window is shown.
pub(crate) const REFRESH_EVERY_MS: u64 = 60_000;

/// A request on its way: the engine's read id, the key asked for, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Flight {
    pub(crate) read: u64,
    pub(crate) key: BriefKey,
    pub(crate) asked_ms: u64,
}

/// One slot's answer and its state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Slot<A> {
    /// The last answer kept, whatever key it was fetched for.
    value: Option<A>,
    /// The key `value` was fetched for, and when.
    fetched: Option<(BriefKey, u64)>,
    /// The request on its way, if any.
    flight: Option<Flight>,
    /// Why the last request for a key failed. Not asked again for that key
    /// until the user asks ("Try again").
    failure: Option<(BriefKey, String)>,
}

impl<A: Clone> Slot<A> {
    /// Keep what the request answered; end one that failed, timed out, or
    /// whose answer is gone from the engine's history.
    pub(crate) fn settle(
        &mut self,
        p: &ProjectMemoryView,
        now: u64,
        take: impl Fn(&ReadAnswer) -> Option<A>,
    ) {
        let Some(flight) = self.flight.clone() else {
            return;
        };
        match p.read(flight.read).map(|r| &r.state) {
            Some(ReadState::Ready(answer)) => {
                match take(answer) {
                    Some(value) => {
                        self.value = Some(value);
                        self.fetched = Some((flight.key.clone(), flight.asked_ms));
                        self.failure = None;
                    }
                    None => {
                        self.failure =
                            Some((flight.key, "The engine answered something else.".into()));
                    }
                }
                self.flight = None;
            }
            Some(ReadState::Failed(why)) => {
                self.failure = Some((flight.key, why.clone()));
                self.flight = None;
            }
            Some(ReadState::Pending) => {
                if now.saturating_sub(flight.asked_ms) >= SLOT_TIMEOUT_MS {
                    self.failure = Some((flight.key, "No answer in time.".into()));
                    self.flight = None;
                }
            }
            // Gone from the history before it answered: asked again.
            None => self.flight = None,
        }
    }

    /// Record the outcome of asking the engine for this slot.
    pub(crate) fn started(&mut self, result: Result<u64, String>, key: BriefKey, now: u64) {
        match result {
            Ok(read) => {
                self.flight = Some(Flight {
                    read,
                    key,
                    asked_ms: now,
                });
            }
            Err(why) => {
                self.flight = None;
                self.failure = Some((key, why));
            }
        }
    }

    /// Whether this slot should be asked for now, for `key`.
    pub(crate) fn wanted(&self, key: &BriefKey, now: u64) -> bool {
        if self.flight.is_some() {
            return false;
        }
        if self.failure.as_ref().is_some_and(|(k, _)| k == key) {
            return false;
        }
        match &self.fetched {
            Some((k, at)) => k != key || now.saturating_sub(*at) >= REFRESH_EVERY_MS,
            None => true,
        }
    }

    /// The last answer kept.
    pub(crate) fn value(&self) -> Option<&A> {
        self.value.as_ref()
    }

    /// Whether the answer shown is not the one for `key`, or one is on its way.
    pub(crate) fn refreshing(&self, key: &BriefKey) -> bool {
        self.flight.is_some() || self.fetched.as_ref().is_none_or(|(k, _)| k != key)
    }

    /// The reason the request for `key` failed, if it did.
    pub(crate) fn failed_for(&self, key: &BriefKey) -> Option<&str> {
        self.failure
            .as_ref()
            .filter(|(k, _)| k == key)
            .map(|(_, why)| why.as_str())
    }

    /// Whether a request is on its way.
    pub(crate) fn in_flight(&self) -> bool {
        self.flight.is_some()
    }
}

/// Every slot of one project's palace page.
#[derive(Debug, Clone, Default)]
pub(crate) struct Cache {
    pub(crate) rooms: Vec<(Room, Slot<Vec<MemoryCard>>)>,
    pub(crate) entrance: Slot<String>,
    /// When the last batch of requests went out.
    pub(crate) last_batch_ms: Option<u64>,
}

impl Cache {
    pub(crate) fn room(&self, room: Room) -> Option<&Slot<Vec<MemoryCard>>> {
        self.rooms.iter().find(|(r, _)| *r == room).map(|(_, s)| s)
    }

    pub(crate) fn room_mut(&mut self, room: Room) -> &mut Slot<Vec<MemoryCard>> {
        let at = match self.rooms.iter().position(|(r, _)| *r == room) {
            Some(at) => at,
            None => {
                self.rooms.push((room, Slot::default()));
                self.rooms.len() - 1
            }
        };
        &mut self.rooms[at].1
    }

    /// Keep every answer that arrived.
    pub(crate) fn settle(&mut self, p: &ProjectMemoryView, now: u64) {
        for (_, slot) in &mut self.rooms {
            slot.settle(p, now, |a| match a {
                ReadAnswer::Room(cards) => Some(cards.clone()),
                _ => None,
            });
        }
        self.entrance.settle(p, now, |a| match a {
            ReadAnswer::Brief(text) => Some(text.clone()),
            _ => None,
        });
    }

    /// Whether any request of ours is on its way.
    pub(crate) fn busy(&self) -> bool {
        self.entrance.in_flight() || self.rooms.iter().any(|(_, s)| s.in_flight())
    }
}
