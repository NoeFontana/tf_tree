// extern "C" bridge over `tf2::BufferCore`, for the tf_tree differential and
// benchmark harnesses.
//
// Why a hand-written C shim rather than `cxx`/`autocxx`: `BufferCore`'s surface
// that we need is four calls wide, it throws (which must not cross the FFI
// boundary), and `geometry_msgs::msg::TransformStamped` is a generated type we
// would otherwise have to mirror. Marshalling flat `double[7]` arrays across the
// boundary keeps the binding trivial and the ownership rules obvious.
//
// `BufferCore` deliberately needs no rclcpp node, no DDS and no ROS graph — it
// links against `-ltf2` alone. That is what makes the comparison against
// tf_tree fair: both sides are plain in-process libraries doing transform math,
// with no middleware in the measurement.
//
// # Pose convention
//
// Every pose crossing this boundary is a `double[7]` laid out as
// `{qw, qx, qy, qz, tx, ty, tz}` — the same order as `tf_tree_math::Iso3::to_bits`,
// so the Rust side never reorders. tf2 stores quaternions **w-last**
// (`rotation.x/y/z/w`), so the transposition happens here, in one place, and is
// covered by a round-trip test.

#include <tf2/buffer_core.hpp>
#include <geometry_msgs/msg/transform_stamped.hpp>

#include <chrono>
#include <cstdint>
#include <cstring>
#include <new>
#include <string>

namespace {

/// The last exception message, **per calling thread**.
///
/// This is deliberately not a member of `Handle`. `tf2::BufferCore` is itself
/// thread-safe (it guards its frame table with an internal mutex), so one buffer
/// is meant to be shared by many reader threads — and measuring exactly that
/// sharing is the point of the concurrent read benchmark. A single error slot on
/// the handle would have been an unsynchronised shared mutable `std::string`,
/// making the whole handle unshareable for a reason that has nothing to do with
/// tf2.
///
/// Per-thread storage fixes that with no lock on the measured path, and gives
/// the semantics a caller actually wants: `tft2_last_error` reports why *your*
/// call failed, not whatever another thread failed at meanwhile.
thread_local std::string t_last_error;

/// Owns the buffer. Deliberately holds nothing else, so it is safe to share.
struct Handle {
  explicit Handle(double cache_secs)
      : buffer(tf2::durationFromSec(cache_secs)) {}
  tf2::BufferCore buffer;
};

/// Indices into the flat `double[7]` pose array.
enum : std::size_t { QW = 0, QX = 1, QY = 2, QZ = 3, TX = 4, TY = 5, TZ = 6 };

tf2::TimePoint time_point(std::int64_t stamp_ns) {
  return tf2::TimePoint(std::chrono::nanoseconds(stamp_ns));
}

}  // namespace

