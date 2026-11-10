//! Which bars to open and which to close so that the bars match the outputs.
//!
//! This is pure bookkeeping, with no GTK or Wayland in it, so that the rules that keep exactly one
//! set of bars on every output can be tested without a display.

/// The changes that make the bars match the outputs.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BarPlan {
    /// Connectors whose bars have to be closed, because the output is gone.
    pub close: Vec<String>,
    /// Connectors that exist and have no bars yet.
    pub open: Vec<String>,
}

/// Compares the connectors of the outputs that exist (`live`) with the connectors that have bars
/// (`bars`, one entry per bar, so an output with several bars repeats).
///
/// The result depends on those two lists alone. It does not matter which events led to them, in
/// which order, or whether any were lost.
#[must_use]
pub fn plan_bars(live: &[&str], bars: &[&str]) -> BarPlan {
    let mut plan = BarPlan::default();

    for &connector in bars {
        if !live.contains(&connector) && !plan.close.iter().any(|closing| closing == connector) {
            plan.close.push(connector.to_string());
        }
    }

    for &connector in live {
        if !bars.contains(&connector) && !plan.open.iter().any(|opening| opening == connector) {
            plan.open.push(connector.to_string());
        }
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(live: &[&str], bars: &[&str]) -> (Vec<String>, Vec<String>) {
        let plan = plan_bars(live, bars);
        (plan.close, plan.open)
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn nothing_changes_when_every_output_has_its_bar() {
        assert_eq!(
            plan(&["DP-1", "eDP-1"], &["DP-1", "eDP-1"]),
            (vec![], vec![])
        );
        assert_eq!(plan(&[], &[]), (vec![], vec![]));
    }

    #[test]
    fn the_initial_load_opens_a_bar_on_every_output() {
        assert_eq!(
            plan(&["DP-1", "eDP-1"], &[]),
            (vec![], names(&["DP-1", "eDP-1"]))
        );
    }

    #[test]
    fn a_removed_output_loses_exactly_its_bar() {
        assert_eq!(
            plan(&["eDP-1"], &["DP-1", "eDP-1"]),
            (names(&["DP-1"]), vec![])
        );
    }

    /// The incident: the output went away while its bar was being built, so the removal was never
    /// seen. Looking at the outputs that exist is enough to close the bar of the missing one, with
    /// no event to tell about it.
    #[test]
    fn a_bar_for_an_output_whose_removal_was_never_seen_is_closed() {
        assert_eq!(
            plan(&["eDP-1"], &["DP-1", "eDP-1"]),
            (names(&["DP-1"]), vec![])
        );
        assert_eq!(
            plan(&["eDP-1"], &["DP-1"]),
            (names(&["DP-1"]), names(&["eDP-1"]))
        );
    }

    #[test]
    fn a_returning_output_gets_its_bar_again_and_nothing_else_changes() {
        assert_eq!(
            plan(&["DP-1", "eDP-1"], &["eDP-1"]),
            (vec![], names(&["DP-1"]))
        );
    }

    /// A repeated or late "connected" event finds the bar already there and must not add a
    /// second one beside it.
    #[test]
    fn an_output_that_already_has_its_bar_gets_no_second_one() {
        for _ in 0..3 {
            assert_eq!(
                plan(&["DP-1", "eDP-1"], &["DP-1", "eDP-1"]),
                (vec![], vec![])
            );
        }
    }

    #[test]
    fn several_bars_on_one_output_stay_together() {
        // One monitor with two configured bars: neither is opened again, neither is closed.
        assert_eq!(
            plan(&["DP-1", "eDP-1"], &["DP-1", "DP-1", "eDP-1"]),
            (vec![], vec![])
        );
        // When that output is gone, both are closed in one step.
        assert_eq!(
            plan(&["eDP-1"], &["DP-1", "DP-1", "eDP-1"]),
            (names(&["DP-1"]), vec![])
        );
    }

    /// A full disable/enable cycle driven through the plan, the way the listener drives it: after
    /// every step the bars are made to match the outputs, however often the step is repeated.
    #[test]
    fn a_disable_enable_cycle_always_ends_with_one_bar_per_output() {
        fn apply(bars: &mut Vec<String>, live: &[&str]) {
            let plan = plan_bars(live, &bars.iter().map(String::as_str).collect::<Vec<_>>());
            bars.retain(|bar| !plan.close.contains(bar));
            bars.extend(plan.open);
        }

        fn sorted(bars: &[String]) -> Vec<String> {
            let mut bars = bars.to_vec();
            bars.sort();
            bars
        }

        let mut bars: Vec<String> = vec![];
        for _ in 0..50 {
            apply(&mut bars, &["DP-1", "eDP-1"]);
            apply(&mut bars, &["DP-1", "eDP-1"]);
            assert_eq!(sorted(&bars), names(&["DP-1", "eDP-1"]));

            apply(&mut bars, &["eDP-1"]);
            apply(&mut bars, &["eDP-1"]);
            assert_eq!(sorted(&bars), names(&["eDP-1"]));
        }
    }

    #[test]
    fn every_output_going_away_closes_every_bar() {
        assert_eq!(
            plan(&[], &["DP-1", "eDP-1"]),
            (names(&["DP-1", "eDP-1"]), vec![])
        );
    }
}
