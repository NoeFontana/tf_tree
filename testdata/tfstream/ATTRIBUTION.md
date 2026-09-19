# Recorded `/tf` streams — sources and licensing

Every `.tfstream` here is a **derived work**: the `/tf` and `/tf_static` topics of
a publicly released ROS 2 bag, converted by `scripts/bag_to_tfstream.py` into this
repository's plain-text replay format. Only permissively-licensed recordings are
used, so they can be redistributed and replayed in CI.

## `indoor_atelier.tfstream`

| | |
|---|---|
| **Source** | *Indoor–Outdoor Synchronized Multi-Sensor Dataset for Mobile Robot Navigation and SLAM* |
| **DOI** | <https://doi.org/10.5281/zenodo.19894190> |
| **Record** | <https://zenodo.org/records/19894190> |
| **License** | Creative Commons Attribution 4.0 International (**CC BY 4.0**) |
| **Robot** | ROSBOT PLUS, indoor run (`dataset/indoor/full/rosbag`) |

**Changes made to the original** (CC BY 4.0 §3(a)(1)(B)): only `/tf` and `/tf_static` were extracted, converted from ROS 2 CDR to the
`.tfstream` text format, quaternions reordered from w-last to w-first (matching
`tf_tree_math::Iso3`), and timestamps rebased so the earliest sample is `0` (the
epoch offset is in the file's header). Values are otherwise unmodified: 17
significant digits round-trip an `f64`.

## Adding another recording

Run `python3 scripts/bag_to_tfstream.py <bag-dir> testdata/tfstream/<name>.tfstream`
inside `just tf2-shell`, then add a section above with source, DOI, license and
changes. **Check the license first**: KITTI, nuScenes, Newer College and Boreas
are CC BY-**NC**-SA, and Autoware's datasets and TUM RGB-D state none.
