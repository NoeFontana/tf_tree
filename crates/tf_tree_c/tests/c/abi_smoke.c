/*
 * The committed headers, compiled and executed as a C program.
 * docs/PHASE4.md §6.2, and the only test of `tf_tree.h` that is not Rust.
 *
 * The Rust integration tests drive the same entry points, but they see the
 * *crate*, not the header — so they cannot catch a header that does not
 * compile, declares a wrong prototype, or spells a constant differently from
 * the library. That is exactly the class of bug a generated header has, and
 * this file is what closes it. An earlier revision of `xtask headers` shipped a
 * header with an unbalanced `#endif`; every Rust test still passed.
 *
 * Built by `just c-header-check` against gcc and clang, as C11 and C++17, with
 * -Wall -Wextra -Wpedantic -Werror. §6.2 asks for both compilers; this is that
 * matrix.
 */
#include "tf_tree.h"

#define TFT_ENABLE_UNSTABLE
#include "tf_tree_unstable.h"

#include <stddef.h>
#include <stdio.h>
#include <string.h>

/*
 * The fixture constructor is a test hook (`--features test-hooks`) and is
 * deliberately absent from both headers — it is not part of the shipped ABI.
 * Declaring it here by hand is the point: a C caller can reach any exported
 * symbol, and this is how the working path gets exercised from C without a
 * running shared arena.
 *
 * The `extern "C"` wrapper is not decoration: without it the C++ rows of the
 * matrix mangle the name and fail to link. The generated headers get theirs
 * from `xtask headers`; a hand-written declaration has to remember.
 */
#ifdef __cplusplus
extern "C" {
#endif
extern tft_status tft_test_publishable_tree_create(tft_tree **out);
extern tft_status tft_test_domain_tree_create(uint8_t domain, tft_tree **out);
#ifdef __cplusplus
}
#endif

static int failures = 0;

#define CHECK(cond, msg)                                                      \
    do {                                                                      \
        if (!(cond)) {                                                        \
            fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, (msg));    \
            failures++;                                                       \
        }                                                                     \
    } while (0)

/* Report what the library last said, for a failure message worth reading. */
static const char *why(void)
{
    static tft_error e;
    e.struct_size = (uint32_t)sizeof(tft_error);
    if (tft_last_error(&e) != TFT_OK) {
        return "<tft_last_error failed>";
    }
    return e.message;
}

static void check_version_and_constants(void)
{
    /* §3.6: the first thing any caller does. */
    CHECK(tft_check_abi(TFT_ABI_VERSION_MAJOR, TFT_ABI_VERSION_MINOR) == TFT_OK,
          "the header and the library disagree about the ABI version");
    CHECK(tft_abi_version_major() == TFT_ABI_VERSION_MAJOR, "major mismatch");
    CHECK(tft_abi_version_minor() == TFT_ABI_VERSION_MINOR, "minor mismatch");
    CHECK(tft_check_abi(TFT_ABI_VERSION_MAJOR + 1, 0) == TFT_ERR_ABI_MISMATCH,
          "a different major must be rejected");

    /* The header's layout sizes must be the library's, or every buffer a
     * caller allocates from `tft_layout_size` is the wrong size. */
    CHECK(tft_layout_size(TFT_LAYOUT_QVEC7_WXYZ) == 56, "QVEC7 size");
    CHECK(tft_layout_size(TFT_LAYOUT_QVEC7_XYZW) == 56, "QVEC7_XYZW size");
    CHECK(tft_layout_size(TFT_LAYOUT_MAT4_ROW) == 128, "MAT4_ROW size");
    CHECK(tft_layout_size(TFT_LAYOUT_MAT4_COL) == 128, "MAT4_COL size");
    CHECK(tft_layout_size(TFT_LAYOUT_AFFINE12_ROW_F32) == 48, "AFFINE12 size");
    CHECK(tft_layout_size(9999) == 0, "an unknown layout has no size");

    CHECK(TFT_MESSAGE_LEN == 256, "message length");
    CHECK(TFT_TWIST_BYTES == 48, "twist is six f64");
}