extern "C" {

/// Allocate a `BufferCore` whose cache spans `cache_secs`. Returns null on
/// allocation failure. Free with `tft2_free`.
void *tft2_new(double cache_secs) {
  return new (std::nothrow) Handle(cache_secs);
}

/// Free a handle from `tft2_new`. Null is a no-op.
void tft2_free(void *h) { delete static_cast<Handle *>(h); }

/// Insert `T_parent_child` at `stamp_ns`, using names from `tft2_name_new`.
///
/// `pose` is `{qw, qx, qy, qz, tx, ty, tz}`. `is_static` mirrors
/// `setTransform`'s static flag (`/tf_static` semantics: one entry, valid at any
/// time). Returns 0 on success, 1 if `setTransform` rejected the transform
/// (tf2's own validation: NaN, self-parent, empty frame id), 2 on an exception.
///
/// The names are `std::string`s assigned into the message, which is what a
/// native C++ publisher does — for names of any realistic length that is an SSO
/// copy and allocates nothing. Taking `const char*` here instead would have made
/// every caller build a NUL-terminated string per call (a heap allocation per
/// name in Rust), and a benchmark would have charged that to tf2.
int tft2_set_pre(void *h, const void *parent, const void *child,
                 std::int64_t stamp_ns, const double *pose, int is_static) {
  Handle *self = static_cast<Handle *>(h);
  try {
    geometry_msgs::msg::TransformStamped t;
    t.header.frame_id = *static_cast<const std::string *>(parent);
    t.child_frame_id = *static_cast<const std::string *>(child);
    // ROS time is (sec: int32, nanosec: uint32) and must be non-negative;
    // `stamp_ns` is validated as non-negative by the Rust caller.
    t.header.stamp.sec = static_cast<std::int32_t>(stamp_ns / 1000000000LL);
    t.header.stamp.nanosec = static_cast<std::uint32_t>(stamp_ns % 1000000000LL);
    // w-first (tf_tree) -> w-last (tf2).
    t.transform.rotation.w = pose[QW];
    t.transform.rotation.x = pose[QX];
    t.transform.rotation.y = pose[QY];
    t.transform.rotation.z = pose[QZ];
    t.transform.translation.x = pose[TX];
    t.transform.translation.y = pose[TY];
    t.transform.translation.z = pose[TZ];
    // `setTransform` takes `const std::string&`. Passing a string literal would
    // construct a temporary on every call, and at 20 characters that is past
    // libstdc++'s 15-byte SSO buffer — i.e. one heap allocation per publish,
    // charged to tf2 by a benchmark. A real broadcaster stores its authority
    // once and passes it by reference; so does this.
    static const std::string kAuthority = "tf_tree_differential";
    if (!self->buffer.setTransform(t, kAuthority, is_static != 0)) {
      t_last_error = "tf2 setTransform rejected the transform";
      return 1;
    }
    return 0;
  } catch (const std::exception &e) {
    t_last_error = e.what();
    return 2;
  } catch (...) {
    t_last_error = "unknown exception in setTransform";
    return 2;
  }
}

/// Allocate a persistent `std::string` for a frame name. Free with
/// `tft2_name_free`.
///
/// `BufferCore::lookupTransform` takes `const std::string&`. Handing it a
/// `const char*` constructs a temporary on **every** call — around 20 ns for the
/// pair, which a benchmark would charge to tf2. A native C++ caller holds its
/// frame names as `std::string` members and pays nothing, so this lets the
/// bridge do the same and keeps the comparison honest.
///
/// The returned string is immutable after construction, so it is safe to share
/// across threads.
void *tft2_name_new(const char *s) { return new (std::nothrow) std::string(s); }

/// Free a name from `tft2_name_new`. Null is a no-op.
void tft2_name_free(void *n) { delete static_cast<std::string *>(n); }

/// Look up `T_target_source` using names from `tft2_name_new`.
///
/// The allocation-free, temporary-free path: byte-for-byte the call a native
/// C++ user makes.
int tft2_lookup_pre(void *h, const void *target, const void *source,
                    std::int64_t stamp_ns, double *out) {
  Handle *self = static_cast<Handle *>(h);
  try {
    auto t = self->buffer.lookupTransform(
        *static_cast<const std::string *>(target),
        *static_cast<const std::string *>(source), time_point(stamp_ns));
    out[QW] = t.transform.rotation.w;
    out[QX] = t.transform.rotation.x;
    out[QY] = t.transform.rotation.y;
    out[QZ] = t.transform.rotation.z;
    out[TX] = t.transform.translation.x;
    out[TY] = t.transform.translation.y;
    out[TZ] = t.transform.translation.z;
    return 0;
  } catch (const std::exception &e) {
    t_last_error = e.what();
    return 1;
  } catch (...) {
    t_last_error = "unknown exception in lookupTransform";
    return 1;
  }
}

/// Whether tf2 believes a lookup would succeed, without throwing. Useful for
/// skipping query pairs tf2 cannot answer (so the differential compares only
/// what both engines can resolve).
int tft2_can_transform(void *h, const char *target, const char *source,
                       std::int64_t stamp_ns) {
  Handle *self = static_cast<Handle *>(h);
  try {
    return self->buffer.canTransform(target, source, time_point(stamp_ns)) ? 1
                                                                          : 0;
  } catch (...) {
    return 0;
  }
}

/// Drop every transform, keeping the handle. Lets a benchmark reuse one buffer
/// across repetitions without paying reallocation.
void tft2_clear(void *h) { static_cast<Handle *>(h)->buffer.clear(); }

/// Everything `tft2_lookup_pre` does **except** the `BufferCore` call: the same
/// FFI crossing, the same by-reference `std::string` access, the same
/// `double[7]` write-back.
///
/// Subtracting this from a `tft2_lookup_pre` measurement gives tf2's own cost
/// with the bridge's overhead removed, so a benchmark can state how much of the
/// reported tf2 latency is this shim rather than tf2. It takes the *same*
/// argument types as the real call deliberately — an earlier version took
/// `const char*` and was therefore measuring a different code path than the one
/// it claimed to model.
///
/// `volatile` and the `out` write stop the optimiser deleting what we are
/// trying to measure.
int tft2_lookup_noop(void *h, const void *target, const void *source,
                     std::int64_t stamp_ns, double *out) {
  (void)h;
  volatile std::size_t sink = 0;
  const auto &t = *static_cast<const std::string *>(target);
  const auto &s = *static_cast<const std::string *>(source);
  auto tp = time_point(stamp_ns);
  sink = t.size() + s.size() +
         static_cast<std::size_t>(tp.time_since_epoch().count() & 1);
  for (std::size_t i = 0; i < 7; ++i) {
    out[i] = static_cast<double>(sink);
  }
  return 0;
}

/// The message from the most recent failure **on the calling thread**, as a
/// NUL-terminated string. Valid until this thread's next failing call. The
/// handle argument is accepted for symmetry but unused.
const char *tft2_last_error(void *h) {
  (void)h;  // the slot is per-thread, not per-handle
  return t_last_error.c_str();
}

}  // extern "C"
