@echo off
rem Dois cliques: compila o Open Explorer com o codigo atual e abre o app (veja executar.ps1).
rem Usa o PowerShell 7 se estiver instalado, senao o do Windows.
set "PS=powershell"
where pwsh >nul 2>nul && set "PS=pwsh"
%PS% -NoProfile -ExecutionPolicy Bypass -File "%~dp0executar.ps1"
