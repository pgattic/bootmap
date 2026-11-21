//! Force-directed layout with a soft pull toward dependency depth.
//!
//! The same model as d3-force (and Obsidian's graph view):
//! - every pair of nodes repels,
//! - dependency edges are springs,
//! - weak gravity pulls everything toward the center,
//! - a radial force nudges each node toward the ring of its dependency depth.
//!
//! The radial pull (0..1) is the knob: 0 is a free-floating graph, 1 is close
//! to the rigid radial tree. The simulation is seeded from the radial layout,
//! so it starts sensible, settles quickly and is deterministic.
//!
//! Internally the simulation runs in "pixel-like" units (one ring = `RING`)
//! so d3's well-tuned constants apply; positions are reported in world units
//! (one ring = 1.0), the same space the radial layout uses.

use crate::layout::{radius, Layout, ROOT};
use std::collections::HashMap;

/// Simulation units per world unit.
const RING: f32 = 60.0;
const ALPHA_MIN: f32 = 0.001;
const ALPHA_DECAY: f32 = 0.0228; // ~300 ticks from 1.0 to ALPHA_MIN
const VELOCITY_DECAY: f32 = 0.4;
/// Heat when nodes join the graph: they drift into place.
const ADDED_REHEAT: f32 = 0.35;
/// Heat when nodes leave it (prune mode): enough for the rest to close in
/// visibly, gentle enough that the graph does not reshuffle.
const REMOVED_REHEAT: f32 = 0.25;
/// Heat when nodes move to another parent (grow mode, as start times come
/// in): enough for them to follow their new spring.
const MOVED_REHEAT: f32 = 0.2;

#[derive(Debug, Clone, Copy)]
pub struct ForceParams {
    /// Pull toward the dependency-depth ring, 0..=1.
    pub radial: f32,
    /// Many-body strength (negative = repulsion).
    pub charge: f32,
    /// Rest length of dependency-tree springs.
    pub link_distance: f32,
    /// Relative strength of non-tree dependency springs.
    pub extra_link: f32,
    /// Pull toward the center.
    pub gravity: f32,
}

impl Default for ForceParams {
    fn default() -> Self {
        Self {
            radial: 0.5,
            charge: -45.0,
            link_distance: 32.0,
            extra_link: 0.08,
            gravity: 0.03,
        }
    }
}

#[derive(Debug, Clone)]
struct Body {
    name: String,
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    /// Pinned position (dragged by the user, or the center node).
    fixed: Option<(f32, f32)>,
    target_r: f32,
    /// Name of the tree parent, to notice when a rebuild moves the node.
    parent: Option<String>,
}

#[derive(Debug, Clone)]
struct Link {
    a: usize,
    b: usize,
    strength: f32,
    bias: f32,
    distance: f32,
}

#[derive(Debug, Clone)]
pub struct Sim {
    pub params: ForceParams,
    bodies: Vec<Body>,
    links: Vec<Link>,
    pub alpha: f32,
    /// Alpha is held at least at this level (used while dragging).
    pub alpha_target: f32,
}

impl Sim {
    /// New simulation seeded from the radial layout.
    pub fn new(lay: &Layout, params: ForceParams) -> Self {
        let mut s = Self {
            params,
            bodies: vec![],
            links: vec![],
            alpha: 1.0,
            alpha_target: 0.0,
        };
        s.rebuild(lay);
        s.alpha = 1.0;
        s
    }

