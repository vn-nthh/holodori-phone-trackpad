# App icons

The source PNGs are unchanged copies of the supplied Doritrack artwork:

| Source | Supplied filename | Use |
| --- | --- | --- |
| `android.png` | `Phone Logo.png` | Android launcher and adaptive icons |
| `pc.png` | `PC Logo Taskbar.png` | Windows executable, taskbar, and Start menu |
| `store.png` | `PC Logo - App Store.png` | MSIX package logo and Microsoft Store listing |

Android uses density-specific launcher icons plus an adaptive icon with a dark
background. The adaptive XML insets the foreground by 15% on each side to keep
the phone inside circular masks.
The PC launcher uses a multi-size Windows ICO and a PNG for other desktop targets.
MSIX uses the PC artwork for its 44px and 150px app icons and the separate Store
artwork for its 50px package logo. Upload `store-listing-300.png` as the **1:1 App
tile icon (300 x 300 pixels)** in Partner Center's Store listing. That listing
image takes priority over the package image; it does not replace the installed
app's taskbar icon. See the [Store packaging guide](../../packaging/MICROSOFT_STORE.md).

To regenerate with the launcher dependencies installed, run from the repository
root in PowerShell:

```powershell
$Tauri = '.\tauri-launcher\node_modules\.bin\tauri.cmd'
& $Tauri icon assets/icons/pc.png --output build/logo-assets/pc
& $Tauri icon assets/icons/android.json --output build/logo-assets/phone
& $Tauri icon assets/icons/store.png --output build/logo-assets/store --png 50 --png 300
foreach ($Icon in @('icon.ico', 'icon.png', 'Square44x44Logo.png', 'Square150x150Logo.png')) {
    Copy-Item "build/logo-assets/pc/$Icon" "tauri-launcher/src-tauri/icons/$Icon"
}
Copy-Item build/logo-assets/store/50x50.png tauri-launcher/src-tauri/icons/StoreLogo.png
Copy-Item build/logo-assets/store/300x300.png assets/icons/store-listing-300.png
Get-ChildItem build/logo-assets/phone/android -Directory |
    Where-Object Name -ne 'mipmap-anydpi-v26' |
    Copy-Item -Destination android-app/app/src/main/res -Recurse -Force
& $Tauri icon assets/icons/android.png --output build/logo-assets/phone/hdpi --png 72
& $Tauri icon build/logo-assets/phone/android/mipmap-xxxhdpi/ic_launcher_round.png --output build/logo-assets/phone/hdpi-round --png 72
Copy-Item build/logo-assets/phone/hdpi/72x72.png android-app/app/src/main/res/mipmap-hdpi/ic_launcher.png
Copy-Item build/logo-assets/phone/hdpi-round/72x72.png android-app/app/src/main/res/mipmap-hdpi/ic_launcher_round.png
```

Keep the generated Android resources, desktop/MSIX icons, and Store listing PNG
in version control so ordinary app builds do not need an icon-generation step.
Preserve the hand-authored adaptive XML when regenerating. The explicit 72px
outputs correct this CLI version's 49px legacy hdpi icons.
