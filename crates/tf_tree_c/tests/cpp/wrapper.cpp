// The C++ wrapper, exercised (docs/PHASE4.md §4, §6.2). Built by `just cpp-check`
// with {gcc, clang} x {exceptions, -fno-exceptions} at C++17 and C++20. No test
// framework, by design.

#include "tf_tree.hpp"

#include <cmath>
#include <cstdio>
#include <cstring>
#include <vector>

// Test-hooks fixture constructor, absent from the shipped headers.
extern "C" {
tft_status tft_test_publishable_tree_create(tft_tree** out);
tft_status tft_test_tree_create(tft_tree** out);
tft_status tft_test_domain_tree_create(std::uint8_t domain, tft_tree** out);
}

// Linker shim (`-Wl,--wrap=tft_plan_at`, see `run.sh`) recording where the ABI is
// told to write, for `check_at_writes_into_the_returned_object`. It is not a Rust
// test-hooks symbol because that would put a store in the gate-2 hot path.
#ifdef TF_TREE_WRAP_PLAN_AT
extern "C" {
tft_status __real_tft_plan_at(const tft_plan* plan, std::int64_t stamp, tft_layout layout,
                              void* out);
static const void* probe_last_out = nullptr;
tft_status __wrap_tft_plan_at(const tft_plan* plan, std::int64_t stamp, tft_layout layout,
                              void* out)
{
    probe_last_out = out;
    return __real_tft_plan_at(plan, stamp, layout, out);
}
}
#endif

static int failures = 0;

#define CHECK(cond, msg)                                                      \
    do {                                                                      \
        if (!(cond)) {                                                        \
            std::fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, msg); \
            failures++;                                                       \
        }                                                                     \
    } while (0)

// `CHECK_R` tests an already-computed result; `CHECK_CALL` evaluates its
// expression exactly once. In exceptions mode an uncaught failure terminates the
// binary, so reaching the next line is the assertion.
#ifdef TF_TREE_NO_EXCEPTIONS
#define VALUE_OF(expr) (*(expr))
#define CHECK_R(expr, msg) CHECK(static_cast<bool>(expr), msg)
#define CHECK_CALL(expr, msg) CHECK(static_cast<bool>(expr), msg)
#else
#define VALUE_OF(expr) (expr)
#define CHECK_R(expr, msg) ((void)(msg))
#define CHECK_CALL(expr, msg)                                                 \
    do {                                                                      \
        (expr);                                                               \
        (void)(msg);                                                          \
    } while (0)
#endif


static_assert(tf_tree::layout_of<tf_tree::Quat7>::value == TFT_LAYOUT_QVEC7_WXYZ, "");
static_assert(tf_tree::layout_of<tf_tree::Mat4Row>::value == TFT_LAYOUT_MAT4_ROW, "");
static_assert(tf_tree::layout_of<tf_tree::Quat7Twist6>::value == TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
              "");

#ifdef TF_TREE_HAS_EIGEN
// The §4.2 trap: `Eigen::Isometry3d` is column-major, so `MAT4_ROW` would return
// the transpose. This is why layouts are chosen by type.
static_assert(tf_tree::layout_of<Eigen::Isometry3d>::value == TFT_LAYOUT_MAT4_COL,
              "Eigen::Isometry3d is column-major; MAT4_ROW would be its inverse");
#endif

#ifdef TF_TREE_HAS_SOPHUS
// Eigen/Sophus store quaternions (x, y, z, w).
static_assert(tf_tree::layout_of<Sophus::SE3d>::value == TFT_LAYOUT_QVEC7_XYZW,
              "Sophus stores x,y,z,w; QVEC7_WXYZ would be a different rotation");
#endif



/// `tf_tree::payload_bytes` agrees with `tft_layout_size` for every layout.
/// The §3.6 ABI check has run before `main`.
static void check_abi_guard_ran()
{
    CHECK(tf_tree::detail::abi_check_ran,
          "the §3.6 ABI check did not run; a mismatched ABI would go undetected");
}

