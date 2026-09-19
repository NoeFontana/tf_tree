//! Replay of a real recorded `/tf` stream: irregular periods, a partly
//! disconnected graph, approximately normalised quaternions.
//! Data: `testdata/tfstream/indoor_atelier.tfstream`, derived from an
//! indoor/outdoor mobile-robot recording released under CC BY 4.0 — see
//! `testdata/tfstream/ATTRIBUTION.md`.
//!
//! The `tf2` half of this file runs only with a ROS 2 install; use
//! `just tf2-replay`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::path::PathBuf;

use tf_tree::{InterpPolicy, Stamp};
use tf_tree_bench::replay::{self, QuerySet, TfStream};

fn stream_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/tfstream/indoor_atelier.tfstream")
}

fn load() -> TfStream {
    TfStream::load(&stream_path()).expect("load recorded tfstream")
}

/// The recording parses into the expected topology; an empty stream would make
/// every downstream test pass vacuously.
#[test]
fn recorded_stream_parses_with_the_expected_shape() {
    let s = load();
    assert_eq!(s.static_edges.len(), 4, "static edges");
    assert_eq!(s.dynamic_edges.len(), 5, "dynamic edges");
    assert_eq!(s.samples.len(), 1066, "dynamic samples");

    // 10 distinct frames: the odom/base/laser/camera/gyro links and four wheels.
    assert_eq!(s.frame_names().len(), 10, "frames: {:?}", s.frame_names());

    // Stamp-ordered, or the engine would reject them.
    assert!(
        s.samples.windows(2).all(|w| w[0].stamp_ns <= w[1].stamp_ns),
        "samples must be stamp-ordered"
    );

    let (lo, hi) = s.common_window().expect("a window covered by every edge");
    assert!(hi > lo, "common window must be non-empty");
    println!(
        "recording: {} frames, {} edges, {} samples, common window {:.1} s",
        s.frame_names().len(),
        s.dynamic_edges.len() + s.static_edges.len(),
        s.samples.len(),
        (hi - lo) as f64 / 1e9
    );
}

/// The engine ingests the whole recording and answers queries over it.
#[test]
fn engine_replays_the_recording_and_answers_queries() {
    let s = load();
    let tree = s
        .build_tree(InterpPolicy::LerpSlerp)
        .expect("build tree from recording");
    let queries = QuerySet::draw(&s, 20_000, 0x5EED_BEEF).expect("draw queries");
    let (poses, declined) = replay::evaluate(&tree, &queries).expect("evaluate");

    assert_eq!(poses.len(), 20_000);
    let answered = poses.len() - declined;
    // The graph is partly disconnected, so some pairs resolve and some do not.
    assert!(
        answered > 0,
        "the engine answered nothing — the replay did not load"
    );
    println!("replay: {answered}/{} queries answered", poses.len());

    // A NaN would sail through a tolerance comparison.
    for p in poses.iter().flatten() {
        for w in p.to_bits() {
            assert!(
                f64::from_bits(w).is_finite(),
                "non-finite component in a replayed lookup"
            );
        }
    }
}

/// Everything replayed stays readable, including the oldest sample: a ring of
/// `cap` slots retains `cap - 1`, so `next_pow2(count)` sizing dropped the first
/// sample of an edge with a power-of-two count (512, as `bench_scale` uses).
#[test]
fn build_tree_retains_every_replayed_sample() {
    for stream in [replay::synth_robot(3, 2, 512, 100.0), load()] {
        let tree = stream
            .build_tree(InterpPolicy::LerpSlerp)
            .expect("build tree");

        for s in &stream.samples {
            let (parent, child) = &stream.dynamic_edges[s.edge];
            let stamp: Stamp = Stamp::from_nanos(s.stamp_ns);
            assert!(
                tree.lookup(parent, child, stamp).is_ok(),
                "sample at {} ns on {parent}->{child} is not readable: {:?}",
                s.stamp_ns,
                tree.lookup(parent, child, stamp).unwrap_err()
            );
        }

        // The first stamp of the common window is the one the off-by-one ate.
        let (lo, _hi) = stream.common_window().expect("common window");
        for (parent, child) in &stream.dynamic_edges {
            let stamp: Stamp = Stamp::from_nanos(lo);
            assert!(
                tree.lookup(parent, child, stamp).is_ok(),
                "the first common-window stamp ({lo} ns) is declined on {parent}->{child}"
            );
        }
    }
}

/// tf_tree vs ROS 2's `tf2::BufferCore` on the identical recorded stream.
#[cfg(feature = "tf2")]
#[test]
fn tf2_agrees_on_the_recorded_stream() {
    use tf_tree_bench::replay_tf2;

    let s = load();
    let tree = s
        .build_tree(InterpPolicy::LerpSlerp)
        .expect("build tree from recording");
    let buffer = replay_tf2::load_tf2(&s).expect("load tf2 from recording");

    let queries = QuerySet::draw(&s, 50_000, 0x5EED_BEEF).expect("draw queries");
    let (ours, _declined) = replay::evaluate(&tree, &queries).expect("evaluate");

    let mut compared = 0usize;
    let mut max_error = 0.0f64;
    let mut worst = String::new();
    let mut only_ours = 0usize;
    let mut only_tf2 = 0usize;

    for (i, (target, source, stamp_ns)) in queries.queries.iter().enumerate() {
        let theirs = buffer.lookup(target, source, *stamp_ns).ok();
        match (ours[i], theirs) {
            (Some(a), Some(b)) => {
                let e = pose_error(&a, &b);
                compared += 1;
                if e > max_error {
                    max_error = e;
                    worst = format!("{target}<-{source} @ {stamp_ns} ns");
                }
            }
            (Some(_), None) => only_ours += 1,
            (None, Some(_)) => only_tf2 += 1,
            (None, None) => {}
        }
    }

    println!(
        "recorded-stream differential: max_error={max_error:e} over {compared} queries \
         (tf_tree-only {only_ours}, tf2-only {only_tf2}) worst: {worst}"
    );

    assert!(
        compared > queries.queries.len() / 10,
        "only {compared} queries were comparable — the comparison proved little"
    );
    assert!(
        max_error <= 1e-12,
        "tf_tree and tf2 disagree on real recorded data: {max_error:e} at {worst}"
    );
}

#[cfg(feature = "tf2")]
fn pose_error(x: &tf_tree::Iso3, y: &tf_tree::Iso3) -> f64 {
    let dq = x.q.conjugate() * y.q;
    tf_tree_math::log_so3(dq).norm().max(x.t.sub(y.t).norm())
}
