// tf_tree — header-only C++17 wrapper over the C ABI. docs/PHASE4.md §4.
//
// This file is written by hand and is NOT generated: `tf_tree.h` is, and this
// sits on top of it.
//
// # It contains no logic, and that is a rule rather than an aspiration
//
// §4.1: "No logic. Every function is a thin inline over the C ABI. If a
// behaviour needs a branch, it belongs in Rust." The reason is not purity — it
// is that everything below is `inline` in a header the compiler inlines into
// *your* translation unit, where it is invisible to the Rust test suite, to
// Miri, and to ASan-instrumented Rust. Anything that can be wrong here should
// be a compile error, not a runtime one, which is why the interop below is
// almost entirely `static_assert`.
//
// # Two error modes, chosen at include time
//
//   default                     -> throws tf_tree::Error
//   #define TF_TREE_NO_EXCEPTIONS -> returns tf_tree::expected<T, Error>
//
// Robotics shops that build with `-fno-exceptions` are common enough that the
// second is not optional (§4.1). Under `-fno-exceptions` the macro is defined
// for you: `__cpp_exceptions` is absent, and silently compiling `throw` would
// fail in a way that reads as a wrapper bug.
//
// # Layouts are selected by type, not by argument
//
// The C ABI's `tft_layout` is the one place §3.5's two traps live, and a C++
// user should never touch it: `plan.at<Eigen::Isometry3d>(t)` picks
// `MAT4_COL` because that is what `Eigen::Isometry3d` *is*, and getting it
// wrong is not expressible. `layout_of<T>` is the whole mechanism, and adding a
// type means adding a specialisation with its own `static_assert`s.

#ifndef TF_TREE_HPP
#define TF_TREE_HPP

#if __cplusplus < 201703L
#error "tf_tree.hpp requires C++17 or later"
#endif

#include "tf_tree.h"

#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <optional>
#include <string>
#include <type_traits>
#include <utility>
#include <vector>

// `-fno-exceptions` implies the no-exceptions mode; do not make the user say it
// twice, and do not emit a `throw` they cannot compile.
#if !defined(TF_TREE_NO_EXCEPTIONS) && !defined(__cpp_exceptions)
#define TF_TREE_NO_EXCEPTIONS 1
#endif

#ifndef TF_TREE_NO_EXCEPTIONS
#include <stdexcept>
#endif

// ---------------------------------------------------------------------------
// Optional third-party interop, detected rather than configured
// ---------------------------------------------------------------------------
//
// `__has_include` so this header costs nothing to a user without Eigen, and so
// a user *with* Eigen gets the interop by including Eigen first — no
// `TF_TREE_USE_EIGEN` to remember.
//
// **These includes are outside `namespace tf_tree` and must stay there.** They
// were briefly inside it, which drags `<cmath>` and the rest of Eigen's
// transitive standard-library includes into the namespace and detonates the
// standard library: `error: 'acos' has not been declared in '::'`, plus twenty
// more. Only the `layout_of` specialisations belong in the namespace.

#if defined(__has_include)
#if __has_include(<Eigen/Geometry>)
#define TF_TREE_HAS_EIGEN 1
#endif
#if __has_include(<sophus/se3.hpp>)
#define TF_TREE_HAS_SOPHUS 1
#endif
#endif

#ifdef TF_TREE_HAS_EIGEN
#include <Eigen/Geometry>
#endif
#ifdef TF_TREE_HAS_SOPHUS
#include <sophus/se3.hpp>
#endif

