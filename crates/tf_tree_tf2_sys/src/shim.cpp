// extern "C" bridge over `tf2::BufferCore` for the differential and benchmark
// harnesses. It links `-ltf2` alone: no rclcpp, no DDS. Exceptions never cross
// the boundary.
//
// Every pose is a `double[7]` `{qw, qx, qy, qz, tx, ty, tz}`, the order of
// `tf_tree_math::Iso3::to_bits`. tf2 is w-last; the transposition happens here,
// once, and is covered by a round-trip test.

#include <tf2/buffer_core.hpp>
#include <geometry_msgs/msg/transform_stamped.hpp>

#include <chrono>
#include <cstdint>
#include <cstring>
#include <new>
#include <string>

namespace {

/// The last exception message, per calling thread. Not a `Handle` member: one
/// buffer is shared by many reader threads, and a shared `std::string` slot would
/// be a data race.
thread_local std::string t_last_error;

/// Owns the buffer and nothing else, so it is shareable.
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
/// `pose` is `{qw, qx, qy, qz, tx, ty, tz}`; `is_static` is `setTransform`'s
/// static flag. Returns 0 on success, 1 if tf2 rejected the transform (NaN,
/// self-parent, empty frame id), 2 on an exception. Names are `std::string`s so
/// no per-call NUL-terminated string is charged to tf2.
int tft2_set_pre(void *h, const void *parent, const void *child,
                 std::int64_t stamp_ns, const double *pose, int is_static) {
  Handle *self = static_cast<Handle *>(h);
  try {
    geometry_msgs::msg::TransformStamped t;
    t.header.frame_id = *static_cast<const std::string *>(parent);
    t.child_frame_id = *static_cast<const std::string *>(child);
    // `stamp_ns` is validated non-negative by the Rust caller.
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
    // Stored once: a literal past the 15-byte SSO limit would allocate per call.
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
/// Avoids a per-call `std::string` temporary that a benchmark would charge to
/// tf2. Immutable after construction, so shareable across threads.
void *tft2_name_new(const char *s) { return new (std::nothrow) std::string(s); }

/// Free a name from `tft2_name_new`. Null is a no-op.
void tft2_name_free(void *n) { delete static_cast<std::string *>(n); }

/// Look up `T_target_source` using names from `tft2_name_new`.
///
/// The allocation-free path a native C++ user takes.
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

/// Whether tf2 believes a lookup would succeed, without throwing.
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

/// Drop every transform, keeping the handle.
void tft2_clear(void *h) { static_cast<Handle *>(h)->buffer.clear(); }

/// Everything `tft2_lookup_pre` does except the `BufferCore` call, with the same
/// argument types. Subtracting it isolates the shim's overhead. `volatile` and
/// the `out` write keep the optimiser from deleting it.
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

/// The most recent failure message on the calling thread, NUL-terminated; valid
/// until this thread's next failing call. `h` is unused.
const char *tft2_last_error(void *h) {
  (void)h;  // the slot is per-thread, not per-handle
  return t_last_error.c_str();
}

}  // extern "C"