static void check_null_is_never_dereferenced(void)
{
    unsigned char buf[128];
    CHECK(tft_plan_at(NULL, 0, TFT_LAYOUT_MAT4_ROW, buf) == TFT_ERR_BAD_HANDLE,
          "a NULL plan must be a bad handle, not a segfault");
    CHECK(tft_last_error(NULL) == TFT_ERR_NULL_ARG, "NULL out");
    /* The new spelling validates before it dereferences, as the old one does. */
    CHECK(tft_plan_create_in_domain(NULL, "a", "b", 0, NULL) == TFT_ERR_BAD_HANDLE,
          "a NULL tree must be a bad handle, not a segfault");
    CHECK(tft_plan_at_extrapolating(NULL, 0, TFT_EXTRAP_HOLD, TFT_LAYOUT_MAT4_ROW,
                                    buf, NULL) == TFT_ERR_BAD_HANDLE,
          "a NULL plan must be a bad handle, not a segfault");
    /* Freeing NULL is documented as a no-op. If it is not, this crashes. */
    tft_tree_free(NULL);
    tft_plan_free(NULL);
    tft_publisher_free(NULL);
}

static void check_struct_size_gate(void)
{
    tft_error e;
    memset(&e, 0, sizeof e);
    e.struct_size = 8; /* far too small — a caller from a different header */
    CHECK(tft_last_error(&e) == TFT_ERR_BAD_STRUCT_SIZE,
          "an unknown struct_size must be refused, not written over");
}

