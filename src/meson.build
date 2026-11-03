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
    ],
    build_by_default: true,
    build_always_stale: true,
    console: true,
    install: true,
    install_dir: get_option('bindir'))