namespace tf_tree {

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A failed call, carrying the full `tft_error` the C ABI recorded.
///
/// The detail is **copied out at the point of failure**, not referenced.
/// `tft_last_error` fills a caller-owned struct from a thread-local slot that
/// the next `tf_tree` call on this thread overwrites, and §3.3 names that
/// lifetime as the single most common C-API misuse. Copying is what makes an
/// `Error` safe to store, return, and log later — which is exactly what a C++
/// user will do with it.
class Error
#ifndef TF_TREE_NO_EXCEPTIONS
    : public std::runtime_error
#endif
{
public:
    explicit Error(tft_status status)
#ifndef TF_TREE_NO_EXCEPTIONS
        : std::runtime_error(fetch_message(status)), detail_(fetch(status))
#else
        : detail_(fetch(status))
#endif
    {
    }

    /// The status code. Always the one the failing call returned.
    tft_status code() const noexcept { return detail_.code; }
    /// The offending edge, or `TFT_INVALID_ID`.
    std::uint32_t edge() const noexcept { return detail_.edge; }
    std::uint32_t frame_a() const noexcept { return detail_.frame_a; }
    std::uint32_t frame_b() const noexcept { return detail_.frame_b; }
    std::int64_t requested() const noexcept { return detail_.requested; }
    std::int64_t oldest() const noexcept { return detail_.oldest; }
    std::int64_t newest() const noexcept { return detail_.newest; }
    std::uint64_t plan_generation() const noexcept { return detail_.plan_generation; }
    std::uint64_t current_generation() const noexcept { return detail_.current_generation; }
    /// The whole struct, for a caller that wants to print it uniformly.
    const tft_error& detail() const noexcept { return detail_; }

    /// The human-readable message. Available in both error modes;
    /// `std::runtime_error::what()` is not, under `-fno-exceptions`.
    const char* message() const noexcept { return detail_.message; }

private:
    static tft_error fetch(tft_status status) noexcept
    {
        tft_error e{};
        e.struct_size = static_cast<std::uint32_t>(sizeof(tft_error));
        if (tft_last_error(&e) != TFT_OK) {
            // The detail could not be retrieved — a struct_size mismatch, which
            // means header and library disagree. Report the status we were
            // actually given rather than inventing one.
            e = tft_error{};
            e.struct_size = static_cast<std::uint32_t>(sizeof(tft_error));
            e.code = status;
            e.edge = TFT_INVALID_ID;
            e.frame_a = TFT_INVALID_ID;
            e.frame_b = TFT_INVALID_ID;
            std::strncpy(e.message, "tf_tree: error detail unavailable (ABI mismatch?)",
                         sizeof(e.message) - 1);
        }
        // A caller can pass a status the slot does not describe if it ignored an
        // earlier failure. The status is authoritative; the detail is context.
        e.code = status;
        return e;
    }

#ifndef TF_TREE_NO_EXCEPTIONS
    static std::string fetch_message(tft_status status)
    {
        const tft_error e = fetch(status);
        return std::string(e.message);
    }
#endif

    tft_error detail_;
};

#ifdef TF_TREE_NO_EXCEPTIONS

/// A minimal `expected`, used only when exceptions are off.
///
/// `std::expected` is C++23 and this header is C++17, so a small one lives
/// here. It is deliberately not a general-purpose type: it has exactly the
/// operations the wrapper needs, so nobody is tempted to depend on it as a
/// utility. When the project moves to C++23 this becomes an alias.
/// The error is held in a `std::optional`, and **that is a performance
/// requirement, not a style choice.**
///
/// The first version stored a plain `Error` and initialised it with
/// `Error(TFT_OK)` on the *success* path. `Error`'s constructor calls
/// `tft_last_error`, so every successful lookup made an extra FFI call and
/// copied 320 bytes out of the thread-local slot. Measured with
/// `-Wl,--wrap=tft_last_error`: exactly **one call per successful lookup**, and
/// the wrapper came out at **1.064x** the raw C ABI against §7 gate 2's 1.02
/// allowance. The exceptions build made zero such calls and measured 1.002x,
/// which is why the benchmark — compiled only with exceptions — reported a pass.
///
/// An empty `std::optional` constructs no `Error` and calls nothing.
/// Tag for constructing an `expected` whose payload is default-initialised
/// **in place**. See `make_result`.
struct in_place_value_t {
    explicit in_place_value_t() = default;
};
inline constexpr in_place_value_t in_place_value{};

template <typename T>
class expected {
public:
    explicit expected(in_place_value_t) : value_() {}
    expected(T value) : value_(std::move(value)) {}
    expected(Error e) : error_(std::move(e)) {}