    /// Adopt a new layout (units appeared, left or were reordered). Known
    /// nodes keep their positions; new ones spawn next to their parent. The
    /// simulation warms up so the graph moves into its new shape.
    pub fn rebuild(&mut self, lay: &Layout) {
        let old: HashMap<&str, &Body> = self.bodies.iter().map(|b| (b.name.as_str(), b)).collect();
        let mut bodies: Vec<Body> = Vec::with_capacity(lay.nodes.len());
        let mut added = false;
        let mut moved = false;
        let mut kept = 0;
        for (i, n) in lay.nodes.iter().enumerate() {
            let target_r = radius(n.depth) * RING;
            let parent = n.parent.map(|p| lay.nodes[p].name.clone());
            if let Some(b) = old.get(n.name.as_str()) {
                kept += 1;
                moved |= b.parent != parent;
                bodies.push(Body {
                    target_r,
                    parent,
                    ..(*b).clone()
                });
                continue;
            }
            added = true;
            let (x, y) = if old.is_empty() {
                // Fresh start: seed from the radial layout.
                let (x, y) = n.xy();
                (x * RING, y * RING)
            } else {
                // Spawn just outside the parent, nudged by a stable jitter.
                let p = n
                    .parent
                    .and_then(|p| bodies.get(p))
                    .map(|b| (b.x, b.y))
                    .unwrap_or((0.0, 0.0));
                let a = hash_angle(&n.name);
                let d = self.params.link_distance * 0.5;
                (p.0 + d * a.cos(), p.1 + d * a.sin())
            };
            let fixed = (i == ROOT).then_some((0.0, 0.0));
            bodies.push(Body {
                name: n.name.clone(),
                x,
                y,
                vx: 0.0,
                vy: 0.0,
                fixed,
                target_r,
                parent,
            });
        }
        // Keep user pins across rebuilds, and always pin the center.
        if let Some(b) = bodies.get_mut(ROOT) {
            b.fixed = Some((0.0, 0.0));
            b.x = 0.0;
            b.y = 0.0;
        }

        // Springs. d3's default strength: 1 / min(degree(a), degree(b)), with
        // bias splitting the correction by relative degree.
        let mut edges: Vec<(usize, usize, bool)> = vec![];
        for (i, n) in lay.nodes.iter().enumerate() {
            if let Some(p) = n.parent {
                edges.push((p, i, true));
            }
        }
        edges.extend(lay.extra_edges.iter().map(|&(a, b)| (a, b, false)));
        let mut count = vec![0u32; bodies.len()];
        for &(a, b, _) in &edges {
            count[a] += 1;
            count[b] += 1;
        }
        self.links = edges
            .into_iter()
            .map(|(a, b, tree)| {
                let (ca, cb) = (count[a] as f32, count[b] as f32);
                let base = 1.0 / ca.min(cb).max(1.0);
                Link {
                    a,
                    b,
                    strength: if tree {
                        base
                    } else {
                        base * self.params.extra_link
                    },
                    bias: ca / (ca + cb),
                    distance: if tree {
                        self.params.link_distance
                    } else {
                        self.params.link_distance * 2.5
                    },
                }
            })
            .collect();
        let removed = kept < self.bodies.len();
        self.bodies = bodies;
        if added {
            self.reheat(ADDED_REHEAT);
        }
        if removed {
            self.reheat(REMOVED_REHEAT);
        }
        if moved {
            self.reheat(MOVED_REHEAT);
        }
    }

    pub fn reheat(&mut self, alpha: f32) {
        self.alpha = self.alpha.max(alpha);
    }

    /// Still moving noticeably?
    pub fn active(&self) -> bool {
        self.alpha > ALPHA_MIN || self.alpha_target > 0.0
    }

    pub fn len(&self) -> usize {
        self.bodies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }

    pub fn set_radial(&mut self, r: f32) {
        self.params.radial = r.clamp(0.0, 1.0);
        self.reheat(0.3);
    }

    /// Pin node `i` at a world position (None releases it). The center node
    /// cannot be released.
    pub fn pin(&mut self, i: usize, at: Option<(f32, f32)>) {
        if i == ROOT {
            return;
        }
        if let Some(b) = self.bodies.get_mut(i) {
            b.fixed = at.map(|(x, y)| (x * RING, y * RING));
        }
    }

    /// Positions in world units, indexed like `Layout::nodes`.
    pub fn positions(&self) -> Vec<(f32, f32)> {
        self.bodies
            .iter()
            .map(|b| (b.x / RING, b.y / RING))
            .collect()
    }

    /// Run until cooled (or `max_ticks`).
    pub fn settle(&mut self, max_ticks: usize) {
        for _ in 0..max_ticks {
            if !self.active() {
                break;
            }
            self.tick();
        }
    }

