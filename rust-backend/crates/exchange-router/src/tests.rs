use super::*;
use exchange_types::SuiAddress;

fn digest(n: u8) -> Digest {
    let mut d = [0u8; 32];
    d[0] = n;
    Digest(d)
}

fn market(n: u8) -> ObjectId {
    let mut a = [0u8; 32];
    a[31] = n;
    SuiAddress(a)
}

fn seg(n: u8, max_in: u64, num: u64, den: u64) -> LiquiditySegment {
    LiquiditySegment { digest: digest(n), max_in, num, den }
}

fn ladder(m: u8, from: &str, to: &str, segs: Vec<LiquiditySegment>) -> HopLadder {
    HopLadder { market: market(m), from: from.into(), to: to.into(), segments: segs }
}

#[test]
fn path_enumeration() {
    let ladders = vec![
        ladder(1, "SUI", "USDC", vec![]),
        ladder(2, "USDC", "WAL", vec![]),
        ladder(3, "SUI", "WAL", vec![]),
        ladder(4, "SUI", "USDT", vec![]),
        ladder(5, "USDT", "WAL", vec![]),
        ladder(6, "WAL", "SUI", vec![]), // cycle back — never used
    ];
    let paths = enumerate_paths(&ladders, "SUI", "WAL", 3);
    // direct, via USDC, via USDT
    assert_eq!(paths.len(), 3);
    let hops: Vec<usize> = paths.iter().map(|p| p.len()).collect();
    assert!(hops.contains(&1) && hops.iter().filter(|h| **h == 2).count() == 2);

    // hop cap
    assert_eq!(enumerate_paths(&ladders, "SUI", "WAL", 1).len(), 1);
}

#[test]
fn single_hop_greedy_consumes_best_levels_first() {
    // two levels: 100 @ 2.0, 100 @ 1.5
    let ladders = vec![ladder(
        1,
        "SUI",
        "USDC",
        vec![seg(1, 100, 2, 1), seg(2, 100, 3, 2)],
    )];
    let plan = plan_route(&ladders, "SUI", "USDC", 150, RouterConfig::default()).unwrap();
    assert_eq!(plan.input, 150);
    assert_eq!(plan.unrouted, 0);
    // 100 * 2.0 + 50 * 1.5 = 275
    assert_eq!(plan.expected_out, 275);
    assert_eq!(plan.paths.len(), 1);
    assert_eq!(
        plan.paths[0].hops[0],
        vec![
            FillLeg { digest: digest(1), amount_in: 100 },
            FillLeg { digest: digest(2), amount_in: 50 },
        ]
    );
}

#[test]
fn split_across_parallel_paths_by_marginal_rate() {
    // direct SUI->WAL at 1.0 for 100, then 0.5
    // SUI->USDC at 2.0 (plenty), USDC->WAL at 0.45 (plenty) => 0.9 composed
    let ladders = vec![
        ladder(1, "SUI", "WAL", vec![seg(1, 100, 1, 1), seg(2, 1_000, 1, 2)]),
        ladder(2, "SUI", "USDC", vec![seg(3, 100_000, 2, 1)]),
        ladder(3, "USDC", "WAL", vec![seg(4, 100_000, 45, 100)]),
    ];
    let plan = plan_route(&ladders, "SUI", "WAL", 300, RouterConfig::default()).unwrap();
    // best 100 direct at 1.0, then 200 via USDC at 0.9 (beats direct 0.5)
    assert_eq!(plan.unrouted, 0);
    let direct: &PathPlan = plan.paths.iter().find(|p| p.tokens.len() == 2).unwrap();
    let via: &PathPlan = plan.paths.iter().find(|p| p.tokens.len() == 3).unwrap();
    assert_eq!(direct.input, 100);
    assert_eq!(direct.expected_out, 100);
    assert_eq!(via.input, 200);
    // 200 -> 400 USDC -> 180 WAL
    assert_eq!(via.expected_out, 180);
    assert_eq!(plan.expected_out, 280);
}

#[test]
fn digest_budget_shared_across_paths() {
    // The same order (digest 9, USDC->WAL) sits on two candidate paths:
    // SUI->USDC->WAL and SUI->USDT->USDC->WAL. Its 100-unit capacity must be
    // consumed at most once globally.
    let shared = seg(9, 100, 1, 1);
    let ladders = vec![
        ladder(1, "SUI", "USDC", vec![seg(1, 1_000, 1, 1)]),
        ladder(2, "USDC", "WAL", vec![shared.clone()]),
        ladder(3, "SUI", "USDT", vec![seg(3, 1_000, 1, 1)]),
        ladder(4, "USDT", "USDC", vec![seg(4, 1_000, 1, 1)]),
    ];
    let plan = plan_route(&ladders, "SUI", "WAL", 500, RouterConfig::default()).unwrap();
    // only 100 WAL of terminal liquidity exists
    assert_eq!(plan.expected_out, 100);
    assert_eq!(plan.input, 100);
    assert_eq!(plan.unrouted, 400);
    // dedup invariant: Σ planned fills for digest 9 <= its capacity
    let total_9: u64 = plan
        .paths
        .iter()
        .flat_map(|p| p.hops.iter().flatten())
        .filter(|l| l.digest == digest(9))
        .map(|l| l.amount_in)
        .sum();
    assert!(total_9 <= 100);
    // and at most one leg per digest per path plan
    for p in &plan.paths {
        for hop in &p.hops {
            let mut seen: Vec<Digest> = hop.iter().map(|l| l.digest).collect();
            seen.sort();
            let n = seen.len();
            seen.dedup();
            assert_eq!(n, seen.len());
        }
    }
}

#[test]
fn fees_net_into_rates() {
    // 10 bps fee on a 2.0 level: num=2*(10000-10)=19980, den=10000
    let ladders = vec![ladder(1, "SUI", "USDC", vec![seg(1, 1_000, 19_980, 10_000)])];
    let plan = plan_route(&ladders, "SUI", "USDC", 1_000, RouterConfig::default()).unwrap();
    assert_eq!(plan.expected_out, 1_998);
}

#[test]
fn insufficient_liquidity_reports_unrouted() {
    let ladders = vec![ladder(1, "SUI", "USDC", vec![seg(1, 100, 2, 1)])];
    let plan = plan_route(&ladders, "SUI", "USDC", 500, RouterConfig::default()).unwrap();
    assert_eq!(plan.input, 100);
    assert_eq!(plan.unrouted, 400);
    assert_eq!(plan.expected_out, 200);
}

#[test]
fn errors() {
    let ladders = vec![ladder(1, "SUI", "USDC", vec![])];
    assert_eq!(
        plan_route(&ladders, "SUI", "WAL", 10, RouterConfig::default()),
        Err(RouterError::NoPath { from: "SUI".into(), to: "WAL".into() })
    );
    assert_eq!(
        plan_route(&ladders, "SUI", "USDC", 0, RouterConfig::default()),
        Err(RouterError::ZeroInput)
    );
}
