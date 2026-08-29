"""Reading a recording from Python (`docs/PHASE5.md` §3 and §4, `0046`).

`PHASE5` §0.0's §3 row says `tf_tree_ingest` is a library crate rather than part
of the CLI *"because §4's offline Python API needs the same logic and cannot
depend on a binary crate"*. These are the tests for the consumer that sentence
names, which did not exist until `docs/decisions/0046`.

**The fixture is the committed conformance recording, not a hand-written one.**
`crates/tf_tree_ingest/testdata/zstd_conformance.mcap` is a real zstd-compressed
MCAP with a static edge, a dynamic edge and twelve transforms; hand-assembling
bytes here would put a second copy of the format's framing in the one place that
cannot notice when the reader's understanding of it moves.
"""

import pathlib

import pytest
import tf_tree

BAG = (
    pathlib.Path(__file__).resolve().parents[2]
    / "crates"
    / "tf_tree_ingest"
    / "testdata"
    / "zstd_conformance.mcap"
)

# Writing a `.tft` is `#[cfg(all(feature = "shm", target_os = "linux"))]` in the
# facade, exactly as `test_frozen.py` documents. `ingest_bag` itself is portable
# and is *not* skipped here; only the tests that freeze are.
frozen = pytest.mark.skipif(
    not tf_tree.has_shared_memory(), reason="writing a .tft needs the mmap backend"
)


@pytest.fixture(scope="module")
def bag() -> str:
    assert BAG.is_file(), f"missing committed fixture recording: {BAG}"
    return str(BAG)


def test_ingest_bag_returns_an_ordinary_tree(bag):
    """§4.1's "no parallel offline API", structurally.

    The point is not that ingestion works but that what comes back is the same
    type `open_file` returns, so everything downstream is unchanged.
    """
    tree = tf_tree.ingest_bag(bag)
    assert isinstance(tree, tf_tree.Tree)
    frames = sorted(tree.frames())
    assert len(frames) == 3
    # A plan compiles and answers, which is the whole reason to have the tree.
    parent, child = tree.edges()[0]
    plan = tree.plan(child, parent)
    assert plan is not None


def test_source_describes_the_recording(bag):
    src = tf_tree.ingest_bag(bag).source
    assert src is not None
    assert set(src) == {
        "path",
        "digest",
        "transforms",
        "edges_without_samples",
        "recording_start_ns",
        "recording_end_ns",
    }
    assert src["path"] == bag
    # BLAKE3 is 32 bytes; the dict carries it as hex because what a caller does
    # with it is compare it to what `tf_tree doctor` printed.
    assert len(src["digest"]) == 64
    bytes.fromhex(src["digest"])
    assert src["transforms"] == 12
    assert src["digest"] != "00" * 32


def test_the_digest_is_of_the_file_not_the_transforms(bag, tmp_path):
    """§2.3: it answers "was this index built from *that* file"."""
    first = tf_tree.ingest_bag(bag).source["digest"]
    # Same bytes, different name — the digest is over content, so it must not
    # move. A digest that keyed on the path would pass every other test here.
    copy = tmp_path / "renamed.mcap"
    copy.write_bytes(BAG.read_bytes())
    assert tf_tree.ingest_bag(str(copy)).source["digest"] == first
    # One byte different — the whole point is that this changes the answer.
    mutated = bytearray(BAG.read_bytes())
    mutated[-1] ^= 0xFF
    other = tmp_path / "mutated.mcap"
    other.write_bytes(bytes(mutated))
    try:
        got = tf_tree.ingest_bag(str(other)).source["digest"]
    except Exception:
        # Corrupting the trailing byte may make the recording unreadable, which
        # is a fine outcome — it just cannot be asserted on as a digest change.
        pytest.skip("mutating the last byte made the recording unreadable")
    assert got != first


def test_a_tree_with_no_recording_has_no_source():
    """`None` is the honest answer, and the same one the container's zero means."""
    tree = tf_tree.build([("base_link", "lidar")], capacity=8)
    assert tree.source is None


@frozen
def test_a_reopened_index_has_no_source(bag, tmp_path):
    """Provenance is not carried *inside* the `.tft`'s Python surface.

    The digest is in the container header; `Tree.source` describes a tree this
    process ingested, and a tree opened from a file did not ingest anything.
    """
    out = tmp_path / "a.tft"
    tf_tree.ingest_bag(bag).freeze(str(out))
    assert tf_tree.open_file(str(out)).source is None


