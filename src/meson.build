# The calculator page works out its answers with libqalculate, in a helper
# process the launcher starts for every expression (see
# rust/src/calculator/evaluation.rs). The launcher looks for the helper
# beside itself, so it also runs from the build tree, and otherwise where
# meson installs it.
add_languages('cpp', native: false)
calculator_helper_dir = get_option('libexecdir')
calculator_helper = executable('ilia-calculator',
    'calculator-helper.cc',
    dependencies: dependency('libqalculate', version: '>= 5.0'),
    install: true,
    install_dir: calculator_helper_dir)

# The launcher is the Rust crate under rust/, built by cargo with the
# locked dependency graph (see rust/meson-build.sh).
cargo = find_program('cargo')
cargo_build = find_program('../rust/meson-build.sh')

ilia_bin = custom_target('ilia',
    output: 'ilia',
    command: [
        cargo_build,
        cargo,
        join_paths(meson.project_source_root(), 'rust'),
        join_paths(meson.current_build_dir(), 'cargo-target'),
        '@OUTPUT@',
        join_paths(get_option('prefix'), calculator_helper_dir, 'ilia-calculator'),
    ],
    build_by_default: true,
    build_always_stale: true,
    console: true,
    install: true,
    install_dir: get_option('bindir'))