/// The no-exceptions `expected` does not touch the error machinery on success
/// (§7 gate 2).
#ifdef TF_TREE_NO_EXCEPTIONS
static void check_success_does_not_touch_the_error_slot()
{
    tft_tree* raw = nullptr;
    CHECK(tft_test_tree_create(&raw) == TFT_OK, "fixture");
    tf_tree::Tree tree = tf_tree::Tree::adopt(raw);

    // Provoke a real failure so the slot holds something specific.
    auto bad = tree.plan("map", "no_such_frame");
    CHECK(!bad, "the provoking call must fail");

    tft_error before{};
    before.struct_size = static_cast<std::uint32_t>(sizeof(tft_error));
    CHECK(tft_last_error(&before) == TFT_OK, "read the slot");
    CHECK(before.code == TFT_ERR_UNKNOWN_FRAME, "the slot holds the failure");

    // A successful wrapper call must not disturb it; construct the expected directly.
    {
        const tf_tree::expected<tf_tree::Quat7> ok{tf_tree::Quat7{1, 0, 0, 0, 0, 0, 0}};
        CHECK(static_cast<bool>(ok), "a value-constructed expected is a success");
    }
    tft_error after{};
    after.struct_size = static_cast<std::uint32_t>(sizeof(tft_error));
    CHECK(tft_last_error(&after) == TFT_OK, "read the slot again");
    CHECK(after.code == before.code,
          "constructing a successful expected must not call into the error machinery");
}
#endif

static void check_payload_sizes_agree()
{
    const tft_layout all[] = {TFT_LAYOUT_QVEC7_WXYZ,        TFT_LAYOUT_QVEC7_XYZW,
                              TFT_LAYOUT_MAT4_COL,          TFT_LAYOUT_MAT4_ROW,
                              TFT_LAYOUT_AFFINE12_ROW_F32,  TFT_LAYOUT_QVEC7_WXYZ_TWIST6};
    for (tft_layout l : all) {
        CHECK(tf_tree::payload_bytes(l) == tft_layout_size(l),
              "the header's compile-time payload size disagrees with the library's");
        CHECK(tft_layout_size(l) != 0, "every listed layout must be one the library defines");
    }
    // An unknown discriminant is 0 in both.
    CHECK(tf_tree::payload_bytes(9999) == 0, "unknown layout, header");
    CHECK(tft_layout_size(9999) == 0, "unknown layout, library");
}

/// `publishable` agrees with the library about which layouts are output-only,
/// checked against `tft_publisher_push` directly.
static void check_publishable_agrees_with_the_library()
{
    static_assert(!tf_tree::publishable(TFT_LAYOUT_QVEC7_WXYZ_TWIST6),
                  "a twist is derived from the arena, never published into it");
    static_assert(!tf_tree::publishable(TFT_LAYOUT_AFFINE12_ROW_F32),
                  "the f32 affine encoding is an output encoding");
    static_assert(tf_tree::publishable(TFT_LAYOUT_QVEC7_WXYZ), "the canonical layout publishes");

    tft_tree* raw = nullptr;
    CHECK(tft_test_publishable_tree_create(&raw) == TFT_OK, "fixture");
    tf_tree::Tree tree = tf_tree::Tree::adopt(raw);
    auto pub_r = tree.claim("robot", "world");
    CHECK_R(pub_r, "claim");
    tf_tree::Publisher pub = std::move(VALUE_OF(pub_r));

    // A valid identity pose, so a refusal is about the layout.
    double buf[16] = {};
    buf[0] = 1.0;  // qw for the QVEC7 orders
    const tft_layout all[] = {TFT_LAYOUT_QVEC7_WXYZ,       TFT_LAYOUT_QVEC7_XYZW,
                              TFT_LAYOUT_MAT4_COL,         TFT_LAYOUT_MAT4_ROW,
                              TFT_LAYOUT_AFFINE12_ROW_F32, TFT_LAYOUT_QVEC7_WXYZ_TWIST6};
    std::int64_t stamp = 1;
    for (tft_layout l : all) {
        const tft_status s = tft_publisher_push(pub.raw(), stamp++, l, buf);
        if (tf_tree::publishable(l)) {
            CHECK(s != TFT_ERR_BAD_ENUM,
                  "the header says publishable but the library refuses the layout");
        } else {
            CHECK(s == TFT_ERR_BAD_ENUM,
                  "the header says unpublishable but the library accepts the layout");
        }
    }
}

