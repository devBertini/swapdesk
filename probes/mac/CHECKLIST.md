# Swapdesk: checklist da sonda M0-mac (para rodar no MacBook)

Projeto de referência: `docs/research/macos.md`, seção 12. A sonda é descartável: serve só para responder sim ou não às perguntas abaixo. Um passo que falha não impede os outros.

## Como os resultados voltam para a sessão

- Cada checagem da sonda escreve uma linha JSON no log: `~/Library/Logs/SwapdeskProbe/<nome>.log` (modo usuário) ou `/Library/Logs/SwapdeskProbe/<nome>.log` (modo root: daemon e agente da tela de login). Os logs de root podem ser lidos sem `sudo`.
- No fim, um comando gera o arquivo **`results.txt`**: o bloco **AUTO** resume cada log (quantos `ok`, quantos falharam, a última linha de cada um) e o bloco **MANUAL** tem uma linha por pergunta, que você preenche com `sim`, `nao` ou `?` e uma nota curta. Rodar o comando de novo reescreve o AUTO e mantém o que você escreveu no MANUAL.
- Quando terminar, leve `results.txt` e a pasta `results/` (cópia dos logs) de volta para o PC, para dentro de `probes/mac/` do repositório (AirDrop, pendrive ou o armazenamento que você já usa), e avise a sessão.
- Nada aqui envia dados para fora do Mac. A notarização (opcional, `--notarize`) é a única coisa que envia algo à Apple, e só roda se você pedir.

Convenções deste documento:
- `PROBE` significa `/Applications/SwapdeskProbe.app/Contents/MacOS/swapdesk-probe` (para rodar a sonda direto no Terminal).
- `APP` significa `/Applications/SwapdeskProbe.app` (para rodar a sonda como app, com `open -W $APP --args <subcomando>`; a saída vai só para o log, `~/Library/Logs/SwapdeskProbe/app.log`).
- Defina no Terminal, uma vez por janela: `PROBE=/Applications/SwapdeskProbe.app/Contents/MacOS/swapdesk-probe; APP=/Applications/SwapdeskProbe.app; LOGS=~/Library/Logs/SwapdeskProbe; RLOGS=/Library/Logs/SwapdeskProbe`
- Horários nos logs estão em UTC (termina em `Z`). `date -u` mostra a hora UTC de agora.
- Para o agente de usuário e o da tela de login, você muda os testes sem refazer o app: escreva `chave=valor` em `$LOGS/agent.conf` (ou `agent-inject.conf`; para o root, `$RLOGS/loginwindow.conf` e `$RLOGS/daemon.conf`). Chaves: `tests` (lista separada por vírgula: `info,tcc,tcc-request,sck,sck-audio,cgstream,inject,net,ipc,bonjour`), `every`, `peer`, `port`, `text`, `inject-delay`, `inject-keycode`, `inject-token`. A sonda relê o arquivo a cada ciclo (15 s).

## Preparação (uma vez)

1. Traga a pasta `probes/mac` do repositório para o Mac (por exemplo para `~/swapdesk-probe/mac`).
2. `xcode-select -p` mostra um caminho? Se não: `xcode-select --install` e aguarde terminar.
3. `rustc --version`. Se não existir: instale Rust pelo método oficial da página https://www.rust-lang.org/tools/install (confira o comando na página antes de rodar). Depois `rustup target list --installed` deve listar `aarch64-apple-darwin`.
4. Conferir se há uma identidade Developer ID: `security find-identity -v -p codesigning`. Anote o nome inteiro, no formato `Developer ID Application: <Nome> (<TEAMID>)`.

## P0. Fatos da máquina (2 min)

1. `sw_vers; uname -m; fdesetup status; pmset -g | grep -Ei 'womp|powernap|tcpkeepalive'`
   Na tela: `ProductVersion` (esperado 14.4 ou mais), `arm64`, `FileVault is On` ou `Off`, `womp 1` ou `0`.
   Anote em `results.txt`, linha `P0.macos_version_and_fdesetup`.
2. `SDK=$(xcrun --show-sdk-path); grep -n 'CGWindowListCreateImage' "$SDK/System/Library/Frameworks/CoreGraphics.framework/Headers/CGWindow.h"`
   Na tela: a linha da declaração, com `API_DEPRECATED` ou `API_UNAVAILABLE` (ou nenhuma linha, se a API saiu do SDK). Copie a linha em `P0.sdk_declaration_CGWindowListCreateImage`.
