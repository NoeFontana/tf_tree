# Frozen `.tft` fixtures

## `sensor_domain.tft`

**Synthetic; no attribution or upstream licence.** Regenerate with
`cargo run -p tf_tree --features shm --example gen_domain_fixture`. Its dynamic
edges carry **time domain 1** (`SensorDomain`), which Python cannot otherwise
construct; [`0038`](../../docs/decisions/0038-the-domain-a-binding-cannot-name.md)
step 4 needs it.

Every `.tft` is at least 2 MiB because `ARENA_FILE_ALIGN` is **2 MiB**
([`0021`](../../docs/decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md));
the padding is zeros, so git stores it in under 5 KB.

`crates/tf_tree/tests/frozen.rs` asserts the file's properties, not its bytes
(the header carries a pid, boot id and uuid), and catches it going stale against a
format change.
