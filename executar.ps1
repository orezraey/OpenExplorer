# Compila o Open Explorer com o código atual e abre o app. Use executar.bat (dois cliques).
#
# Se o app desta pasta estiver aberto, pergunta antes de fechá-lo: o Windows não deixa
# substituir um .exe em uso. Só o que mudou é compilado de novo. O serviço de pesquisa é o
# próprio openexplorer.exe (iniciado como "openexplorer index"): não há outro executável.

$ErrorActionPreference = 'Stop'

function Sair($codigo) {
    if ($codigo -ne 0) { Read-Host 'Pressione Enter para fechar' | Out-Null }
    exit $codigo
}

# Qualquer erro inesperado é mostrado antes de a janela fechar.
trap {
    Write-Host ''
    Write-Host "Erro: $_" -ForegroundColor Red
    Sair 1
}

Set-Location $PSScriptRoot
$exe = Join-Path $PSScriptRoot 'target\release\openexplorer.exe'
# O cargo do rustup, que respeita o rust-toolchain.toml do projeto.
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (-not (Test-Path $cargo)) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Host 'O Rust não foi encontrado: instale o rustup (https://rustup.rs) e tente de novo.' -ForegroundColor Red
        Sair 1
    }
    $cargo = 'cargo'
}

$aberto = @(Get-Process openexplorer -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe })
if ($aberto.Count -gt 0) {
    Write-Host 'O Open Explorer está aberto e precisa ser fechado para ser atualizado.'
    Write-Host 'Cópias, envios e downloads em andamento na fila dele serão interrompidos.'
    $resposta = Read-Host 'Fechar agora? (S/n)'
    if ($resposta -match '^[nN]') { Sair 1 }
    # Pede para a janela fechar; se não fechar em 5 segundos, encerra.
    $aberto | ForEach-Object { [void]$_.CloseMainWindow() }
    $aberto | Wait-Process -Timeout 5 -ErrorAction SilentlyContinue
    $aberto | Where-Object { -not $_.HasExited } | Stop-Process -Force -ErrorAction SilentlyContinue
}

Write-Host 'Compilando o Open Explorer...'
& $cargo build --release -p openexplorer
if ($LASTEXITCODE -ne 0) {
    Write-Host ''
    Write-Host 'A compilação falhou: veja o erro acima.' -ForegroundColor Red
    Sair 1
}

Start-Process -FilePath $exe -WorkingDirectory $PSScriptRoot
Sair 0
