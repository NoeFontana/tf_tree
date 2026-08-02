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

// **A linker-level shim that records where the C ABI was told to write.**
//
// `run.sh` links this file with `-Wl,--wrap=tft_plan_at`, which redirects the
// call sites in this translation unit to `__wrap_tft_plan_at` and leaves the
// real function reachable as `__real_tft_plan_at`. It exists for one assertion
// — `check_at_writes_into_the_returned_object` — and that assertion cannot be
// made any other way: the property is that `Plan::at<T>` hands the ABI the
// address of the object it is about to *return*, and only the callee can see
// the pointer it was given.
//
// **Deliberately not a `--features test-hooks` symbol on the Rust side.** The
// §7 gate-2 benchmark links the same archive, so recording the pointer inside
// `tft_plan_at` would put a store into the hot path that the gate measures. A
// linker wrap is confined to this binary.
//
// The shim forwards unconditionally, so every other test in this file calls
// through it and sees the real function's behaviour.
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
static_assert(tf_tree::layout_of<tf_tree::Quat7Twist6>::value == TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
              "");

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
/// **The §3.6 ABI check must actually have run, before `main`.**
///
/// It did not, for the whole of this file's first life: it lived in a
/// function-local static called only from `Tree::open()`, which is
/// `#ifdef TFT_HAVE_SHM` — a macro nothing in the build defines. So the check
/// was unreachable in every configuration this suite compiles, and no test
/// noticed, because no test asked.
///
/// Mutant: remove `inline const AbiCheck abi_check_instance{};` from the header
/// ⇒ this fails. Mutant: put it back behind `#ifdef TFT_HAVE_SHM` ⇒ also fails.
static void check_abi_guard_ran()
{
    CHECK(tf_tree::detail::abi_check_ran,
          "the §3.6 ABI check did not run; a mismatched ABI would go undetected");
}

/// **The no-exceptions `expected` must not touch the error machinery on
/// success.**
///
/// It did: `expected<T>` stored a plain `Error`, whose constructor calls
/// `tft_last_error`, so every successful lookup made an extra FFI call and
/// copied 320 bytes. That put the `-fno-exceptions` build at 1.064x the raw C
/// ABI against §7 gate 2's 1.02 — while the exceptions build, which is the one
/// the benchmark compiled, measured 1.002x and reported a pass.
///
/// Constructing an `expected` here and observing that the thread-local error
/// slot is untouched is the cheap structural version of that measurement.
///
/// Mutant: give `expected<T>` a plain `Error error_` again ⇒ the slot is
/// overwritten with TFT_OK and this fails.
#ifdef TF_TREE_NO_EXCEPTIONS
static void check_success_does_not_touch_the_error_slot()
{
    tft_tree* raw = nullptr;
    CHECK(tft_test_tree_create(&raw) == TFT_OK, "fixture");
    tf_tree::Tree tree = tf_tree::Tree::adopt(raw);

    // Provoke a real failure, so the thread-local slot holds something specific.
    auto bad = tree.plan("map", "no_such_frame");
    CHECK(!bad, "the provoking call must fail");

    tft_error before{};
    before.struct_size = static_cast<std::uint32_t>(sizeof(tft_error));
    CHECK(tft_last_error(&before) == TFT_OK, "read the slot");
    CHECK(before.code == TFT_ERR_UNKNOWN_FRAME, "the slot holds the failure");

    // A *successful* wrapper call must not disturb it. (`tft_plan_create`
    // itself clears the slot on entry, so go through a path that succeeds
    // without calling into the library again: construct the expected directly.)
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
    // An unknown discriminant must be 0 in both, so neither can be used to size
    // a buffer for a layout this build does not implement.
    CHECK(tf_tree::payload_bytes(9999) == 0, "unknown layout, header");
    CHECK(tft_layout_size(9999) == 0, "unknown layout, library");
}