    explicit operator bool() const noexcept { return !error_.has_value(); }
    bool has_value() const noexcept { return !error_.has_value(); }

    /// **Unchecked.** Reading the value of a failed `expected` is your bug, in
    /// the same way that dereferencing a null pointer is; there is no exception
    /// to throw, which is the point of this mode.
    const T& operator*() const noexcept { return value_; }
    T& operator*() noexcept { return value_; }
    const T* operator->() const noexcept { return &value_; }
    T* operator->() noexcept { return &value_; }

    /// **Unchecked**, like `operator*`: only meaningful when `!*this`.
    const Error& error() const noexcept { return *error_; }

private:
    T value_{};
    std::optional<Error> error_;
};

/// The void case: success or an `Error`, no payload.
template <>
class expected<void> {
public:
    expected() = default;
    expected(Error e) : error_(std::move(e)) {}
    explicit operator bool() const noexcept { return !error_.has_value(); }
    bool has_value() const noexcept { return !error_.has_value(); }
    /// **Unchecked**: only meaningful when `!*this`.
    const Error& error() const noexcept { return *error_; }

private:
    std::optional<Error> error_;
};

template <typename T>
using result = expected<T>;

/// A pointer to the payload **inside the object that will be returned**.
///
/// The wrapper writes the C ABI's bytes straight into the return slot rather
/// than into a local it then moves out. That is not micro-optimisation: with a
/// local, `return out;` converts through `expected(T value)` and costs two
/// moves of `T` — 128 bytes each for `Eigen::Isometry3d` — which NRVO elides in
/// the exceptions mode and cannot elide here. Measured: 1.028x the C ABI with
/// the moves, against §7 gate 2's 1.02.
template <typename T>
inline T* value_ptr(expected<T>& e) noexcept
{
    return &*e;
}

/// An empty result whose payload is default-initialised **in place**.
///
/// `expected<T> out{T{}}` looks equivalent and is not: it constructs a
/// temporary and moves it into `value_`, which for `Eigen::Isometry3d` is a
/// 128-byte copy the exceptions mode never pays. Interleaved measurement put
/// that at a consistent 3-5 % over the C ABI — above §7 gate 2's 2 %, and only
/// visible once the benchmark timed both paths in the same round.
template <typename T>
inline expected<T> make_result()
{
    return expected<T>(in_place_value);
}

#define TF_TREE_FAIL(status) return ::tf_tree::Error(status)
#define TF_TREE_TRY(expr)                                                     \
    do {                                                                      \
        const tft_status s_ = (expr);                                         \
        if (s_ != TFT_OK) {                                                   \
            TF_TREE_FAIL(s_);                                                 \
        }                                                                     \
    } while (0)

#else  // exceptions

template <typename T>
using result = T;

/// See the no-exceptions overload: here `result<T>` *is* `T`, so this is the
/// identity and NRVO does the rest.
template <typename T>
inline T* value_ptr(T& v) noexcept
{
    return &v;
}

/// Here `result<T>` *is* `T`, so this is a default-constructed `T` that the
/// caller's NRVO turns into the return slot itself.
template <typename T>
inline T make_result()
{
    return T{};
}

#define TF_TREE_FAIL(status) throw ::tf_tree::Error(status)
#define TF_TREE_TRY(expr)                                                     \
    do {                                                                      \
        const tft_status s_ = (expr);                                         \
        if (s_ != TFT_OK) {                                                   \
            TF_TREE_FAIL(s_);                                                 \
        }                                                                     \
    } while (0)

#endif  // TF_TREE_NO_EXCEPTIONS

// ---------------------------------------------------------------------------
// ABI check — §3.6
// ---------------------------------------------------------------------------

namespace detail {

/// Verify at load time that the header and the library agree.
///
/// §3.6 asks for a static initializer that throws or aborts with **both**
/// versions named. `tft_check_abi` puts both in the error detail and the
/// message, so this only has to surface it.
///
/// Whether the ABI check has run. Declared before [`AbiCheck`] because its
/// constructor sets it. Exists so a test can assert that the check happened at
/// all — an assertion that is not a formality: it did not, for the whole of
/// this header's first life.
inline bool abi_check_ran = false;

/// Constructed by a namespace-scope `inline` variable below, so it runs during
/// this translation unit's dynamic initialization — before anything the user
/// wrote in the same TU is odr-used, and in practice before `main`'s body. A
/// silently mismatched ABI is a debugging session nobody deserves, and finding
/// out at the first lookup is finding out too late.
struct AbiCheck {
    AbiCheck()
    {
        if (tft_check_abi(TFT_ABI_VERSION_MAJOR, TFT_ABI_VERSION_MINOR) != TFT_OK) {
            tft_error e{};
            e.struct_size = static_cast<std::uint32_t>(sizeof(tft_error));
            const char* msg = (tft_last_error(&e) == TFT_OK)
                                  ? e.message
                                  : "tf_tree: ABI mismatch (detail unavailable)";
            fail(msg);
        }
        abi_check_ran = true;
    }

