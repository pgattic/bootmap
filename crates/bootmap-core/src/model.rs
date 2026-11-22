//! Boot data model. One JSON format serves captures, recordings and fixtures.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Monotonic time in microseconds since kernel start (systemd's `*TimestampMonotonic`).
pub type Usec = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum UnitKind {
    Service,
    Target,
    Socket,
    Mount,
    Automount,
    Swap,
    Device,
    Path,
    Timer,
    Slice,
    Scope,
    Other,
}

impl UnitKind {
    pub fn from_name(name: &str) -> Self {
        match name.rsplit('.').next().unwrap_or("") {
            "service" => Self::Service,
            "target" => Self::Target,
            "socket" => Self::Socket,
            "mount" => Self::Mount,
            "automount" => Self::Automount,
            "swap" => Self::Swap,
            "device" => Self::Device,
            "path" => Self::Path,
            "timer" => Self::Timer,
            "slice" => Self::Slice,
            "scope" => Self::Scope,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UnitState {
    #[default]
    Inactive,
    Activating,
    Active,
    Deactivating,
    Failed,
}

impl UnitState {
    pub fn from_systemd(s: &str) -> Self {
        match s {
            "active" | "reloading" | "refreshing" => Self::Active,
            "activating" => Self::Activating,
            "deactivating" => Self::Deactivating,
            "failed" => Self::Failed,
            _ => Self::Inactive,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Deps {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wants: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binds_to: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    pub comm: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unit {
    pub name: String,
    pub kind: UnitKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// State at the end of the recording.
    pub state: UnitState,
    #[serde(default)]
    pub deps: Deps,
    /// When the unit left "inactive" (started activating).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_activating: Option<Usec>,
    /// When the unit became "active".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_active: Option<Usec>,
    /// When the unit went back to "inactive" after running (oneshots that exited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_inactive: Option<Usec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<Process>,
    /// A job is queued for this unit (it will start soon). Live mode only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub job: bool,
}

impl Unit {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            kind: UnitKind::from_name(&name),
            name,
            description: String::new(),
            state: UnitState::Inactive,
            deps: Deps::default(),
            t_activating: None,
            t_active: None,
            t_inactive: None,
            processes: Vec::new(),
            job: false,
        }
    }

    /// Activation duration, if both ends are known.
    pub fn duration(&self) -> Option<Usec> {
        Some(self.t_done()?.saturating_sub(self.t_activating?))
    }

    /// When the unit finished starting: became active, or (oneshots) exited.
    pub fn t_done(&self) -> Option<Usec> {
        self.t_active.or(self.t_inactive)
    }
}

/// Boot phase timestamps (all monotonic µs, as systemd reports them).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Phases {
    /// Firmware/loader durations are reported by systemd as "time before kernel".
    pub firmware: Usec,
    pub loader: Usec,
    pub initrd: Usec,
    pub userspace: Usec,
    /// Zero while boot has not finished.
    pub finish: Usec,
    /// When a shutdown began (zero unless the system is shutting down).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub shutdown_start: Usec,
    /// True while the timeline is followed from inside the initrd.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_initrd: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub t: Usec,
    pub unit: String,
    pub state: UnitState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BootTimeline {
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub boot_id: String,
    #[serde(default)]
    pub phases: Phases,
    pub units: BTreeMap<String, Unit>,
    /// Sorted by `t`.
    pub events: Vec<Event>,
}

impl BootTimeline {
    pub fn from_json(s: &str) -> anyhow::Result<Self> {
        let mut tl: Self = serde_json::from_str(s)?;
        tl.events.sort_by_key(|e| e.t);
        Ok(tl)
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        Self::from_json(&std::fs::read_to_string(path)?)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("timeline serializes")
    }

    /// Build the event list from per-unit timestamps (used for snapshots).
    pub fn synthesize_events(&mut self) {
        self.events.clear();
        for u in self.units.values() {
            if let Some(t) = u.t_activating {
                self.events.push(Event {
                    t,
                    unit: u.name.clone(),
                    state: UnitState::Activating,
                });
            }
            match (u.t_active, u.state) {
                (Some(t), state) => {
                    // Active once, stopped (or failed) since: show both.
                    if let Some(ti) = u.t_inactive.filter(|&ti| ti > t) {
                        let state = if state == UnitState::Failed {
                            state
                        } else {
                            UnitState::Inactive
                        };
                        self.events.push(Event {
                            t: ti,
                            unit: u.name.clone(),
                            state,
                        });
                    }
                    self.events.push(Event {
                        t,
                        unit: u.name.clone(),
                        state: UnitState::Active,
                    })
                }
                (None, UnitState::Failed) => {
                    if let Some(t) = u.t_inactive.or(u.t_activating) {
                        self.events.push(Event {
                            t,
                            unit: u.name.clone(),
                            state: UnitState::Failed,
                        })
                    }
                }
                (None, _) => {
                    if let Some(t) = u.t_inactive {
                        self.events.push(Event {
                            t,
                            unit: u.name.clone(),
                            state: UnitState::Inactive,
                        })
                    }
                }
            }
        }
        self.events.sort_by_key(|e| e.t);
    }

    /// Merge a fresh snapshot taken from the re-executed PID 1 after the
    /// switch to the real root into the timeline followed from the initrd.
    ///
    /// `host`, `boot_id` and `phases` come from `new`, and every unit in `new`
    /// replaces the one we had. Units only we know (initrd units the new
    /// systemd has dropped) are kept; if they were still running they are
    /// marked inactive at `switch_time` (unless they already have a stop
    /// time), without job or processes. Callers pass the monotonic time at
    /// which the reconnect succeeded.
    ///
    /// Events: what we watched happen in the initrd is kept as it was seen. A
    /// unit's timestamps only describe its latest run, so rebuilding from
    /// them would erase the initrd run of anything the real system starts
    /// again (journald, udevd). From the new systemd's start on, its
    /// timestamps are the record, and so they are before it for units we
    /// never saw in the initrd (a mount that came and went while we were not
    /// looking). Whatever changed while we were disconnected is reconciled
    /// with one event per unit at `switch_time`.
    pub fn merge_snapshot(&mut self, new: BootTimeline, switch_time: Usec) {
        let cut = new.phases.userspace;
        self.host = new.host;
        self.boot_id = new.boot_id;
        self.phases = new.phases;
        for u in self
            .units
            .values_mut()
            .filter(|u| !new.units.contains_key(&u.name))
        {
            if matches!(
                u.state,
                UnitState::Active | UnitState::Activating | UnitState::Deactivating
            ) {
                u.state = UnitState::Inactive;
                u.t_inactive.get_or_insert(switch_time);
            }
            u.job = false;
            u.processes.clear();
        }
        self.units.extend(new.units);

        let mut events = std::mem::take(&mut self.events);
        events.retain(|e| e.t < cut);
        let seen: std::collections::HashSet<String> =
            events.iter().map(|e| e.unit.clone()).collect();
        self.synthesize_events();
        events.extend(
            self.events
                .drain(..)
                .filter(|e| e.t >= cut || !seen.contains(&e.unit)),
        );
        events.sort_by_key(|e| e.t);

        let mut last: std::collections::HashMap<&str, UnitState> = std::collections::HashMap::new();
        for e in &events {
            last.insert(e.unit.as_str(), e.state);
        }
        let missed: Vec<Event> = self
            .units
            .values()
            .filter(|u| last.get(u.name.as_str()).copied().unwrap_or_default() != u.state)
            .map(|u| Event {
                t: switch_time,
                unit: u.name.clone(),
                state: u.state,
            })
            .collect();
        events.extend(missed);
        events.sort_by_key(|e| e.t);
        self.events = events;
    }

    /// Time span covered: (start, end). Start is the initrd start when known,
    /// else the userspace start.
    pub fn span(&self) -> (Usec, Usec) {
        let start = if self.phases.initrd > 0 {
            self.phases.initrd
        } else if self.phases.userspace > 0 {
            self.phases.userspace
        } else {
            self.events.first().map(|e| e.t).unwrap_or(0)
        };
        let last = self.events.last().map(|e| e.t).unwrap_or(start);
        let end = if self.phases.finish > 0 {
            self.phases.finish.max(last)
        } else {
            last
        };
        (start, end)
    }

    /// Unit states at time `t` (relative to nothing: absolute monotonic µs).
    pub fn states_at(&self, t: Usec) -> BTreeMap<&str, UnitState> {
        let mut m: BTreeMap<&str, UnitState> = self
            .units
            .keys()
            .map(|k| (k.as_str(), UnitState::Inactive))
            .collect();
        for e in self.events.iter().take_while(|e| e.t <= t) {
            m.insert(e.unit.as_str(), e.state);
        }
        m
    }

    /// Apply a live event: updates the unit and adds it to the event log,
    /// keeping the log sorted. Returns false if the event was dropped.
    ///
    /// systemd re-sends a unit's whole state whenever any property changes,
    /// and signals queued while we were reading the units arrive after the
    /// snapshot already covers them. So an event is only news if it moves the
    /// unit forward: later than the unit's last event, or at the same instant
    /// but further along (a target is "activating" and "active" at one
    /// timestamp).
    pub fn apply(&mut self, ev: Event) -> bool {
        fn rank(s: UnitState) -> u8 {
            match s {
                UnitState::Activating => 1,
                UnitState::Active => 2,
                UnitState::Deactivating => 3,
                UnitState::Inactive | UnitState::Failed => 4,
            }
        }
        if let Some(last) = self.events.iter().rev().find(|e| e.unit == ev.unit) {
            if ev.t < last.t || (ev.t == last.t && rank(ev.state) <= rank(last.state)) {
                return false;
            }
        }
        let u = self
            .units
            .entry(ev.unit.clone())
            .or_insert_with(|| Unit::new(ev.unit.clone()));
        u.state = ev.state;
        match ev.state {
            UnitState::Activating if u.t_activating.is_none() => u.t_activating = Some(ev.t),
            UnitState::Active if u.t_active.is_none() => {
                u.t_active = Some(ev.t);
                u.t_activating.get_or_insert(ev.t);
            }
            UnitState::Inactive | UnitState::Failed
                if u.t_activating.is_some() && u.t_done().is_none() =>
            {
                u.t_inactive = Some(ev.t)
            }
            _ => {}
        }
        let at = self.events.partition_point(|e| e.t <= ev.t);
        self.events.insert(at, ev);
        true
    }
}

fn is_zero(v: &Usec) -> bool {
    *v == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str, state: UnitState, act: Option<Usec>, active: Option<Usec>) -> Unit {
        let mut u = Unit::new(name);
        u.state = state;
        u.t_activating = act;
        u.t_active = active;
        u
    }

    #[test]
    fn merge_snapshot_keeps_initrd_units() {
        let mut old = BootTimeline::default();
        old.phases.in_initrd = true;
        let mut a = unit(
            "initrd-only.service",
            UnitState::Active,
            Some(100),
            Some(200),
        );
        a.job = true;
        a.processes.push(Process {
            pid: 1,
            comm: "x".into(),
        });
        old.units.insert(a.name.clone(), a);
        old.units.insert(
            "both.service".into(),
            unit("both.service", UnitState::Activating, Some(150), None),
        );
        old.synthesize_events();

        let mut new = BootTimeline {
            host: "h".into(),
            ..Default::default()
        };
        new.phases.userspace = 900;
        new.units.insert(
            "both.service".into(),
            unit("both.service", UnitState::Active, Some(150), Some(300)),
        );
        new.units.insert(
            "new.service".into(),
            unit("new.service", UnitState::Active, Some(50), Some(60)),
        );
        new.synthesize_events();

        old.merge_snapshot(new, 1000);
        assert_eq!(old.host, "h");
        assert!(!old.phases.in_initrd);
        let a = &old.units["initrd-only.service"];
        assert_eq!(a.state, UnitState::Inactive);
        assert_eq!(a.t_inactive, Some(1000));
        assert!(!a.job && a.processes.is_empty());
        assert_eq!(old.units["both.service"].state, UnitState::Active);
        assert_eq!(old.units["both.service"].t_active, Some(300));
        assert!(old.units.contains_key("new.service"));
        assert!(old.events.windows(2).all(|w| w[0].t <= w[1].t));
        assert!(old
            .events
            .iter()
            .any(|e| e.unit == "initrd-only.service" && e.t == 1000));
    }

    #[test]
    fn apply_drops_stale_and_repeated_events() {
        use UnitState::*;
        let ev = |t, state| Event {
            t,
            unit: "a.target".into(),
            state,
        };
        let mut tl = BootTimeline::default();
        tl.units.insert(
            "a.target".into(),
            unit("a.target", Active, Some(100), Some(100)),
        );
        tl.synthesize_events();
        // Queued while the snapshot was taken: already covered by it.
        assert!(!tl.apply(ev(100, Activating)));
        assert!(!tl.apply(ev(100, Active)));
        assert!(!tl.apply(ev(50, Inactive)));
        // News.
        assert!(tl.apply(ev(300, Deactivating)));
        assert!(tl.apply(ev(300, Inactive)));
        assert!(!tl.apply(ev(300, Inactive)));
        // A second run, at one timestamp like targets have.
        assert!(tl.apply(ev(400, Activating)));
        assert!(tl.apply(ev(400, Active)));
        assert_eq!(
            history(&tl, "a.target"),
            [
                (100, Activating),
                (100, Active),
                (300, Deactivating),
                (300, Inactive),
                (400, Activating),
                (400, Active)
            ]
        );
        // An older event for another unit is slotted in, not appended.
        assert!(tl.apply(Event {
            t: 200,
            unit: "b.service".into(),
            state: Activating
        }));
        assert!(tl.events.windows(2).all(|w| w[0].t <= w[1].t));
    }

    fn history(tl: &BootTimeline, unit: &str) -> Vec<(Usec, UnitState)> {
        tl.events
            .iter()
            .filter(|e| e.unit == unit)
            .map(|e| (e.t, e.state))
            .collect()
    }

    #[test]
    fn merge_snapshot_keeps_what_was_seen_in_the_initrd() {
        use UnitState::*;
        let mut old = BootTimeline::default();
        // Ran in the initrd, was stopped there, and the real system starts it again.
        old.units.insert(
            "restart.service".into(),
            unit("restart.service", Active, Some(100), Some(200)),
        );
        // Still running when we lost the connection; stopped before we were back.
        old.units.insert(
            "missed.service".into(),
            unit("missed.service", Active, Some(120), Some(220)),
        );
        old.synthesize_events();
        old.apply(Event {
            t: 400,
            unit: "restart.service".into(),
            state: Inactive,
        });

        let mut new = BootTimeline::default();
        new.phases.userspace = 900;
        new.units.insert(
            "restart.service".into(),
            unit("restart.service", Active, Some(950), Some(960)),
        );
        let mut missed = unit("missed.service", Inactive, Some(120), Some(220));
        missed.t_inactive = Some(850);
        new.units.insert("missed.service".into(), missed);

        old.merge_snapshot(new, 1000);
        assert_eq!(
            history(&old, "restart.service"),
            [
                (100, Activating),
                (200, Active),
                (400, Inactive),
                (950, Activating),
                (960, Active)
            ]
        );
        assert_eq!(
            history(&old, "missed.service"),
            [(120, Activating), (220, Active), (1000, Inactive)]
        );
        assert!(old.events.windows(2).all(|w| w[0].t <= w[1].t));
    }

    #[test]
    fn merge_snapshot_keeps_the_history_of_units_not_seen_in_the_initrd() {
        use UnitState::*;
        let mut old = BootTimeline::default();
        old.units.insert(
            "seen.service".into(),
            unit("seen.service", Active, Some(100), Some(200)),
        );
        old.synthesize_events();

        // Mounted and unmounted in the initrd without us seeing it: only the
        // new systemd knows, and its times are before the real system began.
        let mut new = BootTimeline::default();
        new.phases.userspace = 900;
        let mut mount = unit("sysroot-x.mount", Inactive, Some(300), Some(300));
        mount.t_inactive = Some(800);
        new.units.insert(mount.name.clone(), mount);
        new.units.insert(
            "seen.service".into(),
            unit("seen.service", Active, Some(100), Some(200)),
        );

        old.merge_snapshot(new, 1000);
        assert_eq!(
            history(&old, "sysroot-x.mount"),
            [(300, Activating), (300, Active), (800, Inactive)]
        );
        assert_eq!(
            history(&old, "seen.service"),
            [(100, Activating), (200, Active)],
            "what we watched is not doubled"
        );
    }
}
