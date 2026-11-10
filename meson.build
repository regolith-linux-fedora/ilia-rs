project('ilia',
  version: '0.19',
  default_options: [
    'buildtype=release',
    'b_ndebug=if-release'
  ])

find_program ('glib-compile-schemas')
# The Rust binary links these through its -sys crates; checking
# them here keeps a missing system library a configure-time error. The
# SPARQL library (tinysparql, formerly tracker-sparql) is not linked: the
# Files page loads it at run time.
dependency('gtk+-3.0')
dependency('gtk-layer-shell-0')

meson.add_install_script(join_paths(meson.current_source_dir(),'meson_scripts','install_data.sh'),join_paths(get_option('prefix'),get_option('datadir'),'ilia'),join_paths(meson.current_source_dir(),'data/local','','*'))

meson.add_install_script(join_paths(meson.current_source_dir(),'meson_scripts','install_data.sh'),join_paths(get_option('prefix'),get_option('datadir'),'doc','ilia'),join_paths(meson.current_source_dir(),'doc','','*'))

install_data('data/org.regolith-linux.ilia.gschema.xml', install_dir: join_paths(get_option('prefix'),get_option('datadir'), 'glib-2.0', 'schemas'))
meson.add_install_script('meson_scripts/install_schemas.sh')

subdir('src')
subdir('test')
