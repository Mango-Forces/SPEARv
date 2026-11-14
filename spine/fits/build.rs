use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=SPEARV_CFITSIO_PREFIX");
    println!("cargo:rerun-if-changed=native/shim.c");
    println!("cargo:rerun-if-changed=native/fixture.c");
    let prefix = env::var_os("SPEARV_CFITSIO_PREFIX")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest"))
                .join("../../.build/native/cfitsio-4.7.0-prefix")
        });
    let prefix = prefix.canonicalize().unwrap_or_else(|_| panic!(
        "missing private CFITSIO 4.7.0 prefix at {}; build it with fits/build-cfitsio.sh or set SPEARV_CFITSIO_PREFIX; no system/bundled fallback",
        prefix.display()
    ));
    let include = prefix.join("include");
    for (name, hash) in [
        (
            "fitsio.h",
            "31bb09f0a57d9e29c65ae66ee2dd1d79272f0fd7668720889c9781c2c8ef95cf",
        ),
        (
            "fitsio2.h",
            "98067a68e0c3ef39d8bc3768d759458e8b8abb411ea051f4ee8a8dbee08c352f",
        ),
    ] {
        let path = include.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        let digest = Command::new("sha256sum")
            .arg(&path)
            .output()
            .expect("sha256sum is required for pinned native-header qualification");
        assert!(
            digest.status.success() && String::from_utf8_lossy(&digest.stdout).starts_with(hash),
            "{name} does not match pinned CFITSIO commit d6d27653740864a0483e74cff23c53e34e1dceee"
        );
    }
    let header = fs::read_to_string(include.join("fitsio.h")).expect("CFITSIO header");
    assert!(
        header.contains("#define CFITSIO_MAJOR 4")
            && header.contains("#define CFITSIO_MINOR 7")
            && header.contains("#define CFITSIO_MICRO 0"),
        "CFITSIO must be 4.7.0"
    );
    let archive = prefix.join("lib/libcfitsio.a");
    assert!(archive.is_file(), "private static libcfitsio.a is required");
    let metadata = fs::read_to_string(prefix.join("lib/pkgconfig/cfitsio.pc"))
        .expect("private CFITSIO build metadata is required");
    assert!(
        metadata.contains("Version: 4.7.0")
            && metadata.contains("Libs.private:  -lpthread -lz  -lm")
            && !metadata.contains("curl"),
        "native prefix must be static, reentrant and curl-disabled"
    );
    // --disable-curl alone still leaves CFITSIO's built-in HTTP/FTP parser on
    // Unix. Require a native-network-disabled archive, not merely no libcurl.
    let symbols = Command::new("nm")
        .args(["-g", "--defined-only"])
        .arg(&archive)
        .output()
        .expect("nm is required for native-network qualification");
    assert!(
        symbols.status.success(),
        "cannot inspect private native archive"
    );
    assert!(!String::from_utf8_lossy(&symbols.stdout).lines().filter_map(|line| line.split_whitespace().last())
        .any(|symbol| ["http_", "https_", "ftp_", "ftps_", "root_", "curl_"].iter()
            .any(|prefix| symbol.starts_with(prefix))), "CFITSIO native network parser is present; use the no-network configure cache flags in fits/build-cfitsio.sh");
    println!("cargo:rerun-if-changed={}", archive.display());
    cc::Build::new()
        .include(&include)
        .define("_REENTRANT", None)
        .file("native/shim.c")
        .file("native/fixture.c")
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .compile("spearv_fits_shim");
    println!(
        "cargo:rustc-link-search=native={}",
        prefix.join("lib").display()
    );
    for library in ["cfitsio", "pthread", "z", "m"] {
        println!(
            "cargo:rustc-link-lib={}{}",
            if library == "cfitsio" { "static=" } else { "" },
            library
        );
    }
}