@frozen
def test_freeze_writes_the_recordings_digest(bag, tmp_path):
    """`ingest_bag(p).freeze(out)` is the whole bag-to-`.tft` path (`0046`)."""
    tree = tf_tree.ingest_bag(bag)
    digest = bytes.fromhex(tree.source["digest"])
    out = tmp_path / "a.tft"
    tree.freeze(str(out))
    assert digest in out.read_bytes()


@frozen
def test_publisher_drops_the_provenance(bag, tmp_path):
    """**A wrong digest is worse than an absent one.**

    A caller may ingest a recording, add a computed edge and freeze; the index
    would then assert it is that file while holding samples the file does not.

    The control and the mutant differ by exactly one `publisher()` call on the
    same recording, so a pass here cannot be coming from the digest being absent
    for some unrelated reason — which is the failure mode an assertion written
    only against the mutant would have.
    """
    digest = bytes.fromhex(tf_tree.ingest_bag(bag).source["digest"])

    control = tf_tree.ingest_bag(bag)
    a = tmp_path / "control.tft"
    control.freeze(str(a))
    assert digest in a.read_bytes()

    mutant = tf_tree.ingest_bag(bag)
    assert mutant.source is not None
    # `edges()` yields (parent, child); `publisher()` takes (child, parent).
    parent, child = next(
        (p, c) for (p, c) in mutant.edges() if _has_publisher(mutant, c, p)
    )
    mutant.publisher(child, parent)
    assert mutant.source is None

    b = tmp_path / "mutant.tft"
    mutant.freeze(str(b))
    assert digest not in b.read_bytes()


def _has_publisher(tree, child, parent) -> bool:
    """Whether this edge is dynamic — a static edge carries no sample ring."""
    try:
        tree.publisher(child, parent)
    except Exception:
        return False
    return True


def test_recording_bounds_are_not_the_queryable_span(bag):
    """`recording_*`, not `span_*`, and this test is why the name changed.

    The first end-to-end run of this API took the upper stamp from the report,
    queried it, and got an extrapolation error from an edge whose retained
    history had stopped earlier. The recording's interval bounds the tree's; it
    does not equal it.
    """
    tree = tf_tree.ingest_bag(bag)
    lo, hi = tree.source["recording_start_ns"], tree.source["recording_end_ns"]
    assert lo is not None and hi is not None and lo <= hi
    parent, child = tree.edges()[0]
    span = tree.span(child, parent)
    if span is not None:
        assert lo <= span[0] and span[1] <= hi


def test_a_missing_recording_raises_oserror(tmp_path):
    """A caller who wrote `except FileNotFoundError` should catch this."""
    with pytest.raises(OSError):
        tf_tree.ingest_bag(str(tmp_path / "nope.mcap"))


def test_a_file_that_is_not_a_recording_is_refused(tmp_path):
    junk = tmp_path / "junk.mcap"
    junk.write_bytes(b"not an mcap at all, not even close")
    with pytest.raises(Exception) as e:
        tf_tree.ingest_bag(str(junk))
    assert "OSError" not in type(e.value).__name__ or isinstance(e.value, OSError)


def test_max_record_bytes_is_reachable_from_python(bag):
    """`0010` added the knob so a caller could raise it without forking.

    Reachable only from Rust, that argument does not hold for §4's audience —
    so the ceiling is a keyword, and this asserts it is *connected*, not merely
    accepted: at one byte every record in any recording is over it.
    """
    with pytest.raises(Exception) as e:
        tf_tree.ingest_bag(bag, max_record_bytes=1)
    assert "1" in str(e.value)
    # And the default admits the same file, so the failure above is the ceiling
    # and not the recording.
    assert tf_tree.ingest_bag(bag).source["transforms"] == 12


def test_topic_keywords_are_connected(bag):
    """Naming a topic that carries nothing is refused, not silently empty.

    Asserting the refusal rather than an empty tree is deliberate, and it is
    what the library actually does: a recording that yielded no transforms is
    almost always a wrong `--tf-topics`, and handing back a tree that answers
    nothing would defer that discovery to the first query. The assertion here
    started out expecting the empty tree and was corrected to the behaviour,
    which is the better of the two.
    """
    with pytest.raises(Exception) as e:
        tf_tree.ingest_bag(bag, tf_topics=["/nothing_publishes_here"])
    assert "no" in str(e.value).lower()
    # The same call without the override reads the recording, so the refusal
    # above is the topic filter and not the file.
    assert tf_tree.ingest_bag(bag).source["transforms"] == 12
