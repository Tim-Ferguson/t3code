use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=src/native_bridge.c");
    println!("cargo:rerun-if-changed=vendor/oniguruma/src");
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") { return; }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let pointer_bytes: usize = env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap().parse::<usize>().unwrap() / 8;
    let windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let long_bytes = if windows { 4 } else { pointer_bytes };
    let mut config = format!("#define HAVE_STDINT_H 1\n#define HAVE_INTTYPES_H 1\n#define HAVE_SYS_TYPES_H 1\n#define SIZEOF_INT 4\n#define SIZEOF_LONG {long_bytes}\n#define SIZEOF_LONG_LONG 8\n#define SIZEOF_VOIDP {pointer_bytes}\n");
    if !windows { config.push_str("#define HAVE_UNISTD_H 1\n#define HAVE_SYS_TIME_H 1\n#define HAVE_SYS_TIMES_H 1\n#define HAVE_ALLOCA_H 1\n"); }
    fs::write(out.join("config.h"), config).unwrap();
    let source = PathBuf::from("vendor/oniguruma/src");
    let files = ["regparse", "regcomp", "regexec", "regenc", "regerror", "regext",
        "regsyntax", "regtrav", "regversion", "st", "reggnu", "unicode", "unicode_unfold_key",
        "unicode_fold1_key", "unicode_fold2_key", "unicode_fold3_key", "ascii", "utf8",
        "utf16_be", "utf16_le", "utf32_be", "utf32_le", "euc_jp", "sjis", "iso8859_1",
        "iso8859_2", "iso8859_3", "iso8859_4", "iso8859_5", "iso8859_6", "iso8859_7",
        "iso8859_8", "iso8859_9", "iso8859_10", "iso8859_11", "iso8859_13", "iso8859_14",
        "iso8859_15", "iso8859_16", "euc_tw", "euc_kr", "big5", "gb18030", "koi8_r",
        "cp1251", "onig_init"];
    let mut build = cc::Build::new();
    build.include(&out).include(&source).file("src/native_bridge.c").warnings(false);
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let minimum = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| {
            if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") { "11.0" } else { "10.12" }.into()
        });
        build.flag(format!("-mmacosx-version-min={minimum}"));
    }
    for file in files { build.file(source.join(format!("{file}.c"))); }
    build.compile("t3_oniguruma");
}
