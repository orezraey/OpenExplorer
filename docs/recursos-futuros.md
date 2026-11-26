# Recursos para implementar no futuro

Recursos que o Rune já oferece e que ainda não usamos. Cada item diz o que o Rune traz, onde
entraria no Open Explorer e o que falta decidir.

## Copiar e salvar imagem, e menu de imagem

**O que o Rune traz** (desde o commit 4918969f do Rune, novembro de 2026):

- `Tx::copy_image(&Image)` e `Tx::copy_image_then(&Image, ao_terminar)`: copiam uma imagem para
  a área de transferência fora da thread da interface. No Windows ela vai como PNG e como
  `CF_DIBV5`, para colar em qualquer programa. `CopyImageError` diz por que falhou (`NotReady`,
  `DecodeFailed`, `TooLarge`, `Superseded`, `Failed`).
- `El::image_menu(ImageMenu)`: o botão direito numa imagem mostra **Salvar imagem como…**,
  **Copiar imagem** e **Copiar endereço da imagem**. `ImageMenu::save_path` salva o arquivo
  original sem recodificar (SVG e GIF animado ficam intactos); `file_name` sugere o nome.
- `ClipboardImage::from_rgba`, `Image::to_rgba` e `Pixels::to_rgba` para quem já tem os pixels.

**Onde entraria:** num painel de pré-visualização de imagens (ainda não existe). Com ele, a
imagem mostrada ganharia o menu pronto. Também serviria para um "Copiar imagem" no menu de um
arquivo de imagem, sem abrir outro programa.

**A decidir:** se o painel de pré-visualização vem antes; se "Copiar imagem" no menu do arquivo
lê o arquivo do disco (local) ou baixa da nuvem primeiro.

## Menu "Abrir link" e "Copiar link"

**O que o Rune traz:** um link de verdade num texto (`Link::href(url)`, ou um `id` que comece com
`http:`, `https:` ou `mailto:`) ganha sozinho, no botão direito, **Abrir link** e **Copiar link**.
Funciona também com a tecla Menu, Shift+F10 e leitores de tela. `El::link_menu(false)` desliga.

**Onde entraria:** os endereços que hoje são botões e só abrem o navegador:

- my.telegram.org/apps, na conta do Telegram;
- o console do Google Cloud, na conta de servidor do Google Drive;
- o portal de apps do Dropbox e do OneDrive.

Como links, dariam para copiar o endereço e abrir em outro navegador.

**A decidir:** se o visual deles muda de botão para texto sublinhado.

## Interruptor no estilo Windows 11

**O que o Rune traz:** `Kit::metrics.switch_look` com `SwitchLook::flat` e `Switch::flat()`.
O interruptor perde o brilho e o aro do botão redondo, e o trilho ganha uma borda quando está
desligado. Fica perto do interruptor do Windows 11 (desligado, contorno com a bolinha escura;
ligado, trilho na cor de destaque com a bolinha branca); as cores exatas ainda seriam ajustadas
por nós.

**Onde entraria:**

- o interruptor "Qualquer pessoa com o link", na janela Compartilhar (já é o `Switch` do kit);
- os interruptores de Opções, que hoje desenhamos nós mesmos (`switch_row` em `ui.rs`): ou
  passam a usar o `Switch` do kit com esse visual, ou copiamos o desenho.

**A decidir:** se vale só no Windows ou em todos os sistemas (no macOS o interruptor nativo tem
outro desenho).