#ifdef TF_TREE_HAS_EIGEN
/// The premise behind `raw_writable<Eigen::Isometry3d>`: its storage is a plain
/// `double` array at offset 0.
static void check_eigen_storage_premise()
{
    Eigen::Isometry3d iso = Eigen::Isometry3d::Identity();
    CHECK(static_cast<const void*>(iso.matrix().data()) == static_cast<const void*>(&iso),
          "Eigen::Isometry3d's storage must start at offset 0 for the raw write to be valid");
    CHECK(sizeof(Eigen::Isometry3d) == 128, "and be exactly the payload, so an array is packed");
    // An array is contiguous, which makes the batch path zero-copy.
    Eigen::Isometry3d arr[3];
    const auto* p0 = reinterpret_cast<const unsigned char*>(&arr[0]);
    const auto* p1 = reinterpret_cast<const unsigned char*>(&arr[1]);
    CHECK(static_cast<std::size_t>(p1 - p0) == sizeof(Eigen::Isometry3d),
          "array elements must be exactly sizeof apart");
    CHECK(static_cast<std::size_t>(p1 - p0) == tft_layout_size(TFT_LAYOUT_MAT4_COL),
          "and that must equal the layout's payload, or the batch needs a stride");
}
#endif

static void check_read_path()
{
    tf_tree::Tree tree;
    {
        tft_tree* raw = nullptr;
        CHECK(tft_test_tree_create(&raw) == TFT_OK, "fixture");
        tree = tf_tree::Tree::adopt(raw);
    }
    CHECK(static_cast<bool>(tree), "tree handle");

    auto plan_r = tree.plan("map", "sensor");
    CHECK_R(plan_r, "plan map <- sensor");
    tf_tree::Plan plan = std::move(VALUE_OF(plan_r));

    // All three types describe the same transform.
    const std::int64_t t = 300000000;

    auto q_r = plan.at<tf_tree::Quat7>(t);
    CHECK_R(q_r, "at<Quat7>");
    const tf_tree::Quat7 q = VALUE_OF(q_r);
    CHECK(std::fabs(q.qw * q.qw + q.qx * q.qx + q.qy * q.qy + q.qz * q.qz - 1.0) < 1e-12,
          "a unit quaternion");

    auto m_r = plan.at<tf_tree::Mat4Row>(t);
    CHECK_R(m_r, "at<Mat4Row>");
    const tf_tree::Mat4Row m = VALUE_OF(m_r);
    // Row-major: the translation is the last column of each row.
    CHECK(std::fabs(m.m[3] - q.tx) < 1e-12, "row-major tx");
    CHECK(std::fabs(m.m[7] - q.ty) < 1e-12, "row-major ty");
    CHECK(std::fabs(m.m[11] - q.tz) < 1e-12, "row-major tz");
    CHECK(std::fabs(m.m[15] - 1.0) < 1e-12, "homogeneous bottom-right");

#ifdef TF_TREE_HAS_EIGEN
    auto e_r = plan.at<Eigen::Isometry3d>(t);
    CHECK_R(e_r, "at<Eigen::Isometry3d>");
    const Eigen::Isometry3d iso = VALUE_OF(e_r);
    // Eigen indexes (row, col) whatever the storage order.
    CHECK(std::fabs(iso.translation().x() - q.tx) < 1e-12, "eigen tx");
    CHECK(std::fabs(iso.translation().y() - q.ty) < 1e-12, "eigen ty");
    CHECK(std::fabs(iso.translation().z() - q.tz) < 1e-12, "eigen tz");
    // ...and the rotation matches the row-major matrix element for element.
    for (int r = 0; r < 3; ++r) {
        for (int c = 0; c < 3; ++c) {
            CHECK(std::fabs(iso.linear()(r, c) - m.m[r * 4 + c]) < 1e-12,
                  "eigen rotation vs row-major");
        }
    }
    CHECK(std::fabs(iso.linear().determinant() - 1.0) < 1e-12, "a rotation, not a reflection");
#endif
}


#if defined(TF_TREE_WRAP_PLAN_AT) && defined(TF_TREE_HAS_EIGEN)
/// `Plan::at<T>` hands `tft_plan_at` the address of the object it returns, in
/// both error modes (§7 gate 2 without a stopwatch). Compiled only into the
/// `--wrap` rows of `just cpp-check`; see `run.sh`.
static void check_at_writes_into_the_returned_object()
{
    tft_tree* raw = nullptr;
    CHECK(tft_test_tree_create(&raw) == TFT_OK, "fixture");
    tf_tree::Tree tree = tf_tree::Tree::adopt(raw);
    auto plan_r = tree.plan("map", "sensor");
    CHECK_R(plan_r, "plan map <- sensor");
    tf_tree::Plan plan = std::move(VALUE_OF(plan_r));

    probe_last_out = nullptr;
    // This declaration is the result object; a by-value helper would be vacuous.
#ifdef TF_TREE_NO_EXCEPTIONS
    const auto r = plan.at<Eigen::Isometry3d>(300000000);
    CHECK(static_cast<bool>(r), "the lookup must succeed, or there is nothing to check");
    const void* returned = static_cast<const void*>(&*r);
#else
    const Eigen::Isometry3d r = plan.at<Eigen::Isometry3d>(300000000);
    const void* returned = static_cast<const void*>(&r);
#endif
    CHECK(probe_last_out != nullptr, "the shim did not fire; -Wl,--wrap=tft_plan_at is missing");
    CHECK(probe_last_out == returned,
          "at<T> wrote into a temporary and the payload was copied out of it; "
          "see TF_TREE_FAIL_INTO in tf_tree.hpp");
}
#endif