    // `[[noreturn]]` in both modes: an ABI mismatch is not recoverable, and a
    // process that continues past it will read transforms out of a struct
    // layout it does not have.
    [[noreturn]] static void fail(const char* msg)
    {
        std::fputs("tf_tree: ", stderr);
        std::fputs(msg, stderr);
        std::fputc('\n', stderr);
#ifdef TF_TREE_NO_EXCEPTIONS
        // Nothing to throw, and continuing is worse than stopping.
        std::abort();
#else
        throw Error(TFT_ERR_ABI_MISMATCH);
#endif
    }
};

/// **A namespace-scope `inline` variable, not a function-local static, and not
/// behind any `#ifdef`.**
///
/// The first version was a Meyers singleton called from `Tree::open()`, which is
/// itself `#ifdef TFT_HAVE_SHM` — a macro nothing in the build defines. So the
/// check was unreachable in every shipped configuration, and a comment above it
/// claimed "it runs before `main`", which a function-local static does not do
/// even when it is called. §3.6 asks for a static initializer; this is one.
///
/// C++17 `inline` gives exactly one object across all translation units, so the
/// check runs once per program rather than once per TU, without a `.cpp` file
/// to link — which a header-only library does not have.
inline const AbiCheck abi_check_instance{};

}  // namespace detail

// ---------------------------------------------------------------------------
// Layout selection — §3.5, made unmisusable
// ---------------------------------------------------------------------------

/// The payload size of `layout`, at compile time.
///
/// `tft_layout_size` is the authority and is not `constexpr`, so this mirrors
/// it. The duplication is made safe by a test that walks every layout and
/// asserts the two agree at run time — without which this would be a second
/// source of truth for buffer sizes, which is the last thing an FFI boundary
/// needs.
constexpr std::size_t payload_bytes(tft_layout layout)
{
    return layout == TFT_LAYOUT_QVEC7_WXYZ || layout == TFT_LAYOUT_QVEC7_XYZW ? 56
           : layout == TFT_LAYOUT_MAT4_COL || layout == TFT_LAYOUT_MAT4_ROW   ? 128
           : layout == TFT_LAYOUT_AFFINE12_ROW_F32                            ? 48
                                                                              : 0;
}

/// Whether `T` may receive a raw layout write into its own storage.
///
/// The default is `std::is_trivially_copyable`, which is the correct standard
/// answer. It is **not** the whole answer, because the one type §4.2 is
/// specifically about fails it: `Eigen::Transform` declares its own copy
/// constructor, so `is_trivially_copyable<Eigen::Isometry3d>` is `false` on
/// GCC and Clang — while its storage is, and is documented to be, a plain array
/// of `double` at offset 0. That is the property a layout write actually needs,
/// no standard trait expresses it, and it is the same property `Eigen::Map` and
/// every `memcpy`-into-`.data()` in Eigen's own documentation rest on.
///
/// So the trait is a customisation point with one specialisation, and the
/// specialisation's premise is checked at **run time** by the wrapper's test
/// (`matrix().data()` must equal the object's own address). Asserting something
/// weaker here and hoping would be worse than saying which types are opted in.
template <typename T>
struct raw_writable : std::integral_constant<bool, std::is_trivially_copyable<T>::value> {};

/// The `tft_layout` that `T`'s memory representation *is*.
///
/// Unspecialised on purpose: a type with no specialisation is a compile error
/// naming the type, not a silent fall-through to some default. §3.5 has no
/// default and neither does this.
template <typename T, typename Enable = void>
struct layout_of;

/// `[qw qx qy qz tx ty tz]`, the canonical order. Also the fallback a caller
/// reaches for when their own type is none of the below.
struct Quat7 {
    double qw, qx, qy, qz, tx, ty, tz;
};

template <>
struct layout_of<Quat7> {
    static constexpr tft_layout value = TFT_LAYOUT_QVEC7_WXYZ;
};
static_assert(sizeof(Quat7) == 56, "Quat7 must be tightly packed");

/// Row-major 4x4, the shape a C or NumPy user means by "a transform".
struct Mat4Row {
    double m[16];
};

template <>
struct layout_of<Mat4Row> {
    static constexpr tft_layout value = TFT_LAYOUT_MAT4_ROW;
};
static_assert(sizeof(Mat4Row) == 128, "Mat4Row must be tightly packed");

// ---------------------------------------------------------------------------
// Eigen interop — §4.2
// ---------------------------------------------------------------------------

#ifdef TF_TREE_HAS_EIGEN
template <>
struct layout_of<Eigen::Isometry3d> {
    static constexpr tft_layout value = TFT_LAYOUT_MAT4_COL;
};

/// See [`raw_writable`]. Opted in because Eigen's storage is a plain `double`
/// array at offset 0; verified at run time by the wrapper test.
template <>
struct raw_writable<Eigen::Isometry3d> : std::true_type {};

// §4.2 says to assert this rather than assume it. `Eigen::Isometry3d` stores a
// 4x4 column-major `Matrix4d`: 128 bytes, alignment 16 (32 under AVX), and 128
// is a multiple of both — so an array of them is tightly packed and
// `MAT4_COL` writes into it with no stride and no copy.
static_assert(sizeof(Eigen::Isometry3d) == 128,
              "unexpected Eigen Transform layout; the zero-copy batch path assumes 128 bytes");
static_assert(alignof(Eigen::Isometry3d) <= 128 && (128 % alignof(Eigen::Isometry3d)) == 0,
              "Eigen::Isometry3d's alignment does not divide its size, so an array of them "
              "is not tightly packed");
// C++17's over-aligned `new` is what makes `std::vector<Eigen::Isometry3d>`
// correct without `Eigen::aligned_allocator`. This header requires C++17 at the
// top, so that holds — but say so, because it is the assumption the convenience
// overload rests on.
static_assert(__cplusplus >= 201703L,
              "the std::vector<Eigen::Isometry3d> overload needs C++17 over-aligned new");
#endif  // TF_TREE_HAS_EIGEN

// ---------------------------------------------------------------------------
// Sophus interop and the alignment hazard — §4.3
// ---------------------------------------------------------------------------

#ifdef TF_TREE_HAS_SOPHUS
template <>
struct layout_of<Sophus::SE3d> {
    // Eigen/Sophus coefficient order: the quaternion is stored (x, y, z, w)
    // even though the constructor takes (w, x, y, z). §3.5 is emphatic about
    // this and `TFT_LAYOUT_QVEC7_XYZW` exists for exactly this line.
    static constexpr tft_layout value = TFT_LAYOUT_QVEC7_XYZW;
};

/// Same reasoning as Eigen's: an `Eigen::Quaterniond` followed by a
/// `Vector3d`, both plain scalar arrays. The additional constraint — that the
/// quaternion is first with no interior padding — is what
/// `detail::sophus_is_directly_writable()` checks, and it is checked at run
/// time because `offsetof` on Sophus's private members does not compile.
template <>
struct raw_writable<Sophus::SE3d> : std::true_type {};

namespace detail {

/// Whether an array of `Sophus::SE3d` can be written directly with a stride.
///
/// §4.3's hazard: the payload is 56 bytes (a 32-byte quaternion followed by a
/// 24-byte `Vector3d`), but the type's alignment rounds `sizeof` up — commonly
/// to 64. **An array of `SE3d` is therefore usually not tightly packed**, and a
/// `memcpy` of `n x 56` bytes into it corrupts every element after the first.
/// `out_stride_bytes` is the answer, and `sizeof(Sophus::SE3d)` must be read
/// from *the user's build* because it depends on their vectorization flags.
///
/// The direct path additionally requires that the quaternion precede the
/// translation with no interior padding — otherwise the strided write would put
/// the right 56 bytes in the wrong places inside each element.
///
/// **This is checked at run time, once, not by `offsetof`.** `Sophus::SE3d`'s
/// members are private, so `offsetof` on them does not compile; the public
/// `so3().data()` and `translation().data()` accessors give the same two
/// addresses without reaching inside the type. The result is cached in a
/// function-local static, so the cost is one relaxed load per call after the
/// first — and the alternative is not a cheaper check, it is a wrong answer.
inline bool sophus_is_directly_writable()
{
    static const bool ok = [] {
        const Sophus::SE3d probe;
        const auto* base = reinterpret_cast<const unsigned char*>(&probe);
        const auto* quat = reinterpret_cast<const unsigned char*>(probe.so3().data());
        const auto* tran = reinterpret_cast<const unsigned char*>(probe.translation().data());
        return sizeof(Sophus::SE3d) >= 56          // room for the payload
               && quat == base                     // quaternion first
               && tran == base + 32                // translation immediately after
               && sizeof(Eigen::Quaterniond) == 32 && sizeof(Eigen::Vector3d) == 24;
    }();
    return ok;
}

}  // namespace detail
#endif  // TF_TREE_HAS_SOPHUS

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

namespace detail {

/// RAII over one C handle. Deleted copy, defaulted move (§4.1).
template <typename H, void (*Free)(H*)>
class Handle {
public:
    Handle() noexcept : h_(nullptr) {}
    explicit Handle(H* h) noexcept : h_(h) {}
    ~Handle() { reset(); }

