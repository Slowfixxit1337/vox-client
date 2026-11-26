param(
    [Parameter(Mandatory)][string] $Exe,
    [Parameter(Mandatory)][string] $MakeAppx,
    [Parameter(Mandatory)][string] $Output,
    [Parameter(Mandatory)][string] $Publisher
)

$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path -LiteralPath $Exe).Path
$MakeAppx = (Resolve-Path -LiteralPath $MakeAppx).Path
$Output = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Output)
if (Test-Path -LiteralPath $Output) { throw "Output already exists: $Output" }
$stage = Join-Path (Split-Path -Parent $Output) ('vox-capture-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path (Join-Path $stage 'Assets') -Force | Out-Null
Copy-Item -LiteralPath $Exe -Destination (Join-Path $stage 'vox-client.exe')

[xml] $manifest = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'AppxManifest.xml') -Raw
$manifest.Package.Identity.SetAttribute('Publisher', $Publisher)
$manifest.Save((Join-Path $stage 'AppxManifest.xml'))

Add-Type -AssemblyName System.Drawing
foreach ($asset in @(
    @{ Name = 'StoreLogo'; Size = 50 }
    @{ Name = 'Square150x150Logo'; Size = 150 }
    @{ Name = 'Square44x44Logo'; Size = 44 }
)) {
    $size = [int] $asset.Size
    $bitmap = [Drawing.Bitmap]::new($size, $size)
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    $font = [Drawing.Font]::new('Segoe UI', ($size * 0.3), [Drawing.GraphicsUnit]::Pixel)
    $format = [Drawing.StringFormat]::new()
    try {
        $format.Alignment = 'Center'
        $format.LineAlignment = 'Center'
        $graphics.Clear([Drawing.Color]::FromArgb(37, 38, 49))
        $graphics.DrawString('Vox', $font, [Drawing.Brushes]::White,
            [Drawing.RectangleF]::new(0, 0, $size, $size), $format)
        $bitmap.Save((Join-Path $stage "Assets/$($asset.Name).png"))
    } finally {
        $format.Dispose()
        $font.Dispose()
        $graphics.Dispose()
        $bitmap.Dispose()
    }
}

& $MakeAppx pack /d $stage /p $Output
if ($LASTEXITCODE -ne 0) { throw "MakeAppx failed. Staging: $stage" }
