# Files explorer: design note

Status: proposal, revised 2026-11-10 after the owner's decisions. Nothing here is built. **Decided** marks what the owner settled; **default** marks what is taken as recommended until the owner says otherwise; the texts and the questions in the last section are still open. **GUESS** marks a statement not yet checked against a primary source or the code; **UNPROVEN** marks one that needs a run to confirm. Flow IDs refer to `docs/SPEC.md`, contracts to `docs/CONTRACTS.md`, wire items to `docs/PROTOCOL.md`.

## 0. The order and the answer in short

The request: copying and sending files between the two computers should look and work like the Windows file explorer (the whole view, dragging to move inside one computer, choosing where a download lands on the controlling computer, and sending from the controlling computer to the controlled one).

The design:

1. **Where (decided):** a full **Arquivos view inside the session window**. The toolbar's folder button switches the window body from the picture to the explorer and back; the session, its top bar and its transfers stay as they are. It replaces the drawer's Arquivos tab.
2. **Layout (decided):** a **navigation pane** on the left (the places and the drives or volumes of each computer, grouped by computer) and **two explorer panes side by side**: this computer on the left, the other computer on the right. Each pane has back, forward and up, a breadcrumb address bar, a filter field, a details list (Name, Size, Type, Modified) and its own status line. A command bar on top, a transfers bar at the bottom.
3. **Reach (decided):** on the other computer the explorer reaches **every drive and volume, with the rights of the user logged in there**, not only the home folder. Paths name a volume by a token the other computer published; the helper still checks every component, and the operating system's permissions remain the outer boundary (section 8).
4. **Copy across** with a button ("Copiar para o MacBook" / "Copiar para este computador"), with the in-app clipboard (copy in one pane, paste in the other) and by dragging between the panes. A download lands in the folder open in the left pane.
5. **Manage inside one computer** like an explorer: new folder, rename, move, delete to the bin, properties. On this computer the interface does it itself; on the other computer the user helper does it through new messages of protocol minor 1, and only when the session holds **`FILES` and `CONTROL` (decided)**; with `FILES` alone the explorer browses and copies. **Delete on the other computer goes to its bin only (decided)**; where that place has no bin, nothing is deleted and a notice says why.
6. **Drag from the system's file manager** into either pane or onto the remote picture (upload) in 1.1; **drag out to the system's file manager** in 1.2 (default), with a working interim.
7. **Never** a move across computers (default), and nothing new reaches a peer of minor 0: against a 1.0 peer the explorer sees only that computer's home folder, browses it, copies both ways and downloads whole folders; drives and volumes, uploading folders, new folder, rename, move, delete and replace are unavailable, with one sentence that says why.

## 1. What exists today, and what happens to it

| Part | Today | In this design |
|---|---|---|
| Design board `docs/design/Arquivos.dc.html` | the drawer (448 px): folder caption, "Pasta acima", "Trocar pasta", rows with Abrir/Baixar, "Salvar em", the dashed "Escolher arquivos" box, the transfers list | **replaced** by a new board of the explorer view; its transfer rows (progress, "Pausado: a conexão caiu", "Já existia ... salvo como", "Chegou diferente do original e foi apagado") are **kept** in the transfers panel |
| Painted screen `crates/swapdesk-paint/src/screens_files.rs` (`drawer_files`) | one column, rows of 46, pages of 5 entries with "Anteriores" and "Mais itens" | **replaced** by `screens_explorer.rs`; the drawer loses its Arquivos tab (Terminal, Chat and Copiar e colar stay); the old file is removed once the explorer ships (R-UI-12) |
| State `crates/swapdesk-ui/src/interact/files_panel.rs` | `FilesPanel { recv_dir, waiting_download, page }`, `FILES_PAGE = 5`, controls `FilesChange`, `FilesUp`, `FilesRow`, `FilesPage`, `FilesChoose`, `FilesRecvFolder`, `TransferResume`, `TransferCancel` | **replaced** by an `Explorer` state (two panes, selection, history, sort, filter, queue, drag, places); `TransferResume`, `TransferCancel` and the `TransferView` model are **kept** |
| Commands `FilesList`, `FilesSend`, `FilesGet`, `TransferPause`, `TransferResume`, `TransferCancel` (UI ops 40 to 45) | as in C2.4 "Files" | **kept unchanged**; they carry everything slice 1 needs. New ops 46 to 48 for slice 2b |
| Engine `crates/swapdesk-files` | offers, resume from the verified length, SHA-256 check, never overwrite (`name (1).ext`), `PipeFs` and `FsServer` with virtual paths, `Roots::Home` and `Roots::Tokens` | **kept**; the helper's server gains volume roots (section 8.1) and the calls of section 7.4; the UI's `Roots::Tokens` server refuses every new call |
| Host disk proxy `crates/swapdesk-host/src/service/files.rs` | the D13 gate (`FilesGate`), `ViewerFiles`, events to the UI | **kept**; the new file messages pass the same gate and the permission rule of section 3.3 |
| Mock scenarios `arquivos`, `arquivos-carregando` | the drawer, filled and loading | **replaced** by the `explorador-*` scenarios of section 10 |
| The rule that the service never receives a path of the viewer's own disk (C2.3) | the UI reads the files it sends and writes the files it receives | **kept and extended**: the left pane lists, renames, moves and deletes this computer's files in the UI process itself; nothing about this computer's disk ever reaches the service, except the tokens that already exist |
| The helper's root on the controlled side | the user's home folder only (`Roots::Home`) | **widened (decided)**: the home folder and every volume the helper publishes, each a root of its own (section 8.1) |

**The 1.1 plan's files stage** ("live panel on the Mac; free-space check before accepting; names across systems (NFC/NFD, case collisions); a receive folder without macOS permission") is absorbed as follows:

- *Live panel on the Mac*: the explorer is a shared screen in `swapdesk-paint`, so the Mac paints it with the same code; that part of the stage becomes the explorer's slice 1, proven on both systems.
- *Free-space check before accepting* stays in the engine (receiver side, F-FILE-03) as planned. The explorer adds a pre-check (it knows the free space of the destination volume: this computer's from the OS, the other computer's from the 1.1 volume list) and shows it in each pane's header.
- *Names across systems* stays in the engine (save under a new name, never overwrite). The explorer adds a pre-flight: before a copy to a Windows machine it flags names Windows cannot hold (section 7.5) and offers to replace the characters.
- *A receive folder without macOS permission* becomes the explorer's error state for a folder or volume macOS does not let Swapdesk read or write, on either side (section 2.9).

So that stage keeps its engine work and its cross-machine proof, and its user-interface part moves into the explorer slices of section 10.

## 2. The screen

### 2.1 Where it lives (decided: a full view in the session window)