3. `grep -n 'kVK_ISO_Section\|kVK_ANSI_Grave' "$SDK/System/Library/Frameworks/Carbon.framework/Frameworks/HIToolbox.framework/Headers/Events.h"`
   Esperado: `0x0A` e `0x32`. Resultado em `P0.sdk_kVK_values_0x0A_0x32`.

## P1. Compilar, assinar e instalar (5 min por assinatura)

1. `cd ~/swapdesk-probe/mac`
2. `PROBE_NONCE=1 bash make-app.sh devid "Developer ID Application: <Nome> (<TEAMID>)"`
   O script faz `cargo build --release`, monta `SwapdeskProbe.app` em `out/`, assina de dentro para fora (agente, app principal), verifica e copia para `/Applications`. Na tela, o fim é `installed: /Applications/SwapdeskProbe.app (signer devid, nonce 1)`. A primeira compilação baixa as dependências e leva alguns minutos.
3. `codesign --verify --deep --strict --verbose=2 $APP`
   Esperado: `valid on disk` e `satisfies its Designated Requirement`.
4. `codesign -d -r - $APP; codesign -dvvv $APP 2>&1 | grep -E 'CDHash=|Authority=|TeamIdentifier=|flags='; dwarfdump --uuid $APP/Contents/MacOS/swapdesk-probe`
   Esperado: a linha `designated => anchor apple generic and identifier "local.swapdesk.probe" and ...`, `flags=0x10000(runtime)`, e um UUID.
5. `xattr -lr $APP | grep -c quarantine`
   Esperado: `0`.
6. Primeiro teste do app: `open -W $APP --args info; tail -n 1 $LOGS/app.log`
   Esperado: uma linha com `"check":"info"`, `"ok":true`, `"session":"aqua"`, `"uid":501` (ou o seu), `macos` com a versão e `codesign` com o `Identifier` e `TeamIdentifier`.
7. Gere o arquivo de resultados: `$PROBE report --out ~/swapdesk-probe/mac/results.txt` (diz `wrote ...`). Abra com `open -e ~/swapdesk-probe/mac/results.txt`. Preencha em `P1.codesign_verify_ok_per_signer` o que viu nos passos 3 a 5.
   A cada fim de grupo (P2, P4, ...), rode esse comando de novo para o AUTO se atualizar.

## P2. SMAppService: daemon e agente Aqua (5 min)

1. `open -W $APP --args smapp register daemon; open -W $APP --args smapp status daemon; tail -n 2 $LOGS/app.log`
   Na tela: uma notificação "Item em segundo plano adicionado" (ou parecida). No log: `status` igual a `requiresApproval`. Abra Ajustes do Sistema > Geral > Itens de Início e Extensões, seção "Permitir em segundo plano", e ligue o item "Swapdesk Probe" (pode pedir sua senha). Rode `open -W $APP --args smapp status daemon; tail -n 1 $LOGS/app.log`: esperado `enabled`.
   Resultado em `P2.daemon_register_requires_approval_then_enabled`.
2. `sudo launchctl print system/local.swapdesk.probe.daemon | grep -E 'state|program'; tail -n 3 $RLOGS/daemon.log | cut -c1-200`
   Esperado: `state = running` e linhas `run.cycle` a cada 15 s (uma linha `net` com falha por falta de `peer` é normal agora).
3. `open -W $APP --args smapp register agent; open -W $APP --args smapp status agent; tail -n 2 $LOGS/app.log; launchctl print gui/$(id -u)/local.swapdesk.probe.agent | grep -E 'state|program'`
   Esperado: `enabled` (ou `requiresApproval`) e `state = running`. Anote em `P2.agent_enabled_after_approving_daemon` se aprovar o daemon já aprovou o agente (sim) ou se pediu outra aprovação (nao).

## P3. SMAppService: agente da tela de login (2 min)

1. `open -W $APP --args smapp register loginwindow; tail -n 1 $LOGS/app.log | cut -c1-400`
   Esperado (pela pesquisa): erro "Service cannot load in requested session" ou `enabled` que nunca roda. Copie o texto em `P3.loginwindow_via_smappservice_error_text`.
2. `open -W $APP --args smapp unregister loginwindow`

