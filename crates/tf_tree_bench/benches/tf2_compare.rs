// Head-to-head: tf_tree vs ROS 2 `tf2::BufferCore`, same tree, same history,
// same queries, same process.
//
// Requires `--features tf2` and a ROS 2 install; run it with `just tf2-bench`.
//
// # Why this is a fair comparison
//
// `tf2::BufferCore` links against `-ltf2` alone — no rclcpp, no DDS, no ROS
// graph. Both sides are therefore plain in-process libraries doing transform
// math, driven from the same benchmark loop with the same inputs. Nothing here
// measures middleware.
//
// # What is deliberately NOT equalised
//
// tf_tree's headline structural claim is that it compiles the topology walk once
// into a `Plan` and then only samples (`docs/PROJECT.md` §5 D3), whereas tf2 walks the
// tree on every `lookupTransform`. Equalising that away would benchmark an engine
// nobody would ship. So the rows are:
//
//   * `lookup_hot`      — tf_tree with a pre-compiled plan vs tf2. The real
//                         steady-state comparison: this is how each engine is
//                         meant to be used.
//   * `lookup_cold`     — tf_tree compiling a fresh plan per query vs tf2. The
//                         pessimistic bound, isolating how much of the win is
//                         plan reuse rather than a faster sample path.
//   * `push`            — publishing one sample.
//
// Reading them together shows both what the engine delivers in practice and
// where the delivery comes from.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::hint::black_box;
use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use tf_tree::{InterpPolicy, Iso3, Stamp, Tree};
use tf_tree_bench::{fixture, replay, replay_tf2, tf2::Tf2Fixture};
use tf_tree_tf2_sys::{FrameName, Tf2Buffer};

/// One comparable workload: a populated tf_tree, an equally populated tf2
/// buffer, and a query set valid on both.
struct Load {
    name: &'static str,
    tree: Tree,
    tf2: Tf2Buffer,
    queries: Vec<(String, String, i64)>,
}

/// The synthetic mobile-robot fixture: 24 frames, depth 6, 4 dynamic edges from
/// 10 Hz to 1 kHz. Queries sweep the last 100 ms, so the bracket search does real
/// work rather than repeatedly hitting one cached pair.
fn fixture_load() -> Load {
    let tree = fixture::build_tree_with(InterpPolicy::LerpSlerp).expect("fixture tree");
    let (writers, _) = fixture::spin_up(&tree).expect("populate");
    drop(writers);

    let tf2 = Tf2Fixture::load().expect("tf2 fixture");
    let names = fixture::frame_names();
    let now = fixture::NOW_NS;
    let lo = now - 100_000_000;

    // `camera_optical <- map` is the fixture's longest chain (depth 6), so the
    // row reflects a real chain rather than a one-edge lookup. The stamps sweep
    // the whole 100 ms window so the bracket search does real work instead of
    // hitting one cached pair over and over.
    // `assert!`, not `debug_assert!`: criterion builds in the release profile,
    // where a debug assertion is compiled out and would guard nothing.
    assert!(
        names.contains(&"camera_optical") && names.contains(&"map"),
        "fixture no longer declares `camera_optical` and `map`; the benchmark's \
         query pair must be updated to the current longest chain"
    );
    let queries = (0..1024i64)
        .map(|k| {
            let stamp = lo + (now - lo) * k / 1024;
            ("camera_optical".to_owned(), "map".to_owned(), stamp)
        })
        .collect();

    Load {
        name: "fixture_depth6",
        tree,
        tf2: tf2.into_buffer(),
        queries,
    }
}

/// The real recorded stream: a topology and a publish cadence nobody designed
/// for our convenience.
fn replay_load() -> Load {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/tfstream/indoor_atelier.tfstream");
    let stream = replay::TfStream::load(&path).expect("load recording");
    let tree = stream
        .build_tree(InterpPolicy::LerpSlerp)
        .expect("replay tree");
    let tf2 = replay_tf2::load_tf2(&stream).expect("tf2 replay");
    let qs = replay::QuerySet::draw(&stream, 1024, 0xBEEF_F00D).expect("queries");
    Load {
        name: "recorded_stream",
        tree,
        tf2,
        queries: qs.queries,
    }
}

