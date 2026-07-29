# `rosbag2` sqlite3 fixtures

## `synthetic_empty.db3`

**Synthetic. Nothing here came off a robot**, so unlike `testdata/tfstream/`
there is no attribution and no upstream licence: it was generated in this
repository with the host `sqlite3` CLI, by creating `rosbag2`'s two tables and
inserting one `/tf` row into `topics`.

```sql
CREATE TABLE topics(id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                    type TEXT NOT NULL, serialization_format TEXT NOT NULL,
                    offered_qos_profiles TEXT NOT NULL);
CREATE TABLE messages(id INTEGER PRIMARY KEY, topic_id INTEGER NOT NULL,
                      timestamp INTEGER NOT NULL, data BLOB NOT NULL);
INSERT INTO topics VALUES(1, '/tf', 'tf2_msgs/msg/TFMessage', 'cdr', '');
```

`messages` is **empty**, and that is deliberate: the only thing in this
repository that reads the file is `tf_tree_ingest::source::is_sqlite`, which
looks at the first sixteen bytes and refuses. A file with message rows in it
would suggest an ingest path that does not exist — `docs/PHASE5.md` §3.3's
amendment records why the `rosbag2` sqlite3 reader is absent and what would have
to change.

It is a **real** SQLite database rather than sixteen bytes of magic, because the
point of the fixture is that a genuine `.db3` handed to `tf_tree ingest` is
diagnosed rather than reported as a corrupt MCAP. When the reader does land, the
schema it needs is already here.
