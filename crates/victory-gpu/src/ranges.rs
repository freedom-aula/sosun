//! Sets of byte ranges, for the memory a submission has brought up to date
//! and what it still has to (`crate::host`'s batch of refreshes).

/// Byte ranges `[from, to)`, in order and apart: ranges that touch or
/// overlap are one. A few per host copy, so a vector is the cheapest set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ranges(Vec<(usize, usize)>);

impl Ranges {
    /// The ranges, in order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.0.iter().copied()
    }

    /// Adds `[from, to)`, joined with the ranges that overlap it or lie
    /// within `gap` bytes of it (the bytes between are then in the set).
    pub(crate) fn insert(&mut self, from: usize, to: usize, gap: usize) {
        if from >= to {
            return;
        }
        // The ranges to join: those ending within `gap` of `from` on, and
        // starting within `gap` of `to` before.
        let first = self
            .0
            .partition_point(|&(_, e)| e.saturating_add(gap) < from);
        let last = first + self.0[first..].partition_point(|&(s, _)| s <= to.saturating_add(gap));
        if first == last {
            self.0.insert(first, (from, to));
            return;
        }
        self.0[first] = (self.0[first].0.min(from), self.0[last - 1].1.max(to));
        self.0.drain(first + 1..last);
    }

    /// Takes `[from, to)` out (a range around it falls in two).
    pub(crate) fn remove(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let first = self.0.partition_point(|&(_, e)| e <= from);
        let last = first + self.0[first..].partition_point(|&(s, _)| s < to);
        if first == last {
            return;
        }
        let (s, e) = (self.0[first].0, self.0[last - 1].1);
        let left = (s < from).then_some((s, from));
        let right = (to < e).then_some((to, e));
        self.0.splice(first..last, left.into_iter().chain(right));
    }

    /// The parts of `[from, to)` not in the set, in order.
    pub(crate) fn missing(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut at = from;
        for &(s, e) in &self.0[self.0.partition_point(|&(_, e)| e <= from)..] {
            if s >= to {
                break;
            }
            if s > at {
                out.push((at, s));
            }
            at = at.max(e);
        }
        if at < to {
            out.push((at, to));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::Ranges;

    fn set(ranges: &[(usize, usize)]) -> Ranges {
        let mut r = Ranges::default();
        for &(a, b) in ranges {
            r.insert(a, b, 0);
        }
        r
    }

    fn all(r: &Ranges) -> Vec<(usize, usize)> {
        r.iter().collect()
    }

    #[test]
    fn ranges_that_touch_or_overlap_are_one() {
        let mut r = set(&[(10, 20), (30, 40)]);
        assert_eq!(all(&r), [(10, 20), (30, 40)]);
        r.insert(20, 30, 0);
        assert_eq!(all(&r), [(10, 40)], "touching both");
        r.insert(5, 12, 0);
        r.insert(38, 50, 0);
        assert_eq!(all(&r), [(5, 50)], "overlapping");
        r.insert(60, 60, 0);
        assert_eq!(all(&r), [(5, 50)], "nothing added");
    }

    #[test]
    fn a_gap_joins_ranges_close_together() {
        let mut r = Ranges::default();
        r.insert(0, 10, 5);
        r.insert(15, 20, 5);
        assert_eq!(
            all(&r),
            [(0, 20)],
            "5 bytes apart, joined with the bytes between"
        );
        r.insert(26, 30, 5);
        assert_eq!(all(&r), [(0, 20), (26, 30)], "6 bytes apart");
        r.insert(22, 24, 5);
        assert_eq!(all(&r), [(0, 30)], "within 5 of both");
        r.insert(usize::MAX - 10, usize::MAX, usize::MAX);
        assert_eq!(all(&r), [(0, usize::MAX)], "no overflow");
    }

    #[test]
    fn removing_cuts_ranges_and_splits_one_it_is_inside() {
        let mut r = set(&[(0, 100)]);
        r.remove(40, 60);
        assert_eq!(all(&r), [(0, 40), (60, 100)], "split");
        r.remove(0, 10);
        r.remove(90, 200);
        assert_eq!(all(&r), [(10, 40), (60, 90)], "ends cut");
        r.remove(30, 70);
        assert_eq!(
            all(&r),
            [(10, 30), (70, 90)],
            "a range across two, and what is between"
        );
        r.remove(0, 1000);
        assert_eq!(all(&r), [], "all of it");
        let mut r = set(&[(0, 10)]);
        r.remove(5, 5);
        r.remove(7, 3);
        assert_eq!(all(&r), [(0, 10)], "nothing taken out");
    }

    #[test]
    fn missing_lists_the_parts_not_in_the_set() {
        let r = set(&[(10, 20), (30, 40)]);
        assert_eq!(r.missing(0, 50), [(0, 10), (20, 30), (40, 50)]);
        assert_eq!(r.missing(12, 18), []);
        assert_eq!(r.missing(15, 35), [(20, 30)]);
        assert_eq!(r.missing(5, 12), [(5, 10)]);
        assert_eq!(r.missing(38, 45), [(40, 45)]);
        assert_eq!(r.missing(50, 60), [(50, 60)]);
        assert_eq!(r.missing(25, 25), []);
        assert_eq!(Ranges::default().missing(3, 9), [(3, 9)]);
    }

    /// Every operation against a bitmap, on a long run of random ones.
    #[test]
    fn ranges_match_a_bitmap() {
        const N: usize = 200;
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let mut r = Ranges::default();
        let mut bits = [false; N];
        for step in 0..20_000 {
            let (a, b) = (next(N + 1), next(N + 1));
            let (from, to) = (a.min(b), a.max(b));
            match next(4) {
                0 | 1 => {
                    r.insert(from, to, 0);
                    bits[from..to].fill(true);
                }
                2 => {
                    r.remove(from, to);
                    bits[from..to].fill(false);
                }
                _ => {
                    let mut gaps = [false; N];
                    for (s, e) in r.missing(from, to) {
                        assert!(
                            from <= s && s < e && e <= to,
                            "step {step}: a gap inside the question"
                        );
                        gaps[s..e].fill(true);
                    }
                    for i in 0..N {
                        assert_eq!(
                            gaps[i],
                            (from..to).contains(&i) && !bits[i],
                            "step {step}: byte {i}"
                        );
                    }
                }
            }
            let mut rebuilt = [false; N];
            let mut last_end = None;
            for (s, e) in r.iter() {
                assert!(
                    s < e && last_end.is_none_or(|l| l < s),
                    "step {step}: in order, apart, not empty"
                );
                rebuilt[s..e].fill(true);
                last_end = Some(e);
            }
            assert_eq!(rebuilt, bits, "step {step}");
        }
    }
}
