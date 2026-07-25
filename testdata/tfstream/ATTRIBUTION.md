# Recorded `/tf` streams — sources and licensing

Every `.tfstream` here is a **derived work**: the `/tf` and `/tf_static` topics
of a publicly released ROS 2 bag, converted by `scripts/bag_to_tfstream.py` into
this repository's plain-text replay format. No other topic is retained.

Only permissively-licensed recordings are used, so these files can be
redistributed with the repository and replayed in CI.

## `indoor_atelier.tfstream`

| | |
|---|---|
| **Source** | *Indoor–Outdoor Synchronized Multi-Sensor Dataset for Mobile Robot Navigation and SLAM* |
| **DOI** | <https://doi.org/10.5281/zenodo.19894190> |
| **Record** | <https://zenodo.org/records/19894190> |
| **License** | Creative Commons Attribution 4.0 International (**CC BY 4.0**) |
| **Robot** | ROSBOT PLUS, indoor run (`dataset/indoor/full/rosbag`) |

**Changes made to the original** (CC BY 4.0 §3(a)(1)(B) requires indicating
these):

1. Only `/tf` and `/tf_static` were extracted; every other topic was discarded.
2. Messages were converted from ROS 2 CDR to this repository's `.tfstream`
   text format.
3. Quaternions were reordered from ROS's w-last wire order to w-first, matching
   `tf_tree_math::Iso3`.
4. Timestamps were rebased so the earliest sample is `0`. The original epoch
   offset is recorded in the file's header comments, so any stamp can be mapped
   back to the source recording.

Values themselves are unmodified: floats are written with 17 significant
digits, which round-trips an `f64` exactly.

## Adding another recording

```bash
just tf2-shell
python3 scripts/bag_to_tfstream.py <bag-dir> testdata/tfstream/<name>.tfstream
```

Then add a section above recording the source, DOI, license and the changes
made. **Check the license before adding**: several widely-used robotics
datasets (KITTI, nuScenes, Newer College, Boreas) are CC BY-**NC**-SA, and their
non-commercial clause makes them unsuitable for a permissively-licensed
repository. Autoware's datasets and TUM RGB-D state no clear license at all,
which is equally unusable.
