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
                self.events.push(Event { t, unit: u.name.clone(), state: UnitState::Activating });
            }
            match (u.t_active, u.state) {
                (Some(t), _) => {
                    self.events.push(Event { t, unit: u.name.clone(), state: UnitState::Active })
                }
                (None, UnitState::Failed) => {
                    if let Some(t) = u.t_inactive.or(u.t_activating) {
                        self.events.push(Event { t, unit: u.name.clone(), state: UnitState::Failed })
                    }
                }
                (None, _) => {
                    if let Some(t) = u.t_inactive {
                        self.events.push(Event { t, unit: u.name.clone(), state: UnitState::Inactive })
                    }
                }
            }
        }
        self.events.sort_by_key(|e| e.t);
    }

    /// Time span covered: (start, end). Start is the userspace start.
    pub fn span(&self) -> (Usec, Usec) {
        let start = if self.phases.userspace > 0 {
            self.phases.userspace
        } else {
            self.events.first().map(|e| e.t).unwrap_or(0)
        };
        let last = self.events.last().map(|e| e.t).unwrap_or(start);
        let end = if self.phases.finish > 0 { self.phases.finish.max(last) } else { last };
        (start, end)
    }

    /// Unit states at time `t` (relative to nothing: absolute monotonic µs).
    pub fn states_at(&self, t: Usec) -> BTreeMap<&str, UnitState> {
        let mut m: BTreeMap<&str, UnitState> =
            self.units.keys().map(|k| (k.as_str(), UnitState::Inactive)).collect();
        for e in self.events.iter().take_while(|e| e.t <= t) {
            m.insert(e.unit.as_str(), e.state);
        }
        m
    }

    /// Apply a live event: updates the unit and appends to the event log.
    pub fn apply(&mut self, ev: Event) {
        let u = self.units.entry(ev.unit.clone()).or_insert_with(|| Unit::new(ev.unit.clone()));
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
        self.events.push(ev);
    }
}
