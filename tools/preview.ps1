# Composes a throwaway copy of ui/ in which tools/dev-bridge.js is injected
# before app.js, so the exact production frontend can be opened in a browser
# for visual review. DEV TOOL ONLY: the shipped exe embeds ui/index.html,
# ui/styles.css and ui/app.js verbatim, with no stub and no build step.
#
#   pwsh -File tools/preview.ps1
#   then open ui/.preview/index.html

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$src  = Join-Path $root 'ui'
$out  = Join-Path $src '.preview'

New-Item -ItemType Directory -Force -Path $out | Out-Null

Copy-Item (Join-Path $src 'styles.css') $out -Force
Copy-Item (Join-Path $src 'app.js')     $out -Force
Copy-Item (Join-Path $root 'tools\dev-bridge.js') $out -Force

# Inject the stub immediately before app.js so __violaStub exists first.
$html = Get-Content (Join-Path $src 'index.html') -Raw
$html = $html.Replace(
    '<script src="app.js"></script>',
    '<script src="dev-bridge.js"></script>' + "`n" + '<script src="app.js"></script>')
Set-Content -Path (Join-Path $out 'index.html') -Value $html -NoNewline

Write-Host "preview -> $out\index.html"
