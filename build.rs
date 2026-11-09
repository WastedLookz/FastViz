use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/libraw_shim.c");
    let mut build = cc::Build::new();
    build.file("src/libraw_shim.c");
    // Include path from pkg-config (usually none is needed).
    if let Ok(out) = Command::new("pkg-config")
        .args(["--cflags", "libraw"])
        .output()
    {
        for flag in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            if let Some(dir) = flag.strip_prefix("-I") {
                build.include(dir);
                // The shim includes <libraw/libraw.h>, but libraw.pc points
                // at the include/libraw folder itself.
                if let Some(parent) = dir.strip_suffix("/libraw") {
                    build.include(parent);
                }
            }
        }
    }
    build.compile("fastviz_libraw");
    println!("cargo:rustc-link-lib=raw");
}
