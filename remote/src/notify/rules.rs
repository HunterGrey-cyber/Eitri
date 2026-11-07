use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use eitri_core::agents::{Change, Class, GoneWhy};

pub const NEEDS_YOU_COALESCE: Duration = Duration::from_secs(5);
pub const FINISHED_COALESCE: Duration = Duration::from_secs(60);
pub const DESK_ACTIVE_HOLD: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    NeedsYou,
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub device_id: String,
    pub endpoint: String,
    pub finished_keys: BTreeSet<String>,
}

pub struct Ctx<'a> {
    pub targets: &'a [Target],
    pub connected: &'a BTreeSet<String>,
    pub desk_active_until: Option<Instant>,
}

#[derive(Default)]
pub struct WakeRules {
    rows: BTreeMap<String, Class>,
    /// Classes of rows that left for another window; the row that lands there is compared with it.
    moved: BTreeMap<String, Class>,
    /// The first Reset is the baseline; a later one also carries changes the reader folded into it.
    baselined: bool,
    devices: BTreeMap<String, DeviceWakes>,
}

#[derive(Default)]
struct DeviceWakes {
    needs_you: Coalesced,
    finished: Coalesced,
}

#[derive(Default)]
struct Coalesced {
    last: Option<Instant>,
    pending: bool,
}

impl Coalesced {
    fn take_due(&mut self, now: Instant, interval: Duration) -> bool {
        if !self.pending
            || self
                .last
                .is_some_and(|last| now.saturating_duration_since(last) < interval)
        {
            return false;
        }
        self.pending = false;
        self.last = Some(now);
        true
    }
}

impl WakeRules {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one agents change and returns the wakes due now. The first Reset establishes a silent baseline.
    pub fn on_change(&mut self, change: &Change, now: Instant, ctx: &Ctx<'_>) -> Vec<(Target, Wake)> {
        let mut edges = Vec::new();
        match change {
            Change::Row { row } => {
                let previous = self
                    .moved
                    .remove(&row.key)
                    .or(self.rows.insert(row.key.clone(), row.class));
                edges.extend(edge(previous, row.class).map(|wake| (row.key.as_str(), wake)));
            }
            Change::Gone { key, why, .. } => {
                if let Some(class) = self.rows.remove(key) {
                    if *why == GoneWhy::Moved {
                        self.moved.insert(key.clone(), class);
                    }
                }
            }
            Change::Reset { view } => {
                let previous = std::mem::take(&mut self.rows);
                let moved = std::mem::take(&mut self.moved);
                self.rows = view.rows.iter().map(|row| (row.key.clone(), row.class)).collect();
                if std::mem::replace(&mut self.baselined, true) {
                    for row in &view.rows {
                        let before = moved.get(&row.key).or(previous.get(&row.key)).copied();
                        edges.extend(edge(before, row.class).map(|wake| (row.key.as_str(), wake)));
                    }
                }
            }
            Change::Selection { .. } | Change::Round { .. } => {}
        }
        self.reconcile(ctx);
        if edges.is_empty() && !matches!(change, Change::Row { .. }) {
            return Vec::new();
        }
        for target in ctx.targets {
            if ctx.connected.contains(&target.device_id) {
                continue;
            }
            let device = self.devices.entry(target.device_id.clone()).or_default();
            for (key, wake) in &edges {
                match wake {
                    Wake::NeedsYou => device.needs_you.pending = true,
                    Wake::Finished if target.finished_keys.contains(*key) => device.finished.pending = true,
                    Wake::Finished => {}
                }
            }
        }
        self.tick(now, ctx)
    }

