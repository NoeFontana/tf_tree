// tf_tree — header-only C++17 wrapper over the C ABI. docs/PHASE4.md §4.
// Hand-written; `tf_tree.h` is generated. No logic (§4.1): thin inlines over the C ABI.
//
// Error modes, chosen at include time:
//   default                     -> throws tf_tree::Error
//   #define TF_TREE_NO_EXCEPTIONS -> returns tf_tree::expected<T, Error>
// Layouts are selected by type (§3.5) via `layout_of<T>`.

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

// `-fno-exceptions` implies the no-exceptions mode.
#if !defined(TF_TREE_NO_EXCEPTIONS) && !defined(__cpp_exceptions)
#define TF_TREE_NO_EXCEPTIONS 1
#endif

#ifndef TF_TREE_NO_EXCEPTIONS
#include <stdexcept>
#endif

// --- Optional third-party interop, detected rather than configured ---
//
// Detected with `__has_include`; include Eigen first. Must stay outside
// `namespace tf_tree`.

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

// --- Errors ---

/// A failed call, with the `tft_error` detail copied out at the point of failure
/// (§3.3), so it is safe to store.
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

    /// The status code the failing call returned.
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
    /// The whole struct.
    const tft_error& detail() const noexcept { return detail_; }

    /// The human-readable message (available in both error modes).
    const char* message() const noexcept { return detail_.message; }

