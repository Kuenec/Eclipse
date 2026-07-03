//! The 2-slot publish/ack frame state machine (helper-side policy object).
//!
//! 2026-07-03: torn frames are impossible BY CONSTRUCTION, not by seqlock spinning — the
//! invariant is *"a slot named in an outstanding (un-acked) `FrameReady` is NEVER handed out
//! for writing."* While a `FrameReady` is outstanding, every new paint coalesces latest-wins
//! into the spare slot; the consumer's `FrameAck` both releases the published slot and
//! immediately publishes the dirty spare (swap). The consumer reads slot pixels only between
//! `FrameReady` and its own `FrameAck`, so at most one `FrameReady` is ever in flight
//! (bounded socket traffic) and the read window never overlaps a write.
//!
//! Pure logic, no I/O — property-tested under plain `cargo test`.

#![forbid(unsafe_code)]

/// A `FrameReady` publication the caller must send to the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publish {
    pub generation: u32,
    pub slot: u8,
    pub seq: u32,
}

/// The 2-slot tracker for one view's current buffer generation.
#[derive(Debug)]
pub struct SlotTracker {
    generation: u32,
    next_seq: u32,
    /// The outstanding (published, un-acked) slot and its seq, if any.
    published: Option<(u8, u32)>,
    /// Whether the spare slot holds a newer coalesced frame awaiting the ack-swap.
    spare_dirty: bool,
    /// The slot to write when nothing is outstanding.
    write_slot: u8,
}

