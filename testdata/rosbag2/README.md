# `rosbag2` sqlite3 fixtures

## `synthetic_empty.db3`

**Synthetic; no attribution or upstream licence.** A real SQLite database (made
with the host `sqlite3` CLI: `rosbag2`'s `topics` and `messages` tables and one
`/tf` `topics` row, `tf2_msgs/msg/TFMessage`, `cdr`) so a genuine `.db3` handed to
`tf_tree ingest` is diagnosed rather than reported as a corrupt MCAP. `messages`
is **empty** on purpose: the only reader is `tf_tree_ingest::source::is_sqlite`,
which checks the first sixteen bytes and refuses (`docs/PHASE5.md` §3.3's
amendment says why no sqlite3 reader exists).
