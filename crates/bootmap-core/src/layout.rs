//! The dependency tree behind the graph, laid out radially.
//!
//! Each unit's tree parent is its critical-chain predecessor: the `After=`
//! dependency that became active last before it, which is the same rule
//! `systemd-analyze critical-chain` uses. Units without one hang off the
//! central "system" node. In prune mode the tree is turned around: each
//! unit hangs off the nearest unit that waits for it, so the boot's goal is
//! at the center and the units that start first are the leaves. Each subtree gets an angular wedge proportional
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
    /// Root the tree at the boot's goal instead: each unit hangs off the
    /// nearest unit ordered after it, so the units that start first are the
    /// leaves (see `config::Mode::Prune`).
    pub prune: bool,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            hidden_kinds: [UnitKind::Device, UnitKind::Slice, UnitKind::Scope]
                .into_iter()
                .collect(),
            include_never_started: false,
            processes: true,
            prune: false,
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
                // Prune mode orders by `After=` depth alone, which does not
                // change as the boot goes on: the tree stays the same.
                Some((if opts.prune { 0 } else { t }, rank, e.name.clone()))
            }))
            .collect();

        // Prune mode: the nearest unit that waits for us (has one of our
        // members in its `After=`). Units nothing waits for hang off the root.
        if opts.prune {
            let mut waiters: Vec<Vec<usize>> = vec![Vec::new(); lay.nodes.len()];
            for (ei, e) in ents.iter().enumerate() {
                let me = ei + 1;
                for d in e.members.iter().flat_map(|m| &m.deps.after) {
                    if let Some(&idx) = lay.index.get(d) {
                        if idx != me && !waiters[idx].contains(&me) {
                            waiters[idx].push(me);
                        }
                    }
                }
            }
            for (me, ws) in waiters.iter().enumerate().skip(1) {
                let my_key = keys[me].as_ref().unwrap();
                let parent = ws
                    .iter()
                    .filter_map(|&w| {
                        let k = keys[w].as_ref().unwrap();
                        (k > my_key).then_some((k, w))
                    })
                    .min()
                    .map(|(_, w)| w)
                    .unwrap_or(ROOT);
                lay.nodes[me].parent = Some(parent);
            }
        }

        // Critical-chain parent: the latest `After=` dependency (of any
        // member) that precedes us.
        for (ei, e) in ents.iter().enumerate().filter(|_| !opts.prune) {
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

        lay.link_adjacency();
        lay
    }

    /// The same layout without the nodes `gone` says (the root always stays).
    /// Nodes keep their depth, angle and order. For prune mode, where a
    /// node only goes once everything below it has, so what is left is still
    /// a tree: a node whose parent is gone would hang off the root.
    pub fn without(&self, gone: impl Fn(usize) -> bool) -> Layout {
        let mut new_at = vec![None; self.nodes.len()];
        let mut lay = Layout {
            max_depth: self.max_depth,
            ..Default::default()
        };
        for (i, n) in self.nodes.iter().enumerate() {
            if i == ROOT || !gone(i) {
                new_at[i] = Some(lay.nodes.len());
                lay.nodes.push(n.clone());
            }
        }
        let map = |i: usize| new_at[i];
        for n in lay.nodes.iter_mut() {
            n.parent = n.parent.map(|p| map(p).unwrap_or(ROOT));
            n.children = n.children.iter().filter_map(|&c| map(c)).collect();
        }
        for i in 1..lay.nodes.len() {
            let p = lay.nodes[i].parent.unwrap_or(ROOT);
            if !lay.nodes[p].children.contains(&i) {
                lay.nodes[p].children.push(i);
            }
        }
        lay.index = self
            .index
            .iter()
            .filter_map(|(name, &i)| Some((name.clone(), map(i)?)))
            .collect();
        lay.extra_edges = self
            .extra_edges
            .iter()
            .filter_map(|&(a, b)| Some((map(a)?, map(b)?)))
            .collect();
        lay.procs = self
            .procs
            .iter()
            .filter_map(|p| {
                Some(ProcNode {
                    unit: map(p.unit)?,
                    ..p.clone()
                })
            })
            .collect();
        lay.link_adjacency();
        lay
    }

    /// Undirected adjacency over tree and extra edges.
    fn link_adjacency(&mut self) {
        self.adj = vec![Vec::new(); self.nodes.len()];
        for (i, n) in self.nodes.iter().enumerate() {
            if let Some(p) = n.parent {
                self.adj[i].push(p);
                self.adj[p].push(i);
            }
        }
        for &(a, b) in &self.extra_edges {
            self.adj[a].push(b);
            self.adj[b].push(a);
        }
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

    fn pruned(tl: &BootTimeline) -> Layout {
        let opts = LayoutOptions {
            prune: true,
            ..Default::default()
        };
        Layout::compute(tl, &opts)
    }

    #[test]
    fn prune_tree_hangs_each_unit_off_one_that_waits_for_it() {
        let tl = sample();
        let lay = pruned(&tl);
        let mut deep = 0;
        for (i, n) in lay.nodes.iter().enumerate().skip(1) {
            // Reaches the root without a cycle.
            let mut cur = i;
            for _ in 0..=lay.nodes.len() {
                match lay.nodes[cur].parent {
                    Some(p) => cur = p,
                    None => break,
                }
            }
            assert_eq!(cur, ROOT, "{} does not reach the root", n.name);
            let p = n.parent.unwrap();
            if p == ROOT {
                continue;
            }
            deep += 1;
            let waits = lay.nodes[p]
                .units
                .iter()
                .any(|w| tl.units[w].deps.after.iter().any(|a| n.units.contains(a)));
            assert!(waits, "{} is not waiting for {}", lay.nodes[p].name, n.name);
        }
        assert!(deep > lay.nodes.len() / 2, "most units hang off another");
        // The goal is at the center, what starts first is far out.
        let depth = |u: &str| lay.nodes[lay.index[u]].depth;
        assert_eq!(depth("graphical.target"), 1);
        assert!(depth("sysinit.target") > depth("multi-user.target"));
    }

    #[test]
    fn without_drops_nodes_and_keeps_the_rest_linked() {
        let full = pruned(&sample());
        // Every leaf goes, as in prune mode.
        let leaf = |i: usize| i != ROOT && full.nodes[i].children.is_empty();
        let lay = full.without(leaf);
        let leaves = (0..full.nodes.len()).filter(|&i| leaf(i)).count();
        assert_eq!(lay.nodes.len(), full.nodes.len() - leaves);
        // (The root is not in the index: it is no unit.)
        for (i, n) in lay.nodes.iter().enumerate().skip(1) {
            assert_eq!(lay.index[&n.name], i, "{} indexed", n.name);
            if let Some(p) = n.parent {
                assert!(lay.nodes[p].children.contains(&i));
                let was = &full.nodes[full.index[&n.name]];
                assert_eq!(lay.nodes[p].name, full.nodes[was.parent.unwrap()].name);
            }
            for &c in &n.children {
                assert_eq!(lay.nodes[c].parent, Some(i));
            }
        }
        for &(a, b) in &lay.extra_edges {
            assert!(a < lay.nodes.len() && b < lay.nodes.len());
        }
        for (i, n) in full.nodes.iter().enumerate().skip(1) {
            assert_eq!(lay.index.contains_key(&n.name), !leaf(i), "{}", n.name);
        }
    }

    #[test]
    fn prune_tree_does_not_depend_on_timing() {
        let tl = sample();
        let mut early = tl.clone();
        // The same units, all still waiting for their jobs.
        for u in early.units.values_mut() {
            let in_graph = u.job || u.t_activating.is_some() || u.t_active.is_some();
            u.job = in_graph;
            u.t_activating = None;
            u.t_active = None;
            u.t_inactive = None;
        }
        let (a, b) = (pruned(&tl), pruned(&early));
        let parent =
            |l: &Layout, n: &str| l.nodes[l.nodes[l.index[n]].parent.unwrap()].name.clone();
        for n in &a.nodes[1..] {
            assert_eq!(parent(&a, &n.name), parent(&b, &n.name), "{}", n.name);
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
