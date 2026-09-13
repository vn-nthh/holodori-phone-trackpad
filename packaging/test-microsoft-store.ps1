[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$PackagePath,
    [Parameter(Mandatory)][string]$BundlePath,
    [Parameter(Mandatory)][string]$ExpectedPackageName
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression.FileSystem
$Archive = [IO.Compression.ZipFile]::OpenRead((Resolve-Path -LiteralPath $PackagePath).Path)
try {
    $Entries = @{}
    foreach ($Entry in $Archive.Entries) { $Entries[$Entry.FullName.Replace('\', '/')] = $Entry }
    if ($Entries.Keys | Where-Object { $_ -match '(?i)(\.apk$|\.pfx$|\.cer$|^Android/|AppxSignature\.p7x$)' }) {
        throw 'Submission MSIX must be unsigned and contain no Android package or certificate.'
    }
    foreach ($Required in @('AppxManifest.xml', 'AppxBlockMap.xml', '[Content_Types].xml',
        'Assets/StoreLogo.png', 'Assets/Square44x44Logo.png', 'Assets/Square150x150Logo.png', 'LICENSE')) {
        if (-not $Entries.ContainsKey($Required)) { throw "Missing package entry: $Required" }
    }
    foreach ($File in @('HolodoriUsbController.exe', 'Windows/holodori-native-host.exe')) {
        if (-not $Entries.ContainsKey($File)) { throw "Missing binary: $File" }
        $Stream = $Entries[$File].Open()
        try { $Actual = (Get-FileHash -InputStream $Stream -Algorithm SHA256).Hash }
        finally { $Stream.Dispose() }
        if ($Actual -ne (Get-FileHash -LiteralPath (Join-Path $BundlePath $File) -Algorithm SHA256).Hash) {
            throw "Packaged binary differs from validated portable build: $File"
        }
    }
    if (@($Entries.Keys | Where-Object { $_ -like '*.exe' }).Count -ne 2) {
        throw 'MSIX should contain only the launcher and native host executables.'
    }
    $Reader = [IO.StreamReader]::new($Entries['AppxManifest.xml'].Open())
    try { $Manifest = [xml]$Reader.ReadToEnd() } finally { $Reader.Dispose() }
    if ($Manifest.Package.Identity.Name -cne $ExpectedPackageName) { throw 'Package identity mismatch.' }
    $Version = [version]$Manifest.Package.Identity.Version
    if ($Version.Major -lt 1 -or $Version.Revision -ne 0) { throw 'Invalid Store version numbering.' }
    if ($Manifest.Package.Identity.ProcessorArchitecture -ne 'x64' -or
        $Manifest.Package.Applications.Application.Executable -ne 'HolodoriUsbController.exe' -or
        $Manifest.Package.Applications.Application.EntryPoint -ne 'Windows.FullTrustApplication') {
        throw 'Invalid desktop entry point or architecture.'
    }
    $Capabilities = @($Manifest.Package.Capabilities.Capability | ForEach-Object Name)
    if ($Capabilities.Count -ne 2 -or 'runFullTrust' -notin $Capabilities -or 'allowElevation' -notin $Capabilities) {
        throw 'Package must declare the current desktop and elevation capabilities.'
    }
    if ($ExpectedPackageName -eq 'Doritrack.PackagingTest' -and
        ($Manifest.Package.Identity.Publisher -ne 'CN=Doritrack Packaging Test' -or
         $Manifest.Package.Properties.DisplayName -notlike '*packaging test*')) {
        throw 'Development package must be visibly separate from the Store app.'
    }
} finally { $Archive.Dispose() }
Write-Host 'MSIX contents verified: identity, version, capabilities, entry point, icons, and exact host/launcher bytes.'
