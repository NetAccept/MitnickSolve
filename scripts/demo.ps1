# One-command interactive demo for Windows.
# Starts a local OpenAI-compatible mock, then opens the TUI against a fresh daemon.
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

$python = Get-Command python -ErrorAction SilentlyContinue
if (-not $python) { $python = Get-Command py -ErrorAction SilentlyContinue }
if (-not $python) { throw "Python 3 is required for bench/mock_llm.py" }

if (-not (Test-Path "target/debug/envd.exe")) {
    Push-Location tools/envd
    go build -o ../../target/debug/envd.exe ./cmd/envd
    Pop-Location
}
cargo build --workspace --bins

if (Test-Path ".msk/demo") { Remove-Item -Recurse -Force ".msk/demo" }
$mock = Start-Process -FilePath $python.Source -ArgumentList "bench/mock_llm.py", "18080" `
    -PassThru -NoNewWindow -WorkingDirectory $repo
try {
    Start-Sleep -Milliseconds 600
    $env:MSK_LLM_BASE_URL = "http://127.0.0.1:18080/v1"
    $env:MSK_LLM_MODEL = "mock"
    $env:MSK_LLM_API_KEY = "demo-key"
    $env:MSK_PROVIDER = "openai"
    Write-Host ""
    Write-Host "MitnickSolve demo" -ForegroundColor Cyan
    Write-Host "  Type: exec      and press Enter; approve the exec tool with y."
    Write-Host "  Then try: file, secret, flag, board, spawn, or /help."
    Write-Host "  q quits the TUI." -ForegroundColor DarkGray
    Write-Host ""
    & target/debug/msk-tui.exe --root .msk/demo --provider openai
} finally {
    if ($mock -and -not $mock.HasExited) { Stop-Process -Id $mock.Id -Force }
}
