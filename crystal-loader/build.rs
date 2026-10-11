fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "i686-pc-windows-msvc" {
        // xiloader and Ashita's bootloader\pol.exe are both large-address-aware; FFXI's
        // memory use benefits from the 4 GB user space a 32-bit process gets on x64 Windows.
        println!("cargo:rustc-link-arg-bins=/LARGEADDRESSAWARE");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
