# mira-bots

Et lille statusvindue ("island") øverst på skærmen, der viser dine Claude Code-agenter og lader dig svare Tillad/Afvis, når en agent beder om lov til at bruge et værktøj. Appen kører Claude Code CLI i baggrunden med dit eget abonnement.

## Status: trin 2 — stadig tidligt, ikke brugbart endnu

Trin 2 er bygget: Workplace-vindue, terminal pr. agent, diagnostik og standardmappe til nye agenter. Det er stadig et tidligt trin, og intet af det er afprøvet på Windows endnu (se listen nederst).

Det virker (når det er testet på Windows, se nedenfor):

- Island-vindue, top-centreret på hovedskærmen, der folder ud ved hover.
- Workplace-vindue med pladser (en række arbejdspladser og en række stabspladser), en bot-figur pr. agent og en sidebar med fanerne Tilladelser, Diagnostik og Tickets (Tickets er kun en pladsholder).
- Terminal pr. agent: agentens egen `claude`-session vises og kan styres med tastaturet i Workplace.
- Start af agenter med ét klik i standardmappen, eller med valgfri mappe og rolle fra Workplace.
- Statusvisning pr. agent (starter, klar, tænker, læser, redigerer, kører, afventer tilladelse, afsluttet).
- Tillad / Afvis / "Altid for denne agent" på tilladelsesanmodninger.
- Diagnostik og logfil til fejlsøgning.

Det findes ikke endnu: tickets, kø, MCP, systembakke, autostart.

## Sådan virker det

Island → Workplace → pladser → terminal:

1. Islanden er det lille statusvindue øverst. Den viser agenterne og tilladelsesanmodninger, og har en knap til at åbne Workplace.
2. Workplace er det store vindue. Hver agent sidder på en plads (arbejdsplads eller stabsplads) med en bot-figur, der viser tilstanden: hviler, arbejder, venter eller færdig.
3. Vælg en plads, så åbnes agentens terminal i panelet. Det er den rigtige interaktive `claude`-session (via ConPTY), så du kan svare direkte i den.
4. Roller (kode, research, review, koordinator) er i trin 2 rent visuelle tags: de vælger figur og mappenavn og giver ingen andre prompts, værktøjer eller rettigheder.
5. Lofter: højst 5 agenter på arbejdspladser og 2 på stabspladser (kun kørende tæller med).

## Krav

- Windows 10 version 1809 eller nyere.
- Nyeste Claude Code installeret og logget ind. `claude` skal kunne findes som `%USERPROFILE%\.local\bin\claude.exe` eller i `PATH`. Ellers sæt miljøvariablen `MIRA_CLAUDE_PATH` til den fulde sti.
- Claude Code version 2.1.139 eller nyere. Appens hooks bruger exec-form (`command` + `args`), som ældre versioner ikke understøtter. Fanen Diagnostik viser den fundne version og om `args` understøttes.
- WebView2 (følger med Windows 11; installeren henter den på Windows 10).

## Sådan starter du en agent

