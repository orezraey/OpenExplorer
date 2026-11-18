# Open Explorer

Gerenciador de arquivos de dois painéis para Windows, com Google Drive, SFTP, FTP e WebDAV
integrados. Escrito em Rust.

[English](README.md)

## Recursos

* Dois painéis, cada um com sua árvore de pastas.
* Google Drive: várias contas, drives compartilhados, compartilhamento, estrela, lixeira. As
  pastas abertas se atualizam quando algo muda na web.
* SFTP, FTP/FTPS, WebDAV, Nextcloud e ownCloud.
* Copiar e mover entre qualquer um deles, com fila de transferências.
* Pesquisa instantânea por nome nos discos NTFS locais, parecida com o Everything. Também dá para
  pesquisar no Drive e nos servidores.
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

## Compilar

Instale o Rust pelo [rustup](https://rustup.rs). A versão do toolchain fica fixada no
`rust-toolchain.toml`.

```
cargo build --release
cargo test --workspace
```

O framework de interface, [Rune](https://github.com/anomalyco/rune), é usado a partir da branch
`dev` e fixado no `Cargo.lock`.