impl SlotTracker {
    pub fn new(generation: u32) -> Self {
        Self {
            generation,
            next_seq: 0,
            published: None,
            spare_dirty: false,
            write_slot: 0,
        }
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Per-generation reset (resize): the new memfd starts with both slots free and a fresh
    /// seq space; acks for the old generation become stale and are ignored.
    pub fn reset(&mut self, generation: u32) {
        *self = Self::new(generation);
    }

    /// The helper painted a new frame. Returns `(write_slot, publish)`: the caller must copy
    /// the pixels into `write_slot` and, if `publish` is `Some`, send that `FrameReady`
    /// AFTER the copy completes. `None` means the frame was coalesced into the spare
    /// (latest-wins) because a `FrameReady` is still outstanding.
    pub fn on_paint(&mut self) -> (u8, Option<Publish>) {
        match self.published {
            Some((published_slot, _)) => {
                // Invariant: never hand out the published-unacked slot for writing.
                let spare = 1 - published_slot;
                self.spare_dirty = true;
                (spare, None)
            }
            None => {
                let slot = self.write_slot;
                self.next_seq = self.next_seq.wrapping_add(1);
                let seq = self.next_seq;
                self.published = Some((slot, seq));
                self.write_slot = 1 - slot;
                (
                    slot,
                    Some(Publish {
                        generation: self.generation,
                        slot,
                        seq,
                    }),
                )
            }
        }
    }

    /// The consumer acked `(generation, seq)`. A stale generation or a seq that does not
    /// match the outstanding publication is ignored (`None`, state untouched). A matching
    /// ack releases the published slot; if the spare is dirty it is published immediately
    /// (the swap) and the returned `FrameReady` must be sent.
    pub fn on_ack(&mut self, generation: u32, seq: u32) -> Option<Publish> {
        if generation != self.generation {
            return None;
        }
        let (published_slot, published_seq) = self.published?;
        if published_seq != seq {
            return None;
        }
        self.published = None;
        // The released slot becomes the next free-write target...
        self.write_slot = published_slot;
        if self.spare_dirty {
            // ...unless the spare holds a newer frame: publish it now (latest-wins swap).
            self.spare_dirty = false;
            let spare = 1 - published_slot;
            self.next_seq = self.next_seq.wrapping_add(1);
            let new_seq = self.next_seq;
            self.published = Some((spare, new_seq));
            return Some(Publish {
                generation: self.generation,
                slot: spare,
                seq: new_seq,
            });
        }
        None
    }

    /// The outstanding publication, if any (introspection for the helper/tests).
    pub fn outstanding(&self) -> Option<(u8, u32)> {
        self.published
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift PRNG — no `rand` dependency (no-bloat §2.5).
    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    #[test]
    fn frame_slots_never_write_a_published_unacked_slot() {
        // 2026-07-03: pins the torn-frame impossibility argument — (a) the write slot never
        // equals the published-unacked slot, (b) latest-wins coalescing publishes the newest
        // spare on ack, (c) stale-generation acks are ignored. Exhaustive-ish randomized
        // paint/ack/resize sequences with a deterministic seed.
        let mut rng = XorShift(0x00E0_C11B_5E00_2026);
        let mut tracker = SlotTracker::new(1);

        // Model: stamp every paint; track each slot's latest stamp and the newest stamp
        // overall. `outstanding` mirrors what the consumer would hold.
        let mut stamp: u64 = 0;
        let mut slot_stamp = [0u64; 2];
        let mut newest_stamp = 0u64;
        let mut outstanding: Option<Publish> = None;
        let mut old_generation_acks: Vec<(u32, u32)> = Vec::new();

        for step in 0..50_000u32 {
            match rng.next() % 10 {
                // Paint (most common).
                0..=5 => {
                    let (write_slot, publish) = tracker.on_paint();
                    // (a) never write the published-unacked slot.
                    if let Some(p) = outstanding {
                        assert_ne!(
                            write_slot, p.slot,
                            "step {step}: paint handed out the published-unacked slot"
                        );
                    }
                    stamp += 1;
                    slot_stamp[usize::from(write_slot)] = stamp;
                    newest_stamp = stamp;
                    if let Some(p) = publish {
                        assert!(
                            outstanding.is_none(),
                            "step {step}: second FrameReady while one is in flight"
                        );
                        assert_eq!(p.slot, write_slot);
                        assert_eq!(p.generation, tracker.generation());
                        outstanding = Some(p);
                    }
                }
                // Correct ack of the outstanding FrameReady.
                6 | 7 => {
                    if let Some(p) = outstanding.take() {
                        let next = tracker.on_ack(p.generation, p.seq);
                        if let Some(n) = next {
                            // (b) the ack-swap publishes the spare, which must hold the
                            // NEWEST painted frame (latest-wins coalescing).
                            assert_ne!(n.slot, p.slot, "swap must publish the other slot");
                            assert_eq!(
                                slot_stamp[usize::from(n.slot)],
                                newest_stamp,
                                "step {step}: published spare is not the newest frame"
                            );
                            assert_eq!(n.generation, tracker.generation());
                            outstanding = Some(n);
                        }
                    }
                }
                // Hostile/stale acks: wrong seq, or a recorded previous-generation ack.
                8 => {
                    let before = tracker.outstanding();
                    let bogus_seq = rng.next() as u32 | 0x8000_0000; // far from real seqs
                    assert_eq!(tracker.on_ack(tracker.generation(), bogus_seq), None);
                    if let Some((g, s)) = old_generation_acks.last().copied() {
                        // (c) stale-generation acks are ignored.
                        assert_eq!(tracker.on_ack(g, s), None);
                    }
                    assert_eq!(tracker.outstanding(), before, "ignored ack mutated state");
                }
                // Resize → new generation; everything outstanding becomes stale.
                _ => {
                    if let Some(p) = outstanding.take() {
                        old_generation_acks.push((p.generation, p.seq));
                    }
                    let new_generation = tracker.generation() + 1;
                    tracker.reset(new_generation);
                    slot_stamp = [0; 2];
                    newest_stamp = 0;
                    assert_eq!(tracker.outstanding(), None);
                }
            }
            // Global invariant: the tracker's outstanding view matches the model's.
            assert_eq!(
                tracker.outstanding(),
                outstanding.map(|p| (p.slot, p.seq)),
                "step {step}: tracker/model divergence"
            );
        }

        // Deterministic coalescing scenario: paint→publish, 3 paints coalesce into the one
        // spare, ack swaps in the newest.
        let mut t = SlotTracker::new(7);
        let (s0, p0) = t.on_paint();
        let p0 = p0.expect("first paint publishes");
        assert_eq!(p0.slot, s0);
        let (s1a, none1) = t.on_paint();
        let (s1b, none2) = t.on_paint();
        let (s1c, none3) = t.on_paint();
        assert!(none1.is_none() && none2.is_none() && none3.is_none());
        assert_eq!(s1a, 1 - s0);
        assert_eq!(s1b, s1a, "coalescing writes must reuse the same spare slot");
        assert_eq!(s1c, s1a);
        let swapped = t
            .on_ack(7, p0.seq)
            .expect("ack must publish the dirty spare");
        assert_eq!(swapped.slot, s1a);
        assert_eq!(swapped.generation, 7);
        assert!(
            t.on_ack(7, p0.seq).is_none(),
            "re-acked seq must be ignored"
        );
    }
}