## P4. Agente Aqua: permissões, SCK vídeo e áudio, tela bloqueada (10 min)

1. Em uma segunda janela do Terminal: `tail -f ~/Library/Logs/SwapdeskProbe/agent.log | cut -c1-260`
2. O agente já roda `sck` a cada 15 s (P2.3). Na tela deve aparecer o pedido de **Gravação de Tela e Áudio do Sistema**. Anote **o nome que aparece no pedido** (`P4.screen_recording_prompt_name_shown`) e **como a entrada se chama em** Ajustes do Sistema > Privacidade e Segurança > Gravação de Tela e Áudio do Sistema (`P4.settings_list_entry_name`). Permita. Se o sistema pedir para "Sair e Reabrir", clique, depois rode `launchctl kickstart -k gui/$(id -u)/local.swapdesk.probe.agent`.
   Esperado no log: `"check":"sck.frames","ok":true` com `frames` maior que 0 e `mean_luma` entre 0 e 255.
3. Áudio: `echo 'tests=info,sck-audio' > $LOGS/agent.conf` e, em seguida, `for i in 1 2 3 4 5 6; do afplay /System/Library/Sounds/Ping.aiff; sleep 3; done`
   Esperado no log: `"check":"sck.audio","ok":true` com `audio_buffers` maior que 0 e `peak` maior que 0. Resultado em `P4.sck_video_frames_and_audio_in_aqua_agent` (sim só se vídeo e áudio funcionaram).
4. Acessibilidade: `echo 'tests=info,tcc-request,sck' > $LOGS/agent.conf`. Na tela aparecem pedidos de Acessibilidade (e possivelmente Monitoramento de Entrada). Permita em Privacidade e Segurança > Acessibilidade. Esperado no log: `"check":"tcc"` com `"post_event":true,"ax_trusted":true`. Resultado em `P4.accessibility_granted_post_event_true`. Depois: `echo 'tests=info,tcc,sck' > $LOGS/agent.conf`.
5. Tela bloqueada: rode `date -u` e anote a hora; pressione Control-Command-Q; espere 45 s; desbloqueie. Depois: `grep '"check":"sck.frames"' $LOGS/agent.log | tail -6 | cut -c1-300`. Compare os horários `t` com o período em que a tela estava bloqueada: houve `frames` maior que 0? O `mean_luma` ficou diferente do de antes (a tela de bloqueio é outra imagem)? Resultado em `P4.lock_screen_capture_frames_and_luma`.
6. Injeção com a tela bloqueada: `open -W $APP --args smapp register agent-inject`; `echo 'inject-delay=10' > $LOGS/agent-inject.conf`; `launchctl kickstart -k gui/$(id -u)/local.swapdesk.probe.agent.inject`. Em até 8 segundos, bloqueie a tela (Control-Command-Q) e observe: o ponteiro desenha um quadrado no centro? Aparecem 3 pontinhos no campo de senha? **Não pressione Return.** Depois: `grep inject $LOGS/agent-inject.log | cut -c1-300`. Se o quadrado aparece mas os pontinhos não, repita com `printf 'inject-delay=10\ninject-keycode=0\ninject-token=2\n' > $LOGS/agent-inject.conf` e o mesmo `kickstart`. Resultado em `P4.lock_screen_inject_pointer_moved_and_dots` (escreva quais keycodes funcionaram).
7. Gere o `results.txt` de novo (P1.7).

## P5 e P6. Agente da tela de login: SCK e CGEventPost (10 min)

1. Instale o plist legado (copia, dono root, sem quarentena):
   `sudo mkdir -p $RLOGS && sudo cp $APP/Contents/Resources/legacy/local.swapdesk.probe.loginwindow.plist /Library/LaunchAgents/ && sudo chown root:wheel /Library/LaunchAgents/local.swapdesk.probe.loginwindow.plist && sudo chmod 644 /Library/LaunchAgents/local.swapdesk.probe.loginwindow.plist && sudo xattr -c /Library/LaunchAgents/local.swapdesk.probe.loginwindow.plist && plutil -lint /Library/LaunchAgents/local.swapdesk.probe.loginwindow.plist`
   Esperado: `... .plist: OK`.
