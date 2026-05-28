blueprints = custom_target(
  'blueprints',
  input: files(
    'ui/window.blp',
  ),
  output: ['window.ui'],
  command: [
    blueprint_compiler,
    'batch-compile',
    '@OUTDIR@',
    meson.current_source_dir(),
    '@INPUT@',
  ],
)

gnome.compile_resources(
  app_id,
  'resources.gresource.xml',
  dependencies: blueprints,
  gresource_bundle: true,
  install: true,
  install_dir: pkgdatadir,
)
