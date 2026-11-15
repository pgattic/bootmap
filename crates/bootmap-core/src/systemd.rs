//! systemd client over D-Bus (libdbus).
//!
//! At boot the system bus (dbus-broker) is not up yet, so we first try PID 1's
//! private peer-to-peer socket (`/run/systemd/private`, root only), the one
//! `systemctl` uses, and fall back to the system bus.
//!
//! On the private socket PID 1 stamps messages with the sender
//! `org.freedesktop.systemd1` (a well-known name, not a `:1.x` unique name).
//! libdbus accepts that; zbus asserts on it, which is why we use libdbus.

use crate::model::{BootTimeline, Deps, Event, Phases, Process, Unit, UnitState, Usec};
use anyhow::{Context, Result};
use dbus::arg::{prop_cast, PropMap};
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::blocking::{Connection, Proxy};
use dbus::channel::{Channel, MatchingReceiver};
use dbus::message::{MatchRule, MessageType};
use dbus::{Message, Path as DPath};
use std::sync::{Arc, Mutex};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::time::Duration;

const DEST: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_IFACE: &str = "org.freedesktop.systemd1.Manager";
const UNIT_IFACE: &str = "org.freedesktop.systemd1.Unit";
const PRIVATE_SOCKET: &str = "/run/systemd/private";
const TIMEOUT: Duration = Duration::from_secs(10);

/// Max processes recorded per unit (keeps the picture readable).
const MAX_PROCS: usize = 6;

type ListUnitsRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    DPath<'static>,
    u32,
    String,
    DPath<'static>,
);

/// Messages produced by the live watcher.
#[derive(Debug, Clone)]
pub enum LiveMsg {
    /// A unit we had not seen before (deps included).
    UnitNew(Unit),
    /// A start job was queued for this unit.
    Job(String),
    Event(Event),
    /// `StartupFinished` was emitted.
    Finished(Phases),
    /// A fresh snapshot taken after reconnecting to the re-executed PID 1
    /// (after the switch to the real root).
    Resync(Box<BootTimeline>),
}

pub struct Systemd {
    conn: Connection,
    p2p: bool,
    path_to_name: HashMap<String, String>,
    subscribed: bool,
}

impl Systemd {
    pub fn connect() -> Result<Self> {
        if Path::new(PRIVATE_SOCKET).exists() && is_root() {
            match Self::connect_private() {
                Ok(s) => return Ok(s),
                Err(e) => eprintln!("bootmap: private socket failed ({e:#}), trying system bus"),
            }
        }
        let conn = Connection::new_system().context("connecting to the system bus")?;
        Ok(Self { conn, p2p: false, path_to_name: HashMap::new(), subscribed: false })
    }

    fn connect_private() -> Result<Self> {
        // Peer-to-peer: no Hello / register().
        let ch = Channel::open_private(&format!("unix:path={PRIVATE_SOCKET}"))?;
        let sd = Self { conn: Connection::from(ch), p2p: true, path_to_name: HashMap::new(), subscribed: false };
        sd.prime();
        Ok(sd)
    }

    /// Make the first message on the connection a long one.
    ///
    /// libdbus sends `BEGIN` and then the first message without waiting for an
    /// answer. sd-bus, on PID 1's side, reads up to 256 bytes at a time while
    /// authenticating; if a short first message arrives in the same read as
    /// `BEGIN`, it stays in sd-bus's buffer unprocessed until more input
    /// arrives, so the call gets no reply (seen with systemd 262, most often
    /// while PID 1 is busy, i.e. during boot). A message longer than that
    /// read leaves bytes in the socket, which wakes PID 1 up again. The call
    /// itself just fails with "no such unit".
    fn prime(&self) {
        debug("prime");
        let name = format!("{}.service", "bootmap-primer-".repeat(24));
        let _: std::result::Result<(DPath<'static>,), _> =
            self.proxy(MANAGER_PATH).method_call(MANAGER_IFACE, "GetUnit", (name,));
    }

    pub fn is_private(&self) -> bool {
        self.p2p
    }

    fn proxy<'a>(&'a self, path: &'a str) -> Proxy<'a, &'a Connection> {
        self.conn.with_proxy(DEST, path, TIMEOUT)
    }