static bool fail_into_fell_through = false;

/// Shaped like `Plan::at`: fail through the macro, then `return out;`. The
/// assignment between must never run; `out` is a bare identifier per the macro's
/// contract 2.
static tf_tree::result<double> fail_into_probe()
{
    tf_tree::result<double> out = tf_tree::make_result<double>();
    TF_TREE_FAIL_INTO(out, TFT_ERR_BAD_HANDLE);
    fail_into_fell_through = true;
    return out;
}

/// `TF_TREE_FAIL_INTO` leaves the function immediately in both error modes
/// (contract 1 in `tf_tree.hpp`); `check_at_writes_into_the_returned_object`
/// does not catch a fall-through.
static void check_fail_into_leaves_the_function()
{
    fail_into_fell_through = false;
#ifdef TF_TREE_NO_EXCEPTIONS
    const tf_tree::result<double> r = fail_into_probe();
    CHECK(!r, "the probe must report failure");
    CHECK(r.error().code() == TFT_ERR_BAD_HANDLE, "carrying the status it was handed");
#else
    bool threw = false;
    try {
        const tf_tree::result<double> r = fail_into_probe();
        (void)r;
    } catch (const tf_tree::Error& e) {
        threw = true;
        CHECK(e.code() == TFT_ERR_BAD_HANDLE, "carrying the status it was handed");
    }
    CHECK(threw, "the probe must report failure");
#endif
    CHECK(!fail_into_fell_through,
          "TF_TREE_FAIL_INTO fell through to the next statement; the two error "
          "modes no longer agree on control flow");
}


static void check_batch()
{
    tft_tree* raw = nullptr;
    CHECK(tft_test_tree_create(&raw) == TFT_OK, "fixture");
    tf_tree::Tree tree = tf_tree::Tree::adopt(raw);
    auto plan_r = tree.plan("map", "sensor");
    CHECK_R(plan_r, "plan");
    tf_tree::Plan plan = std::move(VALUE_OF(plan_r));

    const std::size_t n = 64;
    std::vector<std::int64_t> stamps(n);
    for (std::size_t i = 0; i < n; ++i) {
        stamps[i] = static_cast<std::int64_t>(10000000 + i * 5000000);
    }

    std::vector<tf_tree::Quat7> out;
    CHECK_CALL(plan.at_many(stamps, out), "at_many<Quat7>");
    CHECK(out.size() == n, "sized from the input");
    for (std::size_t i = 0; i < n; ++i) {
        const auto& e = out[i];
        CHECK(std::fabs(e.qw * e.qw + e.qx * e.qx + e.qy * e.qy + e.qz * e.qz - 1.0) < 1e-12,
              "every element is a unit quaternion");
    }
    // Every element differs from its neighbour.
    CHECK(std::fabs(out[0].tx - out[n - 1].tx) > 1e-9,
          "the batch must vary across elements, not repeat the first");

    // Derivatives by type: the pose half is the `Quat7` batch's bytes.
    {
        std::vector<tf_tree::Quat7Twist6> d_out;
        CHECK_CALL(plan.at_many(stamps, d_out), "at_many<Quat7Twist6>");
        CHECK(d_out.size() == n, "sized from the input");
        bool moving = false;
        for (std::size_t i = 0; i < n; ++i) {
            CHECK(d_out[i].qw == out[i].qw && d_out[i].tx == out[i].tx,
                  "the pose half must be the Quat7 batch, bit for bit");
            if (std::fabs(d_out[i].vx) > 1e-9 || std::fabs(d_out[i].wz) > 1e-9) {
                moving = true;
            }
        }
        // Non-vacuity: six zeros would satisfy every assertion above.
        CHECK(moving, "the fixture's twist is zero; this would pass against a stub");

        // ...and the scalar form agrees with the batch.
        auto one_r = plan.at<tf_tree::Quat7Twist6>(stamps[7]);
        CHECK_R(one_r, "at<Quat7Twist6>");
        const tf_tree::Quat7Twist6 one = VALUE_OF(one_r);
        CHECK(one.vx == d_out[7].vx && one.wz == d_out[7].wz && one.qw == d_out[7].qw,
              "at<Quat7Twist6> and at_many<Quat7Twist6> must agree");
    }

#ifdef TF_TREE_HAS_EIGEN
    // §4.2 zero-copy: `sizeof(Eigen::Isometry3d)` is the payload and the stride.
    std::vector<Eigen::Isometry3d> eigen_out;
    CHECK_CALL(plan.at_many(stamps, eigen_out), "at_many<Eigen::Isometry3d>");
    CHECK(eigen_out.size() == n, "sized");
    for (std::size_t i = 0; i < n; ++i) {
        CHECK(std::fabs(eigen_out[i].translation().x() - out[i].tx) < 1e-12,
              "the Eigen batch agrees with the Quat7 batch");
        CHECK(std::fabs(eigen_out[i].linear().determinant() - 1.0) < 1e-9,
              "every element is a rotation");
    }

    // The write stays inside the array: sentinels either side stand in for guard pages.
    std::vector<Eigen::Isometry3d> guarded(n + 2);
    const Eigen::Isometry3d sentinel = Eigen::Isometry3d(Eigen::Translation3d(-999.0, -999.0, -999.0));
    guarded[0] = sentinel;
    guarded[n + 1] = sentinel;
    CHECK_CALL(plan.at_many(stamps.data(), n, guarded.data() + 1), "guarded at_many");
    CHECK(guarded[0].translation().x() == -999.0, "wrote before the start of the range");
    CHECK(guarded[n + 1].translation().x() == -999.0, "wrote past the end of the range");
#endif
}