/* Publish through the C ABI and read the result back, all from C. */
static void check_round_trip(void)
{
    tft_tree *tree = NULL;
    tft_publisher *pub = NULL;
    tft_plan *plan = NULL;
    double q[7];
    double out[7];
    double twist[6];
    tft_status rc;

    CHECK(tft_test_publishable_tree_create(&tree) == TFT_OK, "fixture");
    if (tree == NULL) {
        return;
    }

    rc = tft_tree_claim(tree, "robot", "world", &pub);
    CHECK(rc == TFT_OK, why());

    /* Two knots a second apart, so the reader has a segment. */
    q[0] = 1.0; q[1] = 0.0; q[2] = 0.0; q[3] = 0.0;
    q[4] = 0.0; q[5] = 0.0; q[6] = 0.0;
    CHECK(tft_publisher_push(pub, 0, TFT_LAYOUT_QVEC7_WXYZ, q) == TFT_OK, why());
    q[4] = 2.0;
    rc = tft_publisher_push(pub, 1000000000, TFT_LAYOUT_QVEC7_WXYZ, q);
    CHECK(rc == TFT_OK, why());

    /* A backwards stamp is refused, and says so. */
    CHECK(tft_publisher_push(pub, 5, TFT_LAYOUT_QVEC7_WXYZ, q) == TFT_ERR_NON_MONOTONIC,
          "stamps are non-decreasing");

    /* A non-unit quaternion — what an uninitialised C struct looks like. */
    memset(q, 0, sizeof q);
    CHECK(tft_publisher_push(pub, 2000000000, TFT_LAYOUT_QVEC7_WXYZ, q) == TFT_ERR_NOT_A_ROTATION,
          "an all-zero quaternion must not reach the arena");

    CHECK(tft_plan_create(tree, "world", "robot", &plan) == TFT_OK, why());
    rc = tft_plan_at(plan, 500000000, TFT_LAYOUT_QVEC7_WXYZ, out);
    CHECK(rc == TFT_OK, why());
    /* Halfway along a pure translation of 2 m. */
    CHECK(out[4] > 0.99 && out[4] < 1.01, "interpolated translation");

    /* The unstable tier, through its own header. */
    rc = tft_plan_at_with_derivatives(plan, 500000000, TFT_LAYOUT_QVEC7_WXYZ, out, twist);
    CHECK(rc == TFT_OK, why());
    CHECK(twist[3] > 1.99 && twist[3] < 2.01, "2 m in 1 s is 2 m/s");

    /*
     * The appended layout — docs/API.md §3.3. Pose and twist in one row of 13,
     * reachable from the *stable* header: a C caller who never defines
     * TFT_ENABLE_UNSTABLE gets derivatives out of tft_plan_at and, batched, out
     * of tft_plan_at_many. That reachability is the whole delta, so it is
     * checked from C and not only from Rust.
     */
    {
        double row[13];
        double rows[2 * 13];
        int64_t stamps[2];
        size_t i;
        CHECK(tft_layout_size(TFT_LAYOUT_QVEC7_WXYZ_TWIST6) == sizeof row,
              "13 doubles");

        /* The unstable scalar call, which can also split the two halves. */
        for (i = 0; i < 13; i++) { row[i] = -12345.5; }
        rc = tft_plan_at_with_derivatives(plan, 500000000,
                                          TFT_LAYOUT_QVEC7_WXYZ_TWIST6, row, NULL);
        CHECK(rc == TFT_OK, why());
        for (i = 0; i < 7; i++) {
            CHECK(row[i] == out[i], "the pose half is QVEC7_WXYZ");
        }
        CHECK(row[10] > 1.99 && row[10] < 2.01, "the tail's v.x is the same 2 m/s");

        /* The stable scalar entry point: same bytes, no unstable header. */
        {
            double stable_row[13];
            for (i = 0; i < 13; i++) { stable_row[i] = -12345.5; }
            rc = tft_plan_at(plan, 500000000, TFT_LAYOUT_QVEC7_WXYZ_TWIST6, stable_row);
            CHECK(rc == TFT_OK, why());
            for (i = 0; i < 13; i++) {
                CHECK(stable_row[i] == row[i],
                      "tft_plan_at must produce the derivative call's bytes");
            }
        }

        /* ...and the stable batch entry point, tightly packed. */
        stamps[0] = 250000000;
        stamps[1] = 500000000;
        for (i = 0; i < 2 * 13; i++) { rows[i] = -12345.5; }
        rc = tft_plan_at_many(plan, stamps, 2, TFT_LAYOUT_QVEC7_WXYZ_TWIST6, rows, 0);
        CHECK(rc == TFT_OK, why());
        for (i = 0; i < 13; i++) {
            CHECK(rows[13 + i] == row[i], "the batch row must match the scalar call");
        }
        /* A quarter of the way along, and the velocity is the same constant. */
        CHECK(rows[4] > 0.49 && rows[4] < 0.51, "interpolated translation at t=0.25s");
        CHECK(rows[10] > 1.99 && rows[10] < 2.01, "the twist is constant along the segment");
    }

    CHECK(tft_tree_frame_count(tree) == 3, "world/robot/tool");
    CHECK(tft_tree_edge_count(tree) == 2, "one dynamic, one static");

    {
        char name[64];
        CHECK(tft_tree_frame_name(tree, 1, name, sizeof name) == TFT_OK, why());
        CHECK(strcmp(name, "world") == 0, "frame ids start at 1");
        /* A name that does not fit must be refused, never truncated. */
        CHECK(tft_tree_frame_name(tree, 1, name, 3) == TFT_ERR_BUFFER_TOO_SMALL,
              "a truncated frame name is a different frame name");
    }

    tft_plan_free(plan);
    tft_publisher_free(pub);
    tft_tree_free(tree);
}

#if defined(TFT_HAVE_BRIDGE)
/*
 * The ingest-bridge seam, from C — docs/PHASE4.md §5.
 *
 * The Rust test `tests/bridge.rs` covers the behaviour far more thoroughly.
 * What it cannot cover is anything about the *header*, and the bridge surface
 * is where that matters most: `tft_bridge_stats` is both a typedef and (before
 * review) the name of a function, which is legal in Rust and does not compile
 * in C. Nothing on the Rust side could have found that.
 *
 * So this checks the shape a C caller depends on and nothing else: that the
 * declarations compile, that the POD outcome is filled with strings that are
 * printable rather than NULL, and that Rust — not the C++ node — is what
 * decides and what writes.
 */
