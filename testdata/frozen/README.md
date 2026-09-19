# Frozen `.tft` fixtures

## `sensor_domain.tft`

**Synthetic; no attribution or upstream licence.** Regenerate with
`cargo run -p tf_tree --features shm --example gen_domain_fixture`, which carries
the full argument. It is an arena whose dynamic edges carry **time domain 1**
(`SensorDomain`), the only such arena Python can reach (`tf_tree.build` and
`tf_tree.open(create=...)` never reach `EdgeCfg::domain`), which
[`0038`](../../docs/decisions/0038-the-domain-a-binding-cannot-name.md) step 4
needs; `docs/PHASE5.md` §2.1 makes a file the answer.

Every `.tft` is at least 2 MiB because `ARENA_FILE_ALIGN` is **2 MiB**
(`crates/tf_tree_arena/src/frozen.rs:85`;
[`0021`](../../docs/decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md)).
The padding is zeros, so git stores the file in under 5 KB.

`crates/tf_tree/tests/frozen.rs` reads the committed file and asserts its
properties, not its bytes (the header carries `created_unix_ns`, `creator_pid`,
`boot_id` and `instance_uuid`, so two freezes never match byte for byte), and
catches the fixture going stale against a format change in `just test`.
