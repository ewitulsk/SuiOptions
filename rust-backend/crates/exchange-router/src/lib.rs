//! Multi-hop split-route planner (spec §5.8).
//!
//! Liquidity is discrete signed order levels, so each candidate path's
//! marginal price is a non-decreasing staircase and exact-in routing reduces
//! to a greedy that always consumes the globally cheapest next marginal unit
//! across all candidate paths — no AMM-style curve fitting.
//!
//! The one real correctness subtlety is the **order dedup constraint**: the
//! same resting order can appear in the ladders of multiple candidate paths.
//! The planner tracks remaining size per digest globally across the merge,
//! so Σ planned fills per digest ≤ that digest's remaining fillable amount
//! at snapshot (two branches filling one digest would double-count on-chain
//! `taker_token_filled` headroom and abort the route), and each plan emits
//! at most one fill leg per digest per hop.

use exchange_types::{Digest, ObjectId, TypeTagStr};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One consumable slice of liquidity: a resting signed order viewed in a
/// fixed direction (`in` = the token the router pays, `out` = the token it
/// receives; the order's maker sells `out`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiquiditySegment {
    pub digest: Digest,
    /// Maximum input this order can still absorb, in `in`-token units
    /// (its remaining taker-token capacity at snapshot).
    pub max_in: u64,
    /// Net output per input as a rational `num/den` — fees under the order's
    /// signed `max_fee_bps` already netted out.
    pub num: u64,
    pub den: u64,
}

impl LiquiditySegment {
    pub fn out_for_in(&self, input: u64) -> u64 {
        ((input as u128 * self.num as u128) / self.den as u128) as u64
    }
    /// Rate as f64 — for ORDERING candidate paths only; all allocation math
    /// stays in integers.
    fn rate(&self) -> f64 {
        self.num as f64 / self.den as f64
    }
}

/// A directed edge of the markets graph: one market side, best-first.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HopLadder {
    pub market: ObjectId,
    pub from: TypeTagStr,
    pub to: TypeTagStr,
    /// Sorted best (highest out/in) first.
    pub segments: Vec<LiquiditySegment>,
}

/// Planner configuration (spec: the practical cap is planner config,
/// default ≤ 4 paths × ≤ 3 hops).
#[derive(Clone, Copy, Debug)]
pub struct RouterConfig {
    pub max_hops: usize,
    pub max_paths: usize,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig { max_hops: 3, max_paths: 4 }
    }
}

/// One fill call the PTB builder must emit: `amount_in` of the hop's input
/// token against the signed order `digest`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FillLeg {
    pub digest: Digest,
    pub amount_in: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathPlan {
    /// Token chain `[from, mid…, to]`.
    pub tokens: Vec<TypeTagStr>,
    pub markets: Vec<ObjectId>,
    pub input: u64,
    pub expected_out: u64,
    /// Per hop, the ordered fill legs (at most one leg per digest).
    pub hops: Vec<Vec<FillLeg>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutePlan {
    /// Input actually routed.
    pub input: u64,
    /// Input that could not be routed (insufficient liquidity).
    pub unrouted: u64,
    pub expected_out: u64,
    pub paths: Vec<PathPlan>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RouterError {
    #[error("no path from {from} to {to}")]
    NoPath { from: TypeTagStr, to: TypeTagStr },
    #[error("zero input")]
    ZeroInput,
}

/// Enumerate simple paths (no revisited token) from `from` to `to`, hop-capped.
pub fn enumerate_paths<'a>(
    ladders: &'a [HopLadder],
    from: &str,
    to: &str,
    max_hops: usize,
) -> Vec<Vec<&'a HopLadder>> {
    fn dfs<'a>(
        ladders: &'a [HopLadder],
        here: &str,
        to: &str,
        max_hops: usize,
        visited: &mut Vec<String>,
        stack: &mut Vec<&'a HopLadder>,
        out: &mut Vec<Vec<&'a HopLadder>>,
    ) {
        if here == to {
            if !stack.is_empty() {
                out.push(stack.clone());
            }
            return;
        }
        if stack.len() == max_hops {
            return;
        }
        for l in ladders {
            if l.from == here && !visited.iter().any(|v| v == &l.to) {
                visited.push(l.to.clone());
                stack.push(l);
                dfs(ladders, &l.to, to, max_hops, visited, stack, out);
                stack.pop();
                visited.pop();
            }
        }
    }
    let mut out = Vec::new();
    let mut visited = vec![from.to_string()];
    let mut stack = Vec::new();
    dfs(ladders, from, to, max_hops, &mut visited, &mut stack, &mut out);
    out
}

/// Mutable cursor over one path's staircase during the merge.
struct PathState<'a> {
    hops: Vec<&'a HopLadder>,
    /// Accumulated (digest -> amount_in) per hop.
    consumed: Vec<HashMap<Digest, u64>>,
    input: u64,
    out: u64,
}

impl<'a> PathState<'a> {
    fn new(hops: Vec<&'a HopLadder>) -> Self {
        let n = hops.len();
        PathState { hops, consumed: vec![HashMap::new(); n], input: 0, out: 0 }
    }