static const char *const BRIDGE_TOPOLOGY =
    "[[edge]]\n"
    "parent = \"odom\"\n"
    "child = \"base\"\n"
    "kind = \"dynamic\"\n"
    "capacity = 64\n";

static void check_bridge(void)
{
    tft_bridge *b = NULL;
    tft_bridge_options opts;
    tft_bridge_sample s;
    tft_bridge_sample legacy;
    tft_bridge_outcome o;
    tft_bridge_stats stats;
    tft_tree *tree = NULL;
    tft_plan *plan = NULL;
    double out[7];
    unsigned char gid[16];
    int i;

    memset(&opts, 0, sizeof opts);
    opts.struct_size = (uint32_t)sizeof opts;
    opts.authority = TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS;
    opts.on_clock_reset = TFT_BRIDGE_ON_CLOCK_RESET_HALT;
    CHECK(tft_bridge_create(BRIDGE_TOPOLOGY, &opts, &b) == TFT_OK, why());
    if (b == NULL) {
        return;
    }

    for (i = 0; i < 16; i++) {
        gid[i] = (unsigned char)(i + 1);
    }
    CHECK(tft_bridge_attribute(b, gid, "/ekf") == TFT_OK, why());

    memset(&s, 0, sizeof s);
    s.struct_size = (uint32_t)sizeof s;
    s.frame_id = "odom";
    s.child_frame_id = "base";
    s.stamp_nanos = 1000000000;
    s.pose[0] = 1.0; /* qw qx qy qz tx ty tz — NOT geometry_msgs' x y z w */
    s.pose[4] = 3.25;

    /* 0xAA everywhere, so "the ABI wrote a well-formed outcome" cannot pass by
     * the struct happening to be zeroed: TFT_BRIDGE_APPLIED is 0. */
    memset(&o, 0xAA, sizeof o);
    o.struct_size = (uint32_t)sizeof o;
    CHECK(tft_bridge_offer(b, TFT_BRIDGE_TOPIC_TF, &s, gid, &o) == TFT_OK, why());
    CHECK(o.action == TFT_BRIDGE_APPLIED, "a declared edge must be written");
    CHECK(o.parent != NULL && strcmp(o.parent, "odom") == 0, "outcome parent");
    CHECK(o.child != NULL && strcmp(o.child, "base") == 0, "outcome child");
    /* Never NULL, per the header — a C caller printf's these. */
    CHECK(o.owner != NULL && o.intruder != NULL && o.detail != NULL,
          "an unset outcome string is empty, never NULL");
    CHECK(o.owner[0] == '\0', "…and empty means empty");

    /* §3.6's append rule, from the only side that can state it in C: a caller
     * compiled before `received_steady_nanos` was appended declares the smaller size,
     * and must still be served. `offsetof` is where the old struct ended, so
     * this stays correct on any target and does not rot when a size changes.
     *
     * The storage is a full, correctly aligned tft_bridge_sample declaring a
     * short size, not a tight byte buffer: what this file exists to check is
     * that the header compiles and the older size is accepted. The bounded read
     * itself is pinned on the Rust side, where the fixture allocates exactly the
     * prefix and a sanitizer can see past it. */
    legacy = s;
    legacy.stamp_nanos = 1500000000;
    legacy.struct_size = (uint32_t)offsetof(tft_bridge_sample, received_steady_nanos);
    memset(&o, 0xAA, sizeof o);
    o.struct_size = (uint32_t)sizeof o;
    CHECK(tft_bridge_offer(b, TFT_BRIDGE_TOPIC_TF, &legacy, gid, &o) == TFT_OK, why());
    CHECK(o.action == TFT_BRIDGE_APPLIED,
          "an appended field must not lock an older caller out");

    /* An edge the config does not declare: dropped, named, diagnosed once. */
    s.child_frame_id = "camera";
    memset(&o, 0xAA, sizeof o);
    o.struct_size = (uint32_t)sizeof o;
    CHECK(tft_bridge_offer(b, TFT_BRIDGE_TOPIC_TF, &s, gid, &o) == TFT_OK, why());
    CHECK(o.action == TFT_BRIDGE_UNDECLARED, "an undeclared edge has nowhere to go");
    CHECK(o.first_time == 1, "the first sighting is the loud one");
    CHECK(o.detail[0] != '\0', "and it says why");

    memset(&stats, 0, sizeof stats);
    stats.struct_size = (uint32_t)sizeof stats;
    CHECK(tft_bridge_get_stats(b, &stats) == TFT_OK, why());
    CHECK(stats.transforms == 3, "three offers");
    CHECK(stats.applied == 2, "two written, one of them by a caller declaring the older size");
    CHECK(stats.dropped_undeclared == 1, "one with nowhere to go");
    CHECK(stats.queue_capacity == 100, "§5.2's KeepLast(100)");

    /* The arena the bridge built, read back through the ordinary stable API. */
    CHECK(tft_bridge_tree(b, &tree) == TFT_OK, why());
    CHECK(tft_plan_create(tree, "odom", "base", &plan) == TFT_OK, why());
    CHECK(tft_plan_at(plan, 1000000000, TFT_LAYOUT_QVEC7_WXYZ, out) == TFT_OK, why());
    CHECK(out[4] > 3.24 && out[4] < 3.26, "the bridge's write is readable");

    tft_plan_free(plan);
    tft_tree_free(tree);
    tft_bridge_free(b);
    tft_bridge_free(NULL); /* documented no-op */
}

