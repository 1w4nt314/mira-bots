# mira-bots

Et lille statusvindue ("island") øverst på skærmen, der viser dine Claude Code-agenter og lader dig svare Tillad/Afvis, når en agent beder om lov til at bruge et værktøj. Appen kører Claude Code CLI i baggrunden med dit eget abonnement.

## Status: trin 3 — stadig tidligt, ikke brugbart endnu

Trin 3 er bygget: tickets med kø pr. agent, review og drag-and-drop oven på trin 2 (Workplace-vindue, terminal pr. agent, diagnostik og standardmappe til nye agenter). Det er stadig et tidligt trin, og intet af det er afprøvet på Windows endnu (se listen nederst).

Det virker (når det er testet på Windows, se nedenfor):

- Island-vindue, top-centreret på hovedskærmen, der folder ud ved hover.
- Workplace-vindue med pladser (en række arbejdspladser og en række stabspladser), en bot-figur pr. agent og en sidebar med fanerne Tilladelser, Diagnostik og Tickets.
- Terminal pr. agent: agentens egen `claude`-session vises og kan styres med tastaturet i Workplace.
- Start af agenter med ét klik i standardmappen, eller med valgfri mappe og rolle fra Workplace.
- Statusvisning pr. agent (starter, klar, tænker, læser, redigerer, kører, afventer tilladelse, afsluttet).
- Tillad / Afvis / "Altid for denne agent" på tilladelsesanmodninger.
- Tickets: opret, træk på en agent eller en tom plads, kø pr. agent, review med Godkend/Afvis og manuelle flyt (se afsnittet Tickets).
- Diagnostik og logfil til fejlsøgning.

Det findes ikke endnu: MCP (agenter kan ikke oprette tickets selv), systembakke, autostart.

## Sådan virker det

Island → Workplace → pladser → terminal:

1. Islanden er det lille statusvindue øverst. Den viser agenterne og tilladelsesanmodninger, og har en knap til at åbne Workplace.
2. Workplace er det store vindue. Hver agent sidder på en plads (arbejdsplads eller stabsplads) med en bot-figur, der viser tilstanden: hviler, arbejder, venter eller færdig.
3. Vælg en plads, så åbnes agentens terminal i panelet. Det er den rigtige interaktive `claude`-session (via ConPTY), så du kan svare direkte i den.
4. Roller (kode, research, review, koordinator) er stadig rent visuelle tags: de vælger figur og mappenavn og giver ingen andre prompts, værktøjer eller rettigheder.
5. Lofter: højst 5 agenter på arbejdspladser og 2 på stabspladser (kun kørende tæller med).

## Krav

- Windows 10 version 1809 eller nyere.
- Nyeste Claude Code installeret og logget ind. `claude` skal kunne findes som `%USERPROFILE%\.local\bin\claude.exe` eller i `PATH`. Ellers sæt miljøvariablen `MIRA_CLAUDE_PATH` til den fulde sti.
- Claude Code version 2.1.139 eller nyere. Appens hooks bruger exec-form (`command` + `args`), som ældre versioner ikke understøtter. Fanen Diagnostik viser den fundne version og om `args` understøttes.
- WebView2 (følger med Windows 11; installeren henter den på Windows 10).

## Sådan starter du en agent