    Handle(const Handle&) = delete;
    Handle& operator=(const Handle&) = delete;

    Handle(Handle&& other) noexcept : h_(other.h_) { other.h_ = nullptr; }
    Handle& operator=(Handle&& other) noexcept
    {
        if (this != &other) {
            reset();
            h_ = other.h_;
            other.h_ = nullptr;
        }
        return *this;
    }

    H* get() const noexcept { return h_; }
    explicit operator bool() const noexcept { return h_ != nullptr; }

    void reset() noexcept
    {
        if (h_ != nullptr) {
            Free(h_);
            h_ = nullptr;
        }
    }

    H** out() noexcept
    {
        reset();
        return &h_;
    }

private:
    H* h_;
};

}  // namespace detail

class Plan;
class Publisher;

/// A transform tree. `Send + Sync` on the Rust side, so this is safe to share
/// between threads; `Publisher` is not, and says so.
class Tree {
public:
    Tree() = default;

#ifdef TFT_HAVE_SHM
    /// Join the running arena named by the environment, read-only.
    ///
    /// Mirrors `tf_tree::open()`: `$TF_TREE_DOMAIN`, `$TF_TREE_NAME` and
    /// `$TF_TREE_RUNTIME_DIR` select which arena.
    static result<Tree> open()
    {
        Tree t;
        TF_TREE_TRY(tft_tree_open(t.h_.out()));
        return t;
    }
#endif