/*
 * tf_prefix, and the remap table §5.6 requires to be logged at startup.
 *
 * Two things only this side can check: that tft_bridge_get_remap's declaration
 * compiles and that the termination condition a C loop is told to use — the
 * first index past the end returning TFT_ERR_NO_DATA — is really what it does.
 * A prefixed bridge that recognised none of its own declared edges used to be
 * the shipped behaviour, so the offer below is the regression that matters.
 */
static void check_bridge_prefix(void)
{
    tft_bridge *b = NULL;
    tft_bridge_options opts;
    tft_bridge_sample s;
    tft_bridge_outcome o;
    tft_bridge_remap r;
    int rows = 0;
    uint32_t i;

    memset(&opts, 0, sizeof opts);
    opts.struct_size = (uint32_t)sizeof opts;
    opts.authority = TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS;
    opts.on_clock_reset = TFT_BRIDGE_ON_CLOCK_RESET_HALT;
    opts.tf_prefix = "robot1";
    CHECK(tft_bridge_create(BRIDGE_TOPOLOGY, &opts, &b) == TFT_OK, why());
    if (b == NULL) {
        return;
    }

    /* The wire carries the robot's own names; the arena knows the prefixed
     * ones. Setting tf_prefix must not turn every transform into a drop. */
    memset(&s, 0, sizeof s);
    s.struct_size = (uint32_t)sizeof s;
    s.frame_id = "odom";
    s.child_frame_id = "base";
    s.stamp_nanos = 1000000000;
    s.pose[0] = 1.0;
    s.pose[4] = 3.25;
    memset(&o, 0xAA, sizeof o);
    o.struct_size = (uint32_t)sizeof o;
    CHECK(tft_bridge_offer(b, TFT_BRIDGE_TOPIC_TF, &s, NULL, &o) == TFT_OK, why());
    CHECK(o.action == TFT_BRIDGE_APPLIED, "a prefixed bridge still knows its own edge");
    CHECK(strcmp(o.child, "robot1/base") == 0, "and reports the arena's name");

    /* The startup log §5.6 asks for, walked exactly as the header says. */
    for (i = 0; i < 64; i++) {
        memset(&r, 0, sizeof r);
        r.struct_size = (uint32_t)sizeof r;
        if (tft_bridge_get_remap(b, i, &r) != TFT_OK) {
            break;
        }
        CHECK(r.from != NULL && r.to != NULL, "a remap row is printable");
        rows++;
    }
    CHECK(rows == 2, "one row per declared frame, before any traffic");

    tft_bridge_free(b);
}
#endif /* TFT_HAVE_BRIDGE */