private:
    static tft_error fetch(tft_status status) noexcept
    {
        tft_error e{};
        e.struct_size = static_cast<std::uint32_t>(sizeof(tft_error));
        if (tft_last_error(&e) != TFT_OK) {
            e = tft_error{};
            e.struct_size = static_cast<std::uint32_t>(sizeof(tft_error));
            e.code = status;
            e.edge = TFT_INVALID_ID;
            e.frame_a = TFT_INVALID_ID;
            e.frame_b = TFT_INVALID_ID;
            std::strncpy(e.message, "tf_tree: error detail unavailable (ABI mismatch?)",
                         sizeof(e.message) - 1);
        }
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

/// Tag: construct the payload default-initialised in place.
struct in_place_value_t {
    explicit in_place_value_t() = default;
};
inline constexpr in_place_value_t in_place_value{};

/// A minimal `expected` for exceptions-off builds (C++23 will replace it).
/// The error is a `std::optional` so the success path does not call
/// `tft_last_error` (§7 gate 2).
template <typename T>
class expected {
public:
    explicit expected(in_place_value_t) : value_() {}
    expected(T value) : value_(std::move(value)) {}
    expected(Error e) : error_(std::move(e)) {}

    explicit operator bool() const noexcept { return !error_.has_value(); }
    bool has_value() const noexcept { return !error_.has_value(); }

    /// **Unchecked**: reading the value of a failed `expected` is a caller bug.
    const T& operator*() const noexcept { return value_; }
    T& operator*() noexcept { return value_; }
    const T* operator->() const noexcept { return &value_; }
    T* operator->() noexcept { return &value_; }

    /// **Unchecked**: only meaningful when `!*this`.
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

/// A pointer to the payload inside the object that will be returned, so no local is moved out.
template <typename T>
inline T* value_ptr(expected<T>& e) noexcept
{
    return &*e;
}

/// An empty result whose payload is default-initialised in place (avoids a 128-byte move).
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

/// Fail *into* the existing result object, keeping NRVO
/// (`check_at_writes_into_the_returned_object` pins it).
///
/// **Contract 1:** never returns to the next statement, in either mode
/// (`check_fail_into_leaves_the_function`). **Contract 2:** `out` is a bare
/// identifier naming the return object.
#define TF_TREE_FAIL_INTO(out, status)                                        \
    do {                                                                      \
        (out) = ::tf_tree::Error(status);                                     \
        return out;                                                           \
    } while (0)

#else  // exceptions

template <typename T>
using result = T;

/// `result<T>` is `T`.
template <typename T>
inline T* value_ptr(T& v) noexcept
{
    return &v;
}

/// A default-constructed `T` that NRVO turns into the return slot.
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

/// `out` is still evaluated so a misspelling fails to compile in this mode too.
#define TF_TREE_FAIL_INTO(out, status)                                        \
    do {                                                                      \
        (void)(out);                                                          \
        TF_TREE_FAIL(status);                                                 \
    } while (0)

#endif  // TF_TREE_NO_EXCEPTIONS

// --- ABI check — §3.6 ---

namespace detail {

/// Verify at load time that the header and the library agree (§3.6).
inline bool abi_check_ran = false;

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

    [[noreturn]] static void fail(const char* msg)
    {
        std::fputs("tf_tree: ", stderr);
        std::fputs(msg, stderr);
        std::fputc('\n', stderr);
#ifdef TF_TREE_NO_EXCEPTIONS
        std::abort();
#else
        throw Error(TFT_ERR_ABI_MISMATCH);
#endif
    }
};

/// A namespace-scope `inline` variable (§3.6): the check runs once per program.
inline const AbiCheck abi_check_instance{};

}  // namespace detail

// --- Layout selection — §3.5, made unmisusable ---

/// The payload size of `layout`, at compile time.
constexpr std::size_t payload_bytes(tft_layout layout)
{
    return layout == TFT_LAYOUT_QVEC7_WXYZ || layout == TFT_LAYOUT_QVEC7_XYZW ? 56
           : layout == TFT_LAYOUT_MAT4_COL || layout == TFT_LAYOUT_MAT4_ROW   ? 128
           : layout == TFT_LAYOUT_AFFINE12_ROW_F32                            ? 48
           : layout == TFT_LAYOUT_QVEC7_WXYZ_TWIST6                           ? 104
                                                                              : 0;
}

/// Whether `layout` can be published; `push`/`push_many` `static_assert` on it
/// (`docs/API.md` §4):
///
/// * `TFT_LAYOUT_QVEC7_WXYZ_TWIST6` — a twist is derived, never stored.
/// * `TFT_LAYOUT_AFFINE12_ROW_F32` — an `f32` output encoding (`docs/PROJECT.md` §5).
constexpr bool publishable(tft_layout layout)
{
    return layout != TFT_LAYOUT_QVEC7_WXYZ_TWIST6 && layout != TFT_LAYOUT_AFFINE12_ROW_F32;
}

/// Whether `T` may receive a raw layout write into its own storage.
///
/// Defaults to `std::is_trivially_copyable`.
template <typename T>
struct raw_writable : std::integral_constant<bool, std::is_trivially_copyable<T>::value> {};

/// The `tft_layout` of `T`'s memory representation; unknown types do not compile (§3.5).
template <typename T, typename Enable = void>
struct layout_of;

/// `[qw qx qy qz tx ty tz]`, the canonical order.
struct Quat7 {
    double qw, qx, qy, qz, tx, ty, tz;
};

template <>
struct layout_of<Quat7> {
    static constexpr tft_layout value = TFT_LAYOUT_QVEC7_WXYZ;
};
static_assert(sizeof(Quat7) == 56, "Quat7 must be tightly packed");

/// `[qw qx qy qz tx ty tz | wx wy wz vx vy vz]` — a pose and its body twist.
///
/// Requesting this type evaluates with derivatives and can fail with
/// `TFT_ERR_NO_DERIVATIVES` or `TFT_ERR_NO_SEGMENT`. `omega` is rad/s, `v` m/s,
/// in the plan's source frame.
struct Quat7Twist6 {
    double qw, qx, qy, qz, tx, ty, tz;
    double wx, wy, wz;
    double vx, vy, vz;
};

template <>
struct layout_of<Quat7Twist6> {
    static constexpr tft_layout value = TFT_LAYOUT_QVEC7_WXYZ_TWIST6;
};
static_assert(sizeof(Quat7Twist6) == 104, "Quat7Twist6 must be tightly packed");
static_assert(offsetof(Quat7Twist6, wx) == 56,
              "the twist tail must start exactly where the Quat7 pose half ends");

/// Row-major 4x4.
struct Mat4Row {
    double m[16];
};

template <>
struct layout_of<Mat4Row> {
    static constexpr tft_layout value = TFT_LAYOUT_MAT4_ROW;
};
static_assert(sizeof(Mat4Row) == 128, "Mat4Row must be tightly packed");

// --- Extrapolation — docs/decisions/0039 ---

/// A pose, and how far past the route's newest common sample it was extrapolated.
template <typename T>
struct Extrapolated {
    /// The pose, in whatever layout `T` selects.
    T pose;
    /// Nanoseconds past the newest stamp every dynamic edge has data for; `0` means interpolated.
    std::int64_t by_ns;
    /// The edge that ran out of data first, or `TFT_INVALID_ID` when `by_ns` is `0`.
    std::uint32_t edge;
};

// --- Eigen interop — §4.2 ---

#ifdef TF_TREE_HAS_EIGEN
template <>
struct layout_of<Eigen::Isometry3d> {
    static constexpr tft_layout value = TFT_LAYOUT_MAT4_COL;
};

/// See [`raw_writable`].
template <>
struct raw_writable<Eigen::Isometry3d> : std::true_type {};

static_assert(sizeof(Eigen::Isometry3d) == 128,
              "unexpected Eigen Transform layout; the zero-copy batch path assumes 128 bytes");
static_assert(alignof(Eigen::Isometry3d) <= 128 && (128 % alignof(Eigen::Isometry3d)) == 0,
              "Eigen::Isometry3d's alignment does not divide its size, so an array of them "
              "is not tightly packed");
static_assert(__cplusplus >= 201703L,
              "the std::vector<Eigen::Isometry3d> overload needs C++17 over-aligned new");
#endif  // TF_TREE_HAS_EIGEN

// --- Sophus interop and the alignment hazard — §4.3 ---

#ifdef TF_TREE_HAS_SOPHUS
template <>
struct layout_of<Sophus::SE3d> {
    static constexpr tft_layout value = TFT_LAYOUT_QVEC7_XYZW;
};

/// As Eigen's; checked at run time by `detail::sophus_is_directly_writable()`.
template <>
struct raw_writable<Sophus::SE3d> : std::true_type {};

namespace detail {

/// Whether an array of `Sophus::SE3d` can be written directly with a stride.
///
/// §4.3: `sizeof(Sophus::SE3d)` is usually 64, not the 56-byte payload; use
/// `out_stride_bytes`.
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

// --- Handles ---

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

/// A transform tree; safe to share between threads (`Publisher` is not).
class Tree {
public:
    Tree() = default;

#ifdef TFT_HAVE_SHM
    /// Join the running arena named by `$TF_TREE_DOMAIN`, `$TF_TREE_NAME` and
    /// `$TF_TREE_RUNTIME_DIR`, read-only (as `tf_tree::open()`).
    static result<Tree> open()
    {
        Tree t;
        TF_TREE_TRY(tft_tree_open(t.h_.out()));
        return t;
    }
#endif

    /// Adopt a handle from the C ABI, taking ownership.
    static Tree adopt(tft_tree* raw) noexcept
    {
        Tree t;
        *t.h_.out() = raw;
        return t;
    }

    tft_tree* raw() const noexcept { return h_.get(); }
    explicit operator bool() const noexcept { return static_cast<bool>(h_); }

    inline result<Plan> plan(const char* target, const char* source) const;

    /// Compile a plan queried in time domain `domain`; `plan(target, source)` is
    /// `domain = 0`, real time (`docs/PHASE4.md` §5.5, `docs/decisions/0038`). A
    /// mismatch is reported here, not on every `at()`.
    inline result<Plan> plan_in_domain(const char* target, const char* source,
                                       std::uint8_t domain) const;

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
    /// On a hot path prefer `at_many` (`docs/decisions/0022`).
    ///
    /// `T` must be trivially copyable and at least the payload size (`static_assert`ed).
    template <typename T>
    result<T> at(std::int64_t stamp) const
    {
        static_assert(raw_writable<T>::value,
                      "T cannot receive a raw layout write; specialise tf_tree::raw_writable<T> "
                      "if its storage really is a plain scalar array at offset 0");
        static_assert(sizeof(T) >= payload_bytes(layout_of<T>::value),
                      "T is smaller than the layout it selects, so the write would overrun it");
        // NRVO: every `return` names `out` (`TF_TREE_FAIL_INTO`).
        result<T> out = make_result<T>();
        const tft_status s =
            tft_plan_at(h_.get(), stamp, layout_of<T>::value, value_ptr(out));
        if (s != TFT_OK) {
            TF_TREE_FAIL_INTO(out, s);
        }
        return out;
    }

    /// Evaluate at `stamp` under `policy`, and learn how far past the newest sample
    /// that went (`docs/decisions/0039`). `at()` refuses such a stamp.
    ///
    /// `TFT_EXTRAP_ERROR` is `at()`'s refusal; all policies report the distance
    /// in `Extrapolated<T>`.
    ///
    /// `T = Quat7Twist6` is refused with `TFT_ERR_BAD_ENUM`.
    template <typename T>
    result<Extrapolated<T>> at_extrapolating(std::int64_t stamp,
                                             tft_extrap_policy policy) const
    {
        static_assert(raw_writable<T>::value,
                      "T cannot receive a raw layout write; specialise tf_tree::raw_writable<T> "
                      "if its storage really is a plain scalar array at offset 0");
        static_assert(sizeof(T) >= payload_bytes(layout_of<T>::value),
                      "T is smaller than the layout it selects, so the write would overrun it");
        result<Extrapolated<T>> out = make_result<Extrapolated<T>>();
        tft_extrapolated info;
        info.struct_size = sizeof(info);
        const tft_status s =
            tft_plan_at_extrapolating(h_.get(), stamp, policy, layout_of<T>::value,
                                      &value_ptr(out)->pose, &info);
        if (s != TFT_OK) {
            TF_TREE_FAIL_INTO(out, s);
        }
        value_ptr(out)->by_ns = info.by_ns;
        value_ptr(out)->edge = info.edge;
        return out;
    }

    /// Evaluate at `n` stamps, writing straight into `out`. **The hot-path entry
    /// point**: it pays the per-call `Guard` once per batch. **Sort your stamps**;
    /// a scattered sweep restarts the cursor. `sizeof(T)` is the stride when it
    /// differs from the payload (§4.3 `out_stride_bytes`).
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

    /// Convenience over a `std::vector`.
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

/// An exclusive claim on one edge. Move-only; the library checks affinity to the
/// claiming thread (debug `abort()`, release `TFT_ERR_WRONG_THREAD`).
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
        static_assert(publishable(layout_of<T>::value),
                      "T selects an output-only layout: a twist is derived from the arena and "
                      "never published into it, and the f32 affine encoding exists for GPU "
                      "upload. Push the pose type instead (Quat7 is Quat7Twist6's pose half, "
                      "at the same offsets)");
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
        static_assert(publishable(layout_of<T>::value),
                      "T selects an output-only layout: a twist is derived from the arena and "
                      "never published into it, and the f32 affine encoding exists for GPU "
                      "upload. Push the pose type instead (Quat7 is Quat7Twist6's pose half, "
                      "at the same offsets)");
        const tft_status s = tft_publisher_push_many(h_.get(), stamps, n, layout_of<T>::value,
                                                     values, sizeof(T));
        if (s != TFT_OK) {
            TF_TREE_FAIL(s);
        }
#ifdef TF_TREE_NO_EXCEPTIONS
        return expected<void>();
#endif
    }

    /// Give the edge back now.
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
    TF_TREE_TRY(tft_plan_create(h_.get(), target, source, p.h_.out()));
    return p;
}

inline result<Plan> Tree::plan_in_domain(const char* target, const char* source,
                                        std::uint8_t domain) const
{
    // A separate member: one C entry point per function (§4.1).
    Plan p;
    TF_TREE_TRY(tft_plan_create_in_domain(h_.get(), target, source, domain, p.h_.out()));
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
