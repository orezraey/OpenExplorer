# Open Explorer

Gerenciador de arquivos de dois painéis para nuvem (começando pelo Google Drive), em Rust com
[Rune](https://github.com/anomalyco/rune). Cada painel mostra o disco local, um drive
**simulado** em memória ou uma conta do Google Drive conectada: Meu Drive e Drives compartilhados (por enquanto só listagem).

## Conectar o Google Drive

Para ler todo o Drive, o Google exige uma verificação de segurança do app. Até lá, cada pessoa
usa o próprio cliente OAuth (gratuito):

1. Em <https://console.cloud.google.com>, crie um projeto e ative a **Google Drive API**
   (APIs e serviços → Biblioteca).
2. Em **Tela de permissão OAuth** (Google Auth Platform), escolha público **Externo**, deixe em
   modo **Teste** e adicione o seu e-mail em **Usuários de teste**.
3. Em **Clientes** (Credenciais), crie um ID do cliente OAuth do tipo **App para computador** e
   baixe o JSON.
4. Salve o JSON como `%APPDATA%\OpenExplorer\google-client.json`. Ou defina
   `OPENEXPLORER_GOOGLE_CLIENT_ID` e `OPENEXPLORER_GOOGLE_CLIENT_SECRET`.
5. No app, clique em **+ Google Drive**: o navegador abre, você autoriza e a conta aparece no
   painel. Dá para conectar várias contas.

O token de renovação fica no Gerenciador de Credenciais do Windows (nunca em arquivo);
`%APPDATA%\OpenExplorer\accounts.json` guarda só e-mail e nome. Em modo Teste, o Google expira
o acesso a cada 7 dias: o app pede para conectar de novo.

## Pesquisar no computador

O campo de pesquisa de um painel local procura pelo nome em todos os discos NTFS fixos enquanto
você digita, como o Everything: um serviço do Windows (`openexplorer-index`) lê a tabela de
arquivos (MFT) de cada disco para a memória e a mantém em dia pelo diário USN do NTFS; o app o
consulta por um pipe. Na primeira pesquisa o painel oferece **Instalar o serviço de pesquisa**:
o Windows pede permissão de administrador uma vez, o serviço é copiado para
`%ProgramFiles%\OpenExplorer` e inicia com o Windows. Para remover:
`"%ProgramFiles%\OpenExplorer\openexplorer-index.exe" uninstall` como administrador.

No Linux não há MFT: o serviço (`openexplorer-index`, do próprio usuário, sem root) lista as
pastas em paralelo, vigia cada uma com o inotify e escuta num socket em `$XDG_RUNTIME_DIR`. O app
o inicia na primeira pesquisa; `openexplorer-index install` faz o systemd do usuário iniciá-lo
com a sessão (`uninstall` desfaz). O índice fica salvo em `~/.cache/openexplorer` e responde na
hora enquanto os discos são listados de novo, a cada início. Ficam de fora sistemas de arquivos
do kernel, de memória (`tmpfs`), de rede e imagens (`squashfs`).

No Windows, sem administrador, para testar: `cargo run -p openexplorer-index --release -- console --walk`
(lista as pastas em vez de ler a MFT) e o app com `OE_INDEX_CONSOLE=1`. Tempos de busca:
`cargo run -p openexplorer-index --release --example index_timing -- C:\ D:\`.

## Rodar

```sh
cargo run --release
```

O projeto usa o Rust 1.99.1 (`rust-toolchain.toml`), instalado pelo rustup. Se `rustc --version`
mostrar outra versão, há um Rust instalado fora do rustup antes dele no `PATH`: ponha
`%USERPROFILE%\.cargo\bin` primeiro ou desinstale o outro.

Para ver a interface sem abrir janela: `cargo run -- --snap tela.png --scene stress` (abre a pasta
de 50.000 arquivos) ou `--scene account` (uma conta de exemplo, sem rede).

## Código

- `crates/app`: a janela (Rune): painéis, seleção, arrastar e soltar.
- `crates/index`: o índice dos arquivos do computador e o serviço que o mantém (MFT, diário
  USN, pipe).
- `crates/gdrive`: login OAuth (PKCE com redirecionamento para 127.0.0.1), contas e tokens,
  chamadas da Drive API v3. `cargo test -p openexplorer-gdrive` testa o login sem rede.

## O que o protótipo testa

- Lista virtualizada (50.000 itens em *Meu Drive/Teste 50.000 arquivos*), ordenação por coluna.
- Seleção com clique, Ctrl, Shift, setas, Home/End, Ctrl+A; Enter abre pasta, Backspace sobe,
  Tab troca de painel.
- Arrastar entre painéis: local → Drive "envia" (simulado); Drive → local copia um arquivo de
  exemplo.
- Soltar arquivos vindos do Explorer em qualquer painel (no painel local eles são **copiados**
  para a pasta aberta, sem sobrescrever).
- Arrastar do app para o Explorer. Arquivos do Drive simulado viram arquivos de exemplo em
  `%TEMP%\OpenExplorer-drag` antes do arrasto, como um download faria.

## Rune

Não há versão estável do Rune: o projeto segue a branch `dev`, com o commit fixado no
`Cargo.lock`. Para atualizar: `cargo update -p rune`. O Rune exige que o app repita a tabela
`[patch.crates-io]` dele; se uma atualização adicionar uma entrada lá, adicione no `Cargo.toml`
daqui também.
