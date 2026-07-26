// The C++ wrapper, exercised. docs/PHASE4.md §4 and §6.2.
//
// Built four ways by `just cpp-check` — {gcc, clang} x {exceptions,
// -fno-exceptions} — and again at C++17 and C++20, because §6.2 asks for both
// standards and both compilers and the error-mode split doubles it.
//
// There is no test framework on purpose. The wrapper is header-only inline
// code, so adding a dependency to test it would put more third-party code in
// the compilation than there is code under test.

#include "tf_tree.hpp"

#include <cmath>
#include <cstdio>
#include <cstring>
#include <vector>

// The fixture constructor is a `--features test-hooks` symbol and deliberately
// absent from the shipped headers. Declared by hand, `extern "C"` so the name
// does not mangle.
extern "C" {
tft_status tft_test_publishable_tree_create(tft_tree** out);
tft_status tft_test_tree_create(tft_tree** out);
}

static int failures = 0;

#define CHECK(cond, msg)                                                      \
    do {                                                                      \
        if (!(cond)) {                                                        \
            std::fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, msg); \
            failures++;                                                       \
        }                                                                     \
    } while (0)

// The two error modes return different types, so the test bodies need a shim.
// Both halves are real assertions, not one real and one nominal:
//
//   * no-exceptions — `CHECK_R` tests the `expected`'s bool, and `CHECK_CALL`
//     evaluates the call and tests its result.
//   * exceptions — a failure *throws*, and nothing here catches except where
//     the throw is what is under test. An uncaught `tf_tree::Error` calls
//     `std::terminate`, so the binary exits non-zero and the test fails. The
//     assertion is that control reaches the next line.
//
// `CHECK_R` takes an already-computed result; `CHECK_CALL` takes an expression
// that must be evaluated exactly once. Folding them into one macro is what
// produced `((expr), true)` and a wall of `-Wunused-value`.
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

// ---------------------------------------------------------------------------
// Layout selection is by type, and it is the right layout
// ---------------------------------------------------------------------------

static_assert(tf_tree::layout_of<tf_tree::Quat7>::value == TFT_LAYOUT_QVEC7_WXYZ, "");
static_assert(tf_tree::layout_of<tf_tree::Mat4Row>::value == TFT_LAYOUT_MAT4_ROW, "");

#ifdef TF_TREE_HAS_EIGEN
// **The trap §4.2 exists to close.** `Eigen::Isometry3d` is column-major, so
// selecting `MAT4_ROW` for it would hand back the transpose — a valid rotation
// pointing the wrong way, with the translation read out of the bottom row as
// zeros. This assert is the whole reason layouts are chosen by type.
static_assert(tf_tree::layout_of<Eigen::Isometry3d>::value == TFT_LAYOUT_MAT4_COL,
              "Eigen::Isometry3d is column-major; MAT4_ROW would be its inverse");
#endif

#ifdef TF_TREE_HAS_SOPHUS
// Eigen/Sophus store quaternions (x, y, z, w) even though the constructor takes
// (w, x, y, z). WXYZ here would be a different, still-unit quaternion.
static_assert(tf_tree::layout_of<Sophus::SE3d>::value == TFT_LAYOUT_QVEC7_XYZW,
              "Sophus stores x,y,z,w; QVEC7_WXYZ would be a different rotation");
#endif

// ---------------------------------------------------------------------------
// The working path
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The two things the header asserts that only a run can check
// ---------------------------------------------------------------------------