2. Menu Apple > Encerrar Sessão. **Fique na tela de login por 90 segundos.** Observe se o ponteiro desenha um quadrado no centro (a injeção roda uns 30 s depois que o agente sobe) e se `swapdesk` aparece no campo de nome (se a tela de login mostra "Nome e senha") ou pontinhos no campo de senha. **Nunca pressione Return.** Anote o que viu em `P6.loginwindow_pointer_square_plain` e `P6.loginwindow_text_appeared_plain`.
3. Faça login. `cat $RLOGS/loginwindow.log | cut -c1-300 | head -40`
   Esperado: linhas com `"session":"loginwindow"` e `"uid":0`, depois `sck.shareable` e `sck.frames`. Anote ok ou o texto do erro em `P5.loginwindow_sck_frames`.
4. Se o SCK falhou: `sudo sh -c "echo 'tests=info,tcc,cgstream,inject' > $RLOGS/loginwindow.conf"` e repita os passos 2 e 3 (a sonda relê a configuração quando sobe no login). Resultado em `P5.loginwindow_cgstream_fallback_frames`.
5. P6, variante com seção `__CGPreLoginApp`, só se a injeção não funcionou: `sudo mv $RLOGS/loginwindow.log $RLOGS/loginwindow-plain.log`, depois `cd ~/swapdesk-probe/mac && PROBE_FEATURES=prelogin-section PROBE_NONCE=3 bash make-app.sh devid "Developer ID Application: <Nome> (<TEAMID>)"`, e repita os passos 2 e 3. Resultado em `P6.loginwindow_inject_with_prelogin_section` (a injeção mudou?).

## P7. Permissões entre recompilações: Developer ID, ad-hoc, auto-assinado (15 min)

Antes de começar: `open -W $APP --args smapp unregister agent; open -W $APP --args smapp unregister agent-inject; open -W $APP --args smapp unregister daemon`.

Para o assinador `selfsigned`, crie o certificado uma vez: Acesso às Chaves > menu Acesso às Chaves > Assistente de Certificado > Criar um Certificado; nome `Swapdesk Probe Dev`, Tipo de Identidade "Raiz Autoassinada", Tipo de Certificado "Assinatura de Código". Confira com `security find-identity -v -p codesigning`; se não aparecer como válido, abra o certificado, Confiar > Assinatura de Código > Confiar Sempre.

Para cada S em `devid`, `adhoc`, `selfsigned` (identidade: o nome inteiro no `devid`; `Swapdesk Probe Dev` no `selfsigned`; nada no `adhoc`):
1. `tccutil reset ScreenCapture local.swapdesk.probe; tccutil reset Accessibility local.swapdesk.probe; tccutil reset ListenEvent local.swapdesk.probe`
   (os nomes dos serviços são um palpite; se o `tccutil` recusar um, remova a entrada do app em Ajustes do Sistema com o botão "-").
2. `cd ~/swapdesk-probe/mac && PROBE_NONCE=1 bash make-app.sh S "<identidade>"`; depois `open -W $APP --args tcc --request`. Permita Gravação de Tela e Acessibilidade para o app (pode ser preciso rodar de novo). Então `open -W $APP --args tcc; tail -n 1 $LOGS/app.log | cut -c1-300`: esperado `screen_capture`, `post_event` e `ax_trusted` verdadeiros.
3. `codesign -dvvv $APP 2>&1 | grep CDHash` (anote). `PROBE_NONCE=2 bash make-app.sh S "<identidade>"` e `codesign -dvvv $APP 2>&1 | grep CDHash` de novo: o CDHash deve ter mudado.
4. `open -W $APP --args tcc; tail -n 1 $LOGS/app.log | cut -c1-300`. Anote os quatro valores. Pela pesquisa: `devid` mantém, `adhoc` perde, `selfsigned` é desconhecido. Resultado em `P7.devid_tcc_survives_rebuild`, `P7.adhoc_tcc_survives_rebuild` e `P7.selfsigned_tcc_survives_rebuild`.
5. Só para `adhoc` e `selfsigned`: `open -W $APP --args smapp register daemon; tail -n 1 $LOGS/app.log | cut -c1-300`: o registro funciona? Resultado em `P7.adhoc_and_selfsigned_smappservice_daemon_registers`. Depois desfaça: `open -W $APP --args smapp unregister daemon`.
No fim, reinstale com `devid` (`PROBE_NONCE=1 bash make-app.sh devid "..."`) e registre de novo o daemon e o agente (P2) para os próximos testes.