#ifdef TF_TREE_HAS_SOPHUS
static void check_sophus()
{
    // Report the hazard: it depends on the user's vectorization flags.
    std::printf("  Sophus::SE3d: sizeof=%zu payload=56 direct=%s\n", sizeof(Sophus::SE3d),
                tf_tree::detail::sophus_is_directly_writable() ? "yes" : "no");
    CHECK(tf_tree::detail::sophus_is_directly_writable(),
          "the strided path requires quaternion-then-translation with no interior padding");

    tft_tree* raw = nullptr;
    CHECK(tft_test_tree_create(&raw) == TFT_OK, "fixture");
    tf_tree::Tree tree = tf_tree::Tree::adopt(raw);
    auto plan_r = tree.plan("map", "sensor");
    CHECK_R(plan_r, "plan");
    tf_tree::Plan plan = std::move(VALUE_OF(plan_r));

    const std::size_t n = 16;
    std::vector<std::int64_t> stamps(n);
    for (std::size_t i = 0; i < n; ++i) {
        stamps[i] = static_cast<std::int64_t>(10000000 + i * 20000000);
    }

    // §4.3: `sizeof(SE3d)` usually exceeds 56, so the wrapper passes it as the stride.
    std::vector<Sophus::SE3d> out;
    CHECK_CALL(plan.at_many(stamps, out), "at_many<Sophus::SE3d>");

    std::vector<tf_tree::Quat7> reference;
    CHECK_CALL(plan.at_many(stamps, reference), "reference batch");
    for (std::size_t i = 0; i < n; ++i) {
        CHECK(std::fabs(out[i].translation().x() - reference[i].tx) < 1e-12, "sophus tx");
        CHECK(std::fabs(out[i].translation().y() - reference[i].ty) < 1e-12, "sophus ty");
        CHECK(std::fabs(out[i].translation().z() - reference[i].tz) < 1e-12, "sophus tz");
        // Sophus normalizes on construction; compare on the canonical hemisphere.
        const auto& sq = out[i].so3().unit_quaternion();
        const double sign = (sq.w() * reference[i].qw < 0.0) ? -1.0 : 1.0;
        CHECK(std::fabs(sign * sq.w() - reference[i].qw) < 1e-12, "sophus qw");
        CHECK(std::fabs(sign * sq.x() - reference[i].qx) < 1e-12, "sophus qx");
        CHECK(std::fabs(sign * sq.y() - reference[i].qy) < 1e-12, "sophus qy");
        CHECK(std::fabs(sign * sq.z() - reference[i].qz) < 1e-12, "sophus qz");
    }
    CHECK(std::fabs(out[0].translation().x() - out[n - 1].translation().x()) > 1e-9,
          "the batch must vary, or a stride bug that repeats element 0 would pass");
}
#endif


