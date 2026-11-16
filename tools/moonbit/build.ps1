$ErrorActionPreference = "Stop"
Push-Location "$PSScriptRoot\mitnick_tools"
try {
  moon build --target wasm --release
  Copy-Item "_build\wasm\release\build\transform\transform.wasm" "..\..\wasm\transform.wasm" -Force
  Write-Output "wrote tools\wasm\transform.wasm"
} finally {
  Pop-Location
}