    /// Current marginal rate (product of head-segment rates) and the maximum
    /// chunk of PATH-INPUT units consumable before any hop crosses a segment
    /// boundary or a digest's global budget runs out. `None` when exhausted.
    fn head(&self, budget: &HashMap<Digest, u64>) -> Option<(f64, u64)> {
        let mut rate = 1.0f64;
        let mut cap_path_units = u64::MAX;
        // cumulative upstream rate, to translate a hop's local capacity back
        // into path input units (floored — conservative)
        let mut up_num: u128 = 1;
        let mut up_den: u128 = 1;
        for i in 0..self.hops.len() {
            let seg = self.live_segment(i, budget)?;
            let local_cap = self.segment_capacity(i, seg, budget);
            let translated = (local_cap as u128).saturating_mul(up_den) / up_num;
            cap_path_units = cap_path_units.min(translated.min(u64::MAX as u128) as u64);
            rate *= seg.rate();
            up_num = up_num.saturating_mul(seg.num as u128);
            up_den = up_den.saturating_mul(seg.den as u128);
            let g = gcd(up_num, up_den);
            up_num /= g;
            up_den /= g;
        }
        if cap_path_units == 0 {
            return None;
        }
        Some((rate, cap_path_units))
    }

    /// First segment of hop `i` with usable capacity (local and global).
    fn live_segment(&self, i: usize, budget: &HashMap<Digest, u64>) -> Option<&LiquiditySegment> {
        self.hops[i]
            .segments
            .iter()
            .find(|s| self.segment_capacity(i, s, budget) > 0)
    }

    fn segment_capacity(
        &self,
        i: usize,
        seg: &LiquiditySegment,
        budget: &HashMap<Digest, u64>,
    ) -> u64 {
        let global = budget.get(&seg.digest).copied().unwrap_or(seg.max_in);
        let local_used = self.consumed[i].get(&seg.digest).copied().unwrap_or(0);
        global.min(seg.max_in.saturating_sub(local_used))
    }

    /// Push `chunk` path-input units through the hops, consuming segments and
    /// the global digest budget.
    fn allocate(&mut self, chunk: u64, budget: &mut HashMap<Digest, u64>) {
        let mut flow = chunk;
        for i in 0..self.hops.len() {
            let mut hop_in_left = flow;
            let mut hop_out = 0u64;
            while hop_in_left > 0 {
                let Some(seg) = self.live_segment(i, budget).cloned() else { break };
                let cap = self.segment_capacity(i, &seg, budget);
                let take = hop_in_left.min(cap);
                if take == 0 {
                    break;
                }
                hop_out += seg.out_for_in(take);
                *self.consumed[i].entry(seg.digest).or_insert(0) += take;
                let b = budget.entry(seg.digest).or_insert(seg.max_in);
                *b -= take;
                hop_in_left -= take;
            }
            flow = hop_out;
        }
        self.input += chunk;
        self.out += flow;
    }
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a.max(1)
}

/// Plan an exact-in split route for `amount_in` of `from` into `to`.
///
/// Greedy over the merged staircases: repeatedly allocate to the path with
/// the best current marginal rate, up to the next segment boundary, until
/// the input is spent or liquidity runs dry. Staleness is safe by design: a
/// stale plan can only abort on-chain (someone consumed an order first),
/// never execute worse than each order's signed terms.
pub fn plan_route(
    ladders: &[HopLadder],
    from: &str,
    to: &str,
    amount_in: u64,
    config: RouterConfig,
) -> Result<RoutePlan, RouterError> {
    if amount_in == 0 {
        return Err(RouterError::ZeroInput);
    }
    let mut candidates = enumerate_paths(ladders, from, to, config.max_hops);
    if candidates.is_empty() {
        return Err(RouterError::NoPath { from: from.into(), to: to.into() });
    }
    // Prefer shorter paths on ties; cap the candidate set.
    candidates.sort_by_key(|p| p.len());
    candidates.truncate(config.max_paths);

    let mut states: Vec<PathState> = candidates.into_iter().map(PathState::new).collect();
    // Global per-digest budget shared across every path (dedup constraint).
    let mut budget: HashMap<Digest, u64> = HashMap::new();
    for l in ladders {
        for s in &l.segments {
            budget.entry(s.digest).or_insert(s.max_in);
        }
    }

    let mut remaining = amount_in;
    while remaining > 0 {
        let mut best: Option<(usize, f64, u64)> = None;
        for (i, st) in states.iter().enumerate() {
            if let Some((rate, cap)) = st.head(&budget) {
                if best.map(|(_, r, _)| rate > r).unwrap_or(true) {
                    best = Some((i, rate, cap));
                }
            }
        }
        let Some((i, _, cap)) = best else { break };
        let chunk = remaining.min(cap);
        states[i].allocate(chunk, &mut budget);
        remaining -= chunk;
    }

    let mut paths = Vec::new();
    let mut expected_out = 0u64;
    for st in &states {
        if st.input == 0 {
            continue;
        }
        let mut tokens: Vec<TypeTagStr> = vec![st.hops[0].from.clone()];
        tokens.extend(st.hops.iter().map(|h| h.to.clone()));
        let hops = st
            .consumed
            .iter()
            .enumerate()
            .map(|(i, m)| {
                // deterministic leg order: the hop ladder's best-first order;
                // consumed is keyed by digest so legs are unique per digest
                st.hops[i]
                    .segments
                    .iter()
                    .filter(|s| m.contains_key(&s.digest))
                    .map(|s| FillLeg { digest: s.digest, amount_in: m[&s.digest] })
                    .collect()
            })
            .collect();
        expected_out += st.out;
        paths.push(PathPlan {
            tokens,
            markets: st.hops.iter().map(|h| h.market).collect(),
            input: st.input,
            expected_out: st.out,
            hops,
        });
    }

    Ok(RoutePlan { input: amount_in - remaining, unrouted: remaining, expected_out, paths })
}

#[cfg(test)]
mod tests;