/// Check, **once and outside every timed loop**, that the pair the rows actually
/// drive resolves on both engines across the whole stamp range.
///
/// Every row below queries `queries[0]`'s target/source pair at all of the query
/// set's stamps. On the recorded stream that pair is drawn at random
/// (`QuerySet::draw`, seed `0xBEEF_F00D`); nothing about the seed, the RNG or the
/// recording guarantees it stays resolvable. An unresolvable pair does not fail
/// the benchmark, it *corrupts* it silently: tf_tree's `if let Ok` would skip
/// every sample and measure an empty loop, while tf2 would measure a C++
/// throw/catch per call (microseconds), and the ratio of the two would be
/// meaningless. So it is asserted rather than assumed.
fn assert_pair_resolves(load: &Load) {
    let (target, source, _) = &load.queries[0];
    let why = "the benchmark drives this one pair at every stamp; if it does not \
               resolve, the tf_tree rows time an empty loop and the tf2 rows time \
               a C++ throw/catch, and the published ratio is nonsense";

    // First and last stamp of the set: the ends of the window the rows sweep.
    let first = load.queries.first().expect("empty query set").2;
    let last = load.queries.last().expect("empty query set").2;

    let t = load.tree.frame(target);
    let s = load.tree.frame(source);
    assert!(
        t.is_ok() && s.is_ok(),
        "{}: tf_tree does not know frame `{target}` or `{source}` — {why}",
        load.name
    );
    let plan = load.tree.plan(t.unwrap(), s.unwrap());
    assert!(
        plan.is_ok(),
        "{}: tf_tree cannot compile a plan for `{target}` <- `{source}` \
         ({:?}) — {why}",
        load.name,
        plan.err()
    );
    let plan = plan.unwrap();
    let guard = load.tree.guard();

    let tc = FrameName::new(target).expect("target frame name");
    let sc = FrameName::new(source).expect("source frame name");

    for (which, ns) in [("first", first), ("last", last)] {
        let stamp: Stamp = Stamp::from_nanos(ns);
        assert!(
            plan.at(&guard, stamp).is_ok(),
            "{}: tf_tree cannot resolve `{target}` <- `{source}` at the {which} \
             stamp ({ns} ns) — {why}",
            load.name
        );
        assert!(
            load.tf2.lookup_by_name(&tc, &sc, ns).is_ok(),
            "{}: tf2 cannot resolve `{target}` <- `{source}` at the {which} \
             stamp ({ns} ns) — {why}",
            load.name
        );
    }
}

