//! The dependency tree behind the graph, laid out radially.
//!
//! Each unit's tree parent is its critical-chain predecessor: the `After=`
//! dependency that became active last before it, which is the same rule
//! `systemd-analyze critical-chain` uses. Units without one hang off the
//! central "system" node. Each subtree gets an angular wedge proportional
//! to its leaf count, at a radius given by its depth.
//!
//! This radial arrangement is not drawn as such. It is where the force
//! simulation (`force`) starts from, which is why the graph settles quickly
//! and comes out the same every boot, and its depths are what the
//! simulation's radial pull aims for.

use crate::model::{BootTimeline, Unit, UnitKind, UnitState, Usec};
use std::collections::{BTreeSet, HashMap};
use std::f32::consts::TAU;

pub const ROOT: usize = 0;

/// Ring radius for a depth (in "ring units"). Sub-linear so the crowded inner
/// rings get more room than a plain `depth` would give them; still monotonic,
/// so distance from the center keeps meaning dependency depth.
pub fn radius(depth: u32) -> f32 {
    (depth as f32).powf(0.7)
}

#[derive(Debug, Clone)]
pub struct LayoutOptions {
    /// Kinds left out of the graph entirely (e.g. devices, slices).
    pub hidden_kinds: BTreeSet<UnitKind>,
    /// Include units that never started during this boot.
    pub include_never_started: bool,
    /// Show the processes of each unit as satellites.
    pub processes: bool,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            hidden_kinds: [UnitKind::Device, UnitKind::Slice, UnitKind::Scope]
                .into_iter()
                .collect(),
            include_never_started: false,
            processes: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    /// Unit name, the template's name for a group (`modprobe@.service`), or
    /// "system" for the root.
    pub name: String,
    /// The units this node stands for: one, the instances of a template, or
    /// none (the root).
    pub units: Vec<String>,
    pub kind: Option<UnitKind>,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub depth: u32,
    /// Polar coordinates: radius in ring units, angle in radians.
    pub r: f32,
    pub theta: f32,
    /// Angular wedge [start, end) reserved for this subtree.
    pub wedge: (f32, f32),
    /// Angular demand of this subtree: sum over leaves of 1/depth (a deep leaf
    /// sits on a larger circle, so it needs less angle for the same spacing).
    pub weight: f32,
}

impl Node {
    pub fn xy(&self) -> (f32, f32) {
        (self.r * self.theta.cos(), self.r * self.theta.sin())
    }
}

#[derive(Debug, Clone)]
pub struct ProcNode {
    /// Index of the unit this process belongs to.
    pub unit: usize,
    pub pid: u32,
    pub comm: String,
}

#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub nodes: Vec<Node>,
    /// Node of each unit name (a group's members map to the group), and of
    /// each group's template name.
    pub index: HashMap<String, usize>,
    /// Non-tree Requires/Wants/BindsTo edges between visible units.
    pub extra_edges: Vec<(usize, usize)>,
    pub procs: Vec<ProcNode>,
    pub max_depth: u32,
    /// Neighbors of each node (tree parent/children and extra edges).
    pub adj: Vec<Vec<usize>>,
}

/// Sort key that is a strict total order, so the parent relation is acyclic.
/// Started units order by when they finished starting; units that have not
/// started yet (pending jobs) come after all of them, ordered by their
/// `After=` depth so they already sit roughly where they will end up.
type Key = (Usec, u32, String);

fn order_key(u: &Unit, rank: u32) -> Key {
    (
        u.t_done().or(u.t_activating).unwrap_or(Usec::MAX),
        rank,
        u.name.clone(),
    )
}

/// Longest `After=` chain below each visible unit (cycles broken arbitrarily).
fn after_ranks(tl: &BootTimeline, index: &HashMap<String, usize>) -> HashMap<String, u32> {
    fn visit<'a>(
        n: &'a str,
        tl: &'a BootTimeline,
        index: &HashMap<String, usize>,
        memo: &mut HashMap<String, u32>,
        stack: &mut BTreeSet<&'a str>,
    ) -> u32 {
        if let Some(&r) = memo.get(n) {
            return r;
        }
        if !stack.insert(n) {
            return 0;
        }
        let mut r = 0;
        if let Some(u) = tl.units.get(n) {
            for d in &u.deps.after {
                if index.contains_key(d) {
                    r = r.max(visit(d, tl, index, memo, stack) + 1);
                }
            }
        }
        stack.remove(n);
        memo.insert(n.to_string(), r);
        r
    }
    let mut memo = HashMap::new();
    for n in index.keys() {
        visit(n, tl, index, &mut memo, &mut BTreeSet::new());
    }
    memo
}