/// Overwrite the thread-local error slot with a real failure (a null plan gives
/// `TFT_ERR_BAD_HANDLE`) and return the message written.
static const char* clobber_the_error_slot()
{
    double scratch[16] = {};
    const tft_status s = tft_plan_at(nullptr, 0, TFT_LAYOUT_MAT4_COL, scratch);
    CHECK(s == TFT_ERR_BAD_HANDLE, "the clobbering call must itself fail");
    static tft_error e{};
    e.struct_size = static_cast<std::uint32_t>(sizeof(tft_error));
    CHECK(tft_last_error(&e) == TFT_OK, "read the slot back");
    CHECK(e.code == TFT_ERR_BAD_HANDLE, "the slot now holds the clobbering failure");
    return e.message;
}

/// An `Error` keeps the detail of its failure after a later call overwrites the
/// thread-local slot. The probe is `message()`, not `code()`, which `Error::fetch`
/// sets from the returned status.
static void check_errors()
{
    tft_tree* raw = nullptr;
    CHECK(tft_test_tree_create(&raw) == TFT_OK, "fixture");
    tf_tree::Tree tree = tf_tree::Tree::adopt(raw);

#ifdef TF_TREE_NO_EXCEPTIONS
    auto bad = tree.plan("map", "no_such_frame");
    CHECK(!bad, "an unknown frame must fail");
    CHECK(bad.error().code() == TFT_ERR_UNKNOWN_FRAME, "and say why");
    CHECK(std::strlen(bad.error().message()) > 0, "with a message");
    char first[TFT_MESSAGE_LEN];
    std::strncpy(first, bad.error().message(), sizeof(first) - 1);
    first[sizeof(first) - 1] = '\0';

    const char* clobber = clobber_the_error_slot();
    CHECK(std::strcmp(first, clobber) != 0,
          "the two failures must have different messages, or this proves nothing");
    CHECK(std::strcmp(bad.error().message(), first) == 0, "the detail must be a copy, not a view");
#else
    bool threw = false;
    try {
        (void)tree.plan("map", "no_such_frame");
    } catch (const tf_tree::Error& e) {
        threw = true;
        CHECK(e.code() == TFT_ERR_UNKNOWN_FRAME, "the right code");
        CHECK(std::strlen(e.what()) > 0, "what() is populated");
        CHECK(std::strlen(e.message()) > 0, "message() is populated");
        char first[TFT_MESSAGE_LEN];
        std::strncpy(first, e.message(), sizeof(first) - 1);
        first[sizeof(first) - 1] = '\0';

        const char* clobber = clobber_the_error_slot();
        CHECK(std::strcmp(first, clobber) != 0,
              "the two failures must have different messages, or this proves nothing");
        CHECK(std::strcmp(e.message(), first) == 0, "the detail must be a copy, not a view");
    }
    CHECK(threw, "an unknown frame must throw");
#endif
}


/// `Tree::plan_in_domain` is how C++ reads a simulated tree (`0038`): the
/// configured tag reads, the default is refused with `TFT_ERR_TIME_DOMAIN` (§5.5).
static void check_domain()
{
    tft_tree* raw = nullptr;
    CHECK(tft_test_domain_tree_create(1, &raw) == TFT_OK, "fixture");
    tf_tree::Tree tree = tf_tree::Tree::adopt(raw);

    auto plan_r = tree.plan_in_domain("map", "odom", 1);
    CHECK_R(plan_r, "plan_in_domain map <- odom");
    tf_tree::Plan plan = std::move(VALUE_OF(plan_r));

    auto q_r = plan.at<tf_tree::Quat7>(150000000);
    CHECK_R(q_r, "at<Quat7> on a tag-1 arena");
    const tf_tree::Quat7 q = VALUE_OF(q_r);
    CHECK(q.tx > 0.7 && q.tx < 0.8, "a tagged plan reads a transform");

    // A static route has no domain to disagree with, in either wrapper.
    auto st_r = tree.plan_in_domain("odom", "sensor", 7);
    CHECK_R(st_r, "a static route takes any domain");
    (void)VALUE_OF(st_r);

#ifdef TF_TREE_NO_EXCEPTIONS
    auto bad = tree.plan("map", "odom");
    CHECK(!bad, "domain 0 cannot read a tag-1 arena");
    CHECK(bad.error().code() == TFT_ERR_TIME_DOMAIN, "and says which agreement broke");
#else
    bool threw = false;
    try {
        (void)tree.plan("map", "odom");
    } catch (const tf_tree::Error& e) {
        threw = true;
        CHECK(e.code() == TFT_ERR_TIME_DOMAIN, "and says which agreement broke");
    }
    CHECK(threw, "domain 0 cannot read a tag-1 arena");
#endif
}