fn bench_lookup(c: &mut Criterion, load: &Load) {
    assert_pair_resolves(load);

    let mut group = c.benchmark_group(format!("lookup_hot/{}", load.name));
    group.throughput(Throughput::Elements(load.queries.len() as u64));

    // tf_tree, used as intended: compile the plan once, then sample.
    group.bench_function(BenchmarkId::new("tf_tree", "planned"), |b| {
        let guard = load.tree.guard();
        let (t, s, _) = &load.queries[0];
        let tf = load.tree.frame(t).unwrap();
        let sf = load.tree.frame(s).unwrap();
        let plan = load.tree.plan(tf, sf).unwrap();
        b.iter(|| {
            let mut acc = 0.0f64;
            for (_, _, stamp_ns) in &load.queries {
                let stamp: Stamp = Stamp::from_nanos(*stamp_ns);
                if let Ok(p) = plan.at(&guard, stamp) {
                    acc += p.t.x;
                }
            }
            black_box(acc)
        });
    });

    // tf2, used as intended: it has no plan concept, so every call walks.
    // Frame names are converted ONCE, outside the timed loop. `lookup(&str,..)`
    // heap-allocates a C string per name per call; timing that against
    // `Plan::at` — which takes no strings and never allocates — would charge
    // this crate's marshalling to tf2. See `overhead/` below for what remains.
    group.bench_function(BenchmarkId::new("tf2", "lookupTransform"), |b| {
        let (t, s, _) = &load.queries[0];
        let (t, s) = (FrameName::new(t).unwrap(), FrameName::new(s).unwrap());
        b.iter(|| {
            let mut acc = 0.0f64;
            for (_, _, stamp_ns) in &load.queries {
                if let Ok(p) = load.tf2.lookup_by_name(&t, &s, *stamp_ns) {
                    acc += p.t.x;
                }
            }
            black_box(acc)
        });
    });

    // The naive binding, kept as a *control*: `lookup(&str, ..)` converts both
    // frame names to C strings on every call, which is two heap allocations per
    // lookup. An earlier revision of this benchmark used it, which charged this
    // crate's marshalling to tf2. The row stays so the size of that mistake is
    // measured rather than asserted.
    group.bench_function(BenchmarkId::new("tf2", "lookupTransform_alloc"), |b| {
        let (t, s, _) = &load.queries[0];
        b.iter(|| {
            let mut acc = 0.0f64;
            for (_, _, stamp_ns) in &load.queries {
                if let Ok(p) = load.tf2.lookup(t, s, *stamp_ns) {
                    acc += p.t.x;
                }
            }
            black_box(acc)
        });
    });

    // The bridge's own cost: the same FFI crossing and the same
    // `const char*` -> `std::string` marshalling, with the BufferCore call
    // removed. Subtract this from the tf2 row to get tf2's own cost.
    group.bench_function(BenchmarkId::new("tf2", "shim_overhead"), |b| {
        let (t, s, _) = &load.queries[0];
        let (t, s) = (FrameName::new(t).unwrap(), FrameName::new(s).unwrap());
        b.iter(|| {
            let mut acc = 0.0f64;
            for (_, _, stamp_ns) in &load.queries {
                if let Ok(p) = tf_tree_tf2_sys::lookup_overhead_probe(&load.tf2, &t, &s, *stamp_ns)
                {
                    acc += p.t.x;
                }
            }
            black_box(acc)
        });
    });
    group.finish();

    // Cold: tf_tree recompiles per query. The pessimistic bound.
    let mut cold = c.benchmark_group(format!("lookup_cold/{}", load.name));
    cold.throughput(Throughput::Elements(load.queries.len() as u64));
    cold.bench_function(BenchmarkId::new("tf_tree", "replanned"), |b| {
        let guard = load.tree.guard();
        let (t, s, _) = &load.queries[0];
        let tf = load.tree.frame(t).unwrap();
        let sf = load.tree.frame(s).unwrap();
        b.iter(|| {
            let mut acc = 0.0f64;
            for (_, _, stamp_ns) in &load.queries {
                let plan = load.tree.plan(tf, sf).unwrap();
                let stamp: Stamp = Stamp::from_nanos(*stamp_ns);
                if let Ok(p) = plan.at(&guard, stamp) {
                    acc += p.t.x;
                }
            }
            black_box(acc)
        });
    });
    cold.finish();
}

/// Publish throughput: one sample onto one edge, at a 1 kHz cadence.
///
/// Both sides are **bounded**, which matters or the row would degenerate into a
/// memory-growth benchmark. They are bounded differently, and that difference is
/// itself part of what is being compared: tf_tree's ring is count-bounded (a
/// fixed power-of-two slot count, overwritten in place, no allocation ever), and
/// tf2's cache is time-bounded (entries older than the cache span are pruned on
/// insert). A 10 s cache is the realistic ROS default, so that is what is used.
fn bench_push(c: &mut Criterion) {
    let mut group = c.benchmark_group("push");
    group.throughput(Throughput::Elements(1));

    let tree = fixture::build_tree_with(InterpPolicy::LerpSlerp).expect("tree");
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let writer = tree.claim(odom, map).unwrap();
    let pose = Iso3::IDENTITY;
    const STEP_NS: i64 = 1_000_000; // 1 kHz

    let mut t = 0i64;
    group.bench_function("tf_tree", |b| {
        b.iter(|| {
            t += STEP_NS;
            writer.push(t, &pose).unwrap();
        });
    });

    // Names converted once, for the same reason as the lookup rows: `push` takes
    // no strings, so a per-call conversion would charge this bridge to tf2. The
    // names cross as the `std::string`s the message wants and are assigned into
    // it — what a native C++ publisher does, and no allocation for names this
    // short.
    let buf = Tf2Buffer::new(10.0).unwrap();
    let (mp, od) = (
        FrameName::new("map").unwrap(),
        FrameName::new("odom").unwrap(),
    );
    let mut t2 = 0i64;
    group.bench_function("tf2", |b| {
        b.iter(|| {
            t2 += STEP_NS;
            buf.set_transform_by_name(&mp, &od, t2, &pose, false)
                .unwrap();
        });
    });

    // The naive binding as a *control*, exactly as in the lookup group:
    // `set_transform(&str, ..)` converts both names per call. An earlier
    // revision published the tf2 push figure through a path that did this
    // internally; the row stays so the size of that bias is measured rather than
    // asserted.
    let alloc_buf = Tf2Buffer::new(10.0).unwrap();
    let mut t3 = 0i64;
    group.bench_function("tf2_alloc", |b| {
        b.iter(|| {
            t3 += STEP_NS;
            alloc_buf
                .set_transform("map", "odom", t3, &pose, false)
                .unwrap();
        });
    });
    group.finish();
}