Nye agenter starter som standard i en egen mappe under `%USERPROFILE%\mira-bots\agents\<navn>\`, fx `bot-01`, `bot-02` (med rolle: `coder-01` osv.). Appen opretter mappen og vælger det første ledige nummer. Et klik på knappen i islanden starter en agent i standardmappen. I Workplace kan du i stedet vælge rolle, plads og en anden mappe med mappevælgeren.

## Første gang i en mappe

Første gang `claude` startes i en mappe, spørger Claude Code, om du har tillid til filerne i den. Spørgsmålet vises i agentens terminal i Workplace, og det er dér, du besvarer det.

Indtil du har svaret, afvikler Claude Code ingen hooks fra nogen settings-fil (jf. dokumentationen), så agenten ser ud til at stå på "starter". Efter ca. 15 sekunder uden hook-events viser appen derfor en tekst om at vente på svar i terminalen. Først efter accept begynder hooks at virke, og statusvisningen følger med.

Tip: kør `claude` én gang i en almindelig terminal i `%USERPROFILE%\mira-bots\agents` og accepter. Trusten for en overliggende mappe skal ifølge dokumentationen dække nye undermapper, så du slipper for spørgsmålet i hver ny agentmappe. Det er ikke afprøvet. Appen rører aldrig din `~/.claude` eller `~/.claude.json` og forsøger ikke at omgå spørgsmålet.

## Sådan virker hooks

Appen skriver sin egen `hooks.json` i `%APPDATA%\dk.mira.bots\` og starter hver agent som `claude --settings <den fil>`. Hooks-filen peger på `mira-hook.exe`, som sender hændelser til appen over en named pipe og (kun ved tilladelsesanmodninger) venter på dit svar. Er appen ikke startet, gør `mira-hook.exe` ingenting, og Claude Code påvirkes ikke.

Appen sætter `MIRA_BOTS_PIPE` og `MIRA_AGENT_ID` i agentens miljø. `MIRA_AGENT_ID` bruges til at koble hook-events til den rigtige agent, også efter `/clear`.

Din egen `~/.claude/settings.json` røres aldrig. Dine eksisterende globale hooks kører stadig ved siden af.

## Diagnostik og log

Fanen Diagnostik i Workplace viser blandt andet Claude Code-sti og -version, om hooks med `args` understøttes, hooks.json, pipen, antal modtagne hook-events og det sidste event. Knappen Kopiér lægger det hele på udklipsholderen som tekst til en fejlrapport, og Åbn logmappe åbner mappen med loggen.

Loggen ligger i `%LOCALAPPDATA%\dk.mira.bots\logs\mira-bots.log`. Den roteres ved hver start, og de seneste tre gamle filer gemmes. Sæt `MIRA_LOG=debug` (eller `trace`, `info`, `warn`, `error`) for mere detaljeret log; standard er `info`, og debug giver bl.a. én linje pr. hook-event.

## Compliance og ansvar

- Du er selv ansvarlig for din Claude-plan og for at overholde Anthropics vilkår.
- Appen laver intet login og rører ikke dine credentials. Den læser ikke `~/.claude` og ændrer ikke `claude`-programmet.
- Appen bruger kun `--settings` med sin egen hooks-fil. Den bruger ikke `--print`/`-p`, og den slår ikke tilladelsestjek fra.
- Brug af API-nøgle som alternativ kommer i et senere trin. Indtil da bruger `claude` den login, du allerede har.

## Hent en installer

Der er endnu ingen udgivelser. En installer bygges af GitHub Actions:

1. Åbn fanen **Actions** i repoet.
2. Vælg den seneste kørsel af workflowet `ci` på `main` (eller et `v*`-tag).
3. Hent artifact `mira-bots-windows-installers` (NSIS-installer, `.exe`). Hvis MSI kunne bygges, ligger den i `mira-bots-windows-msi`.

Kendt risiko: installeren er ikke kodesigneret. Windows SmartScreen og Defender kan advare eller blokere den, og en uunderskrevet Tauri-app kan udløse falske positiver i Defender (det er set hos Coucou). Kør den kun, hvis du har bygget den selv via Actions fra denne kode.

## Udvikling

Kræver Rust (stable), Node 22 og, på Linux, Tauris systembiblioteker (`libwebkit2gtk-4.1-dev` m.fl.) samt `llvm` (giver `llvm-rc`, som krydstjekket mod Windows-target skal bruge). `npm run tauri dev` kræver Windows.

```
npm ci
npm run tauri dev         # kun Windows
```

Trin 2 tilføjede to npm-afhængigheder, `@xterm/xterm` (6.0) og `@xterm/addon-fit` (0.11), og to Rust-plugins, `tauri-plugin-log` (fil-log) og `tauri-plugin-opener` (åbn mapper i Stifinder). De to plugins kaldes kun fra Rust; der er ingen tilsvarende npm-pakker.

Verifikation fra repo-roden (kan køres på Linux; Windows-koden tjekkes ved cross-check):

```
cargo check --workspace --target x86_64-pc-windows-msvc
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
npm run build
```

Miljøvariabler: `MIRA_CLAUDE_PATH` (sti til `claude`), `MIRA_HOOK_EXE` (sti til `mira-hook`), `MIRA_HOOK_DEBUG=1` (hook-logning på stderr), `MIRA_LOG` (logniveau for appen, standard `info`). `MIRA_BOTS_PIPE` og `MIRA_AGENT_ID` sættes af appen selv.

## Skal testes på Windows

Intet af dette kan afprøves i udviklingsmiljøet; hvert punkt står som `TODO(windows-verify)` i koden.

1. `focusable: false` forhindrer, at klik på Tillad/Afvis stjæler fokus (ellers skal `WS_EX_NOACTIVATE`/`WS_EX_TOOLWINDOW` sættes).
2. Det gennemsigtige vindue blinker ikke hvidt ved opstart.
3. Placering øverst i midten er korrekt ved 125 % og 150 % skalering og med proceslinjen øverst.
4. Interaktiv `claude.exe` kører korrekt i ConPTY (opstart, ingen hængende læser, exitkode ved `/exit`).
5. Hooks med `command` + `args` og sti med skråstreger virker med den installerede Claude Code-version, og de fyres for `--settings`-filen.
6. `MIRA_BOTS_PIPE` arves af hook-processen fra `claude.exe`.
7. Named pipe: ny instans pr. forbindelse ved samtidige hændelser, og genforsøg ved optaget pipe.
8. Ved tilladelsesanmodning vises terminalens egen dialog ikke, mens hooket venter, og den vises efter et "intet svar".
9. Stop af en agent afslutter `claude.exe`, og der efterlades ingen `node`/`claude`-processer efter "Afslut".
10. `mira-hook.exe` findes under `resources/` efter NSIS-installation.
11. Mappevælgeren åbner foran islanden og giver en sti, som start af agent accepterer.
12. Et tomt `resources`-mønster i `tauri.conf.json` passerer `tauri build` på Windows-CI.
13. MSI-target bygger på `windows-latest` (ellers kun NSIS).
14. Defender/SmartScreen-reaktion på den uunderskrevne installer.
15. `MIRA_AGENT_ID` arves af `mira-hook.exe`, frames bærer `agent_id`, og efter `/clear` følger agentens status stadig med.
16. Trust-dialogen vises i terminalpanelet; efter accept kommer hooks, og agenten går til klar. Trust af `agents\` dækker nye agentmapper.
17. `claude --version`-proben afsluttes hurtigt uden blinkende konsolvindue, og outputtet parses.
18. Logfilen ligger på den rigtige sti, roteres ved start, og et panic ender i loggen; `MIRA_LOG=debug` giver én linje pr. hook-event.
19. Workplace åbnes uden deadlock, lukning afslutter ikke appen, genåbning virker, og `invoke`/`listen` virker i vinduet.
20. xterm gengiver Claude Codes TUI korrekt under ConPTY (farver, cursor, resize) og tastatur inkl. Enter, piletaster, Ctrl+C, Tab, Esc og æøå når frem.
21. Output til Workplace, mens vinduet ikke findes, giver hverken fejl-spam i loggen eller tab af data.
22. "Åbn mappe" og "Åbn logmappe" åbner Stifinder på den rigtige mappe.
23. Begge vinduer følger Windows' app-tema (lyst/mørkt), og et skift udskifter bot-figurer og terminaltema live.
24. "Workplace" fra den ikke-fokuserbare island giver Workplace fokus uden at islanden ændrer adfærd.
25. Standardmappen `%USERPROFILE%\mira-bots\agents\bot-01` oprettes, og `bot-01` genbruges efter genstart, når den er ledig.
26. Kopiér i Diagnostik virker i WebView2; ellers vises et tekstfelt til at kopiere fra.
27. Ydelse: 5 agenter med kraftigt output og ét åbent terminalpanel giver ingen mærkbar UI-lag.

## Licens og inspiration

Koden er MIT-licenseret (se `LICENSE`). Alle ikoner og andre assets er lavet til dette projekt. Idéen er inspireret af [Coucou](https://github.com/Louis-CFM/coucou); ingen af dets assets eller kode er genbrugt.
