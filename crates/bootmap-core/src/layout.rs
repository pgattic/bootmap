//! Radial tree layout. Radius = dependency depth.
//!
//! Each unit's tree parent is its critical-chain predecessor: the `After=`
//! dependency that became active last before it, which is the same rule
//! `systemd-analyze critical-chain` uses. Units without one hang off the
//! central "system" node. Each subtree gets an angular wedge proportional
//! to its leaf count, so related units share a sector.

use crate::model::{BootTimeline, Unit, UnitKind, Usec};
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
            hidden_kinds: [UnitKind::Device, UnitKind::Slice, UnitKind::Scope].into_iter().collect(),
            include_never_started: false,
            processes: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    /// Unit name; the root is "system".
    pub name: String,
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
    pub unit: usize,
    pub pid: u32,
    pub comm: String,
    pub r: f32,
    pub theta: f32,
}

impl ProcNode {
    pub fn xy(&self) -> (f32, f32) {
        (self.r * self.theta.cos(), self.r * self.theta.sin())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub nodes: Vec<Node>,
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
    (u.t_done().or(u.t_activating).unwrap_or(Usec::MAX), rank, u.name.clone())
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

impl Layout {
    pub fn compute(tl: &BootTimeline, opts: &LayoutOptions) -> Self {
        let visible: Vec<&Unit> = tl
            .units
            .values()
            .filter(|u| !opts.hidden_kinds.contains(&u.kind))
            .filter(|u| opts.include_never_started || u.job || u.t_activating.is_some() || u.t_active.is_some())
            .collect();

        let mut lay = Layout::default();
        lay.nodes.push(Node {
            name: "system".into(),
            kind: None,
            parent: None,
            children: vec![],
            depth: 0,
            r: 0.0,
            theta: 0.0,
            wedge: (0.0, TAU),
            weight: 0.0,
        });
        for u in &visible {
            lay.index.insert(u.name.clone(), lay.nodes.len());
            lay.nodes.push(Node {
                name: u.name.clone(),
                kind: Some(u.kind),
                parent: None,
                children: vec![],
                depth: 0,
                r: 0.0,
                theta: 0.0,
                wedge: (0.0, 0.0),
                weight: 0.0,
            });
        }

        let ranks = after_ranks(tl, &lay.index);
        let keys: Vec<Option<Key>> = std::iter::once(None)
            .chain(visible.iter().map(|u| Some(order_key(u, ranks[&u.name]))))
            .collect();

        // Critical-chain parent: the latest `After=` dependency that precedes us.
        for u in &visible {
            let me = lay.index[&u.name];
            let my_key = keys[me].as_ref().unwrap();
            let parent = u
                .deps
                .after
                .iter()
                .filter_map(|d| {
                    let idx = *lay.index.get(d)?;
                    let k = keys[idx].as_ref().unwrap();
                    (k < my_key).then_some((k, idx))
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
            let sum: f32 = lay.nodes[n].children.iter().map(|&c| lay.nodes[c].weight).sum();
            lay.nodes[n].weight = own.max(sum);
        }
        lay.max_depth = lay.nodes.iter().map(|n| n.depth).max().unwrap_or(0);

        // Wedges and positions (pre-order). Start at the top (-90°).
        lay.nodes[ROOT].wedge = (-TAU / 4.0, TAU * 0.75);
        for &n in &order {
            let (a0, a1) = lay.nodes[n].wedge;
            let total: f32 = lay.nodes[n].children.iter().map(|&c| lay.nodes[c].weight).sum();
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
        for u in &visible {
            let me = lay.index[&u.name];
            for d in u.deps.requires.iter().chain(&u.deps.wants).chain(&u.deps.binds_to) {
                if let Some(&o) = lay.index.get(d) {
                    let tree = lay.nodes[me].parent == Some(o) || lay.nodes[o].parent == Some(me);
                    if !tree && o != me && seen.insert((me.min(o), me.max(o))) {
                        lay.extra_edges.push((me, o));
                    }
                }
            }
        }

        // Process satellites: a small fan just outside the unit.
        if opts.processes {
            for u in &visible {
                let ui = lay.index[&u.name];
                let n = &lay.nodes[ui];
                let k = u.processes.len();
                let span = (n.wedge.1 - n.wedge.0).min(0.08 * k as f32);
                for (j, p) in u.processes.iter().enumerate() {
                    let f = if k == 1 { 0.5 } else { j as f32 / (k - 1) as f32 };
                    lay.procs.push(ProcNode {
                        unit: ui,
                        pid: p.pid,
                        comm: p.comm.clone(),
                        r: n.r + 0.38,
                        theta: n.theta - span / 2.0 + span * f,
                    });
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
                assert!(w.1 <= n.wedge.1 + eps, "{} escapes parent wedge", lay.nodes[c].name);
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
            let a = tl.units[w[0]].t_done().or(tl.units[w[0]].t_activating).unwrap();
            let b = tl.units[w[1]].t_done().or(tl.units[w[1]].t_activating).unwrap();
            assert!(a <= b, "{} ({a}) after {} ({b})", w[0], w[1]);
        }
    }
}
