fn main() {
    let library = pkg_config::Config::new()
        .atleast_version("3.0")
        .probe("tracker-sparql-3.0")
        .expect("libtracker-sparql-3.0-dev is required");

    assert!(
        library.libs.iter().any(|name| name == "tinysparql-3.0"),
        "tracker-sparql-3.0 must link libtinysparql-3.0"
    );
}
