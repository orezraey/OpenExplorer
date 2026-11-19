# Open Explorer

Gerenciador de arquivos de dois painéis para Windows, com Google Drive, MEGA, SFTP, FTP e WebDAV
integrados. Escrito em Rust.

[English](README.md)

## Recursos

* Dois painéis, cada um com sua árvore de pastas.
* Google Drive: várias contas, drives compartilhados, compartilhamento, estrela, lixeira. As
  pastas abertas se atualizam quando algo muda na web.
* MEGA: várias contas (com login em duas etapas também), Drive na nuvem, Lixeira com restaurar,
  pastas compartilhadas com você e links públicos de arquivos. Tudo cifrado e decifrado no seu
  computador; as pastas abertas se atualizam quando algo muda em outro lugar.
* OneDrive: contas pessoais, corporativas ou de estudante, links de compartilhamento, lixeira com
  desfazer (e, nas corporativas, a Lixeira pelo SharePoint). As pastas abertas se atualizam
  quando algo muda em outro lugar.
* SFTP, FTP/FTPS, WebDAV, Nextcloud e ownCloud.
* Copiar e mover entre qualquer um deles, com fila de transferências.
* Pesquisa instantânea por nome nos discos NTFS locais, parecida com o Everything. Também dá para
  pesquisar no Drive, no MEGA e nos servidores.
* Arrastar e soltar de e para o Explorer do Windows.
* Desfazer e refazer renomear, mover, excluir e copiar.
* Inglês e português.

## Instalação

Baixe a versão mais recente em [Releases](../../releases): o `setup.exe` instala o Open
Explorer com atalho no menu Iniciar e desinstalador, e o `portable.exe` roda sem instalar.
Nenhum dos dois é assinado ainda, então o SmartScreen do Windows avisa na primeira vez.

Há versões para Linux e macOS, mas são experimentais. A pesquisa no computador não existe no
macOS.

A primeira pesquisa local oferece instalar o serviço de pesquisa, o que pede permissão de
administrador uma vez. Para remover, rode como administrador:

```
"%ProgramFiles%\OpenExplorer\openexplorer-index.exe" index uninstall
```

## Google Drive

Enquanto o app não passa pela verificação do Google, você precisa do seu próprio cliente OAuth
(gratuito):

1. Crie um projeto no [console do Google Cloud](https://console.cloud.google.com) e ative a
   Google Drive API.
2. Configure a tela de permissão OAuth como Externo, em Teste, com a sua conta como usuário de
   teste.
3. Crie um ID do cliente OAuth do tipo App para computador e baixe o JSON.
4. No Open Explorer, vá em Adicionar conta → Google Drive e cole o JSON.

No modo Teste, o Google pede para entrar de novo a cada 7 dias.

## OneDrive

Contas pessoais, corporativas ou de estudante entram pelo navegador com um registro de
aplicativo seu (gratuito, sem segredo):

1. No [portal do Microsoft Entra](https://entra.microsoft.com), Registros de aplicativo → Novo
   registro, tipos de conta "Contas em qualquer diretório organizacional e contas pessoais da
   Microsoft".
2. Autenticação: adicione a plataforma "Aplicativos móveis e da área de trabalho" com o URI de
   redirecionamento `http://localhost`, e ligue "Permitir fluxos de clientes públicos".
3. Permissões de API: Microsoft Graph, delegadas, `Files.ReadWrite.All`, `offline_access` e
   `User.Read`. Para a lixeira de contas corporativas ou de estudante, também SharePoint,
   delegada, `AllSites.Write` (veja abaixo).
4. No Open Explorer, vá em Adicionar conta → OneDrive e cole o ID do aplicativo (cliente) (ele
   fica em `%APPDATA%\OpenExplorer\onedrive-client.json`; `OPENEXPLORER_ONEDRIVE_CLIENT_ID`
   também serve).

O Microsoft Graph não lista a lixeira de uma conta corporativa ou de estudante nem restaura dela,
então o Open Explorer a lê pela API REST do SharePoint, que precisa daquela permissão do
SharePoint. Com ela no registro, abra a Lixeira da conta e clique em Autorizar no navegador (ou
peça a um administrador que conceda o consentimento para a organização); daí em diante a Lixeira
lista o que foi excluído, de onde e quando, com Restaurar, Excluir permanentemente e Esvaziar
lixeira, e Desfazer traz de volta o que o Open Explorer excluiu. Sem ela a conta funciona como
antes, sem a Lixeira. Contas pessoais não têm Lixeira (o Graph não lista a delas); Desfazer
restaura o que foi excluído.

## Compilar

Instale o Rust pelo [rustup](https://rustup.rs). A versão do toolchain fica fixada no
`rust-toolchain.toml`.

```
cargo build --release
cargo test --workspace
```

O framework de interface, [Rune](https://github.com/anomalyco/rune), é usado a partir da branch
`dev` e fixado no `Cargo.lock`.