    /// Releases held or coalesced wakes using only the caller's time and current context.
    pub fn tick(&mut self, now: Instant, ctx: &Ctx<'_>) -> Vec<(Target, Wake)> {
        self.reconcile(ctx);
        let desk_active = ctx.desk_active_until.is_some_and(|until| now < until);
        // A row on its way to another window keeps a held wake, which goes only once a row here needs the person.
        let needs_here = self.rows.values().any(|class| *class == Class::Needs);
        let mut due = Vec::new();
        for target in ctx.targets {
            let Some(device) = self.devices.get_mut(&target.device_id) else {
                continue;
            };
            if !desk_active && needs_here && device.needs_you.take_due(now, NEEDS_YOU_COALESCE) {
                due.push((target.clone(), Wake::NeedsYou));
            }
            if device.finished.take_due(now, FINISHED_COALESCE) {
                due.push((target.clone(), Wake::Finished));
            }
        }
        due
    }

    fn reconcile(&mut self, ctx: &Ctx<'_>) {
        let needs_you = self
            .rows
            .values()
            .chain(self.moved.values())
            .any(|class| *class == Class::Needs);
        self.devices
            .retain(|id, _| ctx.targets.iter().any(|target| target.device_id == *id));
        for (id, device) in &mut self.devices {
            let connected = ctx.connected.contains(id);
            // A connected app observes changes locally; old wakes must not replay on disconnect.
            if connected || !needs_you {
                device.needs_you.pending = false;
            }
            if connected {
                device.finished.pending = false;
            }
        }
    }
}

