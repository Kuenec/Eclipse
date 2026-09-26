#![forbid(unsafe_code)]

pub const SLOT_COUNT: u8 = 3;

const MAX_UNRELEASED: usize = SLOT_COUNT as usize - 1;

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

    unreleased: [Option<u32>; SLOT_COUNT as usize],

    write_slot: u8,

    write_dirty: bool,
}

impl SlotTracker {
    pub fn new(generation: u32) -> Self {
        Self {
            generation,
            next_seq: 0,
            unreleased: [None; SLOT_COUNT as usize],
            write_slot: 0,
            write_dirty: false,
        }
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn reset(&mut self, generation: u32) {
        *self = Self::new(generation);
    }

    fn unreleased_count(&self) -> usize {
        self.unreleased.iter().flatten().count()
    }

    fn publish_write_slot(&mut self) -> Publish {
        let slot = self.write_slot;
        self.next_seq = self.next_seq.wrapping_add(1);
        let seq = self.next_seq;
        self.unreleased[usize::from(slot)] = Some(seq);
        self.write_dirty = false;
        self.write_slot = (0..SLOT_COUNT)
            .find(|s| self.unreleased[usize::from(*s)].is_none())
            .expect("MAX_UNRELEASED leaves at least one slot free to write");
        Publish {
            generation: self.generation,
            slot,
            seq,
        }
    }

    pub fn on_paint(&mut self) -> (u8, Option<Publish>) {
        let slot = self.write_slot;
        if self.unreleased_count() < MAX_UNRELEASED {
            return (slot, Some(self.publish_write_slot()));
        }
        self.write_dirty = true;
        (slot, None)
    }

    pub fn on_ack(&mut self, generation: u32, seq: u32) -> Option<Publish> {
        if generation != self.generation {
            return None;
        }
        let released = self.unreleased.iter_mut().find(|s| **s == Some(seq))?;
        *released = None;
        if self.write_dirty && self.unreleased_count() < MAX_UNRELEASED {
            return Some(self.publish_write_slot());
        }
        None
    }

    pub fn unreleased(&self) -> Vec<(u8, u32)> {
        (0..SLOT_COUNT)
            .filter_map(|s| self.unreleased[usize::from(s)].map(|seq| (s, seq)))
            .collect()
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

    fn sorted(mut v: Vec<(u8, u32)>) -> Vec<(u8, u32)> {
        v.sort_unstable();
        v
    }

    #[test]
    fn frame_slots_never_write_an_unreleased_slot() {
        let mut rng = XorShift(0x00E0_C11B_5E00_2026);
        let mut tracker = SlotTracker::new(1);

        let mut stamp: u64 = 0;
        let mut slot_stamp = [0u64; SLOT_COUNT as usize];
        let mut newest_stamp = 0u64;
        let mut unreleased: Vec<Publish> = Vec::new();
        let mut old_generation_acks: Vec<(u32, u32)> = Vec::new();

        for step in 0..50_000u32 {
            match rng.next() % 10 {
                0..=5 => {
                    let (write_slot, publish) = tracker.on_paint();

                    assert!(
                        unreleased.iter().all(|p| p.slot != write_slot),
                        "step {step}: paint handed out an unreleased slot"
                    );
                    stamp += 1;
                    slot_stamp[usize::from(write_slot)] = stamp;
                    newest_stamp = stamp;
                    if let Some(p) = publish {
                        assert!(
                            unreleased.len() < MAX_UNRELEASED,
                            "step {step}: a publish would leave no slot free to write"
                        );
                        assert_eq!(p.slot, write_slot);
                        assert_eq!(p.generation, tracker.generation());
                        unreleased.push(p);
                    }
                }

                6 | 7 => {
                    if !unreleased.is_empty() {
                        let pick = (rng.next() % unreleased.len() as u64) as usize;
                        let p = unreleased.remove(pick);
                        if let Some(n) = tracker.on_ack(p.generation, p.seq) {
                            assert!(
                                unreleased.iter().all(|u| u.slot != n.slot),
                                "step {step}: published a slot that is still unreleased"
                            );
                            assert_eq!(
                                slot_stamp[usize::from(n.slot)],
                                newest_stamp,
                                "step {step}: published slot is not the newest frame"
                            );
                            assert_eq!(n.generation, tracker.generation());
                            unreleased.push(n);
                        }
                    }
                }

                8 => {
                    let before = tracker.unreleased();
                    let bogus_seq = rng.next() as u32 | 0x8000_0000;
                    assert_eq!(tracker.on_ack(tracker.generation(), bogus_seq), None);
                    if let Some((g, s)) = old_generation_acks.last().copied() {
                        assert_eq!(tracker.on_ack(g, s), None);
                    }
                    assert_eq!(tracker.unreleased(), before, "ignored ack mutated state");
                }

                _ => {
                    for p in unreleased.drain(..) {
                        old_generation_acks.push((p.generation, p.seq));
                    }
                    let new_generation = tracker.generation() + 1;
                    tracker.reset(new_generation);
                    slot_stamp = [0; SLOT_COUNT as usize];
                    newest_stamp = 0;
                    assert!(tracker.unreleased().is_empty());
                }
            }

            assert_eq!(
                tracker.unreleased(),
                sorted(unreleased.iter().map(|p| (p.slot, p.seq)).collect()),
                "step {step}: tracker/model divergence"
            );
        }
    }

    #[test]
    fn a_consumer_holding_its_latest_frame_still_receives_newer_ones() {
        let mut t = SlotTracker::new(7);
        let (s0, p0) = t.on_paint();
        let held = p0.expect("first paint publishes");
        assert_eq!(held.slot, s0);

        let (s1, p1) = t.on_paint();
        let next = p1.expect("one held frame still leaves room to publish");
        assert_ne!(s1, held.slot);

        let (s2a, none_a) = t.on_paint();
        let (s2b, none_b) = t.on_paint();
        assert!(none_a.is_none() && none_b.is_none());
        assert_eq!(s2a, s2b, "coalescing paints reuse the one free slot");
        assert!(s2a != held.slot && s2a != next.slot);

        let newest = t
            .on_ack(7, held.seq)
            .expect("releasing the held frame publishes the coalesced one");
        assert_eq!(newest.slot, s2a);
        assert!(t.on_ack(7, held.seq).is_none(), "a re-acked seq is ignored");

        assert!(t.on_ack(7, next.seq).is_none());
        assert_eq!(t.unreleased(), vec![(newest.slot, newest.seq)]);
        let (s3, p3) = t.on_paint();
        assert_ne!(s3, newest.slot);
        assert!(p3.is_some(), "a single held frame never blocks a publish");
    }
}