/*
 * The domain a binding could not name — docs/decisions/0038.
 *
 * `docs/PHASE4.md` §5.5 tells an operator to give a simulated tree its own time
 * domain, and until this entry point existed following that advice made the
 * arena unreadable from C: every query site hard-coded tag 0, so every lookup
 * returned TFT_ERR_TIME_DOMAIN forever. It is checked from *here*, and not only
 * from Rust, because the new declaration is the whole delta and a header nobody
 * compiles is not a checked header.
 */
static void check_domain(void)
{
    tft_tree *tree = NULL;
    tft_plan *plan = NULL;
    double out[7];

    CHECK(tft_test_domain_tree_create(1, &tree) == TFT_OK, why());
    if (tree == NULL) {
        return;
    }

    /* All a C caller could say before: the tag-0 assumption, refused at plan
     * time now rather than on every lookup afterwards. */
    CHECK(tft_plan_create(tree, "map", "odom", &plan) == TFT_ERR_TIME_DOMAIN,
          "domain 0 cannot read a tag-1 arena");
    CHECK(plan == NULL, "a refused plan hands back no handle");

    /* And what it can say now. */
    CHECK(tft_plan_create_in_domain(tree, "map", "odom", 1, &plan) == TFT_OK, why());
    CHECK(tft_plan_at(plan, 150000000, TFT_LAYOUT_QVEC7_WXYZ, out) == TFT_OK, why());
    /* 15 knots of 0.05 m, interpolated: a real pose, not a zeroed buffer. */
    CHECK(out[4] > 0.7 && out[4] < 0.8, "a tagged plan reads a transform");
    tft_plan_free(plan);

    /* A route with no dynamic edge on it has no domain to disagree with. */
    plan = NULL;
    CHECK(tft_plan_create_in_domain(tree, "odom", "sensor", 7, &plan) == TFT_OK, why());
    CHECK(tft_plan_at(plan, 150000000, TFT_LAYOUT_QVEC7_WXYZ, out) == TFT_OK, why());
    tft_plan_free(plan);

    tft_tree_free(tree);
}

/*
 * Extrapolation you cannot fail to notice — docs/decisions/0039.
 *
 * The whole surface is here because the header is the delta: a policy typedef,
 * three constants, a struct and one entry point. `tft_extrapolated *info` is
 * required, and that is the C analogue of a Rust return type with no pose-only
 * accessor — a caller cannot obtain the pose without somewhere to put the
 * distance.
 *
 * The three policies are checked at one stamp, and the third comparison is the
 * one that earns the check: a library that ignored `policy` and always held
 * would satisfy every other line here.
 */
