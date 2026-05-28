# Build the Rust crate via cargo. Meson invokes cargo as a custom target so
# that the GNOME build pipeline (gresource, blueprint, gschemas) drives the
# whole install.

cargo_options = [
  '--manifest-path', meson.project_source_root() / 'Cargo.toml',
  '--target-dir',    meson.project_build_root() / 'cargo',
]

if get_option('buildtype') == 'release'
  cargo_options += [ '--release' ]
  rust_target_subdir = 'release'
else
  rust_target_subdir = 'debug'
endif

cargo_build = custom_target(
  'cargo-build',
  output: 'threedrip',
  build_always_stale: true,
  command: [
    cargo, 'build',
    cargo_options,
  ],
  install: true,
  install_dir: bindir,
)