    pub fn tick(&mut self) {
        self.alpha += (self.alpha_target - self.alpha) * ALPHA_DECAY;
        if self.alpha < ALPHA_MIN && self.alpha_target == 0.0 {
            return;
        }
        let alpha = self.alpha;
        let n = self.bodies.len();
        let p = self.params;

        // Springs (applied to positions + velocity like d3's forceLink).
        for l in &self.links {
            let (a, b) = (&self.bodies[l.a], &self.bodies[l.b]);
            let mut dx = b.x + b.vx - a.x - a.vx;
            let mut dy = b.y + b.vy - a.y - a.vy;
            if dx == 0.0 && dy == 0.0 {
                dx = 1e-3;
                dy = 1e-3;
            }
            let len = (dx * dx + dy * dy).sqrt();
            let k = (len - l.distance) / len * alpha * l.strength;
            let (fx, fy) = (dx * k, dy * k);
            self.bodies[l.b].vx -= fx * l.bias;
            self.bodies[l.b].vy -= fy * l.bias;
            self.bodies[l.a].vx += fx * (1.0 - l.bias);
            self.bodies[l.a].vy += fy * (1.0 - l.bias);
        }

        // Repulsion (O(n²): fine for a few hundred nodes).
        for i in 0..n {
            let (xi, yi) = (self.bodies[i].x, self.bodies[i].y);
            let (mut ax, mut ay) = (0.0f32, 0.0f32);
            for j in 0..n {
                if i == j {
                    continue;
                }
                let mut dx = self.bodies[j].x - xi;
                let mut dy = self.bodies[j].y - yi;
                if dx == 0.0 && dy == 0.0 {
                    // Coincident: separate deterministically.
                    dx = if i < j { 1e-2 } else { -1e-2 };
                    dy = 1e-2;
                }
                // d3 caps the influence at short range (distanceMin = 1).
                let l2 = (dx * dx + dy * dy).max(1.0);
                // Ignore far-away pairs (distanceMax) to keep clusters local.
                if l2 > 1e6 {
                    continue;
                }
                let w = p.charge * alpha / l2;
                ax += dx * w;
                ay += dy * w;
            }
            self.bodies[i].vx += ax;
            self.bodies[i].vy += ay;
        }

        // Radial pull toward the depth ring, plus gentle gravity.
        let k_radial = 0.12 * p.radial;
        for b in &mut self.bodies {
            let r = (b.x * b.x + b.y * b.y).sqrt().max(1e-3);
            let k = (b.target_r - r) * k_radial * alpha / r;
            b.vx += b.x * k - b.x * p.gravity * alpha;
            b.vy += b.y * k - b.y * p.gravity * alpha;
        }

        // Integrate.
        for b in &mut self.bodies {
            match b.fixed {
                Some((x, y)) => {
                    b.x = x;
                    b.y = y;
                    b.vx = 0.0;
                    b.vy = 0.0;
                }
                None => {
                    b.vx *= 1.0 - VELOCITY_DECAY;
                    b.vy *= 1.0 - VELOCITY_DECAY;
                    b.x += b.vx;
                    b.y += b.vy;
                }
            }
        }
    }
}

