fn main() {
    let quickjs_files = [
        "quickjs/libregexp.c",
        "quickjs/libunicode.c",
        "quickjs/cutils.c",
        "quickjs/quickjs.c",
        "quickjs/libbf.c",
    ];

    println!("cargo:rerun-if-changed=quickjs/chidori_snapshot_stub.c");
    println!("cargo:rerun-if-changed=quickjs/chidori_snapshot.h");
    for file in quickjs_files {
        println!("cargo:rerun-if-changed={file}");
    }

    let mut build = cc::Build::new();
    build
        .define("_GNU_SOURCE", None)
        .include("quickjs")
        .warnings(false)
        .extra_warnings(false)
        .flag_if_supported("-Wno-implicit-const-int-float-conversion")
        .file("quickjs/chidori_snapshot_stub.c");

    for file in quickjs_files {
        build.file(file);
    }

    build.compile("chidori_quickjs");
}
