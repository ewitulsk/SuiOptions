//! Explicit timestamp types and the deterministic event queue (doc 08
//! §6.2). Every instant the engine reasons about is one of the eight
//! newtypes below — a bare `i64` "now" no longer exists in the engine —
//! and every event is ordered by `(ms, stage, sub, seq)`:
//!
//! 1. external events occur;
//! 2. events become observable after feed latency;
//! 3. timer and RFQ strategy events run against the observable cache;
//! 4. commands are submitted;
//! 5. acknowledgements, fills and chain results arrive;
//! 6. ledger and book state update.
//!
//! `seq` is the schedule order, so two events at the same instant and
//! stage always replay in the order they were created: same inputs and
//! seed ⇒ the same event trace, byte for byte (doc 08 §1 item 7).

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use serde::Serialize;

macro_rules! time_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        pub struct $name(pub i64);

        impl $name {
            pub fn ms(self) -> i64 {
                self.0
            }
        }
    };
}

time_newtype!(
    /// When the venue/source event happened (bar close, funding settlement).
    EventTime
);
time_newtype!(
    /// When the collector/oracle received it.
    ReceiveTime
);
time_newtype!(
    /// When the strategy could first act on it (observable cache update).
    ActionableTime
);
time_newtype!(
    /// When the strategy submitted an action.
    CommandTime
);
time_newtype!(
    /// When the venue accepted the action.
    AcknowledgementTime
);
time_newtype!(
    /// When an execution occurred at the venue.
    FillTime
);
time_newtype!(
    /// When a Sui transaction finalized.
    ChainInclusionTime
);
time_newtype!(
    /// When the indexer/book observed the chain result.
    DetectionTime
);

/// The §6.2 tie order. Lower runs first at the same instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Stage {
    External = 0,
    Observable = 1,
    Timer = 2,
    Command = 3,
    Outcome = 4,
    Ledger = 5,
}

/// Ordering key of one scheduled event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub ms: i64,
    pub stage: Stage,
    /// Ordering inside a stage (e.g. RFQ timers before the hedge sample
    /// at the same instant, as the desk sees a fill before it re-hedges).
    pub sub: u8,
    pub seq: u64,
}

/// A deterministic priority queue of `(Key, T)`.
#[derive(Debug)]
pub struct EventQueue<T> {
    heap: BinaryHeap<Reverse<(Key, u64)>>,
    slots: Vec<Option<T>>,
    free: Vec<u64>,
    seq: u64,
    scheduled: u64,
}

impl<T> Default for EventQueue<T> {
    fn default() -> Self {
        Self { heap: BinaryHeap::new(), slots: Vec::new(), free: Vec::new(), seq: 0, scheduled: 0 }
    }
}

impl<T> EventQueue<T> {
    pub fn schedule(&mut self, ms: i64, stage: Stage, sub: u8, ev: T) -> Key {
        let key = Key { ms, stage, sub, seq: self.seq };
        self.seq += 1;
        self.scheduled += 1;
        let slot = match self.free.pop() {
            Some(i) => {
                self.slots[i as usize] = Some(ev);
                i
            }
            None => {
                self.slots.push(Some(ev));
                (self.slots.len() - 1) as u64
            }
        };
        self.heap.push(Reverse((key, slot)));
        key
    }

    pub fn peek_key(&self) -> Option<Key> {
        self.heap.peek().map(|Reverse((k, _))| *k)
    }

    pub fn pop(&mut self) -> Option<(Key, T)> {
        let Reverse((key, slot)) = self.heap.pop()?;
        let ev = self.slots[slot as usize].take().expect("scheduled slot is filled");
        self.free.push(slot);
        Some((key, ev))
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Events ever scheduled (diagnostics).
    pub fn scheduled(&self) -> u64 {
        self.scheduled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_orders_by_ms_stage_sub_then_schedule_order() {
        let mut q = EventQueue::default();
        q.schedule(10, Stage::Outcome, 0, "fill@10");
        q.schedule(10, Stage::Timer, 1, "hedge@10");
        q.schedule(10, Stage::Timer, 0, "rfq@10");
        q.schedule(5, Stage::Ledger, 0, "nav@5");
        q.schedule(10, Stage::External, 0, "bar@10");
        q.schedule(10, Stage::Timer, 0, "rfq2@10");
        q.schedule(10, Stage::Command, 0, "cmd@10");
        q.schedule(10, Stage::Observable, 0, "obs@10");
        let mut got = Vec::new();
        while let Some((_, e)) = q.pop() {
            got.push(e);
        }
        assert_eq!(got, ["nav@5", "bar@10", "obs@10", "rfq@10", "rfq2@10", "hedge@10", "cmd@10", "fill@10"]);
        assert!(q.is_empty());
        assert_eq!(q.scheduled(), 8);
    }

    #[test]
    fn newtypes_are_ordered_and_distinct() {
        assert!(EventTime(1) < EventTime(2));
        assert_eq!(FillTime(7).ms(), 7);
        assert!(Stage::External < Stage::Observable && Stage::Outcome < Stage::Ledger);
    }
}