    /// Adopt a handle from the C ABI, taking ownership of it.
    ///
    /// The escape hatch for a caller mixing C and C++ — and the only way to
    /// build a `Tree` around a handle this header did not create.
    static Tree adopt(tft_tree* raw) noexcept
    {
        Tree t;
        *t.h_.out() = raw;
        return t;
    }

    tft_tree* raw() const noexcept { return h_.get(); }
    explicit operator bool() const noexcept { return static_cast<bool>(h_); }

    inline result<Plan> plan(const char* target, const char* source) const;
    inline result<Publisher> claim(const char* child, const char* parent) const;

private:
    detail::Handle<tft_tree, tft_tree_free> h_;
};

/// A compiled plan. Compile once, evaluate many times (D3).
class Plan {
public:
    Plan() = default;

    static Plan adopt(tft_plan* raw) noexcept
    {
        Plan p;
        *p.h_.out() = raw;
        return p;
    }

    tft_plan* raw() const noexcept { return h_.get(); }
    explicit operator bool() const noexcept { return static_cast<bool>(h_); }

    /// Evaluate at `stamp` into a `T` chosen by [`layout_of`].
    ///
    /// `T` must be trivially copyable and exactly the layout's payload size;
    /// both are `static_assert`ed, so a mismatched type is a compile error
    /// rather than a buffer overrun.
    template <typename T>
    result<T> at(std::int64_t stamp) const
    {
        static_assert(raw_writable<T>::value,
                      "T cannot receive a raw layout write; specialise tf_tree::raw_writable<T> "
                      "if its storage really is a plain scalar array at offset 0");
        static_assert(sizeof(T) >= payload_bytes(layout_of<T>::value),
                      "T is smaller than the layout it selects, so the write would overrun it");
        // `result<T>`, not `T`: this local IS the return slot under NRVO, in
        // both error modes, so the C ABI writes once into the caller's storage.
        // See `value_ptr`.
        result<T> out = make_result<T>();
        const tft_status s =
            tft_plan_at(h_.get(), stamp, layout_of<T>::value, value_ptr(out));
        if (s != TFT_OK) {
            TF_TREE_FAIL(s);
        }
        return out;
    }