    fn get_all(&self, path: &str, iface: &str) -> Result<PropMap> {
        {
            debug(&format!("GetAll {path} {iface}"));
        }
        Ok(self.proxy(path).get_all(iface)?)
    }

    /// Boot phase timestamps, one small property at a time (`GetAll` on the
    /// Manager would return hundreds of properties we do not need).
    pub fn phases(&self) -> Result<Phases> {
        let proxy = self.proxy(MANAGER_PATH);
        let get = |k: &str| -> Result<u64> {
            debug(&format!("Get {k}"));
            Ok(proxy.get::<u64>(MANAGER_IFACE, k)?)
        };
        let in_initrd = Path::new("/etc/initrd-release").exists();
        Ok(Phases {
            firmware: get("FirmwareTimestampMonotonic")?,
            loader: get("LoaderTimestampMonotonic")?,
            initrd: get("InitRDTimestampMonotonic")?,
            userspace: get("UserspaceTimestampMonotonic")?,
            // An initrd never "finishes" the boot.
            finish: if in_initrd { 0 } else { get("FinishTimestampMonotonic")? },
            // Newer systemd only; absent means "not shutting down".
            shutdown_start: get("ShutdownStartTimestampMonotonic").unwrap_or(0),
            in_initrd,
        })
    }

    fn list_units(&self) -> Result<Vec<ListUnitsRow>> {
        // Empty state and pattern lists mean "all loaded units".
        debug("ListUnitsByPatterns");
        let none: Vec<&str> = vec![];
        let (rows,): (Vec<ListUnitsRow>,) =
            self.proxy(MANAGER_PATH).method_call(MANAGER_IFACE, "ListUnitsByPatterns", (none.clone(), none))?;
        Ok(rows)
    }

    /// Fetch one unit's properties from its object path.
    pub fn fetch_unit(&self, path: &str) -> Result<Unit> {
        let p = self.get_all(path, UNIT_IFACE)?;
        let mut u = Unit::new(get_str(&p, "Id"));
        u.description = get_str(&p, "Description");
        u.state = UnitState::from_systemd(&get_str(&p, "ActiveState"));
        u.deps = Deps {
            requires: get_strv(&p, "Requires"),
            wants: get_strv(&p, "Wants"),
            binds_to: get_strv(&p, "BindsTo"),
            after: get_strv(&p, "After"),
        };
        u.t_activating = nonzero(get_u64(&p, "InactiveExitTimestampMonotonic"));
        u.t_active = nonzero(get_u64(&p, "ActiveEnterTimestampMonotonic"));
        if matches!(u.state, UnitState::Inactive | UnitState::Failed) {
            let t = nonzero(get_u64(&p, "InactiveEnterTimestampMonotonic"));
            // Only if it stopped after its latest start (not a restart in progress).
            u.t_inactive = t.filter(|&t| u.t_activating.is_some_and(|a| t >= a) && u.t_active.is_none_or(|a| t >= a));
        }
        if let Some(iface) = cgroup_iface(u.kind) {
            if let Ok(sp) = self.get_all(path, iface) {
                let cg = get_str(&sp, "ControlGroup");
                if !cg.is_empty() {
                    u.processes = read_cgroup_procs(&cg);
                }
            }
        }
        Ok(u)
    }

