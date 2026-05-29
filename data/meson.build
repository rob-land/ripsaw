desktop_file = i18n.merge_file(
  input: configure_file(
    input:  'land.rob.ripsaw.desktop.in',
    output: app_id + '.desktop.in',
    configuration: { 'app-id': app_id },
  ),
  output: app_id + '.desktop',
  type: 'desktop',
  po_dir: '../po',
  install: true,
  install_dir: datadir / 'applications',
)

metainfo_file = i18n.merge_file(
  input: configure_file(
    input:  'land.rob.ripsaw.metainfo.xml.in',
    output: app_id + '.metainfo.xml.in',
    configuration: { 'app-id': app_id },
  ),
  output: app_id + '.metainfo.xml',
  type: 'xml',
  po_dir: '../po',
  install: true,
  install_dir: datadir / 'metainfo',
)

install_data(
  'schemas' / 'land.rob.ripsaw.gschema.xml',
  install_dir: datadir / 'glib-2.0' / 'schemas',
  rename: app_id + '.gschema.xml',
)

# App icons: ship multiple raster sizes into the hicolor theme dir so
# system pickers / shells can pick a sensible size. The icon name on
# disk must match app_id since the .desktop's Icon= field is @app-id@.
icon_sizes = ['64x64', '128x128', '256x256', '512x512']
foreach size : icon_sizes
  install_data(
    'icons' / 'hicolor' / size / 'apps' / 'land.rob.ripsaw.png',
    install_dir: datadir / 'icons' / 'hicolor' / size / 'apps',
    rename: app_id + '.png',
  )
endforeach

subdir('resources')
