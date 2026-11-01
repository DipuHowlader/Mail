# Production build (Windows)

Run these commands from the `mailclient` directory in PowerShell.

## Standard production build

```powershell
npm.cmd run build:tauri
```

This runs the build preflight, regenerates the Windows icon, builds the Vite renderer into `dist`, packages the Tauri app, and creates the NSIS installer in `release`.

## Production build used for this release

The standard packager could not extract its Windows resource editor because this Windows environment did not allow it to create symbolic links. The fallback below builds the app in a fresh output folder, uses the already downloaded `rcedit` tool to set the executable icon and metadata, then creates the installer from that app folder.

### 1. Build and validate the app

```powershell
npm.cmd run build
```

This regenerates `src-tauri/icons/icon.ico`, runs the repository's packaging preflight, and builds the renderer into `dist`.

### 2. Package an unpacked app

```powershell
.\node_modules\.bin\tauri build.cmd --win --dir --config.win.signAndEditExecutable=false --config.directories.output=release-production
```

This packages the Windows x64 app into `release-production/win-unpacked`. The command override skips Tauri Builder's resource-editing step, which is applied manually in the next step.

### 3. Set the executable icon and version metadata

```powershell
$rcedit = Get-ChildItem '.cache\tauri build\winCodeSign' -Directory |
  ForEach-Object { Join-Path $_.FullName 'rcedit-x64.exe' } |
  Where-Object { Test-Path -LiteralPath $_ } |
  Select-Object -First 1
$appExe = (Resolve-Path 'release-production\win-unpacked\Novamail.exe').Path
$icon = (Resolve-Path 'src-tauri/icons/icon.ico').Path

& $rcedit $appExe `
  --set-version-string FileDescription 'Premium multi-account desktop email client for Google Workspace and Gmail accounts.' `
  --set-version-string ProductName 'Novamail' `
  --set-version-string LegalCopyright 'Copyright © 2026 IPPO' `
  --set-file-version 1.0.0 `
  --set-product-version 1.0.0.0 `
  --set-version-string InternalName 'Novamail' `
  --set-version-string CompanyName 'IPPO' `
  --set-icon $icon

if ($LASTEXITCODE -ne 0) { throw 'Could not update the Novamail executable resources.' }
```

This locates an extracted `rcedit-x64.exe` from Tauri Builder's cache and applies the same icon and version fields used by the standard build configuration.

### 4. Create the installer

```powershell
.\node_modules\.bin\tauri build.cmd --prepackaged release-production\win-unpacked --win nsis --config.directories.output=release-production
```

This creates the NSIS installer from the prepared app folder. The output files are:

- `release-production/NovaMail Setup.exe`
- `release-production/NovaMail Setup.exe.blockmap`

The installer from this build is not code-signed. Configure a Windows signing certificate in Tauri Builder before distributing it publicly.
