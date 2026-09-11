// The shipped launcher always runs elevated on Windows. USB-tether route
// recovery and playing alongside an elevated game both need it, so asking once
// at launch replaces the mid-session "Restart as admin" detour. Only release
// builds embed the manifest: debug builds share it with the `cargo test`
// harness binary, which Windows would then refuse to start unelevated. Other
// targets ignore the manifest.
const WINDOWS_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"
      />
    </dependentAssembly>
  </dependency>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#;

fn main() {
    println!("cargo:rerun-if-env-changed=PROFILE");
    let release = std::env::var("PROFILE").is_ok_and(|profile| profile == "release");
    let mut attributes = tauri_build::Attributes::new();
    if release {
        attributes = attributes.windows_attributes(
            tauri_build::WindowsAttributes::new().app_manifest(WINDOWS_MANIFEST),
        );
    }
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}