/// **`tf_tree::payload_bytes` must agree with `tft_layout_size`.**
///
/// The header needs the sizes at compile time and the C ABI's function is not
/// `constexpr`, so they are written twice. Two sources of truth for a buffer
/// size on an FFI boundary is exactly the shape of bug that ends in an overrun,
/// so this walks every layout and pins them together.
///
/// Mutant: change any entry in the header's `payload_bytes` ⇒ fails here, at
/// the first affected layout.
static void check_payload_sizes_agree()
{
    const tft_layout all[] = {TFT_LAYOUT_QVEC7_WXYZ, TFT_LAYOUT_QVEC7_XYZW, TFT_LAYOUT_MAT4_COL,
                              TFT_LAYOUT_MAT4_ROW, TFT_LAYOUT_AFFINE12_ROW_F32};
    for (tft_layout l : all) {
        CHECK(tf_tree::payload_bytes(l) == tft_layout_size(l),
              "the header's compile-time payload size disagrees with the library's");
        CHECK(tft_layout_size(l) != 0, "every listed layout must be one the library defines");
    }
    // An unknown discriminant must be 0 in both, so neither can be used to size
    // a buffer for a layout this build does not implement.
    CHECK(tf_tree::payload_bytes(9999) == 0, "unknown layout, header");
    CHECK(tft_layout_size(9999) == 0, "unknown layout, library");
}

#ifdef TF_TREE_HAS_EIGEN
/// **The premise behind `raw_writable<Eigen::Isometry3d>`.**
///
/// `Eigen::Isometry3d` is not `std::is_trivially_copyable` — it declares its own
/// copy constructor — so the wrapper opts it in explicitly. What that opt-in
/// actually claims is that the object's storage is a plain `double` array
/// starting at offset 0, which is the property a raw layout write needs and
/// which no standard trait expresses. It is claimed in a comment in the header;
/// this is where it is checked.
///
/// Mutant: were Eigen ever to add a member before the matrix — a vtable, a
/// tag — this fails, and `at<Eigen::Isometry3d>` would otherwise be writing 128
/// bytes over it.
static void check_eigen_storage_premise()
{
    Eigen::Isometry3d iso = Eigen::Isometry3d::Identity();
    CHECK(static_cast<const void*>(iso.matrix().data()) == static_cast<const void*>(&iso),
          "Eigen::Isometry3d's storage must start at offset 0 for the raw write to be valid");
    CHECK(sizeof(Eigen::Isometry3d) == 128, "and be exactly the payload, so an array is packed");
    // An array of them must be contiguous with no gaps, which is what makes the
    // batch path zero-copy rather than merely convenient.
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

    // The same stamp, read through three types. All three must describe the
    // *same* transform — which is the check that catches a layout mapped to the
    // wrong enum, since each goes through a different C-side writer.
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
    // Eigen indexes (row, col) whatever the storage order, so this reads the
    // same three numbers — and would not if the layout enum were wrong.
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

// ---------------------------------------------------------------------------
// Batch writes go straight into the caller's array
// ---------------------------------------------------------------------------

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
    // Every element must differ from its neighbour, or a batch that wrote the
    // first element n times would pass everything above.
    CHECK(std::fabs(out[0].tx - out[n - 1].tx) > 1e-9,
          "the batch must vary across elements, not repeat the first");

#ifdef TF_TREE_HAS_EIGEN
    // §4.2's zero-copy claim: `sizeof(Eigen::Isometry3d)` is the payload, so the
    // stride is the payload and the write is direct.
    std::vector<Eigen::Isometry3d> eigen_out;
    CHECK_CALL(plan.at_many(stamps, eigen_out), "at_many<Eigen::Isometry3d>");
    CHECK(eigen_out.size() == n, "sized");
    for (std::size_t i = 0; i < n; ++i) {
        CHECK(std::fabs(eigen_out[i].translation().x() - out[i].tx) < 1e-12,
              "the Eigen batch agrees with the Quat7 batch");
        CHECK(std::fabs(eigen_out[i].linear().determinant() - 1.0) < 1e-9,
              "every element is a rotation");
    }

    // **The write must stay inside the array.** §6.2 asks for guard pages; a
    // sentinel element either side is the portable equivalent and catches the
    // same bug — a stride that is wrong by any amount walks into it.
    std::vector<Eigen::Isometry3d> guarded(n + 2);
    const Eigen::Isometry3d sentinel = Eigen::Isometry3d(Eigen::Translation3d(-999.0, -999.0, -999.0));
    guarded[0] = sentinel;
    guarded[n + 1] = sentinel;
    CHECK_CALL(plan.at_many(stamps.data(), n, guarded.data() + 1), "guarded at_many");
    CHECK(guarded[0].translation().x() == -999.0, "wrote before the start of the range");
    CHECK(guarded[n + 1].translation().x() == -999.0, "wrote past the end of the range");
#endif
}

