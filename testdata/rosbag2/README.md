# `rosbag2` sqlite3 fixtures

## `synthetic_empty.db3`

**Synthetic; no attribution or upstream licence.** A real SQLite database
(`rosbag2`'s `topics` and `messages` tables, one `/tf` row) so a genuine `.db3`
handed to `tf_tree ingest` is diagnosed rather than reported as a corrupt MCAP.
`messages` is **empty** on purpose: the only reader is
`tf_tree_ingest::source::is_sqlite`, which checks the first sixteen bytes and
refuses (`docs/PHASE5.md` §3.3).