    fn fetch_unit_by_name(&mut self, name: &str) -> Option<Unit> {
        let (path,): (DPath<'static>,) =
            self.proxy(MANAGER_PATH).method_call(MANAGER_IFACE, "GetUnit", (name,)).ok()?;
        let path = path.to_string();
        let u = self.fetch_unit(&path).ok()?;
        self.path_to_name.insert(path, u.name.clone());
        Some(u)
    }

    /// Ask PID 1 to send us change signals (idempotent).
    fn subscribe(&mut self) -> Result<()> {
        if self.subscribed {
            return Ok(());
        }
        debug("Subscribe");
        let _: () = self.proxy(MANAGER_PATH).method_call(MANAGER_IFACE, "Subscribe", ())?;
        if !self.p2p {
            let mut rule = MatchRule::new();
            rule.msg_type = Some(MessageType::Signal);
            rule.sender = Some(DEST.into());
            self.conn.add_match_no_cb(&rule.match_str())?;
        }
        self.subscribed = true;
        Ok(())
    }

    /// Snapshot every loaded unit. Events are synthesized from timestamps.
    pub fn snapshot(&mut self) -> Result<BootTimeline> {
        // Subscribe first: changes that happen while we read the units are
        // then queued for `watch()` instead of being lost.
        self.subscribe()?;
        let mut tl = BootTimeline {
            host: read_trim("/proc/sys/kernel/hostname"),
            boot_id: read_trim("/proc/sys/kernel/random/boot_id").replace('-', ""),
            phases: self.phases()?,
            ..Default::default()
        };
        for row in self.list_units()? {
            let path = row.6.to_string();
            match self.fetch_unit(&path) {
                Ok(mut u) => {
                    u.job = row.7 != 0;
                    self.path_to_name.insert(path, u.name.clone());
                    tl.units.insert(u.name.clone(), u);
                }
                Err(e) => eprintln!("bootmap: skipping {}: {e:#}", row.0),
            }
        }
        tl.synthesize_events();
        Ok(tl)
    }

    /// Subscribe and stream changes until the connection closes or `send`
    /// returns false. Blocks. Call `snapshot()` first.
    pub fn watch(&mut self, mut send: impl FnMut(LiveMsg) -> bool) -> Result<()> {
        self.subscribe()?;
        let mut rule = MatchRule::new();
        rule.msg_type = Some(MessageType::Signal);
        if !self.p2p {
            rule.sender = Some(DEST.into());
        }
        // Signals are queued here by the callback and handled after process()
        // returns, so handlers are free to make method calls.
        let queue: Arc<Mutex<VecDeque<Message>>> = Arc::default();
        let q = queue.clone();
        self.conn.start_receive(
            rule,
            Box::new(move |msg, _| {
                q.lock().unwrap().push_back(msg);
                true
            }),
        );
        loop {
            self.conn.process(Duration::from_millis(500))?;
            if !self.conn.channel().is_connected() {
                return Ok(());
            }
            loop {
                let Some(msg) = queue.lock().unwrap().pop_front() else { break };
                for m in self.handle_signal(&msg) {
                    if !send(m) {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn handle_signal(&mut self, msg: &Message) -> Vec<LiveMsg> {
        let member = msg.member().map(|m| m.to_string()).unwrap_or_default();
        let path = msg.path().map(|p| p.to_string()).unwrap_or_default();
        let mut out = vec![];
        match member.as_str() {
            "UnitNew" => {
                let Ok((name, upath)) = msg.read2::<String, DPath>() else { return out };
                let upath = upath.to_string();
                if self.path_to_name.contains_key(&upath) {
                    return out;
                }
                self.path_to_name.insert(upath.clone(), name.clone());
                out.push(LiveMsg::UnitNew(self.fetch_unit(&upath).unwrap_or_else(|_| Unit::new(name))));
            }
            "JobNew" => {
                let Ok((_id, _job, name)) = msg.read3::<u32, DPath, String>() else { return out };
                if !self.path_to_name.values().any(|n| *n == name) {
                    if let Some(u) = self.fetch_unit_by_name(&name) {
                        out.push(LiveMsg::UnitNew(u));
                    }
                }
                out.push(LiveMsg::Job(name));
            }
            "PropertiesChanged" => {
                let Ok((iface, changed, _inval)) = msg.read3::<String, PropMap, Vec<String>>() else { return out };
                if iface != UNIT_IFACE {
                    return out;
                }
                let Some(raw) = prop_cast::<String>(&changed, "ActiveState") else { return out };
                let state = UnitState::from_systemd(raw);
                let name = match self.path_to_name.get(&path) {
                    Some(n) => n.clone(),
                    None => match self.fetch_unit(&path) {
                        Ok(u) => {
                            self.path_to_name.insert(path.clone(), u.name.clone());
                            let n = u.name.clone();
                            out.push(LiveMsg::UnitNew(u));
                            n
                        }
                        Err(_) => return out,
                    },
                };
                let ts_key = match state {
                    UnitState::Active => "ActiveEnterTimestampMonotonic",
                    UnitState::Activating => "InactiveExitTimestampMonotonic",
                    UnitState::Inactive | UnitState::Failed => "InactiveEnterTimestampMonotonic",
                    UnitState::Deactivating => "ActiveExitTimestampMonotonic",
                };
                let stamp = nonzero(get_u64(&changed, ts_key));
                debug(&format!("changed {name}: {raw} {ts_key}={stamp:?}"));
                if state == UnitState::Inactive && stamp.is_none() {
                    // Inactive and never left it: the unit has not run yet, so
                    // nothing happened. (Stamping this with "now" would place
                    // it after the start it actually precedes.)
                    return out;
                }
                let t = stamp.unwrap_or_else(monotonic_now);
                out.push(LiveMsg::Event(Event { t, unit: name, state }));
            }
            "StartupFinished" => {
                if let Ok(ph) = self.phases() {
                    out.push(LiveMsg::Finished(ph));
                }
            }
            _ => {}
        }
        out
    }
}

fn cgroup_iface(kind: crate::UnitKind) -> Option<&'static str> {
    use crate::UnitKind::*;
    Some(match kind {
        Service => "org.freedesktop.systemd1.Service",
        Scope => "org.freedesktop.systemd1.Scope",
        Socket => "org.freedesktop.systemd1.Socket",
        Mount => "org.freedesktop.systemd1.Mount",
        Swap => "org.freedesktop.systemd1.Swap",
        _ => return None,
    })
}

fn read_cgroup_procs(cg: &str) -> Vec<Process> {
    let mut out = Vec::new();
    let mut stack = vec![Path::new("/sys/fs/cgroup").join(cg.trim_start_matches('/'))];
    while let Some(dir) = stack.pop() {
        if let Ok(s) = std::fs::read_to_string(dir.join("cgroup.procs")) {
            for pid in s.lines().filter_map(|l| l.trim().parse::<u32>().ok()) {
                if out.len() >= MAX_PROCS {
                    return out;
                }
                let comm = read_trim(&format!("/proc/{pid}/comm"));
                out.push(Process { pid, comm });
            }
        }
        if let Ok(rd) = std::fs::read_dir(&dir) {
            stack.extend(rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()));
        }
    }
    out
}

fn read_trim(p: &str) -> String {
    std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn is_root() -> bool {
    std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0).unwrap_or(false)
}

/// CLOCK_MONOTONIC in µs (same clock as systemd's `*TimestampMonotonic`).
pub fn monotonic_now() -> Usec {
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes into a valid timespec.
    unsafe { clock_gettime(1 /* CLOCK_MONOTONIC */, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}
extern "C" {
    fn clock_gettime(clk: i32, ts: *mut Timespec) -> i32;
}

fn nonzero(v: u64) -> Option<u64> {
    (v != 0).then_some(v)
}

fn get_u64(p: &PropMap, k: &str) -> u64 {
    prop_cast::<u64>(p, k).copied().unwrap_or(0)
}

fn get_str(p: &PropMap, k: &str) -> String {
    prop_cast::<String>(p, k).cloned().unwrap_or_default()
}

fn get_strv(p: &PropMap, k: &str) -> Vec<String> {
    prop_cast::<Vec<String>>(p, k).cloned().unwrap_or_default()
}

/// Trace D-Bus calls when BOOTMAP_DEBUG is set.
fn debug(what: &str) {
    if std::env::var_os("BOOTMAP_DEBUG").is_some() {
        eprintln!("bootmap: [{:.3}] {what}", monotonic_now() as f64 / 1e6);
    }
}
