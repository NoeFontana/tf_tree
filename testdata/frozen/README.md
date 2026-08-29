# Frozen `.tft` fixtures

## `sensor_domain.tft`

**Synthetic. Nothing here came off a robot**, so unlike `testdata/tfstream/`
there is no attribution and no upstream licence. Regenerate it with:

```sh
cargo run -p tf_tree --features shm --example gen_domain_fixture
```

`crates/tf_tree/examples/gen_domain_fixture.rs` is that generator and carries the
argument for why the fixture exists at all. In short: it is an arena whose
dynamic edges carry **time domain 1** (`SensorDomain`), and it is the only such
arena any Python process can reach.

[`0038`](../../docs/decisions/0038-the-domain-a-binding-cannot-name.md) step 4
asks for a pytest that plans in a non-zero domain and reads a transform. Python
cannot *build* one — `tf_tree.build` and `tf_tree.open(create=...)` construct
their `EdgeCfg` from a capacity alone and never reach `EdgeCfg::domain`, and
`open_arena(domain=)` is the unrelated `u32` rendezvous domain. On a tag-0 arena
`check_domain_tag` compares 0 against 0 whichever spelling the binding used, so
before this file, reverting **all six** of `tf_tree_py`'s `Plan`-handle query
sites to the pre-`0038` `Stamp::<SystemDomain>` spelling left the Python suite
fully green. `docs/PHASE5.md` §2.1 is what makes a file the answer: NORMATIVE
that a frozen `.tft` is read by the identical `Plan::at` code as a live arena.

### Why 2 MB for 2597 bytes of data

`ARENA_FILE_ALIGN` is **2 MiB** (`crates/tf_tree_arena/src/frozen.rs:85`): the
arena image starts on a 2 MiB boundary so a huge page can back the mapping
([`0021`](../../docs/decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md)).
Every `.tft` is therefore at least that big, and this one is a 23 KB arena behind
2 MiB of padding. The padding is zeros, so git stores the whole file in **under
5 KB** — smaller than `testdata/tfstream/indoor_atelier.tfstream`. It is not
build output, and `scripts/no-build-output.sh` matches on cargo's own marker
files rather than on size or extension, so nothing here is a false positive for
that gate.

### What holds it to account

`crates/tf_tree/tests/frozen.rs` reads the committed file and asserts its
properties rather than its bytes — a `.tft` header carries `created_unix_ns`,
`creator_pid`, `boot_id` and `instance_uuid`, so two freezes of one tree are
never byte-identical and a `memcmp` would fail on every run. That test is what
catches the fixture going stale against a format change, in `just test`, rather
than leaving it to a Python-only run.