/// `Plan::at_extrapolating` returns the pose and the distance as one value
/// (`0039`); `ConstantTwist != Hold` shows the policy reached the engine.
static void check_extrapolation()
{
    tft_tree* raw = nullptr;
    CHECK(tft_test_domain_tree_create(1, &raw) == TFT_OK, "fixture");
    tf_tree::Tree tree = tf_tree::Tree::adopt(raw);

    auto plan_r = tree.plan_in_domain("map", "sensor", 1);
    CHECK_R(plan_r, "plan_in_domain map <- sensor");
    tf_tree::Plan plan = std::move(VALUE_OF(plan_r));

    // 32 samples 10 ms apart: 310 ms is the newest.
    auto hold_r = plan.at_extrapolating<tf_tree::Quat7>(400000000, TFT_EXTRAP_HOLD);
    CHECK_R(hold_r, "Hold answers past the newest sample");
    const tf_tree::Extrapolated<tf_tree::Quat7> hold = VALUE_OF(hold_r);
    CHECK(hold.by_ns == 90000000, "the distance comes back with the pose");
    CHECK(hold.edge != TFT_INVALID_ID, "and names the edge that ran out of data");

    auto twist_r =
        plan.at_extrapolating<tf_tree::Quat7>(400000000, TFT_EXTRAP_CONSTANT_TWIST);
    CHECK_R(twist_r, "ConstantTwist answers past the newest sample");
    const tf_tree::Extrapolated<tf_tree::Quat7> twist = VALUE_OF(twist_r);
    CHECK(twist.by_ns == hold.by_ns, "the distance does not depend on the policy");
    CHECK(twist.pose.tx != hold.pose.tx,
          "ConstantTwist must differ from Hold, or the policy is being ignored");

    // Inside the window the answer is interpolated, and says so.
    auto near_r = plan.at_extrapolating<tf_tree::Quat7>(150000000, TFT_EXTRAP_HOLD);
    CHECK_R(near_r, "an in-window stamp answers under any policy");
    CHECK(VALUE_OF(near_r).by_ns == 0, "and reports that nothing was invented");

#ifdef TF_TREE_NO_EXCEPTIONS
    auto refused = plan.at_extrapolating<tf_tree::Quat7>(400000000, TFT_EXTRAP_ERROR);
    CHECK(!refused, "TFT_EXTRAP_ERROR refuses, exactly as at() does");
    CHECK(refused.error().code() == TFT_ERR_EXTRAPOLATION, "and says which refusal");
#else
    bool threw = false;
    try {
        (void)plan.at_extrapolating<tf_tree::Quat7>(400000000, TFT_EXTRAP_ERROR);
    } catch (const tf_tree::Error& e) {
        threw = true;
        CHECK(e.code() == TFT_ERR_EXTRAPOLATION, "and says which refusal");
    }
    CHECK(threw, "TFT_EXTRAP_ERROR refuses, exactly as at() does");
#endif
}


static void check_publish()
{
    tft_tree* raw = nullptr;
    CHECK(tft_test_publishable_tree_create(&raw) == TFT_OK, "fixture");
    tf_tree::Tree tree = tf_tree::Tree::adopt(raw);

    auto pub_r = tree.claim("robot", "world");
    CHECK_R(pub_r, "claim");
    tf_tree::Publisher pub = std::move(VALUE_OF(pub_r));

    tf_tree::Quat7 a{1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0};
    tf_tree::Quat7 b{1.0, 0.0, 0.0, 0.0, 4.0, 0.0, 0.0};
    CHECK_CALL(pub.push(0, a), "push t=0");
    CHECK_CALL(pub.push(1000000000, b), "push t=1s");

    auto plan_r = tree.plan("world", "robot");
    CHECK_R(plan_r, "plan");
    tf_tree::Plan plan = std::move(VALUE_OF(plan_r));
    auto mid_r = plan.at<tf_tree::Quat7>(500000000);
    CHECK_R(mid_r, "read back");
    CHECK(std::fabs(VALUE_OF(mid_r).tx - 2.0) < 1e-9, "halfway along a 4 m translation");

    // `release()` gives the edge back and the handle refuses further pushes.
    CHECK_CALL(pub.release(), "release");
#ifdef TF_TREE_NO_EXCEPTIONS
    auto after = pub.push(2000000000, b);
    CHECK(!after && after.error().code() == TFT_ERR_RELEASED, "a released publisher refuses");
#else
    bool threw = false;
    try {
        (void)pub.push(2000000000, b);
    } catch (const tf_tree::Error& e) {
        threw = true;
        CHECK(e.code() == TFT_ERR_RELEASED, "a released publisher refuses");
    }
    CHECK(threw, "a released publisher must refuse");
#endif
}