/// **`publishable` must agree with the library about which layouts are
/// output-only**, or the `static_assert` in `push` is a compile error for a call
/// that would have worked, or — far worse — absent for one that would not.
///
/// The predicate is a mirror of a decision made in `layout::read`, exactly as
/// `payload_bytes` mirrors `tft_layout_size`, so it gets the same treatment: the
/// header's compile-time answer is checked against the library's run-time one
/// for every layout, rather than trusted.
///
/// The direction that matters is the second `CHECK`. `push` cannot be *called*
/// with an unpublishable layout any more — that is the point of the change — so
/// this drives `tft_publisher_push` directly, which is the only way left to ask
/// the library what it thinks.
///
/// Mutant, run: `publishable` returns `true` for everything (and the two
/// negative `static_assert`s above are removed, or they fail to compile first)
/// ⇒ two failures, both "the header says publishable but the library refuses
/// the layout" — one for each output-only layout. Reverse mutant: teach
/// `layout::read` to accept the twist layout ⇒ the `else` arm fires instead.
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

    // Big enough for the widest payload, and a valid identity pose for the
    // layouts that will actually read it — so a refusal is about the layout and
    // not about the bytes.
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
// The C ABI writes into the object `at<T>` returns — no second copy
// ---------------------------------------------------------------------------

#if defined(TF_TREE_WRAP_PLAN_AT) && defined(TF_TREE_HAS_EIGEN)
/// **`Plan::at<T>` must hand `tft_plan_at` the address of the object it
/// returns**, in both error modes.
///
/// This is §7 gate criterion 2 pinned without a stopwatch. When it does not
/// hold, `out` is an ordinary local: the ABI writes the payload into it and the
/// compiler copies the whole `expected<T>` into the caller afterwards — 128
/// bytes of `Eigen::Isometry3d` in eight `movaps` pairs, plus a 328-byte
/// `memcpy` of the `optional<Error>` storage, which is trivially copyable and
/// so gets copied whether it is engaged or not. Per successful lookup.
///
/// The cause is that NRVO is **all-or-nothing per function**: the compiler
/// wants one automatic variable that every `return` names, and gives up for the
/// whole function when another `return` exists. `at` used to have a second one
/// on the failure path (`TF_TREE_FAIL(s)`, i.e. `return Error(s);`). The
/// exceptions build fails by `throw`, which is not a `return` — which is the
/// whole of the asymmetry §0.0 had recorded as unexplained.
///
/// This check is compiled only into the four `--wrap` rows of `just cpp-check`
/// — g++ and clang++ × both error modes — because `-Wl,--wrap` is a GNU-ld/lld
/// option and the eight §6.2 matrix rows are a *portability* gate that should
/// not require a linker family. `run.sh`'s `WRAP` note has the argument.
///
/// Mutant (applied, and the results below are measured, not predicted): put
/// `TF_TREE_FAIL(s)` back in `Plan::at`. **Both `--wrap no-exceptions` rows
/// fail** — g++ and clang++ — and both `--wrap exceptions` rows keep passing,
/// which is the asymmetry itself reproduced as a test result rather than as a
/// timing. `just cpp-bench` moves from an interleaved 1.003x to 1.035x against
/// a 1.02 gate over the same 11-round A/B.
static void check_at_writes_into_the_returned_object()
{
    tft_tree* raw = nullptr;
    CHECK(tft_test_tree_create(&raw) == TFT_OK, "fixture");
    tf_tree::Tree tree = tf_tree::Tree::adopt(raw);
    auto plan_r = tree.plan("map", "sensor");
    CHECK_R(plan_r, "plan map <- sensor");
    tf_tree::Plan plan = std::move(VALUE_OF(plan_r));

    probe_last_out = nullptr;
    // The declaration below *is* the result object of the call — not a copy of
    // one. Routing it through a helper that takes the result by value would
    // introduce exactly the copy under test and make the check vacuous.
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

// ---------------------------------------------------------------------------
// TF_TREE_FAIL_INTO behaves the same way in both error modes
// ---------------------------------------------------------------------------

static bool fail_into_fell_through = false;

/// Shaped exactly like `Plan::at`: fail through the macro, then `return out;`.
/// The assignment in between stands for whatever a future `Plan::at` might grow
/// there — a release, an unlock, a counter — and must never run.
///
/// `out` is a bare identifier because `TF_TREE_FAIL_INTO`'s contract 2 requires
/// one; a probe that passed an expression would be testing a use the macro does
/// not support.
static tf_tree::result<double> fail_into_probe()
{
    tf_tree::result<double> out = tf_tree::make_result<double>();
    TF_TREE_FAIL_INTO(out, TFT_ERR_BAD_HANDLE);
    fail_into_fell_through = true;
    return out;
}

/// **`TF_TREE_FAIL_INTO` must leave the function immediately, in both error
/// modes** — contract 1 in `tf_tree.hpp`.
///
/// The macro's two expansions do visibly different things: one assigns an
/// `Error` into the return object, the other throws. Only the observable
/// control flow has to match, and nothing else in the build enforces that. A
/// version that assigned and *fell through* compiles clean in both modes, so
/// `TF_TREE_FAIL_INTO(out, s); unlock(); return out;` would run `unlock()`
/// under exceptions and skip it under `-fno-exceptions`. Silently, in a header
/// shipped to callers who compile it either way.
///
/// Mutant (applied): drop the `return out;` from the `-fno-exceptions`
/// expansion, leaving the bare assignment. **The six `-fno-exceptions` rows of
/// `just cpp-check` fail** on "fell through", g++ and clang++, C++17, C++20 and
/// `--wrap`; the seven exceptions rows pass. That split is the defect itself: a
/// bug that exists in one error mode only is exactly what this file is here to
/// surface.
///
/// `check_at_writes_into_the_returned_object` does **not** catch it. `Plan::at`
/// has nothing between the macro and its `return`, so falling through there is
/// harmless today and the payload still lands in the return slot. This test
/// guards the macro; that one guards its one current caller.
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

    // **Derivatives reach C++ by type**, which is what makes them reach C++ at
    // all: `layout_of<Quat7Twist6>` is the only thing that lets the templated
    // `at`/`at_many` name the layout. Its pose half must be the `Quat7` batch's
    // bytes — the tail is the only thing that is new.
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

        // ...and the scalar form agrees with the batch, which is the claim the
        // layout makes about being one computation and not two.
        auto one_r = plan.at<tf_tree::Quat7Twist6>(stamps[7]);
        CHECK_R(one_r, "at<Quat7Twist6>");
        const tf_tree::Quat7Twist6 one = VALUE_OF(one_r);
        CHECK(one.vx == d_out[7].vx && one.wz == d_out[7].wz && one.qw == d_out[7].qw,
              "at<Quat7Twist6> and at_many<Quat7Twist6> must agree");
    }

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