/// How each engine scales with tree size and chain depth.
///
/// Two axes, varied independently because they stress different things:
/// **depth** drives how many edges a lookup composes, and **size** drives how
/// much tree the engine has to navigate to find them. tf_tree's structural claim
/// is that size should be nearly free once a plan is compiled (the plan is a
/// flat step array) while depth costs linearly; tf2 walks per call, so it should
/// pay for both. This row is where that either shows up or doesn't.
fn bench_scale(c: &mut Criterion) {
    // (chain_depth, branches_per_link) -> total frames.
    //
    // The last shape grows *wide* rather than deeper on purpose: a lookup
    // composes `chain_depth + 1` steps, and tf_tree caps a compiled plan at
    // `tf_tree_core::MAX_DEPTH` (32 since `0034`, 16 when this shape was
    // chosen), so a spine past it is rejected outright with `TreeTooDeep`. tf2
    // has no such limit. That is a real difference in
    // what the two engines accept — recorded in `docs/benchmarks/tf2.md` rather
    // than papered over — but it is not a *performance* difference, so the
    // scaling row stays inside the budget where both engines can answer.
    const SHAPES: &[(usize, usize)] = &[
        (3, 2),   //   ~12 frames — a small mobile base
        (6, 4),   //   ~35 frames — the fixture's scale
        (12, 8),  //  ~117 frames — a humanoid / dual-arm description
        (14, 24), //  ~375 frames — a large multi-sensor platform, depth 15
    ];
    const QUERIES: usize = 256;

    let mut group = c.benchmark_group("scale/deepest_pair");
    group.throughput(Throughput::Elements(QUERIES as u64));

    for &(depth, branches) in SHAPES {
        let stream = replay::synth_robot(depth, branches, 512, 100.0);
        let frames = stream.frame_names().len();
        let tree = stream
            .build_tree(InterpPolicy::LerpSlerp)
            .expect("scale tree");
        let tf2 = replay_tf2::load_tf2(&stream).expect("scale tf2");

        // Query the deepest pair: a leaf sensor under the last spine link, back
        // to the root. That is the worst case for both engines.
        let target = format!("s_{depth}_0");
        let source = "link_0".to_owned();
        let (lo, hi) = stream.common_window().expect("window");
        let stamps: Vec<i64> = (0..QUERIES as i64)
            .map(|k| lo + (hi - lo) * k / QUERIES as i64)
            .collect();

        let label = format!("{frames}frames_depth{}", depth + 1);

        group.bench_function(BenchmarkId::new("tf_tree", &label), |b| {
            let guard = tree.guard();
            let t = tree.frame(&target).unwrap();
            let s = tree.frame(&source).unwrap();
            let plan = tree.plan(t, s).unwrap();
            b.iter(|| {
                let mut acc = 0.0f64;
                for &ns in &stamps {
                    let stamp: Stamp = Stamp::from_nanos(ns);
                    if let Ok(p) = plan.at(&guard, stamp) {
                        acc += p.t.x;
                    }
                }
                black_box(acc)
            });
        });

        group.bench_function(BenchmarkId::new("tf2", &label), |b| {
            let t = FrameName::new(&target).unwrap();
            let s = FrameName::new(&source).unwrap();
            b.iter(|| {
                let mut acc = 0.0f64;
                for &ns in &stamps {
                    if let Ok(p) = tf2.lookup_by_name(&t, &s, ns) {
                        acc += p.t.x;
                    }
                }
                black_box(acc)
            });
        });
    }
    group.finish();
}

fn benches(c: &mut Criterion) {
    let fixture = fixture_load();
    bench_lookup(c, &fixture);
    let replay = replay_load();
    bench_lookup(c, &replay);
    bench_scale(c);
    bench_push(c);
}

criterion_group!(tf2_compare, benches);
criterion_main!(tf2_compare);