Nye agenter starter som standard i en egen mappe under `%USERPROFILE%\mira-bots\agents\<navn>\`, fx `bot-01`, `bot-02` (med rolle: `coder-01` osv.). Appen opretter mappen og vælger det første ledige nummer. Et klik på knappen i islanden starter en agent i standardmappen. I Workplace kan du i stedet vælge rolle, plads og en anden mappe med mappevælgeren.

## Tickets

En ticket er en opgave, du selv opretter i fanen Tickets i Workplace (titel, valgfri beskrivelse og evt. "Spring review over"). Den går gennem disse trin:

1. **Backlog**: nyoprettet, ingen agent.
2. **I kø**: tildelt en agent. Hver agent har sin egen kø og får én ticket ad gangen.
3. **I gang**: appen har afleveret ticketen til agenten, og agenten arbejder på den.
4. **Review**: agenten er færdig, og du skal godkende. Afsnittet Review står øverst i fanen, og islanden viser en lille chip "n i review", der åbner Workplace på fanen Tickets.
5. **Done** (Godkend) eller **Afvist** (Afvis med en påkrævet note).

Sådan bruger du dem:

- **Tildel**: træk en sticky note fra Backlog hen på en agents plads, eller brug knappen "Tildel…" på noten (tastaturvejen). Slip noten på en tom plads, så åbner appen dialogen "Ny agent til ticket" og starter agenten med ticketen først i køen.
- **Kø**: i agentens terminalpanel vises den aktuelle ticket og køen. Her kan du flytte en ticket én plads frem eller fjerne den fra køen (tilbage til Backlog).
- **Review**: Godkend flytter ticketen til Done. Afvis kræver en note; ticketen kommer så forrest i køen hos agenten (hvis den stadig kører, ellers i Backlog), og noten står i ticket-filen under "Afvist".
- **Manuelle flyt**: knapperne i terminalpanelet kan flytte den aktuelle ticket til Review (eller Done ved "Spring review over") eller tilbage til Backlog, og en ticket i Review kan flyttes tilbage til I gang ("Ikke færdig"). Tickets i Backlog, Done og Afvist uden agent kan slettes.
- **Send igen**: hvis afleveringen ikke blev bekræftet, eller turen fejlede, vises en advarsel på ticketen og i terminalpanelet. "Send igen" afleverer den aktuelle ticket til agenten på ny, når agenten er klar.

**Sådan leveres en ticket.** Når agenten er klar og har en ticket i kø, skriver appen en fil `.mira-bots\tickets\<kort-id>.md` i agentens mappe (titel, id, beskrivelse, evt. afvisningsnote og et par regler) og taster én linje i agentens terminal, der peger på filen. Beskrivelsen sendes aldrig til terminalen, kun filen. Mappen `.mira-bots\` får en egen `.gitignore` med `*`, så den ikke dukker op i `git status` i agentmappen. Titlen renses, før den tastes (usynlige tegn, `@`, `/` og lignende, der kan udløse Claude Codes forslagslister). Appen venter ca. 0,75 sekund efter, at agenten er klar, taster linjen og sender Enter. Bekræftes afleveringen ikke inden for få sekunder, prøver appen Enter igen én gang og markerer derefter ticketen med en advarsel.

**Når agenten er færdig.** Appen bruger Stop-hooket som "turen er slut": ticketen flyttes automatisk til Review (eller til Done, hvis den er oprettet med "Spring review over"), og agentens næste ticket i køen afleveres. Fejler turen (hooket StopFailure, fx en API-fejl), bliver ticketen i gang med advarslen "Turn fejlede", og du kan bruge "Send igen". Afbryder du turen med Esc, kommer der intet Stop; så bliver ticketen stående som I gang, indtil du selv flytter den.

**Agenter kan ikke oprette tickets selv i trin 3.** Det kommer med MCP i trin 4. Roller er stadig kun visuelle og påvirker ikke, hvilke tickets en agent får.

**Lagring.** Tickets ligger i `%APPDATA%\dk.mira.bots\tickets.json` og overlever genstart. Filen skrives atomisk (først en midlertidig fil, som så omdøbes). Er filen beskadiget, omdøbes den til `tickets.json.broken-<tidspunkt>`, appen starter med en tom liste, og Diagnostik viser en advarsel. Kan filen slet ikke åbnes (fx låst af antivirus eller backup), starter Tickets skrivebeskyttet med en advarsel i Tickets-fanen, og filen røres ikke, før du genstarter appen. Ved hver start flyttes tickets, der var i kø eller i gang, tilbage til Backlog med noten "app genstartet"; Review, Done og Afvist er urørte. Stopper eller fjerner du en agent, eller afsluttes den, havner dens tickets også i Backlog med en note.

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

Trin 3 tilføjede én npm-afhængighed, `@dnd-kit/core` (6.3), til drag-and-drop, og ingen nye Rust-afhængigheder. Tickets gemmes i en JSON-fil bag traitet `TicketStore`. SQLite blev fravalgt, fordi den native afhængighed ikke kan krydstjekkes fra Linux mod Windows-target i dette miljø; trait'et gør det muligt at skifte lager senere.

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
28. Levering i ConPTY: linjen skrevet som ét write efterfulgt af Enter som separat write 150 ms senere sender prompten i Claude Codes TUI (ikke indsat som tekst/linjeskift), og `UserPromptSubmit` kommer med en prompt, der begynder med `Ticket <kort-id>`.
29. Bekræftelses-tidslinjen: 750 ms efter Stop er inputfeltet klar, og den ekstra Enter ved genforsøg sender ikke en tom prompt og lukker ingen dialog.
30. Agenten læser `.mira-bots/tickets/<kort-id>.md` med `/` i stien uden tilladelsesspørgsmål, og `.mira-bots\.gitignore` holder mappen ude af `git status`.
31. Træk med dnd-kit i WebView2: en sticky note kan trækkes til en plads med mus og touchpad, et klik (under 6 px) på en plads vælger stadig terminalen, og trækket følger markøren.
32. Slip på en tom plads åbner dialogen med ticketen, "Start med ticket" starter agenten med linjen som første prompt, og den bekræftes inden 8 sekunder efter SessionStart.
33. Stop-hooket flytter ticketen til Review (eller Done ved "Spring review over"), Esc midt i turen giver intet Stop, og en API-fejl giver "Turn fejlede" med fungerende "Send igen".
34. `tickets.json` skrives atomisk i `%APPDATA%\dk.mira.bots\` (omdøbning over en eksisterende fil virker, ingen `.tmp` efterlades), og en beskadiget fil omdøbes til `.broken-<tidspunkt>` med advarsel i Diagnostik.
35. Efter genstart står tickets, der var i kø eller i gang, i Backlog med noten "app genstartet", og Review/Done er urørte.
36. Chippen "n i review" i den ikke-fokuserbare island åbner Workplace på fanen Tickets, både når vinduet oprettes og når det allerede er åbent.
37. Stop/Fjern af en agent med kø: alle dens tickets står i Backlog med note, en igangværende aflevering skriver ikke mere i terminalen, og `queueLength` er 0.
38. xterms automatiske svar (Device Attributes, cursor- og fokusrapporter) tæller ikke som brugerinput under ConPTY og udsætter ikke ticket-levering, mens tastetryk, piletaster og indsat tekst (også bracketed paste via Shift+Insert eller højreklik) gør.

## Licens og inspiration

Koden er MIT-licenseret (se `LICENSE`). Alle ikoner og andre assets er lavet til dette projekt. Idéen er inspireret af [Coucou](https://github.com/Louis-CFM/coucou); ingen af dets assets eller kode er genbrugt.