    /// Evaluate at `n` stamps, writing straight into `out`.
    ///
    /// **No intermediate buffer and no copy** when `sizeof(T)` equals the
    /// layout's payload — which for `Eigen::Isometry3d` it does. When it does
    /// not (`Sophus::SE3d`, usually), `sizeof(T)` is passed as the stride and
    /// the write is still direct; that is what §4.3's `out_stride_bytes` is
    /// for, and why it is not optional.
    template <typename T>
    result<void> at_many(const std::int64_t* stamps, std::size_t n, T* out) const
    {
        static_assert(raw_writable<T>::value,
                      "T cannot receive a raw layout write; specialise tf_tree::raw_writable<T> "
                      "if its storage really is a plain scalar array at offset 0");
        static_assert(sizeof(T) >= payload_bytes(layout_of<T>::value),
                      "T is smaller than the layout it selects, so the write would overrun it");
        const tft_status s =
            tft_plan_at_many(h_.get(), stamps, n, layout_of<T>::value, out, sizeof(T));
        if (s != TFT_OK) {
            TF_TREE_FAIL(s);
        }
#ifdef TF_TREE_NO_EXCEPTIONS
        return expected<void>();
#endif
    }

    /// Convenience over a `std::vector`. Sizes the output from the input.
    template <typename T>
    result<void> at_many(const std::vector<std::int64_t>& stamps, std::vector<T>& out) const
    {
        out.resize(stamps.size());
        return at_many(stamps.data(), stamps.size(), out.data());
    }

private:
    friend class Tree;
    detail::Handle<tft_plan, tft_plan_free> h_;
};

/// An exclusive claim on one edge.
///
/// **`Send + !Sync`: one thread at a time.** C++ cannot express that either, so
/// the type is move-only (which stops it being shared by copy) and the library
/// underneath checks: a debug build `abort()`s on cross-thread use, a release
/// build returns `TFT_ERR_WRONG_THREAD`. Moving a `Publisher` to another thread
/// and using it there is *also* refused — affinity is to the claiming thread,
/// not to the object.
class Publisher {
public:
    Publisher() = default;

