# Open Explorer

Gerenciador de arquivos de dois painéis para nuvem (começando pelo Google Drive), em Rust com
[Rune](https://github.com/anomalyco/rune). Hoje é um protótipo: painel esquerdo no disco local,
painel direito num Google Drive **simulado** em memória (sem login).

## Rodar

```sh
cargo run --release
```

O projeto usa o Rust 1.99.1 (`rust-toolchain.toml`), instalado pelo rustup. Se `rustc --version`
mostrar outra versão, há um Rust instalado fora do rustup antes dele no `PATH`: ponha
`%USERPROFILE%\.cargo\bin` primeiro ou desinstale o outro.

Para ver a interface sem abrir janela: `cargo run -- --snap tela.png --scene stress` (abre a pasta
de 50.000 arquivos).

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