/// The template a unit is an instance of: `name@instance.type` is an
/// instance of `name@.type`. This is systemd's own naming rule (and every
/// instance's unit file is the template's), so it is exact, not a guess.
pub fn template_of(unit: &str) -> Option<String> {
    let (prefix, rest) = unit.split_once('@')?;
    let dot = rest.rfind('.')?;
    let (instance, suffix) = rest.split_at(dot);
    (!prefix.is_empty() && !instance.is_empty()).then(|| format!("{prefix}@{suffix}"))
}

/// What becomes one node: a unit, or the instances of one template.
struct Entity<'a> {
    name: String,
    kind: UnitKind,
    members: Vec<&'a Unit>,
}

/// Group template instances (two or more) into one entity each. Instances
/// that failed stay on their own, so a group never hides a problem.
fn entities<'a>(visible: &[&'a Unit]) -> Vec<Entity<'a>> {
    let mut by_template: HashMap<String, Vec<&Unit>> = HashMap::new();
    for u in visible {
        if u.state != UnitState::Failed {
            if let Some(t) = template_of(&u.name) {
                by_template.entry(t).or_default().push(u);
            }
        }
    }
    let mut out: Vec<Entity> = vec![];
    let mut group_at: HashMap<String, usize> = HashMap::new();
    for u in visible {
        let grouped = template_of(&u.name).filter(|t| {
            by_template
                .get(t)
                .is_some_and(|m| m.len() >= 2 && m.iter().any(|m| m.name == u.name))
        });
        match grouped {
            Some(t) => match group_at.get(&t) {
                Some(&i) => out[i].members.push(u),
                None => {
                    group_at.insert(t.clone(), out.len());
                    out.push(Entity {
                        name: t,
                        kind: u.kind,
                        members: vec![u],
                    });
                }
            },
            None => out.push(Entity {
                name: u.name.clone(),
                kind: u.kind,
                members: vec![u],
            }),
        }
    }
    out
}

