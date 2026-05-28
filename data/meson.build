desktop_file = i18n.merge_file(
  input: configure_file(
    input:  'dev.threedrip.ThreeDrip.desktop.in',
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
    input:  'dev.threedrip.ThreeDrip.metainfo.xml.in',
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
  'schemas' / 'dev.threedrip.ThreeDrip.gschema.xml',
  install_dir: datadir / 'glib-2.0' / 'schemas',
  rename: app_id + '.gschema.xml',
)

subdir('resources')
