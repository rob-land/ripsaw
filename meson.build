project(
  'ripsaw',
  'rust',
  version: '0.2.6',
  license: 'GPL-3.0-or-later',
  meson_version: '>= 1.4.0',
  default_options: ['warning_level=2'],
)

i18n = import('i18n')
gnome = import('gnome')

app_id = get_option('profile') == 'development' ? 'land.rob.ripsaw.Devel' : 'land.rob.ripsaw'
profile = get_option('profile')

prefix = get_option('prefix')
bindir = prefix / get_option('bindir')
datadir = prefix / get_option('datadir')
localedir = prefix / get_option('localedir')
pkgdatadir = datadir / 'ripsaw'

cargo = find_program('cargo', required: true)
blueprint_compiler = find_program('blueprint-compiler', required: true)

subdir('data')
subdir('src')

gnome.post_install(
  glib_compile_schemas: true,
  gtk_update_icon_cache: true,
  update_desktop_database: true,
)