// ---------------------------------------------------------------------------
// Sophus — the stride case, §4.3
// ---------------------------------------------------------------------------

#ifdef TF_TREE_HAS_SOPHUS
static void check_sophus()
{
    // The hazard itself: report it, because whether it fires depends on the
    // user's vectorization flags and a reader of the log should know which
    // build they got.
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

    // The whole point of §4.3: sizeof(SE3d) is usually > 56, so a packed write
    // would corrupt every element after the first. The wrapper passes sizeof(T)
    // as the stride, so this must come back correct element by element.
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

// ---------------------------------------------------------------------------
// Errors, in whichever mode this build uses
// ---------------------------------------------------------------------------

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
#else
    bool threw = false;
    try {
        (void)tree.plan("map", "no_such_frame");
    } catch (const tf_tree::Error& e) {
        threw = true;
        CHECK(e.code() == TFT_ERR_UNKNOWN_FRAME, "the right code");
        CHECK(std::strlen(e.what()) > 0, "what() is populated");
        CHECK(std::strlen(e.message()) > 0, "message() is populated");
        // The detail must survive a later call, because it was copied out of
        // the thread-local rather than referenced. A caller that logs an error
        // after doing more work is the normal case, not an exotic one.
        (void)tft_layout_size(TFT_LAYOUT_MAT4_ROW);
        CHECK(e.code() == TFT_ERR_UNKNOWN_FRAME, "the detail must be a copy, not a view");
    }
    CHECK(threw, "an unknown frame must throw");
#endif
}

// ---------------------------------------------------------------------------
// Publishing
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// RAII
// ---------------------------------------------------------------------------

static void check_raii()
{
    static_assert(!std::is_copy_constructible<tf_tree::Tree>::value, "Tree must not be copyable");
    static_assert(!std::is_copy_assignable<tf_tree::Tree>::value, "Tree must not be copy-assignable");
    static_assert(std::is_move_constructible<tf_tree::Tree>::value, "Tree must be movable");
    static_assert(!std::is_copy_constructible<tf_tree::Plan>::value, "Plan must not be copyable");
    static_assert(!std::is_copy_constructible<tf_tree::Publisher>::value,
                  "Publisher must not be copyable");

    // Moving must leave the source empty, so the destructor of a moved-from
    // handle is a no-op rather than a second free. ASan is what would catch the
    // alternative; this catches it without needing ASan.
    tft_tree* raw = nullptr;
    CHECK(tft_test_tree_create(&raw) == TFT_OK, "fixture");
    tf_tree::Tree a = tf_tree::Tree::adopt(raw);
    CHECK(static_cast<bool>(a), "a holds the handle");
    tf_tree::Tree b = std::move(a);
    CHECK(static_cast<bool>(b), "b holds it now");
    // NOLINTNEXTLINE(bugprone-use-after-move) — checking the moved-from state
    // is the point of this assertion.
    CHECK(!static_cast<bool>(a), "a must be empty after the move, or this double-frees");

    // A plan outliving its tree is the natural C++ ordering as well as the C
    // one; the Arc underneath is what makes it sound (§3.2).
    {
        tft_tree* r2 = nullptr;
        CHECK(tft_test_tree_create(&r2) == TFT_OK, "fixture");
        tf_tree::Tree t = tf_tree::Tree::adopt(r2);
        auto p_r = t.plan("map", "sensor");
        CHECK_R(p_r, "plan");
        tf_tree::Plan p = std::move(VALUE_OF(p_r));
        t.~Tree();                   // free the tree first, on purpose
        new (&t) tf_tree::Tree();    // and leave the object valid for its real destructor
        // The value is checked, not merely the status: a plan reading through a
        // freed `Arc` could plausibly return zeros and a status of OK.
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

    check_payload_sizes_agree();
#ifdef TF_TREE_HAS_EIGEN
    check_eigen_storage_premise();
#endif
    check_read_path();
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