/// The wake a row's class change makes, if any.
fn edge(previous: Option<Class>, class: Class) -> Option<Wake> {
    if class == Class::Needs && previous != Some(Class::Needs) {
        Some(Wake::NeedsYou)
    } else if previous == Some(Class::Working) && matches!(class, Class::Quiet | Class::Ended) {
        Some(Wake::Finished)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::agents::{GoneWhy, Row, View};

    fn target(id: &str, keys: &[&str]) -> Target {
        Target {
            device_id: id.into(),
            endpoint: format!("https://push.example/{id}"),
            finished_keys: keys.iter().map(|key| (*key).into()).collect(),
        }
    }

    fn row(key: &str, class: Class) -> Change {
        Change::Row {
            row: Row {
                key: key.into(),
                class,
                ..Row::default()
            },
        }
    }

    fn ctx<'a>(targets: &'a [Target], connected: &'a BTreeSet<String>) -> Ctx<'a> {
        Ctx {
            targets,
            connected,
            desk_active_until: None,
        }
    }

    #[test]
    fn needs_you_on_the_edge_only() {
        let now = Instant::now();
        let targets = [target("a", &[])];
        let connected = BTreeSet::new();
        let ctx = ctx(&targets, &connected);
        let mut rules = WakeRules::new();
        assert!(rules.on_change(&row("x", Class::Working), now, &ctx).is_empty());
        assert_eq!(
            rules.on_change(&row("x", Class::Needs), now, &ctx),
            vec![(targets[0].clone(), Wake::NeedsYou)]
        );
        assert!(rules
            .on_change(&row("x", Class::Needs), now + NEEDS_YOU_COALESCE, &ctx)
            .is_empty());
        assert!(rules.tick(now + FINISHED_COALESCE, &ctx).is_empty());
        rules.on_change(&row("x", Class::Quiet), now + FINISHED_COALESCE, &ctx);
        assert_eq!(
            rules
                .on_change(&row("x", Class::Needs), now + FINISHED_COALESCE, &ctx)
                .len(),
            1
        );
    }

    #[test]
    fn finished_only_for_belled_keys() {
        for end in [Class::Quiet, Class::Ended] {
            let now = Instant::now();
            let targets = [target("a", &["x"]), target("b", &[])];
            let connected = BTreeSet::new();
            let ctx = ctx(&targets, &connected);
            let mut rules = WakeRules::new();
            assert!(rules.on_change(&row("x", end), now, &ctx).is_empty());
            rules.on_change(&row("x", Class::Working), now, &ctx);
            assert_eq!(
                rules.on_change(&row("x", end), now, &ctx),
                vec![(targets[0].clone(), Wake::Finished)]
            );
            assert!(rules
                .on_change(&row("x", end), now + FINISHED_COALESCE, &ctx)
                .is_empty());
            rules.on_change(&row("y", Class::Working), now, &ctx);
            assert!(rules.on_change(&row("y", end), now, &ctx).is_empty());
        }
    }

    #[test]
    fn connected_device_gets_nothing() {
        let now = Instant::now();
        let targets = [target("a", &["x"])];
        let connected = BTreeSet::from(["a".into()]);
        let ctx = ctx(&targets, &connected);
        let mut rules = WakeRules::new();
        rules.on_change(&row("x", Class::Working), now, &ctx);
        assert!(rules.on_change(&row("x", Class::Needs), now, &ctx).is_empty());
        rules.on_change(&row("x", Class::Working), now, &ctx);
        assert!(rules.on_change(&row("x", Class::Quiet), now, &ctx).is_empty());
        assert!(rules.tick(now + DESK_ACTIVE_HOLD, &ctx).is_empty());
        let disconnected = BTreeSet::new();
        assert!(rules
            .tick(now + DESK_ACTIVE_HOLD, &self::ctx(&targets, &disconnected))
            .is_empty());
    }

    #[test]
    fn desk_active_holds_needs_you_until_it_passes() {
        let now = Instant::now();
        let targets = [target("a", &["y"])];
        let connected = BTreeSet::new();
        let mut ctx = ctx(&targets, &connected);
        ctx.desk_active_until = Some(now + DESK_ACTIVE_HOLD);
        let mut rules = WakeRules::new();
        assert!(rules.on_change(&row("x", Class::Needs), now, &ctx).is_empty());
        rules.on_change(&row("y", Class::Working), now, &ctx);
        assert_eq!(
            rules.on_change(&row("y", Class::Quiet), now, &ctx),
            vec![(targets[0].clone(), Wake::Finished)]
        );
        assert!(rules
            .tick(now + DESK_ACTIVE_HOLD - Duration::from_nanos(1), &ctx)
            .is_empty());
        assert_eq!(
            rules.tick(now + DESK_ACTIVE_HOLD, &ctx),
            vec![(targets[0].clone(), Wake::NeedsYou)]
        );
        assert!(rules.tick(now + DESK_ACTIVE_HOLD + NEEDS_YOU_COALESCE, &ctx).is_empty());
    }

    #[test]
    fn held_wake_dropped_when_nothing_needs_any_more() {
        let now = Instant::now();
        let targets = [target("a", &[])];
        let connected = BTreeSet::new();
        let mut ctx = ctx(&targets, &connected);
        ctx.desk_active_until = Some(now + DESK_ACTIVE_HOLD);
        let mut rules = WakeRules::new();
        rules.on_change(&row("x", Class::Needs), now, &ctx);
        rules.on_change(&row("x", Class::Quiet), now, &ctx);
        assert!(rules.tick(now + DESK_ACTIVE_HOLD, &ctx).is_empty());
    }

    #[test]
    fn coalesced_per_device_and_kind() {
        let now = Instant::now();
        let targets = [target("a", &["x"]), target("b", &["x"])];
        let connected = BTreeSet::from(["b".into()]);
        let mut rules = WakeRules::new();
        assert_eq!(
            rules.on_change(&row("x", Class::Needs), now, &ctx(&targets, &connected)),
            vec![(targets[0].clone(), Wake::NeedsYou)]
        );
        let connected = BTreeSet::new();
        let ctx = ctx(&targets, &connected);
        rules.on_change(&row("x", Class::Working), now, &ctx);
        assert_eq!(rules.on_change(&row("x", Class::Quiet), now, &ctx).len(), 2);
        assert_eq!(
            rules.on_change(&row("x", Class::Needs), now + Duration::from_secs(1), &ctx),
            vec![(targets[1].clone(), Wake::NeedsYou)]
        );
        for seconds in [2, 3] {
            let at = now + Duration::from_secs(seconds);
            rules.on_change(&row("x", Class::Working), at, &ctx);
            assert!(rules.on_change(&row("x", Class::Quiet), at, &ctx).is_empty());
            assert!(rules.on_change(&row("x", Class::Needs), at, &ctx).is_empty());
        }
        assert!(rules
            .tick(now + NEEDS_YOU_COALESCE - Duration::from_nanos(1), &ctx)
            .is_empty());
        assert_eq!(
            rules.tick(now + NEEDS_YOU_COALESCE, &ctx),
            vec![(targets[0].clone(), Wake::NeedsYou)]
        );
        assert_eq!(
            rules.tick(now + NEEDS_YOU_COALESCE + Duration::from_secs(1), &ctx),
            vec![(targets[1].clone(), Wake::NeedsYou)]
        );
        assert!(rules
            .tick(now + FINISHED_COALESCE - Duration::from_nanos(1), &ctx)
            .is_empty());
        assert_eq!(
            rules.tick(now + FINISHED_COALESCE, &ctx),
            targets
                .iter()
                .cloned()
                .map(|target| (target, Wake::Finished))
                .collect::<Vec<_>>()
        );
        assert!(rules.tick(now + FINISHED_COALESCE * 2, &ctx).is_empty());
    }

    #[test]
    fn reset_wakes_nobody() {
        let now = Instant::now();
        let targets = [target("a", &["x"])];
        let connected = BTreeSet::new();
        let ctx = ctx(&targets, &connected);
        let mut rules = WakeRules::new();
        let Change::Row { row } = row("x", Class::Needs) else {
            unreachable!()
        };
        assert!(rules
            .on_change(
                &Change::Reset {
                    view: View {
                        rows: vec![row],
                        ..View::default()
                    }
                },
                now,
                &ctx
            )
            .is_empty());
        assert!(rules.tick(now + DESK_ACTIVE_HOLD, &ctx).is_empty());
        assert!(rules.on_change(&self::row("x", Class::Needs), now, &ctx).is_empty());
        rules.on_change(&self::row("x", Class::Working), now, &ctx);
        assert_eq!(
            rules.on_change(&self::row("x", Class::Quiet), now, &ctx),
            vec![(targets[0].clone(), Wake::Finished)]
        );
    }

    #[test]
    fn gone_row_wakes_nobody() {
        let now = Instant::now();
        let targets = [target("a", &["x"])];
        let connected = BTreeSet::new();
        let mut ctx = ctx(&targets, &connected);
        ctx.desk_active_until = Some(now + DESK_ACTIVE_HOLD);
        let mut rules = WakeRules::new();
        for class in [Class::Working, Class::Needs] {
            rules.on_change(&row("x", class), now, &ctx);
            assert!(rules
                .on_change(
                    &Change::Gone {
                        id: String::new(),
                        key: "x".into(),
                        why: GoneWhy::Closed
                    },
                    now,
                    &ctx
                )
                .is_empty());
        }
        assert!(rules.tick(now + DESK_ACTIVE_HOLD, &ctx).is_empty());
        assert!(rules
            .on_change(&row("x", Class::Quiet), now + DESK_ACTIVE_HOLD, &ctx)
            .is_empty());
    }

    fn reset(rows: &[(&str, Class)]) -> Change {
        Change::Reset {
            view: View {
                rows: rows
                    .iter()
                    .map(|(key, class)| Row {
                        key: (*key).into(),
                        class: *class,
                        ..Row::default()
                    })
                    .collect(),
                ..View::default()
            },
        }
    }

    fn gone(key: &str, why: GoneWhy) -> Change {
        Change::Gone {
            id: String::new(),
            key: key.into(),
            why,
        }
    }

    #[test]
    fn later_reset_wakes_on_its_edges() {
        let now = Instant::now();
        let targets = [target("a", &["y"])];
        let connected = BTreeSet::new();
        let ctx = ctx(&targets, &connected);
        let mut rules = WakeRules::new();
        assert!(rules
            .on_change(&reset(&[("x", Class::Working), ("y", Class::Working)]), now, &ctx)
            .is_empty());
        assert_eq!(
            rules.on_change(&reset(&[("x", Class::Needs), ("y", Class::Quiet)]), now, &ctx),
            vec![
                (targets[0].clone(), Wake::NeedsYou),
                (targets[0].clone(), Wake::Finished)
            ]
        );
        assert!(rules
            .on_change(&reset(&[("x", Class::Needs)]), now + FINISHED_COALESCE, &ctx)
            .is_empty());
        assert!(rules
            .on_change(&row("x", Class::Needs), now + FINISHED_COALESCE, &ctx)
            .is_empty());
    }

    #[test]
    fn moved_row_keeps_its_class() {
        let now = Instant::now();
        let targets = [target("a", &["y"])];
        let connected = BTreeSet::new();
        let ctx = ctx(&targets, &connected);
        let mut rules = WakeRules::new();
        assert_eq!(rules.on_change(&row("x", Class::Needs), now, &ctx).len(), 1);
        let later = now + FINISHED_COALESCE;
        assert!(rules.on_change(&gone("x", GoneWhy::Moved), later, &ctx).is_empty());
        assert!(rules.on_change(&row("x", Class::Needs), later, &ctx).is_empty());
        rules.on_change(&row("y", Class::Working), later, &ctx);
        rules.on_change(&gone("y", GoneWhy::Moved), later, &ctx);
        assert_eq!(
            rules.on_change(&row("y", Class::Quiet), later, &ctx),
            vec![(targets[0].clone(), Wake::Finished)]
        );
    }

    #[test]
    fn moved_row_keeps_its_held_wake_until_it_lands() {
        let now = Instant::now();
        let targets = [target("a", &[])];
        let connected = BTreeSet::new();
        let mut ctx = ctx(&targets, &connected);
        ctx.desk_active_until = Some(now + DESK_ACTIVE_HOLD);
        let mut rules = WakeRules::new();
        assert!(rules.on_change(&row("x", Class::Needs), now, &ctx).is_empty());
        rules.on_change(&gone("x", GoneWhy::Moved), now, &ctx);
        assert!(rules.on_change(&row("x", Class::Needs), now, &ctx).is_empty());
        assert_eq!(
            rules.tick(now + DESK_ACTIVE_HOLD, &ctx),
            vec![(targets[0].clone(), Wake::NeedsYou)],
            "the tab still needs the person where it landed"
        );

        let mut rules = WakeRules::new();
        rules.on_change(&row("x", Class::Needs), now, &ctx);
        rules.on_change(&gone("x", GoneWhy::Moved), now, &ctx);
        rules.on_change(&row("x", Class::Quiet), now, &ctx);
        assert!(
            rules.tick(now + DESK_ACTIVE_HOLD, &ctx).is_empty(),
            "it landed no longer needing anyone"
        );
    }

    #[test]
    fn moved_row_that_never_lands_releases_no_held_wake() {
        let now = Instant::now();
        let targets = [target("a", &[])];
        let connected = BTreeSet::new();
        let mut ctx = ctx(&targets, &connected);
        ctx.desk_active_until = Some(now + DESK_ACTIVE_HOLD);
        let mut rules = WakeRules::new();
        assert!(rules.on_change(&row("x", Class::Needs), now, &ctx).is_empty());
        rules.on_change(&gone("x", GoneWhy::Moved), now, &ctx);
        assert!(rules.tick(now + DESK_ACTIVE_HOLD, &ctx).is_empty());
    }

    #[test]
    fn two_targets_both_woken() {
        let now = Instant::now();
        let targets = [target("a", &[]), target("b", &[])];
        let connected = BTreeSet::new();
        let ctx = ctx(&targets, &connected);
        assert_eq!(
            WakeRules::new().on_change(&row("x", Class::Needs), now, &ctx),
            targets
                .iter()
                .cloned()
                .map(|target| (target, Wake::NeedsYou))
                .collect::<Vec<_>>()
        );
    }
}
