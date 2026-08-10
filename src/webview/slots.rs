#![forbid(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publish {
    pub generation: u32,
    pub slot: u8,
    pub seq: u32,
}

#[derive(Debug)]
pub struct SlotTracker {
    generation: u32,
    next_seq: u32,

    published: Option<(u8, u32)>,

    spare_dirty: bool,

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

    pub fn reset(&mut self, generation: u32) {
        *self = Self::new(generation);
    }

    pub fn on_paint(&mut self) -> (u8, Option<Publish>) {
        match self.published {
            Some((published_slot, _)) => {
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

    pub fn on_ack(&mut self, generation: u32, seq: u32) -> Option<Publish> {
        if generation != self.generation {
            return None;
        }
        let (published_slot, published_seq) = self.published?;
        if published_seq != seq {
            return None;
        }
        self.published = None;

        self.write_slot = published_slot;
        if self.spare_dirty {
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

    pub fn outstanding(&self) -> Option<(u8, u32)> {
        self.published
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut rng = XorShift(0x00E0_C11B_5E00_2026);
        let mut tracker = SlotTracker::new(1);

        let mut stamp: u64 = 0;
        let mut slot_stamp = [0u64; 2];
        let mut newest_stamp = 0u64;
        let mut outstanding: Option<Publish> = None;
        let mut old_generation_acks: Vec<(u32, u32)> = Vec::new();

        for step in 0..50_000u32 {
            match rng.next() % 10 {
                0..=5 => {
                    let (write_slot, publish) = tracker.on_paint();

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

                6 | 7 => {
                    if let Some(p) = outstanding.take() {
                        let next = tracker.on_ack(p.generation, p.seq);
                        if let Some(n) = next {
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

                8 => {
                    let before = tracker.outstanding();
                    let bogus_seq = rng.next() as u32 | 0x8000_0000;
                    assert_eq!(tracker.on_ack(tracker.generation(), bogus_seq), None);
                    if let Some((g, s)) = old_generation_acks.last().copied() {
                        assert_eq!(tracker.on_ack(g, s), None);
                    }
                    assert_eq!(tracker.outstanding(), before, "ignored ack mutated state");
                }

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

            assert_eq!(
                tracker.outstanding(),
                outstanding.map(|p| (p.slot, p.seq)),
                "step {step}: tracker/model divergence"
            );
        }

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
