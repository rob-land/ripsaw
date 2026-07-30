# Build the Rust crate via cargo. Meson invokes cargo as a custom target so
# that the GNOME build pipeline (gresource, blueprint, gschemas) drives the
# whole install.

cargo_target_dir = meson.project_build_root() / 'cargo'

# Map any optimised meson buildtype to cargo's --release (see cargo_build.py).
# Critical for performance: libmvc's MVC decode is ~13x slower in a debug build,
# so a debug binary makes 3D conversion painfully slow. Only an explicitly
# unoptimised buildtype (debug/plain) yields a debug cargo build.
if get_option('buildtype') in ['release', 'debugoptimized', 'minsize']
  rust_target_subdir = 'release'
else
  rust_target_subdir = 'debug'
endif

# A wrapper runs cargo and copies the binary to @OUTPUT@; cargo's --target-dir
# and Meson's custom_target output directory differ, so the copy is required
# for `meson install` to locate the binary.
cargo_build = custom_target(
  'cargo-build',
  output: 'ripsaw',
  build_always_stale: true,
  command: [
    find_program(meson.project_source_root() / 'build-aux/meson/cargo_build.py'),
    cargo,
    meson.project_source_root() / 'Cargo.toml',
    cargo_target_dir,
    rust_target_subdir,
    '@OUTPUT@',
  ],
  install: true,
  install_dir: bindir,
)
