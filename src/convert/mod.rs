// 3D conversion pipeline.
//
// For an MVC input (separate base + dependent view) the conversion is
// gated on the libmvc decoder landing -- see docs/mvc3d.md. For an
// already-packed stereo source (side-by-side / over-under / frame-
// sequential), conversion to other layouts is just an ffmpeg stereo3d
// filter invocation; we run that path today.

pub mod format;
pub mod plan;
pub mod runner;