static void check_raii()
{
    static_assert(!std::is_copy_constructible<tf_tree::Tree>::value, "Tree must not be copyable");
    static_assert(!std::is_copy_assignable<tf_tree::Tree>::value, "Tree must not be copy-assignable");
    static_assert(std::is_move_constructible<tf_tree::Tree>::value, "Tree must be movable");
    static_assert(!std::is_copy_constructible<tf_tree::Plan>::value, "Plan must not be copyable");
    static_assert(!std::is_copy_constructible<tf_tree::Publisher>::value,
                  "Publisher must not be copyable");

    // A moved-from handle is empty, so its destructor is a no-op.
    tft_tree* raw = nullptr;
    CHECK(tft_test_tree_create(&raw) == TFT_OK, "fixture");
    tf_tree::Tree a = tf_tree::Tree::adopt(raw);
    CHECK(static_cast<bool>(a), "a holds the handle");
    tf_tree::Tree b = std::move(a);
    CHECK(static_cast<bool>(b), "b holds it now");
    // NOLINTNEXTLINE(bugprone-use-after-move) — checking the moved-from state
    // is the point of this assertion.
    CHECK(!static_cast<bool>(a), "a must be empty after the move, or this double-frees");

    // A plan outlives its tree; the Arc makes it sound (§3.2).
    {
        tft_tree* r2 = nullptr;
        CHECK(tft_test_tree_create(&r2) == TFT_OK, "fixture");
        tf_tree::Tree t = tf_tree::Tree::adopt(r2);
        auto p_r = t.plan("map", "sensor");
        CHECK_R(p_r, "plan");
        tf_tree::Plan p = std::move(VALUE_OF(p_r));
        t.~Tree();                   // free the tree first, on purpose
        new (&t) tf_tree::Tree();    // and leave the object valid for its real destructor
        // The value is checked, not merely the status.
        const tf_tree::Quat7 v = VALUE_OF(p.at<tf_tree::Quat7>(300000000));
        CHECK(std::fabs(v.qw * v.qw + v.qx * v.qx + v.qy * v.qy + v.qz * v.qz - 1.0) < 1e-12,
              "the plan still evaluates after its tree was freed");
    }
}

int main()
{
    std::printf("tf_tree C++ wrapper: C++%ld, %s, Eigen %s, Sophus %s\n",
                static_cast<long>(__cplusplus),
#ifdef TF_TREE_NO_EXCEPTIONS
                "no-exceptions",
#else
                "exceptions",
#endif
#ifdef TF_TREE_HAS_EIGEN
                "yes",
#else
                "no",
#endif
#ifdef TF_TREE_HAS_SOPHUS
                "yes"
#else
                "no"
#endif
    );

    check_abi_guard_ran();
    check_payload_sizes_agree();
    check_publishable_agrees_with_the_library();
#ifdef TF_TREE_NO_EXCEPTIONS
    check_success_does_not_touch_the_error_slot();
#endif
#ifdef TF_TREE_HAS_EIGEN
    check_eigen_storage_premise();
#endif
    check_read_path();
    check_domain();
    check_extrapolation();
#if defined(TF_TREE_WRAP_PLAN_AT) && defined(TF_TREE_HAS_EIGEN)
    check_at_writes_into_the_returned_object();
#endif
    check_fail_into_leaves_the_function();
    check_batch();
    check_errors();
    check_publish();
    check_raii();
#ifdef TF_TREE_HAS_SOPHUS
    check_sophus();
#endif

    if (failures == 0) {
        std::printf("  OK\n");
        return 0;
    }
    std::fprintf(stderr, "  %d failure(s)\n", failures);
    return 1;
}