The view is **Arquivos**, entered from the toolbar's folder button (today it opens the drawer's tab) and left with the command bar's "Tela" button or the same toolbar shortcut. In full screen (D30) the view fills the screen the same way. One new screen in `swapdesk-paint`, no new window kind. The picture is not visible while the explorer is open; the stream keeps running, so going back is instant. The shells hide the video layer when the frame reports no video rectangle (GUESS: the Mac's video view sits under the transparent canvas and follows `Output.video`; slice 1 checks it is hidden when that is `None`). While the view is open the keyboard belongs to the explorer: no key goes to the other computer (the grab is released the way the drawer's terminal and chat already take the keys; GUESS on the exact mechanism, checked in slice 1).

### 2.2 Panes (decided: two panes side by side, with a navigation pane)

This computer on the left, the other computer on the right, the same order as the device screen's "you control" strip. The copy direction is visible, a drag across is natural, and "where does the download go" is answered by what the left pane shows. At the 900 px minimum each pane has about 330 px, so columns drop by width (below).

### 2.3 Geometry

All numbers in DIPs, as `swapdesk-paint` lays out every screen (`Painter`, `Frame`, `layout`, `text` and the palette of `swapdesk_ui::tokens`). Both systems paint the same frame; only the window chrome is the OS's. Design size 1200 x 760 (`layout::DESIGN_W`, `DESIGN_H`). No `Painter` change is needed: rows, icons and states use the existing fills, strokes, SVG paths and labels.

```
y=0    +------------------------------------------------------------------------------------+
       | top bar (existing, 40): status dot, peer name, "você controla há 18 min", stats     |
y=40   +------------------------------------------------------------------------------------+
       | command bar (56): [<- Tela] | [+ Nova pasta][Renomear][Excluir][Propriedades]   [Copiar para MacBook ->] |
y=96   +---------------+--------------------------------------+-----------------------------+
       | navigation    | pane: Este computador                | pane: MacBook do escritório |
       | pane (208)    | header 44 / nav row 40 / columns 28  |  (same parts)               |
       | ESTE COMPUT.  | rows of 30 ...                       |                             |
       |  Início ...   |                                      |                             |
       |  Disco (C:)   |                                      |                             |
       | MACBOOK ...   | status line 28                       |                             |
       |  Macintosh HD +--------------------------------------+-----------------------------+
y=H-44 | transfers bar (44): Transferências, 2 em andamento, 1,1 de 1,8 GB, 94 MB/s, falta 1 min  [Mostrar] |
y=H    +------------------------------------------------------------------------------------+
```

- **Top bar**: unchanged (`layout::VIEWER_BAR_H` = 40).
- **Command bar**: `Rect(0, 40, W, 56)`, fill `surface`, one hairline at the bottom in `line`. Buttons are the existing 40 px buttons at y = 48, gap 8. Left: "Tela" (`Kind::Default`, picture icon). Then a separator 1 x 24 in `line` at y = 56, 12 px each side. Then the management commands as quiet buttons with an 18 px icon (12 padding, 8 between icon and label, `text::BTN`): Nova pasta, Renomear, Excluir, Propriedades. Right edge (x = W - 16 - width): the one primary button (`Kind::Primary`) "Copiar para {peer}" with an arrow pointing right when the focused pane is this computer, "Copiar para este computador" with an arrow pointing left when it is the other computer. Below 1100 px of width the quiet buttons show the icon only (40 x 40) and get the session bar's hint card (`Output.hint`) on hover. A command that cannot act now is drawn **disabled**: fill `control`, ink `soft`, no hover tint, not in the Tab order but kept in the accessibility tree with its reason as its description. Disabled is new in `Frame` (a flag on `button`), not a new `Painter` call.
- **Navigation pane**: `Rect(0, 96, 208, H - 96 - 44)`, fill `sunken`, a hairline on its right edge, scrolling on its own when it overflows. Padding 12; group headings in `text::GROUP` (12/600) `soft` at 16 above their first row ("Este computador", then the peer's name); rows 32 high, radius 6, a 16 px icon at x + 10 and the label in `text::BODY` at x + 34, ellipsised; a removable or network volume has a 16 px eject-style mark at the right edge that is information only (no eject command in 1.1). The place whose folder a pane shows is selected (`link_tint` fill, `link` ink). 16 between groups. Each group holds, in order:
  - **Places**: this computer: Início, Área de trabalho, Documentos, Downloads; the other computer: Pasta pessoal, Área de trabalho, Documentos, Downloads, Recebidos do Swapdesk (`Downloads/Swapdesk`).
  - a hairline, then **drives or volumes**, in the order the system gives them: on Windows the drive letters, labelled "{label} ({letter}:)" or, without a label, by kind ("Disco local (C:)", "Unidade removível (E:)", "Unidade de rede (Z:)"); on the Mac the startup volume ("Macintosh HD" or its own name) and every mounted volume the Finder shows. A volume shows its free space as a tooltip-style hint on hover, not in the row.
  - **Volumes come and go**: when a drive or volume appears (a USB stick, a disk image, a mounted share) its row is added at its place in the order, without moving the rows above it, and when one goes away its row is removed; a pane showing a folder on a volume that went away switches to its error state (section 2.9). For this computer the shells report the change (GUESS on the APIs: device-change messages on Windows, the workspace's mount and unmount notifications on the Mac; read in slice 1); for the other computer a 1.1 helper pushes the new list (section 7.3). A 1.0 peer has no drives in its group.
- **Panes area**: x from 208 to W, padding 12, gap 12. Pane width `pw = (W - 208 - 36) / 2`; at 1200 that is 478, at 900 it is 328. Pane height `H - 44 - 12 - 108`, top at 108. Each pane is a panel (`Frame::panel`, radius 10, fill `surface`, border `line`); the pane that has the keyboard draws its border in `link` at 1.5 px.
- **Pane header** (44): a 20 px laptop glyph (`Frame::laptop`) at x + 14, the pane's title in `text::NAME` beside it; at the right, `text::CAPTION` `soft`, the free space of the shown folder's volume ("120 GB livres") when it is known (this computer always; the other computer from a 1.1 peer).
- **Navigation row** (40, from y + 44): three icon buttons of 32 x 32, radius 8, quiet with the hover tint: back at x + 8, forward at x + 44, up at x + 80. The **address bar** from x + 120 to the filter field minus 8, 32 high, bordered radius 8 (`Frame::edit_field` look): the first segment is the volume ("Disco local (C:)", "Macintosh HD") or the home folder ("Pasta pessoal"), then the folders, in `text::BODY` with a 12 px chevron in `soft` between them; each segment is a control that opens that folder; when they do not fit, the first ones after the volume fold into a "…" segment that opens a menu of them (`Effect::Menu`). Up from a volume's root does nothing (the button is disabled); up from the home folder's first level goes to the home folder, and up from the home folder goes to the folder that contains it on its volume (a 1.1 peer; on a 1.0 peer the home folder is the top). A click on the empty part of the bar turns it into a native text field (an edit slot) holding the path as the computer spells it (`C:\Users\ana\Documents`, `/Users/ana/Documents`); Enter goes there when the path lies on a published volume, Esc returns to the breadcrumbs. The **filter field** at the right: 168 wide, 32 high, placeholder "Filtrar nesta pasta"; below a pane width of 420 it becomes a 32 x 32 search icon that opens the field over the address bar.
- **Column header** (28, from y + 84): fill `sunken`, hairline at the bottom; labels in `text::TABLE_HEAD` (12/600) `soft`. Name from x + 40; Size 88 wide, right-aligned; Type 128; Modified 132. The sorted column shows a 12 px chevron after its label (up for ascending). Columns drop by pane width: Type below 560, Modified below 400; Name keeps at least 140.
- **Rows** (30 each, from y + 112): a 16 px icon at x + 14 (centred), the name in `text::BODY` from x + 40, ellipsised; Size, Type and Modified in `text::CAPTION` `soft`. A folder's Size is empty (Explorer does the same). Selected rows: fill `link_tint`, radius 4, inset 4 horizontally. Hover: `ink` at 0.04. Rows marked by a pending "cut": icon and text at 50% alpha. A link or junction has the link badge on its icon. The focused row draws the existing focus ring only after keyboard navigation (`focus_visible`).
- **Status line** (28, the pane's bottom, hairline above): `text::CAPTION` `soft` at x + 14: "48 itens" or "3 selecionados, 12,4 MB"; at the right, when a 1.0 peer cut the listing short, "Mostrando os primeiros 2000".
- **Transfers bar**: `Rect(0, H - 44, W, 44)`, fill `surface`, hairline on top. At x + 16 a 16 px transfer icon, "Transferências" in `text::NAME`, the summary in `text::CAPTION` `soft` ("2 em andamento, 1,1 de 1,8 GB, 94 MB/s, falta 1 min"), an overall bar 160 x 6 (radius 3, the drawer's `bar` helper) after the summary; at the right a 32 high "Mostrar" / "Ocultar" button. Hidden when the session has no transfer. Expanded, it becomes a panel 264 high (bar included) that pushes the panes up (the layout reflows; nothing overlaps), listing the transfers in rows of 58 (`TRANSFER_H` of today), newest first, scrolling past four.

**Icons** are the design's own 24-unit SVG paths, the same on both systems (never the OS's file icons, which differ): folder (exists, `icons::FOLDER`), file, image, video, audio, archive, document, code, application, link badge, drive, removable drive, network drive, home, desktop, download, back, forward, up, search, refresh, new folder, rename, trash, information, picture ("Tela"), transfer. The kind of a file comes from the extension through one table in `swapdesk-ui` (the same table names the Type column).

### 2.4 Sorting, the Type column, dates

- Folders first, then files; within each group by the sorted column. Name order is natural (a number inside a name compares by value, so "foto 2" comes before "foto 10"), case-insensitive, computed by Swapdesk's own code so both systems sort the same way. Clicking a column header sorts by it; clicking again reverses. The choice is remembered per pane for the session.
- Type: from the extension through the table above ("Documento PDF", "Imagem PNG", "Pasta", ...), else "Arquivo {EXT}" in capitals, else "Arquivo".
- Modified: `dd/mm/aaaa hh:mm` in pt-BR and `MMM d, yyyy h:mm a` in en, in this computer's time zone, through `swapdesk_ui::format`.

### 2.5 Hidden and internal entries (default: hidden, with a per-pane switch)

- Names starting with "." and the engine's own `*.swapdesk-part` and `*.swapdesk-meta` files are hidden. A 1.1 peer also flags entries hidden by the system (the Windows hidden and system attributes, the Mac's hidden flag), which hides `NTUSER.DAT`, `$Recycle.Bin`, `System Volume Information` and their kind; a 1.0 peer cannot say, so only the name rule applies there. "Mostrar itens ocultos" in the pane's "…" menu shows them, per pane, for the session.
- A partial file of a transfer that is still being received shows as its final name with a "recebendo" caption and no actions until it finishes.

### 2.6 Responsiveness (SPEC R-UI-1 to R-UI-12)

- **R-UI-1 placeholders**: a folder whose listing takes over 100 ms shows 8 placeholder rows (an icon block of 16 x 16, a name block of 120 to 220 x 12, a size block of 48 x 12, `Frame::block`) under the real header and column header, so nothing moves when the rows arrive. Under 100 ms nothing is shown in between.
- **R-UI-3**: a second request for the folder being listed joins the first; going back to a folder listed in the last 30 s shows the cached listing at once and refreshes it in the background (GUESS on the 30 s).
- **R-UI-4**: the filter acts 150 ms after the last key; it filters the loaded listing by name, case- and accent-insensitive.
- **R-UI-5**: the list draws only the visible rows plus 10 on each side, so a folder of 2 000 or 20 000 entries scrolls at the same cost. A 1.1 peer sends a long folder in pages of up to 2000 as the list scrolls toward the end (section 7.3); a 1.0 peer sends up to 2000 entries once and the status line says the rest is not shown.
- **R-UI-7**: a remote listing fails visibly after 8 s (the existing `FilesList` timeout); a remote operation after 30 s; a local listing never blocks the window. A slow volume (a network share, a sleeping disk) shows placeholders, then the timeout error, never a frozen pane.
- **R-UI-8**: this computer's disk is read and changed on a worker thread of the UI, never on the UI thread.
- **R-UI-10**: every listing and operation slower than its threshold is traced with its name and duration, never a path or name.
- **R-UI-11**: the explorer's state, the local listing and the remote listing are made the first time the view opens.

### 2.7 First open and memory

At its first open in a session the left pane shows this computer's Downloads folder (or the folder it last showed for this peer, kept in the per-user UI settings) and the right pane the other computer's home folder (or the last folder for this peer, when its volume is still published; otherwise the home folder). Back and forward histories are per pane, for the session; an entry on a volume that went away is skipped.

### 2.8 Empty states

- An empty folder: in the rows area, centred, the folder icon at 32 px in `soft` and "Esta pasta está vazia." in `text::BODY` `soft`.
- A filter with no match: "Nada nesta pasta tem “{0}” no nome."
- No transfer: the transfers bar is not drawn.

### 2.9 Error states

In the rows area, centred: a 32 px alert icon in `soft`, a title in `text::NAME` and the reason in `text::BODY` `soft`, then the buttons: "Voltar" on the left, "Tentar de novo" on the right (positive action on the right).

| Cause | Reason line |
|---|---|
| the folder does not exist any more (`NOT_FOUND`) | "Ela não existe mais. Pode ter sido movida ou apagada." |
| its volume went away (`VOLUME_GONE`, or this computer's shell reported the unmount) | "O disco {0} foi removido do {1}." / for this computer: "O disco {0} foi removido deste computador." Buttons: "Voltar" and "Ir para a pasta pessoal" |
| the user cannot read it (`IO_FAILED` on a listing) | "O {0} não deixou ler esta pasta." / for this computer: "Este computador não deixou ler esta pasta." |
| macOS has not given Swapdesk access (this computer is a Mac, the folder or volume is protected) | "O macOS não deu ao Swapdesk acesso a esta pasta." with "Abrir Ajustes do Sistema" in place of "Tentar de novo" |
| the other computer is a Mac and its agent was refused (GUESS: the host side cannot tell this apart from `IO_FAILED` today; macOS also protects removable and network volumes, GUESS until read on an Apple page) | "O macOS do {0} não deu ao Swapdesk acesso a esta pasta. Alguém no {0} precisa permitir em Ajustes do Sistema." |
| no answer in time (`TIMEOUT`) | "O {0} não respondeu a tempo." |
| D13: the other computer's user said no | "O usuário do {0} não deixou abrir os arquivos." (the pane stays in this state for the session, as D13 holds the answer) |
| `FILES` not granted | "O {0} não deu a este computador permissão para arquivos." (no buttons; the copy commands are disabled) |

While the session reconnects (F-NET-01), the right pane is dimmed under "Reconectando ao {0}…" and keeps its last listing; the left pane stays usable.

## 3. Selection and commands

### 3.1 Selection

| Gesture | Windows | Mac |
|---|---|---|
| select one | click | click |
| add or remove one | Ctrl+click | Command+click |
| range from the anchor | Shift+click | Shift+click |
| select all | Ctrl+A | Command+A |
| move the focus and select | arrows, Home, End, Page Up/Down | the same |
| extend | Shift+arrows | Shift+arrows |
| move the focus without selecting, then toggle | Ctrl+arrows, Ctrl+Space | Command+arrows, Command+Space (GUESS: the Finder has no such gesture; the same keys with Command) |
| jump by name | type the first letters | the same |
| box selection | drag on the empty part of the list | the same |
| clear | click on empty space, Esc | the same |

One behaviour on both systems; the keys are named the way each system names them, as the viewer shortcuts already are. Tab moves between the navigation pane, the left pane's parts, the right pane's parts, the command bar and the transfers bar; inside a list the arrows move.

### 3.2 Commands and where they live

"Here" = this computer (the UI does it on its own disk). "There" = the other computer (the user helper does it, as the logged-in user, on any published volume).

| Command | Command bar | Context menu | Windows keys | Mac keys | Acts on | Exists today |
|---|---|---|---|---|---|---|
| Open a folder | | Abrir | Enter, double click | Command+Down, Enter, double click | either pane | yes (`DirRequest`, home only) |
| Open a file | | Abrir | Enter, double click | the same | here only, with the system's app; there: not offered, the status line says "Para abrir no {0}, use a tela." (default) | here: new, local |
| Back, Forward, Up | pane buttons | | Alt+Left, Alt+Right, Alt+Up | Command+[, Command+], Command+Up | either | new (state only) |
| Refresh | pane "…" menu | Atualizar | F5 | Command+R | either | yes (list again) |
| Go to an address | address bar | | Ctrl+L, Alt+D | Command+L | either | yes |
| Filter | filter field | | Ctrl+F | Command+F | either | new (state only) |
| Copy to the other computer | primary button | Copiar para {peer} / Copiar para este computador | Ctrl+C then Ctrl+V in the other pane | Command+C then Command+V | across | home: yes (`FileGet`, `FileOffer`); other volumes, folders up: new |
| Copy inside one computer | | Copiar, Colar | Ctrl+C, Ctrl+V | Command+C, Command+V | here only (a copy inside the other computer is not in 1.1; section 11) | here: new, local |
| Move inside one computer | | Recortar, Colar | Ctrl+X, Ctrl+V; drag | Command+X, Command+V; drag | here: local; there: new `Move` (also between volumes of that computer: the helper copies, verifies, then sends the source to the bin, GUESS on how each system's move across volumes behaves) | no |
| Rename | Renomear | Renomear | F2 | F2 | either; there: new `Rename` | no |
| New folder | Nova pasta | Nova pasta | Ctrl+Shift+N | Command+Shift+N | either; there: new `MakeDir` | no |
| Delete to the bin | Excluir | Excluir | Delete | Command+Backspace | here: the system's bin; there: new `Trash`, to its bin only (decided) | no |
| Delete permanently | | (here only, with Shift held) | Shift+Delete | Command+Option+Backspace | here only, after a confirmation; never there (decided) | no |
| Properties | Propriedades | Propriedades | Alt+Enter | Command+I | either (from the listing; a folder's size is counted on demand with a listing per subfolder and a "Parar" button; a volume shows its total and free space) | yes (listing data) |
| Show in the system's file manager | | Mostrar no Explorador de Arquivos / Mostrar no Finder | | | here only | new, local |
| Show hidden items | pane "…" menu | | | | either | flags: new (1.1 listing) |

Enter opens and F2 renames on both systems (default). The Finder renames with Return; one interface means one behaviour, so that Mac habit does not carry over.

The **context menu** opens on a right click (or Control+click, or the Menu key / Shift+F10) on a row, on the empty part of a list, or on a place or volume. It is drawn by each system's own menu, the way the `Select` menus already are (`Effect::Menu`): the same items, in the same order, with the same texts on both. Items that cannot act now are disabled, never hidden.

**Inline rename**: the row's name becomes a native text field over the name cell (an edit slot), the name selected without its extension; Enter commits, Esc cancels, a click elsewhere commits. A name that breaks the rules (section 7.5) keeps the field open with the reason under it.

**New folder**: creates "Nova pasta" (then "Nova pasta (2)" ...) in the pane's folder and starts the inline rename on it.

**Delete**: to the bin with no question when everything selected is here and the bin takes it (Explorer and Finder do the same and the bin is the undo). There: always a confirmation card in the window: title "Mandar 3 itens para a Lixeira do {0}?" (pt-BR "Lixeira" on both systems; en "Recycle Bin" for a Windows peer, "Trash" for a Mac), body "Eles podem ser recuperados na Lixeira do {0}.", buttons "Cancelar" on the left, "Excluir" on the right in the danger style, never the default button. Permanent delete here: "Excluir 3 itens para sempre?", "Isso não pode ser desfeito.", "Cancelar" left, "Excluir para sempre" right, danger.

**Where there is no bin (decided)**: a place whose volume has no bin (a network share, many removable drives on Windows, a volume without a Trash on the Mac) refuses the delete before anything happens: the helper checks first and answers `NO_BIN`, nothing is deleted, and the window shows the notice "Este local não tem Lixeira; nada foi excluído." The Excluir command is disabled in advance for such a volume when the volume list says it has no bin. Which API each system uses (GUESS until slice 2a reads the primary pages; only the drag-and-drop facts were in scope today): Windows `IFileOperation` with the recycle flag, which on a volume without a bin would delete permanently, hence the check first; macOS `NSFileManager trashItemAtURL`, which fails on volumes without a Trash. In the user helper on Windows, `IFileOperation` needs a COM apartment on the helper's thread (GUESS).

### 3.3 Which commands act on the other computer's disk, and with which permission (decided)

Reading and copying are what `FILES` gates today (PROTOCOL 6.1: "file messages, 0x05 streams, `Dir*`"). New folder, rename, move, delete and replace **change** the other computer's files.

**Decided: a change needs `FILES` and `CONTROL` together; no new grant.** With `FILES` alone the explorer browses and copies, and the change commands are disabled with "Mudar arquivos no {0} precisa também da permissão de controlar." The reasoning: a viewer holding `CONTROL` can already rename, move and delete any of the user's files by driving the other computer's own file manager with the mouse, so the explorer adds convenience, not power. There is no new permission bit, no new prompt row and no change to the trust store. With view-only switched on (`SetViewOnly`) `CONTROL` is still granted, so changes stay allowed (the view-only switch is about input).

The host checks the rule on every change message (`NOT_PERMITTED` otherwise), whatever the viewer's interface shows. In every case the D13 gate holds: in an unattended session the new messages wait for the logged-in user's answer like the existing ones.

**Reach is not a separate grant (decided with decision 4):** `FILES` now means "the files of this computer, with the rights of the user logged in", on every published volume. A device granted `FILES` under 1.0 reached the home folder only; after the host updates to 1.1 the same grant reaches its drives and volumes. Section 12 asks whether the Permissões row's description should say so.

## 4. Drag and drop

### 4.1 Inside the app (slice 3, painted by Swapdesk on both systems)

- **Start**: a press on a selected row (or an unselected one, which selects it first) and a move of more than 4 px (GUESS: the system drag threshold; Swapdesk uses 4 on both). The drag image is a painted pill under the pointer: the first item's icon and "3 itens" in `text::SMALL_BOLD`, `surface` fill, `line` border, shadow.
- **Inside one pane**: dropped on a folder row, a breadcrumb segment, a place or a volume of the same computer: **move**; with the copy key held (Ctrl on Windows, Option on the Mac): **copy** (here only; there a copy is not in 1.1, so the copy key does nothing in the right pane and the pill says so). Dropped on the pane's empty area: nothing (it is the same folder). The pill's caption follows: "Mover para Documentos" / "Copiar para Documentos".
- **Between panes**: always **copy** ("Copiar para Documentos, no MacBook do escritório"); the move key does nothing across (default: no move across computers).
- **Drop targets and feedback**: a folder row under the pointer gets the `link_tint` fill and a `link` outline; the pane under the pointer (for its current folder) gets its border in `link` at 2 px; a breadcrumb segment, a place and a volume highlight the same way. Hovering a folder row for 800 ms opens it (spring-loaded, GUESS on the delay). Near the top or bottom 24 px of a list, the list scrolls. A target that cannot take the drop (the dragged folder itself or one inside it, a 1.0 peer for a folder upload, a pane without `FILES`, a move there without `CONTROL`) shows no highlight and the pill's caption says why.
- **Cancel**: Esc, or releasing outside any target. Nothing is changed until the release.
- After the release a move or copy goes through the same path as the commands (conflict check, queue, transfers bar).

### 4.2 From the system's file manager into the app (upload; slice 4, feasible in 1.1)

Targets: the right pane (its current folder, or a folder row under the pointer), a remote place or volume, and also the **picture** in the Tela view (into the other computer's `Downloads/Swapdesk`, the receive folder of a `FileOffer` without `dest`, as the 1.0 design board already suggested: "arraste para este painel ou para a tela"). Dropped on the left pane: a copy on this computer (GUESS: only copy in 1.1).

- **Windows**: the window registers an OLE drop target (`RegisterDragDrop` with an `IDropTarget`) and reads `CF_HDROP` for files from the file system; the target sees the pointer through `DragOver`, so the folder under it can be highlighted, which `WM_DROPFILES` cannot do. Microsoft: "Implement and register an OLE drop target. Avoid using Windows 3.1 targets or the WM_DROPFILES message, if possible" and "If your application will accept existing files, it must be able to handle the CF_HDROP format" (Handling Shell Data Transfer Scenarios). **A change found in the code**: the Windows shell starts COM with `CoInitializeEx(COINIT_APARTMENTTHREADED)` on its UI thread, and Microsoft says that then "RegisterDragDrop will always return an E_OUTOFMEMORY error" unless `OleInitialize` is used; the thread "must be pumping messages" (RegisterDragDrop page). So the UI thread switches to `OleInitialize`. Files that are not on the file system (`CFSTR_FILEDESCRIPTOR` / `CFSTR_FILECONTENTS` from a mail client or a zip folder) are refused in 1.1 with "Só dá para enviar arquivos que estão no disco." Implementing `IDropTarget` in Rust needs the `windows` crate's COM implementation support; its macro crate is already in `Cargo.lock` (GUESS: whether a feature flag of the workspace dependency must be turned on is checked in the slice; that is a `Cargo.toml` change of the main session, not a new crate).
- **Mac**: the canvas view registers for file URLs (`registerForDraggedTypes`) and implements the `NSDraggingDestination` methods; Apple: "Registering an NSView object for dragged types automatically makes it a candidate destination object for a dragging session." Promised files from other apps (`NSFilePromiseReceiver`) are refused in 1.1 with the same sentence.
- The dropped paths become the existing pick tokens (`Tokens::pick`), so the service still never sees a path; a dropped folder is walked by the UI and sent file by file after `MakeDir` (1.1 peer only).

### 4.3 From the app to the system's file manager or desktop (download; slice 5, 1.2 by default)

The file does not exist on this computer when the drag starts, so both systems' "promised file" mechanisms apply:

- **Windows**: the data object offers `CFSTR_FILEDESCRIPTOR` (name, size, time; `FD_PROGRESSUI` shows Explorer's own progress: "A progress indicator is shown with drag-and-drop operations", FILEDESCRIPTORW page) and one `CFSTR_FILECONTENTS` per file, rendered on request, as an `IStream` ("You should normally present the data as an IStream interface", Handling Shell Data Transfer Scenarios). Because a download is long, the data object also implements `IDataObjectAsyncCapability`, so Explorer extracts it on a background thread and `DoDragDrop` returns ("Once data extraction is handed off to the background thread, the primary threads of both applications are free to proceed", same page). The stream would read from a staging copy under the user's app data as the transfer writes it, blocking a read until its bytes arrived, and fail the last read if the SHA-256 does not match (GUESS: how Explorer reports a failed stream read). `DoDragDrop` runs a modal loop on the UI thread and needs `OleInitialize` too (DoDragDrop page). `cFileName` is limited to `MAX_PATH` (FILEDESCRIPTORW page).
- **Mac**: one `NSFilePromiseProvider` per file (macOS 10.12 and later), started with `beginDraggingSession(with:event:source:)`; the delegate's `filePromiseProvider(_:writePromiseTo:completionHandler:)` "is called after the drag is complete" and runs "on the OperationQueue supplied by operationQueue(for:)"; Apple advises "to avoid blocking your main thread, provide an operation queue other than the main operation queue". The download goes to the promised URL through a receive token for that folder, and the completion handler is called after verification. The file type "must be a Uniform Type Identifier (UTI) that ultimately conforms to kUTTypeData or kUTTypeDirectory", so folders can be promised.
- **Why 1.2**: both need COM objects or AppKit delegates that live for the length of a network transfer, and failure paths (link lost mid-drop, verification failure, the user cancelling Explorer's dialog) that 1.1's proofs would have to cover on both systems. **Interim in 1.1**: drag the item onto the left pane (or press "Copiar para este computador"), then "Mostrar no Explorador de Arquivos" / "Mostrar no Finder" on the finished transfer, from where the system's own drag works.

A later step can make every drag a system drag from the start (an OLE drag on Windows, a dragging session on the Mac) carrying a private type for in-app drops, so one gesture covers inside and outside the window; slice 3's painted drag is then replaced by the system's drag image (GUESS: decided at slice 5).

## 5. Transfers

- **Queue**: every copy becomes a job: a list of files (a folder is walked first: remote folders with listings, local ones on the worker thread), each with its destination folder. The engine runs at most 8 unfinished transfers per session (`Busy` beyond, C2.4); the rest wait in the UI's queue, in order, and start as slots free. Folders are created before their files (here by the UI, there with `MakeDir`). Limits per job: 10 000 files and 32 levels (GUESS values); beyond, the job does not start and says so.
- **Progress**: per item (bytes, speed, state) from the existing `Transfer` notifications; overall in the transfers bar: items done of total, bytes done of total, speed (the sum), time left from the speed of the last 10 s (GUESS window).
- **Pause, resume, cancel**: per item and for the whole job ("Pausar tudo", "Retomar tudo", "Cancelar tudo"); rows keep "Retomar" for an item paused by a lost link, as today. Button order follows the rule: "Cancelar" on the left, "Retomar" on the right (today's drawer has them the other way round).
- **Lost link**: as today (C2.4 "Resume"): every unfinished transfer pauses and resumes from the verified length when F-NET-01 brings the session back, with no click; queued items wait.
- **A volume that goes away mid-transfer**: the transfer whose source or destination lies on it fails at the next read or write with `VOLUME_GONE` (a 1.1 peer; `IO_FAILED` from a 1.0 peer, which cannot reach other volumes anyway); its row says "O disco {0} foi removido." with "Tentar de novo"; the queued items for that volume are held, not failed one by one, and the job says "Esperando o disco {0} voltar". When the same volume comes back (the same volume identity, section 8.1), "Tentar de novo" resumes from the verified length kept on the receiving side. A volume never comes back under another volume's token, so a retry cannot land on a different disk.
- **Conflicts**: before a job starts, the UI compares its names with the destination's listing (it has it, or reads it) and, when some exist, asks once: card "3 itens já existem em Documentos", the first names listed, a check "Fazer o mesmo com os outros" (checked when more than one), and the buttons from the left: "Cancelar", "Pular", "Substituir" (danger style), "Manter os dois" (primary and the default answer, by default: it is today's behaviour and loses nothing). "Manter os dois" lets the engine save `name (1).ext` (the existing `SavedAs` line). "Pular" leaves the item out. "Substituir" sends the existing file to the bin first (here: the UI; there: `Trash`, so a 1.1 peer, `FILES` and `CONTROL` and a volume with a bin are needed, else the button is disabled with the reason) and then copies: a failed copy leaves the old file recoverable from the bin. A name that appears between the check and the write still never gets overwritten (the engine's rule).
- **Verification failure**: as today, the receiver deletes the partial file; the row says "Chegou diferente do original e foi apagado." with "Enviar de novo" (upload) or "Baixar de novo" (download).
- **Session end**: "Desconectar", closing the window or the disconnect shortcut while items run or wait asks first: card "Há 2 transferências em andamento", body "Se desconectar agora, elas param. O que já chegou fica guardado e continua se você copiar os mesmos arquivos de novo.", buttons "Continuar transferindo" (left, the safe default) and "Desconectar" (right, danger). After the end, unfinished rows say "Parou: a sessão terminou." The receiving side keeps the partial and meta files, so the same copy in a later session resumes from the verified length (the meta record matches name, size and time, `recv.rs`; UNPROVEN across sessions end to end).
- **Where downloads land**: in the folder the left pane showed when the copy started (or the folder row it was dropped on). There is no separate "Salvar em" setting any more; the first left-pane folder is Downloads. Each finished download row has "Mostrar" (opens that folder in the left pane with the files selected) and, in its menu, "Mostrar no Explorador de Arquivos" / "Mostrar no Finder".

## 6. Both directions, both roles

- **Controller browsing the controlled computer** (the order): everything above, on every published volume of that computer; the controller's own disk is the left pane.
- **Controller sending to the controlled computer**: the copy button from the left pane, a paste in the right pane, a drag between panes, or a drop from the system onto the right pane or the picture.
- **The controlled side's user (default)**: sees no explorer. The session banner gains one line while files move or change, without file names (anyone near the screen sees it): "Copiando arquivos: 2 de 5" / "Recebendo arquivos: 2 de 5", and after a change "Renomeou 1 item", "Moveu 3 itens", "Mandou 4 itens para a Lixeira", for 10 s (GUESS duration); and once per session, when the viewer first opens a folder outside the home folder, "Abrindo arquivos de outros discos" for 10 s. The D13 consent card is unchanged ("{0} quer abrir os arquivos desta conta"). The host's user can stop file access by ending the session or by removing the files permission in Permissões, which takes effect at once (`PermissionsChanged`, F-PERM-02).
- **Watch sessions** ("Assistir", `Intent::View`): the viewer does not ask for `FILES` (`requested_for` asks for it only with control), so the folder button is disabled with the hint "Arquivos precisa de uma sessão com controle."
- **The controlled side pushing files to the controller**: not offered. A viewer accepts a file only as the answer to its own request (F-FILE-01, THREAT-MODEL "Malicious host, to the viewer"); the controlled user who wants to hand over a file asks for a swap ("Inverter") and becomes the controller.

## 7. Protocol, helper and interface: what exists and what is added

### 7.1 What exists

Wire (PROTOCOL 5.3, all minor 0): `FileOffer` 0x0401, `FileAccept` 0x0402, `FileDecline` 0x0403, `FileDone` 0x0404, `FileVerified` 0x0405, `FilePause` 0x0406, `FileResume` 0x0407, `FileCancel` 0x0408, `DirRequest` 0x0409, `DirListing` 0x040A (at most 2000 entries, `truncated`), `FileGet` 0x040B; the 0x05 file data stream; `ConsentPending { Files }`. Every path in them is relative to the host's home folder.

Helper calls (C2.4 "Files", `FsCall` 1 to 17): `ListDir`, `Stat`, `OpenRead`, `ReadAt`, `ReadStat`, `CloseRead`, `StatFile`, `PartCreate`, `PartTruncate`, `PartAppend`, `PartLen`, `PartRead`, `PartSync`, `PartRename`, `RemoveFile` (part and meta files only), `MetaRead`, `MetaWrite`.

UI ops 40 to 45 (`FilesList`, `FilesSend`, `FilesGet`, `TransferPause`, `TransferResume`, `TransferCancel`), notifications 20 (`Transfer`) and 21 (`SavedAs`).

### 7.2 Each command against what exists, and what a 1.0 peer still limits

| Command | Wire (peer) | Helper | Against a 1.0 peer |
|---|---|---|---|
| browse the home folder, sort, filter, back/forward/up | `DirRequest`/`DirListing` | `ListDir` | works (up to 2000 entries) |
| browse other drives and volumes, and above the home folder | **new** volume paths (8.1) in `DirPage` | **new** volume roots | **not available**: the group of that computer shows its places only, and the right pane's status line says "O {0} usa o Swapdesk 1.0: só a pasta pessoal aparece." |
| hidden flags, link kind, pages past 2000, free space | **new** `DirPage` / `DirPageListing` | **new** `ListPage` | names only; first 2000; no free space shown |
| places and volumes there | **new** `VolumesRequest` / `VolumesList` (pushed again on change) | **new** `Volumes` | places derived from the home listing: `Desktop`, `Documents`, `Downloads` when present (GUESS: a redirected folder, such as a cloud-synced Documents, is missed); no volumes |
| copy a file down | `FileGet` (home path) or `FileGet` with a volume path | existing, plus volume roots | home only |
| copy a folder down | listings + `FileGet` per file; the UI creates the folders here | existing | home only |
| copy a file up | `FileOffer` with `dest` | existing, plus volume roots | into the home folder only |
| copy a folder up | `FileOffer` per file after **new** `FsOp::MakeDir` | **new** `MakeDir` | not available |
| new folder, rename, move, delete there | **new** `FsOp` | **new** `MakeDir`, `Rename`, `Move`, `Trash` | not available |
| replace on conflict there | **new** `FsOp::Trash`, then `FileOffer` | **new** `Trash` | not available ("Manter os dois" and "Pular" work) |
| properties | listing data; folder size by listings | existing | home only |
| anything here (this computer) | none | none (the UI's own disk) | works |

The text of every command a 1.0 peer cannot serve (its accessibility description and its hint): "O {0} usa o Swapdesk 1.0. Atualize o Swapdesk de lá para ver outros discos, criar, renomear, mover, apagar ou enviar pastas."

The other way round, a 1.0 viewer on a 1.1 host keeps the 1.0 drawer and the 1.0 meaning of every path (the home folder); the host never answers it with anything of minor 1.

### 7.3 New wire items (minor 1 only)

`PROTO_MINOR` becomes 1 with the first 1.1 change that lands; everything new in 1.1 shares minor 1 (the accounts, grants-sync and HEVC work included): the minor counts protocol versions, not features. A peer of minor 0 never receives any of the items below. All are gated by `FILES`; `FsOp` also by the rule of 3.3; all pass the D13 gate.

Numbers taken, checked against `docs/ACCOUNTS.md` and `docs/GRANTS-SYNC.md`, which take message types 0x000B, 0x000C and 0x0801, `ErrorCode` 22 (`ACCOUNT_AWAY`), `Desktop` 4, audit tag 18, and UI ops 35 to 39 and 50: this note takes **message types 0x040C to 0x0411**, **`ErrorCode` 23 to 26**, **audit tags 19 and 20**, **UI ops 46 to 48** (49 stays free), **UI notification kind 22**, **`FsCall` 18 to 24** and **`FsAnswer` 8 to 10**. None collides.

| Type | Name | Dir | Fields | Notes |
|---|---|---|---|---|
| 0x040C | DirPage | V→H | `req_id: u32`, `path: str(1024)`, `start: u32`, `max: u16` (≤ 2000) | `path` is a 1.1 path (8.1): the empty string is the home folder |
| 0x040D | DirPageListing | H→V | `req_id: u32`, `path: str(1024)`, `start: u32`, `entries: Vec<DirEntry2>(2000)` with `DirEntry2 { name: str(255), kind: EntryKind {File, Dir, Link}, size: u64, mtime_ms: u64, flags: u8 }` (bit 0 hidden, 1 system, 2 read-only; other bits 0), `next: Option<u32>`, `free_bytes: Option<u64>` | a page stops before the frame passes 65 535 bytes, like `DirListing`; `next` is the `start` of the following page |
| 0x040E | VolumesRequest | V→H | `req_id: u32` | |
| 0x040F | VolumesList | H→V | `req_id: u32` (0 when pushed unasked), `home: str(1024)` (the home folder as a 1.1 path), `places: Vec<Place>(16)` with `Place { kind: PlaceKind {Desktop, Documents, Downloads, Pictures, Music, Videos, Received}, path: str(1024) }`, `volumes: Vec<Volume>(32)` with `Volume { id: u32, kind: VolumeKind {System, Fixed, Removable, Network, Optical, Image}, label: str(64), letter: Option<u8>` (Windows `A` to `Z`), `total_bytes: u64`, `free_bytes: u64`, `has_bin: bool }` | sent again, with `req_id` 0, whenever the volume set changes (at most once a second); a place outside every published volume is left out |
| 0x0410 | FsOp | V→H | `op_id: u32`, `op: FsOpKind` = `MakeDir { parent: str(1024), name: str(255) }` / `Rename { dir: str(1024), name: str(255), new_name: str(255) }` / `Move { from_dir: str(1024), names: Vec<str(255)>(64), to_dir: str(1024) }` / `Trash { dir: str(1024), names: Vec<str(255)>(64) }` | at most 4 in flight per session (`BUSY` beyond) |
| 0x0411 | FsOpResult | H→V | `op_id: u32`, `results: Vec<OpItem>(64)` with `OpItem { name: str(255), code: ErrorCode }` | one result per name; `NO_ERROR` when done |
| `ErrorCode` 23 | NO_SPACE | | | the free-space refusal of F-FILE-03 (`FileDecline`); toward a minor-0 peer the encoder sends `IO_FAILED` |
| `ErrorCode` 24 | NO_BIN | | | `Trash` on a volume without a bin; nothing deleted (decision 3) |
| `ErrorCode` 25 | EXISTS | | | `MakeDir`, `Rename`, `Move` onto an existing name; never replaced |
| `ErrorCode` 26 | VOLUME_GONE | | | a path whose volume token is no longer published, or a volume that went away during a read or write; toward a minor-0 peer `NOT_FOUND` (it cannot arise there: a 1.0 peer never gets a volume path) |

The 1.0 messages `FileGet` (its `path`) and `FileOffer` (its `dest`) keep their layout; between minor-1 peers their path may also be a 1.1 volume path (8.1). That is a new value in an old field, so a 1.1 viewer sends a volume path in them only to a host of minor 1. A 1.0 host that received one anyway would refuse it with `INVALID_NAME`, since a volume token starts with `:`, which no 1.0 name may hold.

**Fixtures and minors**: in the change that adds them, every new message type (0x040C to 0x0411) goes into `TYPES_AFTER_MINOR_0` and every new value of an existing enum (`ErrorCode` 23 to 26) into `ENUM_VALUES_AFTER_MINOR_0` of `crates/swapdesk-proto/tests/wire_1_0.rs`, each with minor 1, and into the per-type `min_minor` table the accounts note introduces; the new enums that live only inside the new messages (`EntryKind`, `PlaceKind`, `VolumeKind`, `FsOpKind`) get samples of their own for the 1.1 messages. The 1.0 fixtures stay byte for byte as they are.

### 7.4 New helper calls and interface ops

- `FsCall` 18 `ListPage{path, start, max}` (answer 8 `Entries2{entries, next, free}`), 19 `MakeDir{dir, name}`, 20 `Rename{dir, name, new_name}`, 21 `Move{from_dir, name, to_dir}`, 22 `Trash{dir, name}` (answers `NoBin` before touching anything on a volume without a bin), 23 `Volumes` (answer 9 `Volumes{home, places, volumes}`), 24 `FreeSpace{dir}` (answer 10 `Free(u64)`). The helper also tells the service when its volume set changed, and the service pushes `VolumesList`. The helper's server serves them on its roots (8.1); the UI's `Roots::Tokens` server refuses every one of them with `InvalidName`.
- UI ops 46 `FilesListPage{session_id, path, start}` [`Listing2`], 47 `FilesOp{session_id, op}` [`OpResults`, after the host's answer, `Timeout` after 30 s], 48 `FilesVolumes{session_id}` [`Volumes`]; notification kind 22 `FilesVolumes{session_id, volumes}` when the other computer's set changes. Each op answers `Unsupported` at once when the peer's minor is 0, so the interface disables commands from the session's minor without asking (GUESS: today the UI does not receive the peer's minor; one field is added to the `Session` notification's active state, IPC only).
- This computer's places, volumes and their changes, the bin, "open with the system's app" and "show in the file manager" are new **effects** handled by each shell (the pattern of `PickFiles` and `PickReceiveFolder`, and the test that fails when an `Effect` has no handler): `OpenLocal(path)`, `RevealLocal(paths)`, `TrashLocal(paths)`, `LocalPlaces`, and a shell event `LocalVolumesChanged`. Listing, renaming, moving and creating folders here use `std::fs` on the UI's worker thread (`swapdesk_files::LocalFs::list_dir` is reused for listings).

### 7.5 Names across systems (with the plan's files stage)

`validate_name` (shared) refuses `/`, `\`, `:`, control characters, reserved device names, a leading space and a trailing dot or space. A copy to a Windows computer also fails on `* ? " < > |` (Windows refuses them; today that shows as `IO_FAILED`). The explorer's pre-flight lists such names before the job starts: "2 nomes não valem no Windows" with "Trocar os caracteres por _" (primary) or "Pular esses"; a Mac name holding `:` (shown by the Finder as `/`) gets the same treatment on its way to either system. Such names are still listed in the right pane (with the reason on hover) but cannot be named in a path, so a file whose own name breaks the rules can be seen and not copied (GUESS: rare enough for 1.1). Unicode normalization and case collisions are the engine's (F-FILE-03).

## 8. Security

### 8.1 Paths on the controlled side, with the wider reach (decided)

The service still never opens any path. The user helper (the Aqua agent on the Mac) does every read and change, as the logged-in user, so the operating system's permissions remain the outer boundary: nothing new can reach what that user could not touch by hand at the machine.

**Roots.** The helper's server gets a list of roots in place of the single home root:

- the **home folder**, named by the empty first component, exactly as in 1.0 (a 1.0 path is a 1.1 path and keeps its meaning);
- each **volume** it publishes, named by a first component `:<id>`, where `id` is a decimal `u32` the helper chose. The `:` can never start a 1.0 name (section 5.3 of PROTOCOL refuses `:`), so a volume path never collides with a home path and a 1.0 host refuses it.

Which volumes are published: on Windows, the drive letters whose drive type is fixed, removable, remote (mapped by that user's logon) or optical with a medium present (GUESS on the APIs, read in slice 2b); on the Mac, the startup volume and the mounted volumes the Finder shows, never the hidden system volumes (GUESS: the startup volume's root `/` is published as one volume and its hidden system parts are left out by the list's own options). A volume is published only while it is mounted.

**Volume identity.** An `id` is never reused for another volume in the life of the helper: a stick removed and another inserted under the same letter gets a new `id`. The helper keeps, per `id`, the volume's own identity (GUESS: the volume serial or GUID on Windows, the volume UUID on the Mac) and refuses with `VOLUME_GONE` any path whose `id` is no longer mounted or now names another volume. A transfer or operation cannot therefore land on a disk the viewer never saw. When the same volume comes back it gets its old `id` again, so a paused transfer can resume (section 5).

**Components.** After the root, every component is checked by `validate_name` in the helper itself, unchanged: no empty component, no `.` or `..`, no `/` or `\`, no `:` (so no alternate data stream and no drive-relative form), no control character, no reserved device name with or without an extension, no trailing dot or space, no leading space, no `.swapdesk-part` or `.swapdesk-meta` name. An absolute path, a UNC path (`\\server\share`), a device path (`\\?\`, `\\.\`) or a drive letter typed inside a path cannot be formed, because `\` and `:` never pass. The address bar turns what the user types (`D:\Fotos`, `/Volumes/Backup/2026`) into a 1.1 path by matching it against the published volumes; anything that does not start on a published volume is refused in the interface with "Esse endereço não está em nenhum disco do {0}." The path limit stays 1024 bytes, a name 255.

**Above the home folder.** The home folder is also reachable as a folder of its volume (`:<id>/Users/ana`); the two spellings name the same files, and the helper serves both. Nothing extra is granted by either spelling: the rights are the user's.

**Links.**
- Reading (listing, opening a file to send) may follow a link or junction, as 1.0 does: every target is something the user could open by hand, and with every volume published a link no longer leads outside the reach.
- Changes (`MakeDir`, `Rename`, `Move`, `Trash`, writing a received file, `PartRename`) never pass through a link: a symbolic link or a junction as any component of the path, not only the last, is refused with `INVALID_NAME`, and an operation on a link acts on the link itself (renaming or trashing the link, never its target). Files written by the engine already open without following a link and re-check the handle after opening (C2.4); that rule now covers every root.
- Cloud placeholders of synced folders are reparse points too and must keep working, both for reading and for changes (GUESS: told apart by the reparse tag; checked in slice 2b). A mount point that is the root of a published volume is reached by that volume's `id`, not through the mount folder.

**Protected locations (default, section 12 asks).** The OS refuses most changes to the system's own folders for a standard user, but an administrator's session on the Mac can change `/Applications`, and some folders on Windows are writable by every user. As a second guard the helper refuses every change (not reading) inside: on Windows the Windows folder, `Program Files`, `Program Files (x86)`, `ProgramData` and the root of the system drive; on the Mac `/System`, `/Library`, `/Applications`, `/usr`, `/bin`, `/sbin`, `/private` and the root of the startup volume. The refusal is `NOT_PERMITTED` with "O Swapdesk não muda arquivos do sistema do {0}." (GUESS on the exact list, settled in slice 2b.)

**A volume that goes away.** During a listing: the listing fails with `VOLUME_GONE` (or the page already sent stands and the next page fails); the pane shows its error state. During a transfer: section 5. During an operation: each name not yet done answers `VOLUME_GONE`; what was done stays done. The helper never retries on another volume.

### 8.2 What the wider reach and the changes add to the threat model

| Threat | Before | Added by the design | Mitigation |
|---|---|---|---|
| A paired viewer with files access reads more than the user's own documents | the home folder only | every published volume: other folders of the system volume, other users' folders where the OS lets this user read, removable media, mapped network drives | the OS's permissions (the helper is the user's process, never SYSTEM or root); D13 asks the logged-in user in unattended sessions; the banner line "Abrindo arquivos de outros discos"; the audit entry of 8.3 |
| Through a **network drive** mapped on the controlled computer, the viewer reads or writes a third machine's share with that user's credentials | not reachable | mapped network volumes are published | open question in section 12 (recommended: network volumes left out in 1.1, so LAN-only stays about the two paired machines); if published, they are marked "Unidade de rede" and every change there refuses delete (`NO_BIN` for most shares) |
| A paired viewer with files and control destroys files | could only read and add files (never overwrite) | rename, move, delete, replace, on every volume | changes need `FILES` and `CONTROL` (3.3); delete goes to the bin only and is refused where there is none (decision 3); replace goes through the bin; no change in the protected locations of 8.1; D13; every change audited |
| A link or junction leads a change somewhere the user did not see | writes refused links on the final file | changes on every volume | no change through a link at any component; operations act on the link itself |
| A path or token confusion makes a change land on another disk | n/a | volume tokens | ids never reused for another volume; volume identity checked on every call; `VOLUME_GONE` |
| A crafted path escapes a root | components checked against the home root | volume roots | the same component rules; `\` and `:` never pass, so no absolute, UNC, device or stream path can be formed |
| Move into a folder that is inside itself, rename onto an existing name | n/a | `Move`, `Rename` | refused (`INVALID_NAME`, `EXISTS`); never replace |
| A slow or hung volume ties up the helper | the home folder | removable and network volumes | the 30 s call timeout (C2.4); 4 operations in flight per session; listings in pages |
| Flooding the helper with operations | listing only | many small operations | 4 in flight and 64 names per operation per session; the existing peer backlog and work queue bounds |
| Hiding activity from the user at the machine | n/a | reach and changes | the banner lines (section 6) and the audit entries |
| A malicious host feeding the viewer bad names or volumes | names validated on receipt | `DirEntry2`, `VolumesList` | the same name rules on every name; a volume label is shown, never used as a path; a volume path is only ever sent back to the host that published it |

The threat model document gains rows for "a paired device with files reads every volume the user can read", "a mapped network drive reached through the controlled computer" and "a paired device with files and control changes the user's files", with these mitigations.

### 8.3 Audit (default: counts on the controlled side)

On the controlled side, with counts and kinds only, never a name or path (like `FilesSent` and `FilesReceived` today):

- tag 19 `FilesChanged { op: u8 (0 new folder, 1 rename, 2 move, 3 to the bin), count: u32, area: u8 (0 the home folder, 1 elsewhere on the system volume, 2 another volume) }`, one entry per operation message;
- tag 20 `FilesReach { kinds: u8 }`, once per session, when the viewer first lists outside the home folder, with a bit per volume kind reached so far (bit 0 system volume outside home, 1 fixed, 2 removable, 3 network, 4 optical or image); a later first visit to another kind adds one more entry.

Atividade lines: `AuditFilesChanged` and `AuditFilesReach` (section 9). Tag 18 belongs to the accounts note; entries of tags 19 and 20 are written where that note says per-account entries go.

### 8.4 Limits

Path 1024 bytes, name 255, 2000 entries per page, 32 volumes and 16 places per list, 64 names per operation, 4 operations in flight, 8 unfinished transfers per session (existing), 10 000 files and 32 levels per copy job (GUESS values), the 30 s helper timeout (existing), one pushed volume list per second.

## 9. Texts (proposals; every text is the owner's decision)

| Key (proposed) | pt-BR | en |
|---|---|---|
| ExplorerTitle | Arquivos | Files |
| ExplorerBackToScreen | Tela | Screen |
| ExplorerThisComputer | Este computador | This computer |
| ExplorerCopyTo | Copiar para {0} | Copy to {0} |
| ExplorerCopyHere | Copiar para este computador | Copy to this computer |
| ExplorerNewFolder | Nova pasta | New folder |
| ExplorerNewFolderName | Nova pasta | New folder |
| ExplorerRename | Renomear | Rename |
| ExplorerDelete | Excluir | Delete |
| ExplorerProperties | Propriedades | Properties |
| ExplorerOpen | Abrir | Open |
| ExplorerCopy | Copiar | Copy |
| ExplorerCut | Recortar | Cut |
| ExplorerPaste | Colar | Paste |
| ExplorerRefresh | Atualizar | Refresh |
| ExplorerBack | Voltar | Back |
| ExplorerForward | Avançar | Forward |
| ExplorerUp | Pasta acima | Up |
| ExplorerFilter | Filtrar nesta pasta | Filter this folder |
| ExplorerShowHidden | Mostrar itens ocultos | Show hidden items |
| ExplorerRevealWindows | Mostrar no Explorador de Arquivos | Show in File Explorer |
| ExplorerRevealMac | Mostrar no Finder | Show in Finder |
| ExplorerColName | Nome | Name |
| ExplorerColSize | Tamanho | Size |
| ExplorerColType | Tipo | Type |
| ExplorerColModified | Modificado | Modified |
| ExplorerItems | {0} itens | {0} items |
| ExplorerOneItem | 1 item | 1 item |
| ExplorerSelected | {0} selecionados, {1} | {0} selected, {1} |
| ExplorerFree | {0} livres | {0} free |
| ExplorerShowingFirst | Mostrando os primeiros {0} | Showing the first {0} |
| ExplorerEmpty | Esta pasta está vazia. | This folder is empty. |
| ExplorerNoMatch | Nada nesta pasta tem “{0}” no nome. | Nothing in this folder has “{0}” in its name. |
| ExplorerPlaceHome | Início | Home |
| ExplorerPlacePersonal | Pasta pessoal | Home folder |
| ExplorerPlaceDesktop | Área de trabalho | Desktop |
| ExplorerPlaceDocuments | Documentos | Documents |
| ExplorerPlaceDownloads | Downloads | Downloads |
| ExplorerPlacePictures | Imagens | Pictures |
| ExplorerPlaceMusic | Música | Music |
| ExplorerPlaceVideos | Vídeos | Videos |
| ExplorerPlaceReceived | Recebidos do Swapdesk | Received from Swapdesk |
| ExplorerDriveLabeled | {0} ({1}:) | {0} ({1}:) |
| ExplorerDriveLocal | Disco local ({0}:) | Local disk ({0}:) |
| ExplorerDriveRemovable | Unidade removível ({0}:) | Removable drive ({0}:) |
| ExplorerDriveNetwork | Unidade de rede ({0}:) | Network drive ({0}:) |
| ExplorerDriveOptical | Unidade de CD/DVD ({0}:) | CD/DVD drive ({0}:) |
| ExplorerVolumeUnnamed | Volume sem nome | Untitled volume |
| ExplorerVolumeSpace | {0} livres de {1} | {0} free of {1} |
| ExplorerTypeFolder | Pasta | Folder |
| ExplorerTypeFile | Arquivo | File |
| ExplorerTypeExt | Arquivo {0} | {0} file |
| ExplorerTypePdf | Documento PDF | PDF document |
| ExplorerTypeImage | Imagem {0} | {0} image |
| ExplorerTypeVideo | Vídeo {0} | {0} video |
| ExplorerTypeAudio | Áudio {0} | {0} audio |
| ExplorerTypeArchive | Arquivo compactado {0} | {0} archive |
| ExplorerTypeText | Documento de texto | Text document |
| ExplorerTypeApp | Aplicativo | Application |
| ExplorerErrTitle | Não foi possível abrir esta pasta | This folder could not be opened |
| ExplorerErrGone | Ela não existe mais. Pode ter sido movida ou apagada. | It no longer exists. It may have been moved or deleted. |
| ExplorerErrVolumeGone | O disco {0} foi removido do {1}. | The disk {0} was removed from {1}. |
| ExplorerErrVolumeGoneHere | O disco {0} foi removido deste computador. | The disk {0} was removed from this computer. |
| ExplorerGoHome | Ir para a pasta pessoal | Go to the home folder |
| ExplorerErrDenied | O {0} não deixou ler esta pasta. | {0} did not allow reading this folder. |
| ExplorerErrDeniedHere | Este computador não deixou ler esta pasta. | This computer did not allow reading this folder. |
| ExplorerErrMacHere | O macOS não deu ao Swapdesk acesso a esta pasta. | macOS has not given Swapdesk access to this folder. |
| ExplorerErrMacThere | O macOS do {0} não deu ao Swapdesk acesso a esta pasta. Alguém no {0} precisa permitir em Ajustes do Sistema. | The macOS on {0} has not given Swapdesk access to this folder. Someone at {0} has to allow it in System Settings. |
| ExplorerOpenSettings | Abrir Ajustes do Sistema | Open System Settings |
| ExplorerErrTimeout | O {0} não respondeu a tempo. | {0} did not answer in time. |
| ExplorerErrDeclined | O usuário do {0} não deixou abrir os arquivos. | The user of {0} did not allow opening the files. |
| ExplorerErrNoGrant | O {0} não deu a este computador permissão para arquivos. | {0} has not given this computer permission for files. |
| ExplorerBadAddress | Esse endereço não está em nenhum disco do {0}. | That address is not on any disk of {0}. |
| ExplorerSystemProtected | O Swapdesk não muda arquivos do sistema do {0}. | Swapdesk does not change system files on {0}. |
| ExplorerRetry | Tentar de novo | Try again |
| ExplorerGoBack | Voltar | Go back |
| ExplorerReconnecting | Reconectando ao {0}… | Reconnecting to {0}… |
| ExplorerOpenThere | Para abrir no {0}, use a tela. | To open it on {0}, use the screen. |
| ExplorerNeeds11 | O {0} usa o Swapdesk 1.0. Atualize o Swapdesk de lá para ver outros discos, criar, renomear, mover, apagar ou enviar pastas. | {0} runs Swapdesk 1.0. Update Swapdesk there to see other disks, create, rename, move, delete or send folders. |
| ExplorerHomeOnly10 | O {0} usa o Swapdesk 1.0: só a pasta pessoal aparece. | {0} runs Swapdesk 1.0: only the home folder is shown. |
| ExplorerNeedsControl | Mudar arquivos no {0} precisa também da permissão de controlar. | Changing files on {0} also needs permission to control. |
| ExplorerWatchOnly | Arquivos precisa de uma sessão com controle. | Files needs a session with control. |
| ExplorerDiskOnly | Só dá para enviar arquivos que estão no disco. | Only files that are on a disk can be sent. |
| ExplorerNoBin | Este local não tem Lixeira; nada foi excluído. | This place has no bin; nothing was deleted. |
| ExplorerNameTaken | Já existe um item com esse nome aqui. | An item with that name already exists here. |
| ExplorerNameInvalid | Esse nome não pode ser usado. | That name cannot be used. |
| ExplorerNameInvalidWindows | Esse nome não vale no Windows: tire {0}. | That name is not valid on Windows: remove {0}. |
| ExplorerDragMove | Mover para {0} | Move to {0} |
| ExplorerDragCopy | Copiar para {0} | Copy to {0} |
| ExplorerDragCopyThere | Copiar para {0}, no {1} | Copy to {0}, on {1} |
| ExplorerDragItems | {0} itens | {0} items |
| ExplorerDragIntoItself | Uma pasta não vai para dentro dela mesma. | A folder cannot go inside itself. |
| TrashThereTitle | Mandar {0} itens para a Lixeira do {1}? | Move {0} items to the Trash on {1}? |
| TrashThereTitleWindows | Mandar {0} itens para a Lixeira do {1}? | Move {0} items to the Recycle Bin on {1}? |
| TrashThereBody | Eles podem ser recuperados na Lixeira do {0}. | They can be restored from the bin on {0}. |
| DeleteForeverTitle | Excluir {0} itens para sempre? | Delete {0} items permanently? |
| DeleteForeverBody | Isso não pode ser desfeito. | This cannot be undone. |
| DeleteForever | Excluir para sempre | Delete permanently |
| ConflictTitle | {0} itens já existem em {1} | {0} items already exist in {1} |
| ConflictOneTitle | “{0}” já existe em {1} | “{0}” already exists in {1} |
| ConflictApplyAll | Fazer o mesmo com os outros | Do the same for the others |
| ConflictSkip | Pular | Skip |
| ConflictReplace | Substituir | Replace |
| ConflictKeepBoth | Manter os dois | Keep both |
| ConflictReplaceNote | O que for substituído vai para a Lixeira. | What is replaced goes to the bin. |
| NamesWindowsTitle | {0} nomes não valem no Windows | {0} names are not valid on Windows |
| NamesReplaceChars | Trocar os caracteres por _ | Replace the characters with _ |
| NamesSkip | Pular esses | Skip these |
| TransfersSummary | {0} em andamento, {1} de {2}, {3}/s, falta {4} | {0} in progress, {1} of {2}, {3}/s, {4} left |
| TransfersShow | Mostrar | Show |
| TransfersHide | Ocultar | Hide |
| TransfersPauseAll | Pausar tudo | Pause all |
| TransfersResumeAll | Retomar tudo | Resume all |
| TransfersCancelAll | Cancelar tudo | Cancel all |
| TransferPause | Pausar | Pause |
| TransferWaiting | Na fila | Waiting |
| TransferRoute | {0} → {1}, {2} | {0} → {1}, {2} |
| TransferVerifyFailed | Chegou diferente do original e foi apagado. | It arrived different from the original and was deleted. |
| TransferSendAgain | Enviar de novo | Send again |
| TransferGetAgain | Baixar de novo | Download again |
| TransferStoppedEnded | Parou: a sessão terminou. | Stopped: the session ended. |
| TransferVolumeGone | O disco {0} foi removido. | The disk {0} was removed. |
| TransferWaitingVolume | Esperando o disco {0} voltar | Waiting for the disk {0} to come back |
| TransferShow | Mostrar | Show |
| TransferNoSpace | Não há espaço livre suficiente em {0}. | There is not enough free space on {0}. |
| TransferTooMany | A cópia tem itens demais (mais de {0}). | The copy has too many items (more than {0}). |
| EndWithTransfersTitle | Há {0} transferências em andamento | {0} transfers are in progress |
| EndWithTransfersBody | Se desconectar agora, elas param. O que já chegou fica guardado e continua se você copiar os mesmos arquivos de novo. | If you disconnect now, they stop. What already arrived is kept and continues if you copy the same files again. |
| EndKeepTransferring | Continuar transferindo | Keep transferring |
| BannerFilesSending | Copiando arquivos: {0} de {1} | Copying files: {0} of {1} |
| BannerFilesReceiving | Recebendo arquivos: {0} de {1} | Receiving files: {0} of {1} |
| BannerFilesRenamed | Renomeou {0} item(ns) | Renamed {0} item(s) |
| BannerFilesMoved | Moveu {0} item(ns) | Moved {0} item(s) |
| BannerFilesTrashed | Mandou {0} item(ns) para a Lixeira | Moved {0} item(s) to the bin |
| BannerFilesNewFolder | Criou uma pasta | Created a folder |
| BannerFilesOtherDisks | Abrindo arquivos de outros discos | Opening files on other disks |
| AuditFilesChanged | Mudou {0} arquivo(s) neste computador | Changed {0} file(s) on this computer |
| AuditFilesReach | Abriu arquivos fora da pasta pessoal | Opened files outside the home folder |
| PropsTitle | Propriedades de {0} | Properties of {0} |
| PropsLocation | Local | Location |
| PropsSize | Tamanho | Size |
| PropsContains | Contém | Contains |
| PropsCounting | Contando… | Counting… |
| PropsStop | Parar | Stop |
| PropsClose | Fechar | Close |

New keys are appended at the end of the strings table (a key travels as its index). The keys of the old drawer (`FilesFolderOn`, `ChangeFolder`, `FilesDownload`, `FilesOpen`, `FilesItems`, `FilesDrop`, `ChooseFiles`, `FilesPrevPage`, `FilesNextPage`, `FilesSaveTo`, `FilesPickFolder`) stay in the table, unused, until a release where the index shift is safe (GUESS: the R-UI-12 cleanup decides whether an unused key may be dropped while services of an older build may still send indices). The plural forms ("item(ns)") wait for the 1.2 plural work (F-UI-01).

## 10. Build plan: thin slices

`swapdesk-paint`, `swapdesk-proto` and `swapdesk-platform` are edited by the main session only. Every slice that changes a screen is proven on both systems in the same slice, with its captures on the design canvas. Each test name below is a proposal.

### Slice 1: the explorer, browse and copy both ways (no wire change; works with 1.0 and 1.1 peers, the other computer's home folder only)

- **Builds**: the Arquivos view (command bar, navigation pane with this computer's places and volumes appearing and going, two panes, address bar with breadcrumbs, back/forward/up, columns, sort, filter, selection with mouse and keyboard, status lines, placeholders, empty and error states including a local volume that went away), the copy button and in-app copy/paste across panes (files and folders down; files up), the transfers bar and its expanded panel with the job queue past 8, conflicts with "Manter os dois" and "Pular", the disconnect question, "Mostrar" and the show-in-file-manager effect, open a local file. Remote places derived from the home listing; local places and volumes from the shells. The drawer loses its Arquivos tab; the old panel is removed.
- **Files**: `crates/swapdesk-ui/src/interact/explorer.rs` and `explorer/tests.rs` (new), `state.rs` (the view, effects `OpenLocal`, `RevealLocal`, `LocalPlaces`, event `LocalVolumesChanged`), `strings.rs` (appended keys), `mock.rs` (scenarios), `a11y.rs` (rows as list items with a selected state), `format.rs` (dates, time left); `crates/swapdesk-paint/src/screens_explorer.rs` (new), `view.rs` (disabled buttons, icons), `screens_viewer.rs` (the view switch, drawer tabs), `tests.rs`; `crates/swapdesk-shell-win` and `crates/swapdesk-shell-mac` (effects, volume notifications, the video layer hidden in the view, key routing); `files_panel.rs` and `screens_files.rs` removed.
- **Tests**: `explorer_opens_with_this_computer_left_and_the_peer_right`, `a_click_selects_one_ctrl_click_toggles_and_shift_click_extends_from_the_anchor`, `arrows_move_the_focus_and_shift_arrows_extend`, `select_all_and_type_ahead_find_by_name`, `copy_from_the_remote_pane_gets_into_the_folder_of_the_local_pane`, `copy_from_the_local_pane_sends_into_the_folder_of_the_remote_pane`, `a_folder_download_lists_every_level_and_queues_every_file`, `more_than_eight_files_wait_and_start_as_slots_free`, `names_sort_naturally_with_folders_first`, `the_filter_acts_150_ms_after_the_last_key`, `a_long_folder_draws_only_the_visible_rows`, `placeholders_show_after_100_ms_and_never_before`, `an_unreadable_folder_shows_the_reason_and_try_again`, `a_local_volume_that_appears_or_goes_updates_the_navigation_pane_without_moving_the_rows_above`, `a_pane_on_a_removed_local_volume_shows_its_error`, `a_conflict_asks_once_and_keep_both_is_the_default`, `disconnecting_with_transfers_running_asks_first`, `a_watch_session_disables_files_with_the_reason`, `a_1_0_peer_shows_the_home_folder_only_with_the_note`; in `swapdesk-paint`: the fake-painter test finds every explorer control in every explorer scenario; on the Mac: the window checks and the accessibility comparison against the shared tree.
- **Scenarios and captures**: `explorador` (both panes, three items selected on the right, drives in the navigation pane, the transfers bar with two running), `explorador-carregando`, `explorador-vazio`, `explorador-erro`, `explorador-disco-removido`, `explorador-conflito`, `explorador-transferencias` (expanded: running, paused with Retomar, verification failure, done with Mostrar, waiting), `explorador-desconectar`, `explorador-1-0` (a 1.0 peer: the home folder only, unavailable commands and the note), `explorador-escuro`. One capture per scenario from the Windows offscreen renderer and from the Mac app, published on the design canvas in light theme.
- **Cross-machine**: the harness's files switch browses the other computer, downloads a folder with an accented name and uploads two files, in both directions of the session, Windows and Mac.

### Slice 2a: managing this computer's files (no wire change)

- **Builds**: new folder, inline rename, move by cut and paste (also between volumes here), delete to the bin and permanently (with the card), properties with the folder count and a volume's space, hidden items, in the left pane. Effect `TrashLocal`. The bin APIs of both systems are read on their primary pages here.
- **Tests**: `a_new_folder_is_named_and_renamed_in_place`, `rename_refuses_a_taken_or_invalid_name_and_keeps_the_field_open`, `cut_and_paste_moves_inside_this_computer`, `delete_sends_to_the_bin_and_shift_delete_asks_first`, `delete_on_a_local_volume_without_a_bin_refuses_with_the_notice`, `properties_count_a_folder_and_stop_when_asked`.
- **Scenarios**: `explorador-renomear`, `explorador-excluir-para-sempre`, `explorador-propriedades`.

### Slice 2b: the other computer's volumes and its files (wire minor 1)

- **Builds**: the wire items of 7.3 with the numbers taken there; the helper's volume roots, volume identity and the volume list pushed on change (8.1); `FsCall` 18 to 24 in `swapdesk-files` and the helper; UI ops 46 to 48 and notification kind 22; the permission rule of 3.3; the D13 gate for the new messages; the link rule for changes at every component; the protected locations; the audit tags 19 and 20; the banner lines; folder upload; "Substituir"; pages, hidden flags and free space; places and volumes from the helper; the 1.0 fallbacks.
- **Wire fixtures**: in this change, every new message type (0x040C to 0x0411) is listed with minor 1 in `TYPES_AFTER_MINOR_0` and every new `ErrorCode` value (23 to 26) in `ENUM_VALUES_AFTER_MINOR_0` of `crates/swapdesk-proto/tests/wire_1_0.rs`, and both go into the `min_minor` table; the 1.0 fixtures are not regenerated.
- **Files**: `crates/swapdesk-proto` (types, codec, `PROTO_MINOR` if this lands first, the fixture lists), `docs/PROTOCOL.md`, `docs/CONTRACTS.md`, `docs/THREAT-MODEL.md`, `crates/swapdesk-ipc`, `crates/swapdesk-files` (`proxy.rs` roots, `local.rs`, `handler.rs`), `crates/swapdesk-host` (`service/files.rs`, audit), `crates/swapdesk-trust` (audit tags 19 and 20), `crates/swapdesk-ui`, the helper on both systems (volumes, bin and known folders through the OS: Windows in `swapdesk-sys-win` or the helper, macOS in the agent).
- **Tests**: `dir_page_volumes_and_fs_op_round_trip_and_reject_bad_input` (proto proptests), the 1.0 fixture test with the new types and values listed after minor 0, `nothing_of_minor_1_reaches_a_minor_0_peer` (session), `an_error_code_of_minor_1_becomes_its_1_0_equivalent_toward_minor_0`, `a_volume_path_resolves_only_to_a_published_volume`, `a_volume_id_is_never_reused_for_another_volume`, `a_path_on_a_removed_volume_answers_volume_gone`, `no_component_can_form_an_absolute_unc_device_or_stream_path`, `changes_never_pass_through_a_link_at_any_component`, `changes_in_protected_locations_are_refused`, `rename_and_move_never_replace`, `move_refuses_a_folder_into_itself`, `trash_on_a_volume_without_a_bin_deletes_nothing`, `the_ui_disk_refuses_every_new_call`, `changes_need_files_and_control`, `changes_wait_for_the_d13_answer`, `changes_and_reach_are_audited_by_count`, `replace_sends_the_old_file_to_the_bin_first`, `a_1_0_viewer_on_a_1_1_host_keeps_the_home_meaning_of_every_path`; the new decoders join the fuzz targets.
- **Cross-machine**: browse a second volume (a disk image on the Mac, a second drive or a stick on the PC), copy from it both ways, remove it mid-transfer and see the transfer wait and resume when it returns; create, rename, move and delete on the other computer both ways, the bin checked by hand on each system; a 1.1 viewer against the published 1.0.0 host shows the home folder only and the note.
- **Scenarios**: `explorador-outro-disco` (the right pane on a removable volume), `explorador-sem-lixeira` (the notice).

### Slice 3: drag and drop inside the app, and the context menu

- **Builds**: the painted drag (start threshold, pill, targets including places and volumes, highlight, auto-scroll, spring-loaded folders, Esc), move or copy inside one pane with the copy key, copy across panes, box selection; the context menu through the systems' menus.
- **Tests**: `a_drag_inside_one_pane_moves_and_with_the_copy_key_copies`, `a_drag_between_panes_always_copies`, `a_folder_cannot_be_dropped_into_itself`, `esc_cancels_a_drag_and_nothing_changes`, `the_context_menu_lists_the_same_items_on_both_systems`.
- **Scenarios**: `explorador-arrastando` (a drag over a folder row of the left pane), `explorador-menu` (the menu is native, so the capture shows the row it opened on and the menu is described, not captured on the Mac).

### Slice 4: drag from the system into the app

- **Builds**: Windows: `OleInitialize` on the UI thread, an `IDropTarget` on the session window, `CF_HDROP`; Mac: `registerForDraggedTypes` with file URLs on the canvas view and the destination methods; drops on the right pane, its folder rows, places and volumes, and on the picture in the Tela view; the same hover feedback.
- **Tests**: the target's decisions are pure functions in `swapdesk-ui` (`a_system_drop_on_a_remote_folder_uploads_into_it`, `a_drop_on_the_picture_sends_to_the_default_folder`, `a_drop_without_disk_files_is_refused_with_the_reason`); the OS parts are `#[ignore = "interactive: ..."]` tests that post a drop with real input on an idle PC.

### Slice 5 (1.2 by default): drag from the app to the system

- **Builds**: Windows: an `IDataObject` with `CFSTR_FILEDESCRIPTOR`, `CFSTR_FILECONTENTS` as `IStream` over a staging copy, `IDataObjectAsyncCapability`, `DoDragDrop`; Mac: `NSFilePromiseProvider` per item with its delegate on its own operation queue.
- **Tests**: interactive tests that drag a remote file onto a folder of the system's file manager and check the file and its SHA-256; a lost link mid-drag fails the system's copy without leaving a partial file in the destination.

### Where the slices go in the 1.1 plan

Proposal: the plan's files stage keeps its engine work (free space, names across systems, the Mac permission state) and its cross-machine proof, and runs first; slices 1, 2a, 2b, 3 and 4 follow it as five stages of the parity milestone, before its owner batch, which then validates the explorer's captures on the canvas. Slice 2b shares minor 1 with the accounts, grants-sync and HEVC work, and takes its numbers from 7.3. Slice 5 goes to the 1.2 list. The plan and its stage list change only through the main session.

## 11. Options rejected

- A separate window per session for files: more platform work in two shells for a view that a mode of the session window gives.
- A wider drawer: too narrow for two panes beside the picture.
- One pane with a computer switch: the copy destination must then be chosen elsewhere; it is the drawer's model.
- A remote folder tree: a listing per expanded node; the places and volumes give the common reach.
- A separate grant for changing files: `CONTROL` already allows the same changes through the picture (decision 2).
- A separate grant for drives and volumes: the reach follows the user's own rights (decision 4).
- Paths that carry a drive letter or an absolute path on the wire: they would need new parsing on the helper and could collide with 1.0 home paths; a published volume token cannot.
- The systems' own file icons: they differ between Windows and macOS, against one interface.
- Moving files across computers: a copy plus a delete of the source on the other machine is a destructive step the user did not see.
- Permanent delete on the other computer: no undo for a remote action (decision 3).
- The controlled side browsing or pushing files into the controller: breaks the rule that a viewer accepts a file only as the answer to its own request.
- Opening a remote file on the other computer from the explorer: it starts a program there; the picture is the way to do that.
- Overwriting on conflict without the bin: loses data on a failed copy.
- A flag on the offer to replace on arrival: sending the old file to the bin first needs no new offer layout and keeps the engine's never-overwrite rule.
- `WM_DROPFILES` for drops from the system: no hover position, so no target highlight; Microsoft advises an OLE drop target.
- System drag-out in 1.1: long-lived COM objects and delegates over a network transfer on both systems; the interim covers the need.
- Searching recursively on the other computer: a listing per folder; the filter covers the open folder.
- A copy inside the other computer (one folder there to another folder there): it needs a new helper copy with progress; download and upload, or a move, cover 1.1.
- Ejecting a volume of the other computer: a new command with its own risks; the picture can do it.

## 12. For the owner

Decided on 2026-11-10: the full Arquivos view in the session window with two panes; changes there need `FILES` and `CONTROL`, no new grant; delete there to the bin only, refused with a notice where there is none; the explorer reaches the other computer's drives and volumes with the rights of the user there.

Defaults, taken as recommended unless the owner says otherwise: no move across computers; dragging out to the system's file manager in 1.2; a banner line on the controlled side without file names; hidden items hidden with a per-pane switch; Enter opens and F2 renames on both systems; a double click on a remote file only hints; "Manter os dois" as the default conflict answer; audit with counts on the controlled side.

Still open, recommendation first:

1. **Network drives and network volumes of the other computer** (raised by decision 4: through them the viewer reaches a third machine's share with the controlled user's credentials): left out in 1.1, so files stay between the two paired machines / published like any other volume, marked "Unidade de rede".
2. **Protected system locations** (raised by decision 4): reading allowed, changes refused there by Swapdesk on top of the OS's own rules (8.1) / only the OS's rules.
3. **What `FILES` says now** (raised by decision 4: a grant given under 1.0 reached the home folder only): the Permissões row and the prompt describe files as "os arquivos deste computador, com os direitos de quem está usando", with no new question to existing devices / also ask each user once, after the update, to confirm the files grants they gave.
4. **Every text of section 9**, in both languages.

No third-party dependency is needed: the Windows parts use the `windows` crate already in the tree, the Mac parts the `objc2` crates already in the tree.

## Sources

Read on 2026-11-10:

- Microsoft Learn, Handling Shell Data Transfer Scenarios: https://learn.microsoft.com/en-us/windows/win32/shell/datascenarios
- Microsoft Learn, FILEDESCRIPTORW structure: https://learn.microsoft.com/en-us/windows/win32/api/shlobj_core/ns-shlobj_core-filedescriptorw
- Microsoft Learn, DoDragDrop function: https://learn.microsoft.com/en-us/windows/win32/api/ole2/nf-ole2-dodragdrop
- Microsoft Learn, RegisterDragDrop function: https://learn.microsoft.com/en-us/windows/win32/api/ole2/nf-ole2-registerdragdrop
- Apple Developer, NSFilePromiseProvider: https://developer.apple.com/documentation/appkit/nsfilepromiseprovider
- Apple Developer, filePromiseProvider(_:writePromiseTo:completionHandler:): https://developer.apple.com/documentation/appkit/nsfilepromiseproviderdelegate/filepromiseprovider(_:writepromiseto:completionhandler:)
- Apple Developer, operationQueue(for:): https://developer.apple.com/documentation/appkit/nsfilepromiseproviderdelegate/operationqueue(for:)
- Apple Developer, beginDraggingSession(with:event:source:): https://developer.apple.com/documentation/appkit/nsview/begindraggingsession(with:event:source:)
- Apple Developer, NSFilePromiseReceiver: https://developer.apple.com/documentation/appkit/nsfilepromisereceiver
- Apple Developer, NSDraggingDestination: https://developer.apple.com/documentation/appkit/nsdraggingdestination
- Apple Developer, registerForDraggedTypes(_:): https://developer.apple.com/documentation/appkit/nsview/registerfordraggedtypes(_:)
- Apple Developer, NSDraggingSource: https://developer.apple.com/documentation/appkit/nsdraggingsource

Not read today, and so marked GUESS where used: the bin APIs (Windows `IFileOperation`, macOS `trashItemAtURL`), the volume enumeration and change notification APIs of both systems, volume identity (serials, GUIDs, UUIDs), and macOS's protection of removable and network volumes. Slices 1, 2a and 2b read their primary pages before building on them.
