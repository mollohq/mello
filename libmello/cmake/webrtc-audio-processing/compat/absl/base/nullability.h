// Compat shim: absl removed the `absl::Nullable` / `absl::Nonnull` C++
// wrapper templates (macros retained) while webrtc-audio-processing v2.1
// still names them (scoped_refptr.h, audio_processing.h, make_ref_counted.h,
// audio_processing_impl.h, aec_dump_factory.h). They are deliberately
// identity aliases here, matching the direction upstream took after v2.1
// (wrappers replaced by raw pointers plus attributes).
//
// Identity aliases additionally heal a genuine v2.1 skew that no absl
// version accepts: the base class declares
// `CreateAndAttachAecDump(absl::Nonnull<FILE*>, ...)` while the override
// declares `(FILE*, ...)` — distinct types with real wrappers, identical
// types with aliases, so the override resolves.
//
// Annotations carry no runtime behavior in this build; null safety at our
// own call sites is unaffected (we pass raw pointers / smart pointers).
//
// It shadows vcpkg's absl/base/nullability.h through include-path order
// (this compat dir precedes absl interface includes on the WAP target only)
// and carries the same include guard, so the real header becomes a no-op.
// Remove this file when the vendored tree moves past the wrapper types.
#pragma once

#ifndef ABSL_BASE_NULLABILITY_H_
#define ABSL_BASE_NULLABILITY_H_

#include "absl/base/config.h"

// ---- Upstream macro block, verbatim ----
#define ABSL_POINTERS_DEFAULT_NONNULL

#if defined(__clang__) && !defined(__OBJC__) && \
    ABSL_HAVE_FEATURE(nullability_on_classes)
#define absl_nonnull _Nonnull
#define absl_nullable _Nullable
#define absl_nullability_unknown _Null_unspecified
#else
// No-op for non-Clang compilers or Objective-C.
#define absl_nonnull
// No-op for non-Clang compilers or Objective-C.
#define absl_nullable
// No-op for non-Clang compilers or Objective-C.
#define absl_nullability_unknown
#endif

#if ABSL_HAVE_FEATURE(nullability_on_classes)
#define ABSL_NULLABILITY_COMPATIBLE _Nullable
#else
#define ABSL_NULLABILITY_COMPATIBLE
#endif
// ---- End upstream macro block ----

#include <cstddef>

namespace absl {

// Identity aliases, per the header note above.
template <typename T>
using Nullable = T;
template <typename T>
using Nonnull = T;

}  // namespace absl

#endif  // ABSL_BASE_NULLABILITY_H_
