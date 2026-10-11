/// Embeds the 32-bit loader (`pol.exe`) so the launcher ships as one exe. The loader is built
/// separately (`tools/build-release.ps1`): a nested cargo from here would contend for locks.
fn embed_loader() {
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    println!("cargo:rerun-if-env-changed=CRYSTAL_LOADER_EXE");
    let explicit = std::env::var_os("CRYSTAL_LOADER_EXE").map(PathBuf::from);
    let source = explicit.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("crystal-loader/target/i686-pc-windows-msvc/release/pol.exe")
    });
    println!("cargo:rerun-if-changed={}", source.display());
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let release = std::env::var("PROFILE").as_deref() == Ok("release");
    let bytes = match std::fs::read(&source) {
        Ok(bytes) => bytes,
        Err(e) if explicit.is_some() || release => panic!(
            "\n\nThe 32-bit loader to embed is missing: {} ({e}).\n\
             Build it first (cd crystal-loader; cargo build --release --target i686-pc-windows-msvc),\n\
             or run tools\\build-release.ps1, or point CRYSTAL_LOADER_EXE at a pol.exe.\n",
            source.display()
        ),
        Err(_) => {
            // Debug builds and unit tests work without it; the launcher then embeds nothing.
            println!(
                "cargo:warning=no loader at {}; this build embeds none (release builds require it)",
                source.display()
            );
            Vec::new()
        }
    };
    let hash = if bytes.is_empty() {
        String::new()
    } else {
        Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect()
    };
    std::fs::write(out.join("embedded-pol.exe"), &bytes).unwrap();
    std::fs::write(out.join("embedded-pol.sha256"), hash).unwrap();
}

fn main() {
    embed_loader();
    println!("cargo:rerun-if-changed=launcher.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rerun-if-changed=assets/crystal.ico");
        embed_resource::compile("crystal.rc", embed_resource::NONE).manifest_required().unwrap();
        let template = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("launcher.manifest");
        // Unit tests and hidden smoke checks must run without elevation. The
        // delivered UI build explicitly enables the administrator manifest.
        let source = std::fs::read_to_string(template).unwrap();
        let source = if std::env::var_os("CARGO_FEATURE_ELEVATED_UI").is_some() {
            source
        } else {
            source.replace("requireAdministrator", "asInvoker")
        };
        let manifest = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
            .join("launcher.manifest");
        std::fs::write(&manifest, source).unwrap();
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        if std::env::var_os("CARGO_FEATURE_ELEVATED_UI").is_some() {
            println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'");
        }
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