    static Publisher adopt(tft_publisher* raw) noexcept
    {
        Publisher p;
        *p.h_.out() = raw;
        return p;
    }

    tft_publisher* raw() const noexcept { return h_.get(); }
    explicit operator bool() const noexcept { return static_cast<bool>(h_); }

    /// Publish one transform. `T` selects the layout by type.
    template <typename T>
    result<void> push(std::int64_t stamp, const T& value)
    {
        static_assert(raw_writable<T>::value,
                      "T cannot be read as a raw layout; specialise tf_tree::raw_writable<T> "
                      "if its storage really is a plain scalar array at offset 0");
        static_assert(sizeof(T) >= payload_bytes(layout_of<T>::value),
                      "T is smaller than the layout it selects, so the read would overrun it");
        const tft_status s = tft_publisher_push(h_.get(), stamp, layout_of<T>::value, &value);
        if (s != TFT_OK) {
            TF_TREE_FAIL(s);
        }
#ifdef TF_TREE_NO_EXCEPTIONS
        return expected<void>();
#endif
    }

    /// Publish a batch, reading `sizeof(T)` apart. See `Plan::at_many`.
    template <typename T>
    result<void> push_many(const std::int64_t* stamps, std::size_t n, const T* values)
    {
        static_assert(raw_writable<T>::value,
                      "T cannot be read as a raw layout; specialise tf_tree::raw_writable<T> "
                      "if its storage really is a plain scalar array at offset 0");
        static_assert(sizeof(T) >= payload_bytes(layout_of<T>::value),
                      "T is smaller than the layout it selects, so the read would overrun it");
        const tft_status s = tft_publisher_push_many(h_.get(), stamps, n, layout_of<T>::value,
                                                     values, sizeof(T));
        if (s != TFT_OK) {
            TF_TREE_FAIL(s);
        }
#ifdef TF_TREE_NO_EXCEPTIONS
        return expected<void>();
#endif
    }

    /// Give the edge back now, without destroying the handle.
    result<void> release()
    {
        const tft_status s = tft_publisher_release(h_.get());
        if (s != TFT_OK) {
            TF_TREE_FAIL(s);
        }
#ifdef TF_TREE_NO_EXCEPTIONS
        return expected<void>();
#endif
    }

private:
    friend class Tree;
    detail::Handle<tft_publisher, tft_publisher_free> h_;
};

inline result<Plan> Tree::plan(const char* target, const char* source) const
{
    Plan p;
    // `p.h_.out()` through friendship, **not** a cast of `&p` to `tft_plan**`.
    // The first version of this did exactly that, on the assumption that a class
    // holding one pointer is layout-compatible with that pointer. It is not
    // guaranteed to be, and it would have written the C handle over whatever the
    // compiler chose to put first.
    TF_TREE_TRY(tft_plan_create(h_.get(), target, source, p.h_.out()));
    return p;
}

inline result<Publisher> Tree::claim(const char* child, const char* parent) const
{
    Publisher p;
    TF_TREE_TRY(tft_tree_claim(h_.get(), child, parent, p.h_.out()));
    return p;
}

}  // namespace tf_tree

#endif  // TF_TREE_HPP