impl Layout {
    pub fn compute(tl: &BootTimeline, opts: &LayoutOptions) -> Self {
        let visible: Vec<&Unit> = tl
            .units
            .values()
            .filter(|u| !opts.hidden_kinds.contains(&u.kind))
            .filter(|u| {
                opts.include_never_started
                    || u.job
                    || u.t_activating.is_some()
                    || u.t_active.is_some()
            })
            .collect();
        let ents = entities(&visible);

        let mut lay = Layout::default();
        lay.nodes.push(Node {
            name: "system".into(),
            units: vec![],
            kind: None,
            parent: None,
            children: vec![],
            depth: 0,
            r: 0.0,
            theta: 0.0,
            wedge: (0.0, TAU),
            weight: 0.0,
        });
        for e in &ents {
            let i = lay.nodes.len();
            lay.index.insert(e.name.clone(), i);
            for m in &e.members {
                lay.index.insert(m.name.clone(), i);
            }
            lay.nodes.push(Node {
                name: e.name.clone(),
                units: e.members.iter().map(|m| m.name.clone()).collect(),
                kind: Some(e.kind),
                parent: None,
                children: vec![],
                depth: 0,
                r: 0.0,
                theta: 0.0,
                wedge: (0.0, 0.0),
                weight: 0.0,
            });
        }

        // A group orders by its last member to finish (pending if any member
        // is), at the deepest of its members' `After=` chains.
        let ranks = after_ranks(tl, &lay.index);
        let keys: Vec<Option<Key>> = std::iter::once(None)
            .chain(ents.iter().map(|e| {
                let t = e
                    .members
                    .iter()
                    .map(|m| order_key(m, 0).0)
                    .max()
                    .unwrap_or(Usec::MAX);
                let rank = e
                    .members
                    .iter()
                    .map(|m| ranks.get(&m.name).copied().unwrap_or(0))
                    .max()
                    .unwrap_or(0);
                Some((t, rank, e.name.clone()))
            }))
            .collect();

        // Critical-chain parent: the latest `After=` dependency (of any
        // member) that precedes us.
        for (ei, e) in ents.iter().enumerate() {
            let me = ei + 1;
            let my_key = keys[me].as_ref().unwrap();
            let parent = e
                .members
                .iter()
                .flat_map(|m| &m.deps.after)
                .filter_map(|d| {
                    let idx = *lay.index.get(d)?;
                    let k = keys[idx].as_ref().unwrap();
                    (idx != me && k < my_key).then_some((k, idx))
                })
                .max()
                .map(|(_, idx)| idx)
                .unwrap_or(ROOT);
            lay.nodes[me].parent = Some(parent);
        }

        // Children, ordered by activation time.
        for i in 1..lay.nodes.len() {
            let p = lay.nodes[i].parent.unwrap();
            lay.nodes[p].children.push(i);
        }
        for n in lay.nodes.iter_mut() {
            n.children.sort_by(|a, b| keys[*a].cmp(&keys[*b]));
        }

        // Depths (BFS from root), leaf counts (post-order).
        let mut order = vec![ROOT];
        let mut i = 0;
        while i < order.len() {
            let n = order[i];
            let d = lay.nodes[n].depth;
            for &c in &lay.nodes[n].children.clone() {
                lay.nodes[c].depth = d + 1;
                order.push(c);
            }
            i += 1;
        }
        for &n in order.iter().rev() {
            let own = 1.0 / radius(lay.nodes[n].depth.max(1));
            let sum: f32 = lay.nodes[n]
                .children
                .iter()
                .map(|&c| lay.nodes[c].weight)
                .sum();
            lay.nodes[n].weight = own.max(sum);
        }
        lay.max_depth = lay.nodes.iter().map(|n| n.depth).max().unwrap_or(0);

        // Wedges and positions (pre-order). Start at the top (-90°).
        lay.nodes[ROOT].wedge = (-TAU / 4.0, TAU * 0.75);
        for &n in &order {
            let (a0, a1) = lay.nodes[n].wedge;
            let total: f32 = lay.nodes[n]
                .children
                .iter()
                .map(|&c| lay.nodes[c].weight)
                .sum();
            let mut a = a0;
            for c in lay.nodes[n].children.clone() {
                let w = (a1 - a0) * lay.nodes[c].weight / total.max(f32::EPSILON);
                let cn = &mut lay.nodes[c];
                cn.wedge = (a, a + w);
                cn.theta = a + w / 2.0;
                cn.r = radius(cn.depth);
                a += w;
            }
        }

        // Extra (non-tree) dependency edges.
        let mut seen = BTreeSet::new();
        for (ei, e) in ents.iter().enumerate() {
            let me = ei + 1;
            for d in e.members.iter().flat_map(|m| {
                m.deps
                    .requires
                    .iter()
                    .chain(&m.deps.wants)
                    .chain(&m.deps.binds_to)
            }) {
                if let Some(&o) = lay.index.get(d) {
                    let tree = lay.nodes[me].parent == Some(o) || lay.nodes[o].parent == Some(me);
                    if !tree && o != me && seen.insert((me.min(o), me.max(o))) {
                        lay.extra_edges.push((me, o));
                    }
                }
            }
        }

        // Processes of each unit (the renderer places them around its node).
        if opts.processes {
            for (ei, e) in ents.iter().enumerate() {
                for m in &e.members {
                    lay.procs.extend(m.processes.iter().map(|p| ProcNode {
                        unit: ei + 1,
                        pid: p.pid,
                        comm: p.comm.clone(),
                    }));
                }
            }
        }

        // Undirected adjacency over tree + extra edges.
        lay.adj = vec![Vec::new(); lay.nodes.len()];
        for (i, n) in lay.nodes.iter().enumerate() {
            if let Some(p) = n.parent {
                lay.adj[i].push(p);
                lay.adj[p].push(i);
            }
        }
        for &(a, b) in &lay.extra_edges {
            lay.adj[a].push(b);
            lay.adj[b].push(a);
        }
        lay
    }

    pub fn degree(&self, i: usize) -> usize {
        self.adj.get(i).map_or(0, |a| a.len())
    }