## P8. Rede local (15 min; precisa do PC com Windows)

1. No PC (Windows), em `probes/mac`: `cargo build --release`, depois `target\release\swapdesk-probe.exe net-echo --port 47999`. Se o Windows perguntar, permita na rede Privada. Descubra o IPv4 do PC: `ipconfig`. Chame de `IP`.
2. No Mac, no Terminal: `$PROBE net --peer IP --port 47999 --seconds 20 --name terminal`. Esperado: nenhum alerta e `net.send` com `ok:true` desde o primeiro segundo; no PC aparecem linhas `net_echo.recv`. Resultado em `P8.terminal_no_prompt_and_ok`.
3. Como app: `open -W $APP --args net --peer IP --port 47999 --seconds 60`. Esperado: o alerta de **Rede Local** com o texto "O Swapdesk Probe testa a conexão...". **Espere 10 segundos antes de clicar em Permitir.** Depois: `grep -E 'net\.(send|summary)' $LOGS/app.log | tail -70 | cut -c1-220`. Os envios antes do clique foram `ok:false`? Com qual `os_error`? Depois do Permitir virou `ok:true`? O `net.summary` mostra `errors_before_first_ok`. Resultado em `P8.app_prompt_and_errors_before_allow`.
4. `open -W $APP --args bonjour --seconds 30` e, ao mesmo tempo, em outra janela: `dns-sd -B _swapdesk._udp`. Esperado: no `dns-sd` aparece a instância `swapdesk-probe`; no log, `bonjour.registered` com `ok:true`. Resultado em `P8.bonjour_instance_seen_by_dns_sd`.
5. Agente Aqua: `printf 'tests=info,net\npeer=IP\nport=47999\n' > $LOGS/agent.conf` (troque `IP`). Em até 15 s: aparece um segundo alerta de Rede Local? Com qual nome? Anote também o nome em Privacidade e Segurança > Rede Local. Depois `grep '"check":"net.send"' $LOGS/agent.log | tail -3 | cut -c1-220`. Resultado em `P8.agent_prompt_name`.
6. Daemon (root): `sudo sh -c "printf 'peer=IP\nport=47999\n' > $RLOGS/daemon.conf"`. Em até 15 s: `grep '"check":"net.send"' $RLOGS/daemon.log | tail -3 | cut -c1-220`. Esperado: `ok:true`, sem alerta. Resultado em `P8.daemon_ok_no_prompt`.
7. Agente root na tela de login: `sudo sh -c "printf 'tests=info,net\npeer=IP\nport=47999\n' > $RLOGS/loginwindow.conf"`, saia da sessão (como em P5), espere 60 s na tela de login, entre e veja `grep net.send $RLOGS/loginwindow.log | tail -3 | cut -c1-220` (confira `"session":"loginwindow"`). Esperado: `ok:true`. Resultado em `P8.loginwindow_agent_ok_no_prompt`. Depois `sudo rm $RLOGS/loginwindow.conf`.
8. Agente para o daemon por socket Unix: `printf 'tests=info,ipc\n' > $LOGS/agent.conf`; em até 15 s `grep ipc $LOGS/agent.log | tail -2 | cut -c1-220`. Esperado: `ipc.roundtrip` com `ok:true` e `reply` `pong ping`, sem alerta. Resultado em `P8.ipc_unix_socket_ok_no_prompt`. Depois: `printf 'tests=info,tcc,sck\n' > $LOGS/agent.conf`.
- Antes de refazer um teste de Rede Local no macOS anterior ao 27.2 não há como zerar a permissão: use outra conta de usuário padrão para repetir (TN3179).

## P9. Teclas: troca ISO e o tap do visualizador (5 min)

1. `open -W $APP --args keytap --seconds 30; tail -n 3 $LOGS/app.log | cut -c1-300`
   Na primeira vez: `keytap.create` com `ok:false` e um pedido de **Monitoramento de Entrada**. Permita em Privacidade e Segurança > Monitoramento de Entrada e rode de novo.
