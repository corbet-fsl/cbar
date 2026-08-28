// usage.rs — how often you actually reach for something, and when that is allowed to move it.
//
// Two separate ideas, and conflating them is the classic mistake:
//
//   1. WHAT you use, as a moving average. A raw launch count says a thing you used fifty times
//      last year beats a thing you used twice this morning. An exponentially-weighted score with a
//      half-life says the opposite, which is what "frequently used" actually means to a person,
//      and it needs no history to be stored -- one number and a timestamp per entry.
//
//   2. WHETHER the difference is real. This is the part every naive frecency implementation skips,
//      and it is why they feel unusable: they re-sort on every launch, so the thing you are aiming
//      at slides out from under the cursor while you are reaching for it. A spatial launcher's
//      entire speed advantage is that positions are learnable, and an order that reshuffles on
//      noise destroys exactly that.
//
// So ordering here is not a sort. It starts from the CURRENT order and only swaps neighbours when
// the evidence clears a statistical bar -- adjacent bubble passes, gated. Positions hold still
// under noise and move only when the difference is real, which is the whole of the requirement.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Launch scores, keyed "<machine>\u{1}<app name>" -- the same key shape placement uses, and for
/// the same reason: "Firefox on one box" is not the same object as "Firefox on another".
pub type Usage = HashMap<String, Entry>;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Entry {
    /// The decayed score AS OF `last`. Decay is applied lazily on read rather than by any
    /// background pass, so an entry nobody touches costs nothing and the file stays a plain
    /// record of what happened.
    pub score: f64,
    /// Unix seconds of the most recent launch.
    pub last: u64,
}

pub fn key(machine: &str, app: &str) -> String {
    format!("{machine}\u{1}{app}")
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Half-life in days: after this long untouched, a score is worth half as much.
///
/// Thirty days is a deliberate middle. Much shorter and the order chases whatever you did
/// yesterday; much longer and it is a lifetime counter wearing a moving average's clothes.
pub const HALF_LIFE_DAYS: f64 = 30.0;

/// The score an entry is worth NOW, given when it was last touched.
pub fn decayed(entry: &Entry, now: u64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return entry.score;
    }
    let elapsed_days = now.saturating_sub(entry.last) as f64 / 86_400.0;
    entry.score * 0.5_f64.powf(elapsed_days / half_life_days)
}

pub fn score_of(usage: &Usage, machine: &str, app: &str, now: u64, half_life_days: f64) -> f64 {
    usage
        .get(&key(machine, app))
        .map(|e| decayed(e, now, half_life_days))
        .unwrap_or(0.0)
}

/// Record one launch: decay what was there, then add one.
///
/// Decaying BEFORE adding is what makes this a moving average rather than a running total. Add
/// first and every launch would be worth the same regardless of when it happened.
pub fn record(usage: &mut Usage, machine: &str, app: &str, now: u64, half_life_days: f64) {
    let e = usage.entry(key(machine, app)).or_default();
    e.score = decayed(e, now, half_life_days) + 1.0;
    e.last = now;
}

/// Is `a` beating `b` by more than chance?
///
/// Treating scores as effective counts, the difference of two Poisson-ish quantities has standard
/// error sqrt(a + b), so `z` standard errors is the bar. With z = 2 that is roughly 95%
/// confidence, and it behaves the way a person expects at both ends:
///
///   2 vs 3 launches   -> sqrt(5) = 2.24, bar 4.5, difference 1   -> NOISE, nothing moves
///   20 vs 40 launches -> sqrt(60) = 7.75, bar 15.5, difference 20 -> REAL, they swap
///
/// Which is exactly the property that stops the grid rearranging itself while you reach for
/// something: early on, when you have used two things once or twice each, no order is justified
/// and none is imposed.
pub fn significantly_greater(a: f64, b: f64, z: f64) -> bool {
    let spread = (a + b).max(0.0).sqrt();
    a - b > z * spread
}