    /// Chain from the root to `name` (inclusive), root first.
    pub fn chain(&self, name: &str) -> Vec<&str> {
        let mut out = vec![];
        let mut cur = self.index.get(name).copied();
        while let Some(i) = cur {
            out.push(self.nodes[i].name.as_str());
            cur = self.nodes[i].parent;
        }
        out.reverse();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BootTimeline {
        BootTimeline::from_json(include_str!("../../../samples/mbair-boot.json")).unwrap()
    }

    #[test]
    fn template_names() {
        assert_eq!(
            template_of("modprobe@drm.service").as_deref(),
            Some("modprobe@.service")
        );
        assert_eq!(
            template_of("getty@tty1.service").as_deref(),
            Some("getty@.service")
        );
        assert_eq!(
            template_of("modprobe@.service"),
            None,
            "the template itself"
        );
        assert_eq!(template_of("sysinit.target"), None);
        assert_eq!(template_of("-.mount"), None);
    }

    #[test]
    fn template_instances_share_a_node_unless_they_failed() {
        let mut tl = BootTimeline::default();
        let mut add = |name: &str, t: Usec, state: UnitState, after: &[&str]| {
            let mut u = Unit::new(name);
            u.state = state;
            u.t_activating = Some(t);
            if state != UnitState::Failed {
                u.t_active = Some(t + 10);
            }
            u.deps.after = after.iter().map(|s| s.to_string()).collect();
            tl.units.insert(name.into(), u);
        };
        add("modprobe@drm.service", 100, UnitState::Active, &[]);
        add("modprobe@fuse.service", 120, UnitState::Active, &[]);
        add("modprobe@loop.service", 130, UnitState::Failed, &[]);
        add("getty@tty1.service", 140, UnitState::Active, &[]);
        add(
            "sysinit.target",
            200,
            UnitState::Active,
            &["modprobe@drm.service", "modprobe@fuse.service"],
        );
        tl.synthesize_events();
        let lay = Layout::compute(&tl, &LayoutOptions::default());

        let g = lay.index["modprobe@.service"];
        assert_eq!(
            lay.nodes[g].units,
            ["modprobe@drm.service", "modprobe@fuse.service"]
        );
        assert_eq!(lay.index["modprobe@drm.service"], g);
        assert_eq!(lay.index["modprobe@fuse.service"], g);
        // The failed instance and a lone instance keep their own nodes.
        assert_eq!(
            lay.nodes[lay.index["modprobe@loop.service"]].units,
            ["modprobe@loop.service"]
        );
        assert_eq!(
            lay.nodes[lay.index["getty@tty1.service"]].units,
            ["getty@tty1.service"]
        );
        // Dependencies on members point at the group.
        assert_eq!(lay.nodes[lay.index["sysinit.target"]].parent, Some(g));
        assert_eq!(lay.nodes.len(), 1 + 4);
    }

    #[test]
    fn every_node_reaches_root() {
        let lay = Layout::compute(&sample(), &LayoutOptions::default());
        assert!(lay.nodes.len() > 50, "sample has many visible units");
        for (i, _) in lay.nodes.iter().enumerate() {
            let mut cur = i;
            let mut steps = 0;
            while let Some(p) = lay.nodes[cur].parent {
                cur = p;
                steps += 1;
                assert!(steps <= lay.nodes.len(), "cycle at {}", lay.nodes[i].name);
            }
            assert_eq!(cur, ROOT);
        }
    }

    #[test]
    fn wedges_nest_and_do_not_overlap() {
        let lay = Layout::compute(&sample(), &LayoutOptions::default());
        let eps = 1e-4;
        for n in &lay.nodes {
            let mut prev_end = n.wedge.0;
            for &c in &n.children {
                let w = lay.nodes[c].wedge;
                assert!(w.0 >= prev_end - eps, "overlap under {}", n.name);
                assert!(
                    w.1 <= n.wedge.1 + eps,
                    "{} escapes parent wedge",
                    lay.nodes[c].name
                );
                prev_end = w.1;
            }
        }
    }

    #[test]
    fn depth_matches_radius() {
        let lay = Layout::compute(&sample(), &LayoutOptions::default());
        for n in &lay.nodes[1..] {
            let p = &lay.nodes[n.parent.unwrap()];
            assert_eq!(n.depth, p.depth + 1);
            assert_eq!(n.r, radius(n.depth));
            assert!(n.r > p.r);
        }
    }

    #[test]
    fn graphical_target_is_deep_and_follows_multi_user() {
        let lay = Layout::compute(&sample(), &LayoutOptions::default());
        let chain = lay.chain("graphical.target");
        assert!(chain.len() > 3, "chain: {chain:?}");
        assert_eq!(chain[0], "system");
        // Every link must have become active no later than the next one.
        let tl = sample();
        for w in chain[1..].windows(2) {
            let a = tl.units[w[0]]
                .t_done()
                .or(tl.units[w[0]].t_activating)
                .unwrap();
            let b = tl.units[w[1]]
                .t_done()
                .or(tl.units[w[1]].t_activating)
                .unwrap();
            assert!(a <= b, "{} ({a}) after {} ({b})", w[0], w[1]);
        }
    }
}