2. Durante os 30 s, pressione, nesta ordem: a tecla à esquerda do `1`, a tecla à direita do Shift esquerdo (se o teclado tiver), `ç`, Option direito. Depois: `grep '"check":"keytap.key"' $LOGS/app.log | grep '"kind":10' | tail -12 | cut -c1-260`. Cada linha mostra `kvk` (código) e `character`. Anote os quatro pares em `P9.key_left_of_1_code_and_char`, `P9.key_right_of_left_shift_code_and_char`, `P9.cedilla_code_and_char`, `P9.right_option_code_and_char`. Layout: `defaults read com.apple.HIToolbox AppleSelectedInputSources` em `P9.keyboard_layout`.
3. Tap ativo: `open -W $APP --args keytap --seconds 20 --active` e pressione Command+Tab. O seletor de apps fica oculto (evento engolido)? Rode `grep swallowed $LOGS/app.log | tail -3 | cut -c1-260` (`"swallowed":true`). Qual permissão foi pedida (Monitoramento de Entrada ou Acessibilidade)? Resultado em `P9.active_tap_swallows_cmd_tab_and_permission_asked`.

## P10. Acordar por rede (10 min; precisa do PC)

1. Mac na tomada. Ajustes do Sistema > Bateria > Opções > "Acordar para acesso à rede": anote as opções que aparecem e escolha a mais permissiva (`P10.wake_for_network_access_options`). `pmset -g | grep womp`.
2. `networksetup -getmacaddress Wi-Fi` (anote o MAC e o IP do Mac, `ipconfig getifaddr en0`). `sudo pmset sleepnow` e espere 2 minutos.
3. No PC: `target\release\swapdesk-probe.exe wol --mac <MAC> --broadcast <sub-rede>.255` e então `ping -n 20 <IP-do-Mac>`.
4. No Mac, depois de acordá-lo à mão se preciso: `pmset -g log | grep -E 'Wake from|DarkWake' | tail -5`. Houve uma ativação com motivo `WOL` ou `Network`? Resultado em `P10.wakes_on_magic_packet_lid_open`. Repita com a tampa fechada (na tomada): `P10.wakes_on_magic_packet_lid_closed`.

## P11. Desbloqueio do FileVault por SSH (opcional; macOS 26 ou mais)

Só se você decidir ligar o Login Remoto (aumenta a superfície de ataque do Mac): Ajustes do Sistema > Geral > Compartilhamento > Login Remoto. Reinicie. Do Windows: `ssh <usuario>@<IP-do-Mac>`; o prompt de desbloqueio aparece, digite a senha; o Mac sobe até a tela de login e o log do agente (P5) recomeça. Desligue o Login Remoto depois, se não for manter. Resultado em `P11.filevault_unlock_over_ssh_optional`.

## P12. VideoToolbox de baixa latência (opcional, 5 min)

`$PROBE vt --frames 600 --size 1920x1080 --force-key 100,300; grep '"check":"vt' $LOGS/app.log | tail -2 | cut -c1-500`
Anote: `vt.session` com `ok:true` (sessão criada com `EnableLowLatencyRateControl`), e em `vt.frames` os `keyframes_at` (esperado 0, 100 e 300), `forced_honoured`, `mean_ms` e `p95_ms`. Resultado em `P12.vt_low_latency_optional`. (Observação: a saída deste comando no Terminal vai para a tela e para `app.log`.)

## Entrega e limpeza

1. Gere o resultado final: `$PROBE report --out ~/swapdesk-probe/mac/results.txt` e confira o bloco MANUAL.
2. Copie os logs: `mkdir -p ~/swapdesk-probe/mac/results && cp $LOGS/*.log ~/swapdesk-probe/mac/results/ ; for f in $RLOGS/*.log; do cp "$f" ~/swapdesk-probe/mac/results/root-$(basename "$f"); done`
3. Leve `results.txt` e `results/` para `probes/mac/` no PC e avise a sessão.
4. Limpeza (assim que terminar):
   - `open -W $APP --args smapp unregister agent; open -W $APP --args smapp unregister agent-inject; open -W $APP --args smapp unregister daemon`
   - `sudo rm /Library/LaunchAgents/local.swapdesk.probe.loginwindow.plist` (um arquivo só, sem recursão)
   - `tccutil reset All local.swapdesk.probe; tccutil reset All local.swapdesk.probe.agent`
   - Arraste `/Applications/SwapdeskProbe.app` e `/Library/Logs/SwapdeskProbe` para a Lixeira (depois de copiar os logs). Remova qualquer conta de usuário criada para o P8. Se criou o certificado `Swapdesk Probe Dev`, apague-o no Acesso às Chaves.
