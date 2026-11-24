# build script — ejecuta en la carpeta DiscordDumperGUI
# requiere rustup/cargo instalado: https://rustup.rs

cargo build --release
if ($LASTEXITCODE -eq 0) {
    Write-Host ""
    Write-Host "[done] binary -> $PSScriptRoot\target\release\discord_dumper_gui.exe" -ForegroundColor Green
    Start-Process "$PSScriptRoot\target\release\discord_dumper_gui.exe"
} else {
    Write-Host "[error] build failed" -ForegroundColor Red
}