static void check_extrapolation(void)
{
    tft_tree *tree = NULL;
    tft_plan *plan = NULL;
    tft_extrapolated info;
    double held[7];
    double twisted[7];
    double newest[7];

    CHECK(tft_test_domain_tree_create(1, &tree) == TFT_OK, why());
    if (tree == NULL) {
        return;
    }
    CHECK(tft_plan_create_in_domain(tree, "map", "sensor", 1, &plan) == TFT_OK, why());
    if (plan == NULL) {
        tft_tree_free(tree);
        return;
    }

    /* The fixture publishes 32 samples 10 ms apart, so 310 ms is the newest and
     * 400 ms is 90 ms past it. */
    memset(&info, 0, sizeof info);
    info.struct_size = (uint32_t)sizeof info;
    CHECK(tft_plan_at_extrapolating(plan, 400000000, TFT_EXTRAP_ERROR,
                                    TFT_LAYOUT_QVEC7_WXYZ, held, &info)
              == TFT_ERR_EXTRAPOLATION,
          "TFT_EXTRAP_ERROR refuses, exactly as tft_plan_at does");

    info.struct_size = (uint32_t)sizeof info;
    CHECK(tft_plan_at_extrapolating(plan, 400000000, TFT_EXTRAP_HOLD,
                                    TFT_LAYOUT_QVEC7_WXYZ, held, &info) == TFT_OK,
          why());
    CHECK(info.by_ns == 90000000, "the distance is measured from the newest common stamp");
    CHECK(info.edge != TFT_INVALID_ID, "an extrapolated answer names the stale edge");
    CHECK(tft_plan_at(plan, 310000000, TFT_LAYOUT_QVEC7_WXYZ, newest) == TFT_OK, why());
    CHECK(memcmp(held, newest, sizeof newest) == 0, "Hold is the newest pose");

    info.struct_size = (uint32_t)sizeof info;
    CHECK(tft_plan_at_extrapolating(plan, 400000000, TFT_EXTRAP_CONSTANT_TWIST,
                                    TFT_LAYOUT_QVEC7_WXYZ, twisted, &info) == TFT_OK,
          why());
    CHECK(info.by_ns == 90000000, "the distance does not depend on the policy");
    CHECK(memcmp(twisted, held, sizeof held) != 0,
          "ConstantTwist must differ from Hold, or the policy is being ignored");

    /* Inside the window nothing is extrapolated, whatever was asked for. */
    info.struct_size = (uint32_t)sizeof info;
    CHECK(tft_plan_at_extrapolating(plan, 150000000, TFT_EXTRAP_HOLD,
                                    TFT_LAYOUT_QVEC7_WXYZ, held, &info) == TFT_OK,
          why());
    CHECK(info.by_ns == 0, "an in-window stamp is interpolated, not invented");
    CHECK(info.edge == TFT_INVALID_ID, "and has no stale edge to name");

    /* The distance is not optional and the struct size is not optional. */
    CHECK(tft_plan_at_extrapolating(plan, 400000000, TFT_EXTRAP_HOLD,
                                    TFT_LAYOUT_QVEC7_WXYZ, held, NULL)
              == TFT_ERR_NULL_ARG,
          "there is no pose-only spelling of this call");
    info.struct_size = 4;
    CHECK(tft_plan_at_extrapolating(plan, 400000000, TFT_EXTRAP_HOLD,
                                    TFT_LAYOUT_QVEC7_WXYZ, held, &info)
              == TFT_ERR_BAD_STRUCT_SIZE,
          "a struct_size this build does not know is refused");

    /* An undefined policy is an error, never a silent fallback to Error. */
    info.struct_size = (uint32_t)sizeof info;
    CHECK(tft_plan_at_extrapolating(plan, 400000000, 3, TFT_LAYOUT_QVEC7_WXYZ,
                                    held, &info) == TFT_ERR_BAD_ENUM,
          "an undefined policy is not quietly TFT_EXTRAP_ERROR");

    tft_plan_free(plan);
    tft_tree_free(tree);
}

int main(void)
{
    check_version_and_constants();
    check_null_is_never_dereferenced();
    check_struct_size_gate();
    check_round_trip();
    check_domain();
    check_extrapolation();
#if defined(TFT_HAVE_BRIDGE)
    check_bridge();
    check_bridge_prefix();
#endif

    if (failures == 0) {
        printf("tf_tree C ABI smoke: OK (abi %u.%u)\n",
               tft_abi_version_major(), tft_abi_version_minor());
        return 0;
    }
    fprintf(stderr, "tf_tree C ABI smoke: %d failure(s)\n", failures);
    return 1;
}
