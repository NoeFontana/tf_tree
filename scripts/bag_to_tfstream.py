#!/usr/bin/env python3
"""Convert the /tf and /tf_static topics of a ROS 2 bag into a `.tfstream` file.

Run this inside the ROS container (`just tf2-shell`); the *output* has no ROS
dependency at all, which is the point. The Rust replay harness reads only
`.tfstream`, so it builds and runs on any host, and any bag from any source —
any distro, any storage plugin, any message ordering — reduces to one format we
control.

Usage:
    python3 scripts/bag_to_tfstream.py <bag-dir-or-db3> <out.tfstream>

Format (`.tfstream` v1), line-oriented ASCII so it stays greppable and diffable:

    # comment
    S <parent> <child> <qw> <qx> <qy> <qz> <tx> <ty> <tz>
    D <parent> <child> <stamp_ns> <qw> <qx> <qy> <qz> <tx> <ty> <tz>

* `S` is a static transform (`/tf_static`): one per edge, valid at any stamp.
* `D` is a dynamic sample (`/tf`), emitted in non-decreasing `stamp_ns` order.
* Quaternions are **w-first** — matching `tf_tree_math::Iso3`, not ROS's w-last
  wire order. The transposition happens here, once, at the boundary.
* `stamp_ns` is rebased so the earliest sample in the file is 0. Real bags carry
  wall-clock epochs (~1.8e18 ns), which is fine for i64 but makes every dump
  unreadable and would collide with tf2's unsigned-time handling on any bag
  recorded before 1970. The rebase offset is recorded in a header comment so a
  stamp can be traced back to the original recording.

Header comments also record provenance (source bag, message counts, duration),
so a `.tfstream` is self-describing when it turns up in a benchmark directory
six months later.
"""

import pathlib
import sys
from collections import Counter

import rosbag2_py
from rclpy.serialization import deserialize_message
from tf2_msgs.msg import TFMessage


def open_reader(path: pathlib.Path):
    """Open a bag by directory or by .db3 file, letting rosbag2 sniff the format."""
    uri = str(path.parent if path.suffix == ".db3" else path)
    reader = rosbag2_py.SequentialReader()
    reader.open(
        rosbag2_py.StorageOptions(uri=uri, storage_id=""),
        rosbag2_py.ConverterOptions("", ""),
    )
    return reader


def stamp_ns(msg_stamp) -> int:
    return int(msg_stamp.sec) * 1_000_000_000 + int(msg_stamp.nanosec)


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    bag_path = pathlib.Path(sys.argv[1])
    out_path = pathlib.Path(sys.argv[2])

    reader = open_reader(bag_path)
    wanted = {"/tf": False, "/tf_static": True}

    statics = {}  # (parent, child) -> pose, last write wins
    dynamics = []  # (stamp_ns, parent, child, pose)
    counts = Counter()

    while reader.has_next():
        topic, data, _recv_ns = reader.read_next()
        if topic not in wanted:
            continue
        is_static = wanted[topic]
        for t in deserialize_message(data, TFMessage).transforms:
            q = t.transform.rotation
            v = t.transform.translation
            # ROS wire order is w-last; emit w-first to match Iso3.
            pose = (q.w, q.x, q.y, q.z, v.x, v.y, v.z)
            key = (t.header.frame_id, t.child_frame_id)
            if is_static:
                statics[key] = pose
            else:
                dynamics.append((stamp_ns(t.header.stamp), key[0], key[1], pose))
            counts[topic] += 1

    if not dynamics and not statics:
        print(f"error: no /tf or /tf_static messages in {bag_path}", file=sys.stderr)
        return 1

    dynamics.sort(key=lambda r: r[0])
    base = dynamics[0][0] if dynamics else 0
    span_s = (dynamics[-1][0] - base) / 1e9 if dynamics else 0.0

    edges = Counter((p, c) for _, p, c, _ in dynamics)

    def fmt(pose) -> str:
        # 17 significant digits round-trips an f64 exactly, so the replay feeds
        # both engines bit-identical values.
        return " ".join(f"{x:.17g}" for x in pose)

    with out_path.open("w") as f:
        f.write("# tfstream v1\n")
        f.write(f"# source: {bag_path}\n")
        f.write(f"# stamp_ns rebased by -{base} (original epoch of first sample)\n")
        f.write(f"# static edges: {len(statics)}\n")
        f.write(f"# dynamic edges: {len(edges)}, samples: {len(dynamics)}\n")
        f.write(f"# duration: {span_s:.3f} s\n")
        for (p, c), n in sorted(edges.items()):
            f.write(f"#   {p} -> {c}: {n} samples ({n / span_s:.1f} Hz)\n" if span_s else "")
        for (p, c), pose in sorted(statics.items()):
            f.write(f"S {p} {c} {fmt(pose)}\n")
        for ns, p, c, pose in dynamics:
            f.write(f"D {p} {c} {ns - base} {fmt(pose)}\n")

    print(
        f"wrote {out_path}: {len(statics)} static, {len(dynamics)} dynamic "
        f"samples over {len(edges)} edges, {span_s:.1f} s"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