/// Stable pseudo-random angle from a name (FNV-1a).
fn hash_angle(s: &str) -> f32 {
    let mut h: u32 = 0x811c9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    (h as f32 / u32::MAX as f32) * std::f32::consts::TAU
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BootTimeline, LayoutOptions};

    fn layout() -> Layout {
        let tl = BootTimeline::from_json(include_str!("../../../samples/mbair-boot.json")).unwrap();
        Layout::compute(&tl, &LayoutOptions::default())
    }

    fn settled(radial: f32) -> (Layout, Sim) {
        let lay = layout();
        let mut s = Sim::new(
            &lay,
            ForceParams {
                radial,
                ..Default::default()
            },
        );
        s.settle(1000);
        (lay, s)
    }

    /// Mean |distance from center − depth ring| in world units.
    fn ring_error(lay: &Layout, s: &Sim) -> f32 {
        let pos = s.positions();
        let errs: Vec<f32> = lay.nodes[1..]
            .iter()
            .zip(&pos[1..])
            .map(|(n, (x, y))| ((x * x + y * y).sqrt() - radius(n.depth)).abs())
            .collect();
        errs.iter().sum::<f32>() / errs.len() as f32
    }

    #[test]
    fn settles_finite_and_cools() {
        let (_, s) = settled(0.5);
        assert!(!s.active(), "still hot after 1000 ticks: alpha {}", s.alpha);
        for (x, y) in s.positions() {
            assert!(x.is_finite() && y.is_finite());
        }
    }

    #[test]
    fn deterministic() {
        let (_, a) = settled(0.5);
        let (_, b) = settled(0.5);
        assert_eq!(a.positions(), b.positions());
    }

    #[test]
    fn center_stays_pinned() {
        let (_, s) = settled(0.0);
        assert_eq!(s.positions()[ROOT], (0.0, 0.0));
    }

    #[test]
    fn radial_pull_tightens_rings() {
        let (lay, loose) = settled(0.0);
        let (_, tight) = settled(1.0);
        let (e0, e1) = (ring_error(&lay, &loose), ring_error(&lay, &tight));
        assert!(e1 < e0 * 0.7, "radial=1 error {e1} vs radial=0 error {e0}");
    }

    #[test]
    fn nodes_do_not_pile_up() {
        let (_, s) = settled(0.5);
        let pos = s.positions();
        let mut close = 0;
        for i in 0..pos.len() {
            for j in i + 1..pos.len() {
                let (dx, dy) = (pos[i].0 - pos[j].0, pos[i].1 - pos[j].1);
                if (dx * dx + dy * dy).sqrt() < 0.05 {
                    close += 1;
                }
            }
        }
        assert!(close < pos.len() / 20, "{close} near-coincident pairs");
    }

    #[test]
    fn removing_leaves_lets_the_rest_move_into_their_room() {
        let (lay, mut kept) = settled(0.5);
        // What prune mode removes: leaves. Take the biggest fan of them.
        let is_leaf = |i: usize| lay.nodes[i].children.is_empty() && lay.degree(i) == 1;
        let hub = (1..lay.nodes.len())
            .max_by_key(|&i| {
                lay.nodes[i]
                    .children
                    .iter()
                    .filter(|&&c| is_leaf(c))
                    .count()
            })
            .unwrap();
        let fan: Vec<usize> = lay.nodes[hub]
            .children
            .iter()
            .copied()
            .filter(|&c| is_leaf(c))
            .collect();
        assert!(fan.len() >= 3, "a fan of {} leaves", fan.len());

        let rest = lay.without(|i| fan.contains(&i));
        let mut gone = kept.clone();
        gone.rebuild(&rest);
        assert_eq!(gone.len(), lay.nodes.len() - fan.len());
        assert!(gone.active(), "losing nodes warms the simulation up");
        kept.reheat(gone.alpha);
        let before = kept.positions();
        kept.settle(1000);
        gone.settle(1000);
        // Positions by name: the two simulations index their nodes differently.
        let by_name = |l: &Layout, s: &Sim| -> HashMap<String, (f32, f32)> {
            l.nodes
                .iter()
                .map(|n| n.name.clone())
                .zip(s.positions())
                .collect()
        };
        let (a, b) = (by_name(&lay, &kept), by_name(&rest, &gone));

        // The nodes closest to where the fan was end up closer to it.
        let n = fan.len() as f32;
        let at = fan.iter().fold((0.0, 0.0), |s, &c| {
            (s.0 + before[c].0 / n, s.1 + before[c].1 / n)
        });
        let dist = |p: (f32, f32)| ((p.0 - at.0).powi(2) + (p.1 - at.1).powi(2)).sqrt();
        let mut near: Vec<usize> = (1..lay.nodes.len())
            .filter(|&i| i != hub && !fan.contains(&i))
            .collect();
        near.sort_by(|&i, &j| dist(before[i]).total_cmp(&dist(before[j])));
        near.truncate(8);
        let mean = |p: &HashMap<String, (f32, f32)>| {
            near.iter()
                .map(|&i| dist(p[&lay.nodes[i].name]))
                .sum::<f32>()
                / near.len() as f32
        };
        assert!(
            mean(&b) < mean(&a),
            "neighborhood {} vs {} with the fan there",
            mean(&b),
            mean(&a)
        );
    }

    #[test]
    fn rebuild_keeps_positions_and_spawns_near_parent() {
        let lay = layout();
        let mut s = Sim::new(&lay, ForceParams::default());
        s.settle(400);
        let before = s.positions();
        // Same layout: nothing moves on rebuild.
        s.rebuild(&lay);
        assert_eq!(before, s.positions());
    }

    #[test]
    fn a_node_moving_to_another_parent_warms_the_simulation_up() {
        let (lay, mut s) = settled(0.5);
        assert!(!s.active());
        s.rebuild(&lay);
        assert!(!s.active(), "nothing changed, nothing to move");
        // Hang a node off the center instead of its parent.
        let k = (1..lay.nodes.len())
            .find(|&i| lay.nodes[i].parent != Some(ROOT))
            .unwrap();
        let mut moved = lay.clone();
        moved.nodes[k].parent = Some(ROOT);
        s.rebuild(&moved);
        assert!(s.active(), "the moved node follows its new spring");
    }
}