/// Reorder `items` by score WITHOUT sorting them.
///
/// A sort would impose a total order on evidence that does not support one. This instead starts
/// from the order given -- whatever the user arranged, or whatever was there last time -- and runs
/// bubble passes that swap two neighbours only when `significantly_greater` says the difference is
/// real. An item therefore rises exactly as far as the evidence carries it and no further, and an
/// order nobody has evidence against never changes at all.
///
/// Bounded by `items.len()` passes, which is the most a bubble sort can need, so a pathological
/// score set cannot spin here.
pub fn reorder_stable<T, F>(items: &mut [T], score: F, z: f64)
where
    F: Fn(&T) -> f64,
{
    let n = items.len();
    // The score may involve a hash lookup and key construction. It is a property of the item for
    // this pass, not of the comparison, so compute it once rather than O(n²) times during bubbles.
    let mut scores: Vec<f64> = items.iter().map(&score).collect();
    for _ in 0..n {
        let mut moved = false;
        for i in 1..items.len() {
            let (lo, hi) = (scores[i - 1], scores[i]);
            if significantly_greater(hi, lo, z) {
                items.swap(i - 1, i);
                scores.swap(i - 1, i);
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
}

pub fn usage_path() -> Option<std::path::PathBuf> {
    crate::model::state_path("usage.json")
}

pub fn load() -> (Usage, Option<String>) {
    let Some(path) = usage_path() else {
        return (Usage::new(), None);
    };
    match crate::model::read_state(&path) {
        Ok(None) => (Usage::new(), None),
        Err(error) => (Usage::new(), Some(error)),
        Ok(Some(text)) => match serde_json::from_str(&text) {
            Ok(usage) => (usage, None),
            Err(e) => (Usage::new(), Some(format!("{}: {e}", path.display()))),
        },
    }
}

/// Atomic, for the same reason placement is: a truncated score file parses as "never used
/// anything", which silently discards every ordering the user has earned.
pub fn save(usage: &Usage) {
    // Not from a test, for the reason `save_placement` sets out at length: recording a launch
    // saves, so every test that records one was overwriting the developer's real score file.
    if cfg!(test) {
        return;
    }
    if let Some(path) = usage_path() {
        crate::model::write_atomic(&path, usage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the gate ─────────────────────────────────────────────────────────────────────────────

    /// The requirement in one test: small differences are noise and must not move anything.
    #[test]
    fn a_small_lead_is_not_significant() {
        assert!(
            !significantly_greater(3.0, 2.0, 2.0),
            "2 vs 3 launches is nothing"
        );
        assert!(!significantly_greater(6.0, 4.0, 2.0));
        assert!(
            !significantly_greater(1.0, 0.0, 2.0),
            "one launch beats none by nothing"
        );
    }

    /// And large ones are real.
    #[test]
    fn a_large_lead_is_significant() {
        assert!(significantly_greater(40.0, 20.0, 2.0));
        assert!(significantly_greater(100.0, 50.0, 2.0));
    }

    /// The bar scales with the square root, so the SAME absolute gap stops being significant as
    /// the numbers grow. This is the property that keeps heavily-used items from thrashing.
    #[test]
    fn the_bar_grows_with_the_totals() {
        assert!(
            significantly_greater(9.0, 1.0, 2.0),
            "gap of 8 on small totals is real"
        );
        assert!(
            !significantly_greater(108.0, 100.0, 2.0),
            "the same gap of 8 is noise up here"
        );
    }

    #[test]
    fn ties_never_swap() {
        assert!(!significantly_greater(50.0, 50.0, 2.0));
        assert!(!significantly_greater(0.0, 0.0, 2.0));
    }

    // ── the decay ────────────────────────────────────────────────────────────────────────────

    #[test]
    fn one_half_life_halves_the_score() {
        let e = Entry {
            score: 8.0,
            last: 0,
        };
        let after = decayed(&e, (30.0 * 86_400.0) as u64, 30.0);
        assert!((after - 4.0).abs() < 1e-9, "got {after}");
    }

    /// The point of a moving average: recent beats voluminous-but-old.
    #[test]
    fn recent_use_outranks_stale_volume() {
        let now = 400 * 86_400;
        let mut u = Usage::new();
        // Fifty launches, a year ago.
        u.insert(
            key("m", "old"),
            Entry {
                score: 50.0,
                last: 35 * 86_400,
            },
        );
        // Two launches, today.
        u.insert(
            key("m", "new"),
            Entry {
                score: 2.0,
                last: now,
            },
        );

        let old = score_of(&u, "m", "old", now, 30.0);
        let new = score_of(&u, "m", "new", now, 30.0);
        assert!(new > old, "new {new} should beat old {old}");
    }

    /// Decay before increment, not after: otherwise every launch is worth the same forever and
    /// this is a running total wearing a moving average's clothes.
    #[test]
    fn recording_decays_first() {
        let mut u = Usage::new();
        u.insert(
            key("m", "a"),
            Entry {
                score: 8.0,
                last: 0,
            },
        );
        record(&mut u, "m", "a", (30.0 * 86_400.0) as u64, 30.0);
        let e = &u[&key("m", "a")];
        assert!(
            (e.score - 5.0).abs() < 1e-9,
            "8 halved to 4, plus 1 = 5, got {}",
            e.score
        );
    }

    // ── the reorder ──────────────────────────────────────────────────────────────────────────

    /// THE anti-racing test. Scores that differ but not significantly must leave the order exactly
    /// as it was, including when the order contradicts the scores.
    #[test]
    fn noise_does_not_reorder() {
        let mut v = vec![("a", 2.0), ("b", 3.0), ("c", 1.0)];
        reorder_stable(&mut v, |x| x.1, 2.0);
        assert_eq!(
            v.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    /// A real difference does move, and moves all the way.
    #[test]
    fn a_real_difference_reorders() {
        let mut v = vec![("rare", 1.0), ("common", 60.0)];
        reorder_stable(&mut v, |x| x.1, 2.0);
        assert_eq!(
            v.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec!["common", "rare"]
        );
    }

    /// An item rises exactly as far as the evidence carries it: past the ones it clearly beats,
    /// and no further.
    #[test]
    fn an_item_rises_only_as_far_as_its_evidence() {
        // 100 clearly beats 1, but not 90.
        let mut v = vec![("top", 90.0), ("mid", 1.0), ("riser", 100.0)];
        reorder_stable(&mut v, |x| x.1, 2.0);
        assert_eq!(
            v.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec!["top", "riser", "mid"],
            "riser passed mid but not top"
        );
    }

    /// Idempotence is what "does not race" means formally: running it again changes nothing, so
    /// the display cannot oscillate between two orders.
    #[test]
    fn reordering_is_idempotent() {
        let mut v = vec![("a", 1.0), ("b", 60.0), ("c", 30.0), ("d", 4.0)];
        reorder_stable(&mut v, |x| x.1, 2.0);
        let once: Vec<&str> = v.iter().map(|x| x.0).collect();
        reorder_stable(&mut v, |x| x.1, 2.0);
        let twice: Vec<&str> = v.iter().map(|x| x.0).collect();
        assert_eq!(once, twice);
    }

    /// Everything at zero -- a fresh install -- must not permute anything at all.
    #[test]
    fn an_unused_grid_keeps_its_declared_order() {
        let mut v = vec![("a", 0.0), ("b", 0.0), ("c", 0.0), ("d", 0.0)];
        reorder_stable(&mut v, |x| x.1, 2.0);
        assert_eq!(
            v.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"]
        );
    }
}
