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

/// A `BufferCore` plus a slot for the last exception message, so a throwing
/// lookup becomes a return code plus a readable reason instead of unwinding
/// across the FFI boundary (which would be undefined behaviour).
struct Handle {
  explicit Handle(double cache_secs)
      : buffer(tf2::durationFromSec(cache_secs)) {}
  tf2::BufferCore buffer;
  std::string last_error;
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

/// Insert `T_parent_child` at `stamp_ns`.
///
/// `pose` is `{qw, qx, qy, qz, tx, ty, tz}`. `is_static` mirrors
/// `setTransform`'s static flag (`/tf_static` semantics: one entry, valid at any
/// time). Returns 0 on success, 1 if `setTransform` rejected the transform
/// (tf2's own validation: NaN, self-parent, empty frame id), 2 on an exception.
int tft2_set(void *h, const char *parent, const char *child,
             std::int64_t stamp_ns, const double *pose, int is_static) {
  Handle *self = static_cast<Handle *>(h);
  try {
    geometry_msgs::msg::TransformStamped t;
    t.header.frame_id = parent;
    t.child_frame_id = child;
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
    if (!self->buffer.setTransform(t, "tf_tree_differential", is_static != 0)) {
      self->last_error = "tf2 setTransform rejected the transform";
      return 1;
    }
    return 0;
  } catch (const std::exception &e) {
    self->last_error = e.what();
    return 2;
  } catch (...) {
    self->last_error = "unknown exception in setTransform";
    return 2;
  }
}

/// Look up `T_target_source` at `stamp_ns`, writing `{qw,qx,qy,qz,tx,ty,tz}`
/// into `out`. Returns 0 on success, non-zero on any tf2 exception (which is
/// the *expected* outcome for an extrapolation or a disconnected pair — read the
/// reason with `tft2_last_error`).
int tft2_lookup(void *h, const char *target, const char *source,
                std::int64_t stamp_ns, double *out) {
  Handle *self = static_cast<Handle *>(h);
  try {
    auto t = self->buffer.lookupTransform(target, source, time_point(stamp_ns));
    // w-last (tf2) -> w-first (tf_tree).
    out[QW] = t.transform.rotation.w;
    out[QX] = t.transform.rotation.x;
    out[QY] = t.transform.rotation.y;
    out[QZ] = t.transform.rotation.z;
    out[TX] = t.transform.translation.x;
    out[TY] = t.transform.translation.y;
    out[TZ] = t.transform.translation.z;
    return 0;
  } catch (const std::exception &e) {
    self->last_error = e.what();
    return 1;
  } catch (...) {
    self->last_error = "unknown exception in lookupTransform";
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

/// The message from the most recent failure on this handle, as a NUL-terminated
/// string owned by the handle. Valid until the next failing call.
const char *tft2_last_error(void *h) {
  return static_cast<Handle *>(h)->last_error.c_str();
}

}  // extern "C"
