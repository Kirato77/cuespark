//! Windows: embed the icon and the version info shown in Explorer's Properties.
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let numeric = version.replace('.', ",") + ",0";
    let icon = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets/icon.ico");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let rc = out.join("cuespark.rc");
    let res = out.join("cuespark.res");
    fs::write(
        &rc,
        format!(
            r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Kirato77"
      VALUE "FileDescription", "Cue Spark - show sound cue player"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "cuespark"
      VALUE "OriginalFilename", "CueSpark.exe"
      VALUE "ProductName", "Cue Spark"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
            icon = icon.display().to_string().replace('\\', "\\\\"),
        ),
    )
    .unwrap();
    // Zig's resource compiler is part of the cross toolchain (cargo zigbuild).
    let ok = Command::new("zig").args(["rc", "/fo"]).arg(&res).arg("--").arg(&rc).status().is_ok_and(|s| s.success());
    if ok {
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        println!("cargo:warning=zig rc unavailable: building without icon and version info");
    }
}