/// Overwrite the thread-local error slot with a failure that is *not* the one
/// under test, and return the message it wrote there.
///
/// It has to be a real failure. `tft_last_error` reads a slot only a *failing*
/// call writes, so a successful call — or a pure one like `tft_layout_size`,
/// which reads no state and writes none — leaves the slot byte-for-byte as it
/// was and cannot tell a copy from a view. `TFT_ERR_BAD_HANDLE` from a null
/// plan is the cheapest real one.
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

/// **An `Error` keeps the detail of the failure that produced it**, in both
/// error modes, even after a later `tf_tree` call has overwritten the
/// thread-local slot it came from.
///
/// This is what pays for `expected<T>` carrying a whole `tft_error` — see the
/// note on `expected` in `tf_tree.hpp` — so it is the assertion that has to be
/// load-bearing rather than decorative.
///
/// The probe is `message()`, **not** `code()`. `Error::fetch` overwrites `code`
/// with the status the failing call actually returned, precisely so a stale
/// slot cannot misreport it; that makes `code()` correct whether or not
/// anything was copied, and therefore useless here. The message comes from the
/// slot and nowhere else.
///
/// Mutant (applied): give `Error::message()` the body
/// `{ static tft_error live{}; live.struct_size = sizeof(live);
/// tft_last_error(&live); return live.message; }` — the "view, not a copy"
/// implementation. **Every row of `just cpp-check` fails** — all thirteen — on
/// the "must be a copy, not a view" line. The previous version of this test
/// used `tft_layout_size` as the intervening call and checked `code()`; it
/// survived that mutant in every row, because neither half of it could
/// discriminate.
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
