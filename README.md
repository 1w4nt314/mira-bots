# mira-bots

Et lille statusvindue ("island") øverst på skærmen, der viser dine Claude Code-agenter og lader dig svare Tillad/Afvis, når en agent beder om lov til at bruge et værktøj. Appen kører Claude Code CLI i baggrunden med dit eget abonnement.

## Status: trin 5 — stadig tidligt, ikke brugbart endnu

Trin 5 er bygget: agenter startes fra profiler med roller (koder, researcher, reviewer, koordinator, planlægger, debugger og specialist), hver rolle har sine egne værktøjer, review kan gå til en reviewer-agent, agenter kan lægge rapporter på tickets, og model og effort kan vælges pr. profil og skiftes undervejs. Det ligger oven på trin 4 (agentens værktøjer via en lokal MCP-server, `mira-mcp.exe`), trin 3 (tickets med kø pr. agent, review og drag-and-drop) og trin 2 (Workplace-vindue, terminal pr. agent, diagnostik og standardmappe til nye agenter). Det er stadig et tidligt trin, og intet af det er afprøvet på Windows endnu (se listen nederst).

Det virker (når det er testet på Windows, se nedenfor):

- Island-vindue, top-centreret på hovedskærmen, der folder ud ved hover.
- Workplace-vindue med pladser (en række arbejdspladser og en række stabspladser), en bot-figur pr. agent og en sidebar med fanerne Tilladelser, Diagnostik, Tickets og Agenter.
- Terminal pr. agent: agentens egen `claude`-session vises og kan styres med tastaturet i Workplace.
- Agentprofiler: syv indbyggede og dine egne, med roller, prompt-tillæg, model, effort og standardplads (se afsnittet Agentprofiler og roller).
- Start af agenter med ét klik i standardmappen, eller fra en valgfri profil, mappe og plads i Workplace.
- Statusvisning pr. agent (starter, klar, tænker, læser, redigerer, kører, afventer tilladelse, afsluttet) og live visning af model og effort.
- Tillad / Afvis / "Altid for denne agent" på tilladelsesanmodninger.
- Tickets: opret, træk på en agent eller en tom plads, kø pr. agent, review med Godkend/Afvis og manuelle flyt (se afsnittet Tickets).
- Review med reviewer-agenter, rapporter på tickets og en koordinator-rolle (se afsnittene nedenfor).
- Agentens værktøjer: 15 værktøjer, hvoraf nogle kun gives til bestemte roller (se afsnittet Agentens værktøjer).
- Skift af model og effort på en kørende agent (genstart med `--resume`).
- Diagnostik og logfil til fejlsøgning.

Det findes ikke endnu: systembakke, autostart, projekter og workspace-fil, chat, git worktrees, lyde og brug af API-nøgle. Alle agenter ligger stadig under `%USERPROFILE%\mira-bots\agents\` (eller den mappe, du vælger).

## Sådan virker det

Island → Workplace → pladser → terminal:

1. Islanden er det lille statusvindue øverst. Den viser agenterne og tilladelsesanmodninger, og har en knap til at åbne Workplace.
2. Workplace er det store vindue, indrettet som et kontor. Hver agent sidder ved sit skrivebord (arbejdsplads eller stabsplads) med en bot-figur, der viser tilstanden: hviler, arbejder, venter eller færdig. På bordet står en laptop, hvis skærm lyser i tilstandens farve (grå klar, blå arbejder, gul venter, grøn færdig), og når agenten arbejder, "skriver" den på skærmen. En post-it på bordet betyder, at agenten har en ticket i gang. Staben sidder øverst i et glaskontor (3 pladser), arbejdspladserne (5) nedenunder. En ledig plads er et tomt bord med en stol.
3. Vælg en plads, så åbnes agentens terminal i panelet. Det er den rigtige interaktive `claude`-session (via ConPTY), så du kan svare direkte i den.
4. Terminalpanelet og kontoret deler venstre side. Træk i grebet (splitteren) mellem dem for at ændre højden, eller fokusér det og brug pil op/ned (16 px ad gangen), Home og End; dobbeltklik nulstiller. Knappen ▁ minimerer terminalen til én linje nederst, så kontoret får hele højden; "Gendan" eller et klik på en plads henter den tilbage i den gemte højde. Knappen ⤢ maksimerer terminalen, og kontoret bliver en smal strimmel med alle pladser, hvor du stadig kan skifte agent; ⤡ gendanner den delte visning. Højde og tilstand huskes mellem starter.
5. En agent startes fra en profil. Profilen bestemmer agentens roller (de vælger figur, mappenavn, systemprompt og hvilke af appens værktøjer agenten får), model, effort og standardplads. Se afsnittet Agentprofiler og roller.
6. Lofter: højst 5 agenter på arbejdspladser og 3 på stabspladser (kun kørende tæller med).

### Kontor-detaljer

Knappen "Kontor: Diskret" / "Kontor: Lidt mere" i Workplace-headeren skifter detaljeniveau. "Lidt mere" sætter et par ting på hvert bord (krus, penne, papirer, plante eller lampe; altid de samme for den samme agent), arkivskab og papirkurv til venstre for glaskontoret og vandkøler og plante til højre, en væg med vinduer, et ur (lokal tid), et billede, en hylde og en whiteboard, og en korkbaggrund bag noterne på fanen Tickets. Valget huskes.

Indstillingerne (kontor-detaljer, terminalens tilstand og splitterens højde) gemmes i WebView2's lokale lager (localStorage) under `%LOCALAPPDATA%\dk.mira.bots`, ikke i `tickets.json`. Appens vinduer deler lageret. `tauri dev` har sit eget lager.

## Krav

- Windows 10 version 1809 eller nyere.
- Nyeste Claude Code installeret og logget ind. `claude` skal kunne findes som `%USERPROFILE%\.local\bin\claude.exe` eller i `PATH`. Ellers sæt miljøvariablen `MIRA_CLAUDE_PATH` til den fulde sti.
- Claude Code version 2.1.274 eller nyere for agentens værktøjer (trin 4: `mira-mcp` og tilladelser via `mcp_server.source`). Trin 1–3 (hooks med exec-form `command` + `args`, tickets) virker fra 2.1.139, men uden værktøjerne. Alt er verificeret mod 2.1.286. Trin 5 tilføjer hook-eventet `PostModelSwitch` og en `statusLine` i profilernes settings-fil; om ældre versioner stille ignorerer et ukendt hook-navn er ikke afprøvet (punkt 62 nederst). Fanen Diagnostik viser den fundne version, om `args` understøttes (≥ 2.1.139) og om agentværktøjerne er understøttet (≥ 2.1.274).
- `mira-mcp.exe` (agentens værktøjer) skal ligge ved siden af `mira-hook.exe`; installeren lægger dem begge i `resources`. Findes den ikke, kører agenterne uden værktøjer, og Diagnostik viser en advarsel.
- WebView2 (følger med Windows 11; installeren henter den på Windows 10).

## Sådan starter du en agent

Nye agenter starter som standard i en egen mappe under `%USERPROFILE%\mira-bots\agents\<navn>\`. Navnet kommer af profilens roller: en enkelt rolle uden specialist giver `coder-01`, `researcher-01`, `reviewer-01`, `koord-01`, `planner-01` eller `debugger-01`; en specialist eller flere roller giver `specialist-01`; ingen roller giver `bot-01`. Appen opretter mappen og vælger det første ledige nummer. Et klik på knappen i islanden starter en agent fra profilen Koder i standardmappen. I Workplace kan du i stedet vælge profil, plads og en anden mappe med mappevælgeren, og du kan overskrive profilens model og effort for netop denne agent.

## Tickets

En ticket er en opgave, du selv opretter i fanen Tickets i Workplace (titel, valgfri beskrivelse og evt. "Spring review over"). Den går gennem disse trin:

1. **Backlog**: nyoprettet, ingen agent.
2. **I kø**: tildelt en agent. Hver agent har sin egen kø og får én ticket ad gangen.
3. **I gang**: appen har afleveret ticketen til agenten, og agenten arbejder på den.
4. **Review**: agenten har afleveret (`mira_submit_for_review`) eller du har flyttet ticketen selv, og den skal godkendes af en reviewer-agent (se Review med reviewer-agenter) eller af dig. Agentens opsummering og rapporter står på kortet. Afsnittet Review står øverst i fanen, og islanden viser en lille chip "n i review", der åbner Workplace på fanen Tickets.
5. **Done** (Godkend) eller **Afvist** (Afvis med en påkrævet note).

Sådan bruger du dem:

- **Tildel**: træk en sticky note fra Backlog hen på en agents plads, eller brug knappen "Tildel…" på noten (tastaturvejen). Slip noten på en tom plads, så åbner appen dialogen "Ny agent til ticket" og starter agenten med ticketen først i køen.
- **Kø**: i agentens terminalpanel vises den aktuelle ticket og køen. Her kan du flytte en ticket én plads frem eller fjerne den fra køen (tilbage til Backlog).
- **Review**: Godkend flytter ticketen til Done. Afvis kræver en note; ticketen kommer så forrest i køen hos agenten (hvis den stadig kører, ellers i Backlog), og noten står i ticket-filen under "Afvist". Du kan altid godkende eller afvise, også når en reviewer-agent er sat på.
- **Manuelle flyt**: knapperne i terminalpanelet kan flytte den aktuelle ticket til Review (eller Done ved "Spring review over") eller tilbage til Backlog, og en ticket i Review kan flyttes tilbage til I gang ("Ikke færdig"). Tickets i Backlog, Done og Afvist uden agent kan slettes.
- **Send igen**: hvis afleveringen ikke blev bekræftet, eller turen fejlede, vises en advarsel på ticketen og i terminalpanelet. "Send igen" afleverer den aktuelle ticket til agenten på ny, når agenten er klar.
- **Ikke afleveret**: se "Når agenten er færdig" nedenfor.
- **Fra agent**: tickets, som en agent selv har oprettet, har et lille mærke "fra agent" og ligger i Backlog; historikken viser handlinger fra agenten som "agenten".

**Sådan leveres en ticket.** Når agenten er klar og har en ticket i kø, skriver appen en fil `.mira-bots\tickets\<kort-id>.md` i agentens mappe (titel, id, beskrivelse, evt. afvisningsnote og et par regler) og taster én linje i agentens terminal, der peger på filen. Beskrivelsen sendes aldrig til terminalen, kun filen. Mappen `.mira-bots\` får en egen `.gitignore` med `*`, så den ikke dukker op i `git status` i agentmappen. Titlen renses, før den tastes (usynlige tegn, `@`, `/` og lignende, der kan udløse Claude Codes forslagslister). Appen venter ca. 0,75 sekund efter, at agenten er klar, taster linjen og sender Enter. Bekræftes afleveringen ikke inden for få sekunder, prøver appen Enter igen én gang og markerer derefter ticketen med en advarsel.

**Når agenten er færdig.** Agenten afleverer selv ved at kalde værktøjet `mira_submit_for_review` med en kort opsummering: ticketen flyttes til Review (eller til Done, hvis den er oprettet med "Spring review over"), opsummeringen vises på review-kortet, og agentens næste ticket i køen afleveres, når agenten er klar. Stop-hooket betyder kun "turen er slut". Slutter turen uden aflevering, bliver ticketen stående som I gang med advarslen "Ikke afleveret" (agentens statuslinje: "Turn afsluttet uden aflevering"), og køen rykker ikke. Terminalpanelet viser to knapper:

- **Send til review**: flytter ticketen til Review (eller Done) uden agentens opsummering, når du selv vurderer, at opgaven er løst.
- **Bed om aflevering**: taster en kort linje i terminalen, der beder agenten kalde `mira_submit_for_review`. Den virker kun, når agenten er klar, og linjen sendes først, når du ikke selv har skrevet i terminalen et øjeblik.

Mens en agent har en ticket i gang, taster appen aldrig den næste ticket ind; først når ticketen er afleveret eller flyttet. Det gælder også, hvis du afbryder turen med Esc (der kommer intet Stop): ticketen står som I gang, indtil agenten afleverer, eller du flytter den. Fejler turen (hooket StopFailure, fx en API-fejl), bliver ticketen i gang med advarslen "Turn fejlede", og du kan bruge "Send igen". Konstanten `AUTO_REVIEW_ON_STOP` i `src-tauri/src/config.rs` (standard `false`) gendanner trin 3-adfærden, hvor Stop selv flytter ticketen til Review; det kræver en ny bygning.

**Roller** påvirker ikke, hvilke tickets en agent får: en ticket kan trækkes på alle agenter. En ticket på en stabsagent (typisk en koordinator) hedder "koordineringsopgave" i Workplace.

**Lagring.** Tickets ligger i `%APPDATA%\dk.mira.bots\tickets.json` og overlever genstart. Filen skrives atomisk (først en midlertidig fil, som så omdøbes). Er filen beskadiget, omdøbes den til `tickets.json.broken-<tidspunkt>`, appen starter med en tom liste, og Diagnostik viser en advarsel. Kan filen slet ikke åbnes (fx låst af antivirus eller backup), starter Tickets skrivebeskyttet med en advarsel i Tickets-fanen, og filen røres ikke, før du genstarter appen. Ved hver start flyttes tickets, der var i kø eller i gang, tilbage til Backlog med noten "app genstartet"; Review, Done og Afvist er urørte. Stopper eller fjerner du en agent, eller afsluttes den, havner dens tickets også i Backlog med en note.

## Agentprofiler og roller

En **profil** er opskriften på en agent: navn, roller, prompt-tillæg, model, effort, standardplads og evt. en indsnævring af værktøjerne. Fanen Agenter i Workplace lister profilerne og lader dig oprette, rette, nulstille og slette dem. Redigerer du en profil, gælder ændringen kun for nye agenter; en kørende agent beholder de roller, den blev startet med (en rolle kan ikke skiftes midt i en session).

**De seks roller** er koder, researcher, reviewer, koordinator, planlægger og debugger. Hver rolle giver et kort afsnit i agentens systemprompt ("Din rolle"), en figur og et mappepræfiks, og de styrer, hvilke af appens værktøjer agenten får (tabellen i afsnittet Agentens værktøjer). En profil kan have nul, én eller flere roller.

**Specialist** er en profil, ikke en rolle: profilen "Specialist (alle roller)" har alle seks roller og får en dynamisk figur, der tegnes ud fra rollerne. Din egen profil med flere roller (eller med "Specialist" slået til) er også en specialist og kan have vilkårlige roller, fx kun planlægger og researcher. En profil med netop én rolle får rollens egen figur; en profil uden roller får en neutral figur og kun fællesværktøjerne.

**Indbyggede profiler** (de genskabes, hvis filen mangler, og kan nulstilles, men ikke slettes):

| Profil | Roller | Standardplads |
|---|---|---|
| Koder | coder | arbejdsplads |
| Researcher | researcher | arbejdsplads |
| Reviewer | reviewer (må læse git-historik, men ikke committe eller pushe) | stabsplads |
| Koordinator | coordinator | stabsplads |
| Planlægger | planner | arbejdsplads |
| Debugger | debugger | arbejdsplads |
| Specialist (alle roller) | alle seks | arbejdsplads |

**Egne profiler** oprettes med "Ny profil": navn (1-60 tegn), roller, prompt-tillæg (højst 4000 tegn, kommer sidst i rolleafsnittet i systemprompten), model, effort og standardplads. Profilerne gemmes som én JSON-fil hver i `%USERPROFILE%\mira-bots\agents\.mira-bots\profiles\<id>.json`. En ugyldig fil omdøbes til `<id>.json.broken-<tidspunkt>`, og Diagnostik viser en advarsel.

**Model og effort.** Begge kan stå på "standard", og så vælger Claude Code selv (appen sender intet `--model`/`--effort`; standardmodellen afhænger af din konto). Model er et alias (`sonnet`, `opus`, `haiku`, `fable`, `best`, `opusplan`, `sonnet[1m]`, `opus[1m]`) eller et fuldt id som `claude-sonnet-5-5`; effort er `low`, `medium`, `high`, `xhigh` eller `max`. Appen kan kun tjekke formen på et model-id, ikke om din konto har adgang til modellen: en model, du ikke har adgang til, giver en fejl ved første forespørgsel i agentens terminal, ikke i appen. Headeren i terminalpanelet viser den aktuelle model og effort; først de ønskede værdier, derefter de observerede (markeret "observeret"), også hvis du selv skriver `/effort` i terminalen.

**Skift undervejs genstarter sessionen.** "Skift model" og "Skift effort" i terminalpanelet virker kun, når agenten er klar og ikke har en ticket i gang. Appen stopper da `claude` og starter den igen med `--resume <session-id>` og de nye `--model`/`--effort`, på samme plads og med samme terminalhistorik; samtalen fortsætter, og køen rykker videre, når agenten er klar igen. Har agenten endnu ikke fået nogen besked, findes der ingen samtale at genoptage, og den starter i stedet en ny session (`--session-id <ny uuid>`) med de samme flag. Fejler en genoptagelse alligevel ved opstart, står agenten som afsluttet med "Genstart fejlede: samtalen kunne ikke genoptages". Appen taster aldrig `/model` eller `/effort` i terminalen, fordi de skriver i din egen Claude Code-konfiguration (`~/.claude/settings.json`), som appen aldrig må røre. Efter første start kan effort kun skiftes til et konkret niveau, ikke tilbage til "standard".

## Review med reviewer-agenter

Når en ticket kommer i Review (agenten afleverede, du trykkede "Send til review", eller auto-review er slået til), vælger appen selv en reviewer:

- **Routing**: en kørende agent med rollen reviewer, aldrig den agent, der afleverede, og den med færrest åbne reviews (ved lighed den ældste). Findes ingen, bliver ticketen stående i Review til dig. Starter eller stopper en reviewer senere, forsøger appen igen; stopper en reviewer midt i et review, mister ticketen sin reviewer og bliver givet til en anden.
- **Levering**: appen skriver en review-fil `.mira-bots\reviews\<kort-id>.md` i reviewerens mappe (opsummering, rapporter, opgaven og regler) og taster en linje, der begynder med "Review af ticket", når revieweren er klar og ikke har en ticket i gang. Reviews går foran reviewerens arbejdstickets. Fejler leveringen, prøver appen igen ved næste ledige tidspunkt, i alt tre forsøg.
- **Afgørelse**: revieweren kalder `mira_approve_ticket` (ticketen bliver Done) eller `mira_reject_ticket` med en påkrævet note (ticketen kommer forrest i afsenderens kø og runden tælles op). Reviewere kan læse ændringerne med `git -C <mappe> diff`, `log`, `status` og `show`, men ikke committe eller pushe. Revieweren kan ikke afgøre sin egen aflevering.
- **Runde 1 af 3**: kortet viser "Runde r af 3". Efter tre afvisninger markeres ticketen "Eskaleret til dig": der sendes ikke flere review-linjer, og du afgør selv, eller vælger en reviewer manuelt.
- **Vælg reviewer / Fjern reviewer**: på review-kortet kan du vælge en bestemt kørende reviewer (ikke afsenderen) eller fjerne den nuværende, hvorefter appen vælger igen.

## Rapporter

Agenter kan lægge en rapport (markdown, højst 20 000 tegn, højst 20 pr. ticket) på en ticket med `mira_add_report`, eller aflevere og vedlægge rapporten i ét kald med parameteren `report` til `mira_submit_for_review`. En agent kan lægge rapporter på sine egne tickets; en reviewer kan lægge en review-rapport på en ticket i Review, som den er reviewer på. Alle agenter kan læse en rapport med `mira_get_report`.

Du kan selv tilføje en note under "Rapporter (n)" på ticketen (titel og tekst), og knappen "Åbn mappe" åbner rapportmappen i Stifinder. Rapporterne vises som markdown med en lille indbygget fortolker; HTML vises som tekst. Filerne ligger i `%APPDATA%\dk.mira.bots\tickets\<ticket-id>\reports\<nn>-<titel>.md` (aldrig i agentens mappe) og slettes sammen med ticketen. Metadata (titel, forfatter, tidspunkt) står i `tickets.json`.

## Koordinator

Rollen koordinator har ingen egen logik i appen; den får værktøjer og en systemprompt om arbejdsgangen. Koordinatorens ticket-kilde er den samme som din: Backlog (både dine og agenternes tickets), som den læser med `mira_list_tickets` (`all`) og `mira_get_ticket`. Den kan oprette tickets og tildele dem med det samme (`mira_create_ticket` med `assignTo`), tildele og fjerne tildelinger (`mira_assign_ticket`, `mira_unassign_ticket`; kun tickets i Backlog eller Afvist, kun til kørende agenter), se agenter og profiler (`mira_list_agents`, `mira_list_profiles`) og starte nye agenter fra en profil med `mira_spawn_agent`. En startet agent går gennem samme start-kode og samme lofter som i Workplace (5 arbejdspladser og 3 stabspladser; en afvisning kommer som den danske lofttekst). Koordinatoren godkender aldrig tickets; det gør reviewere eller du.

## Agentens værktøjer

Fra trin 4 har hver agent en lille lokal MCP-server, `mira-mcp.exe`, som Claude Code starter ved siden af sessionen. Den taler kun med appen over den samme named pipe som hooks (ingen porte, ingen netværk). Fra trin 5 får agenten højst 15 værktøjer (hos Claude hedder de `mcp__mira-bots__<navn>`), afhængigt af rollerne:

| Værktøj | Roller | Hvad det gør |
|---|---|---|
| `mira_submit_for_review` | alle | afleverer agentens igangværende ticket med en opsummering (1-2000 tegn) og evt. en rapport; ticketen går til Review, eller til Done ved "Spring review over" |
| `mira_create_ticket` | alle; `assignTo` kun koordinator | opretter en ticket i Backlog (titel, valgfri beskrivelse, evt. "spring review over"); højst 20 pr. time pr. agent; koordinatoren kan tildele den med det samme |
| `mira_list_tickets` | alle | lister tickets uden beskrivelse og historik, opsummeringen afkortet til 160 tegn (`mine`, `backlog` eller `all`) |
| `mira_get_ticket` | alle | henter én ticket med beskrivelse, fuld opsummering, historik og rapportliste (fuldt id eller kort-id) |
| `mira_update_status` | alle | sætter en kort statuslinje (højst 120 tegn), der vises ved agenten og noteres på ticketen; ændrer ingen tilstand |
| `mira_get_workspace_rules` | alle | viser reglerne: lofter, maks review-runder, grænser for tickets og rapporter |
| `mira_add_report` | alle | lægger en rapport på en ticket |
| `mira_get_report` | alle | henter teksten i en rapport |
| `mira_approve_ticket` | reviewer | godkender en ticket, man er reviewer på (aldrig sin egen aflevering) |
| `mira_reject_ticket` | reviewer | afviser med en påkrævet note; runden tælles op |
| `mira_assign_ticket` | koordinator | sætter en ticket fra Backlog eller Afvist bagest i en kørende agents kø |
| `mira_unassign_ticket` | koordinator | tager en ticket i kø ud af køen og tilbage til Backlog |
| `mira_spawn_agent` | koordinator | starter en agent fra en profil (evt. med en første ticket); samme lofter som i appen |
| `mira_list_agents` | koordinator | lister agenterne med profil, roller, plads, status, ticket i gang, kølængde og åbne reviews |
| `mira_list_profiles` | koordinator | lister profilerne til brug for `mira_spawn_agent` |

Rollerne coder, researcher, planner og debugger har kun de otte fællesværktøjer; en agent uden roller også. En specialist med flere roller får foreningen af rollernes værktøjer. Rollematrixen findes ét sted (`ROLE_TOOLS` i `crates/mira-mcp/src/tools.rs`) og håndhæves tre steder: `mira-mcp.exe` viser kun de tilladte værktøjer (rollerne følger med i miljøvariablen `MIRA_AGENT_ROLES`), profilens `permissions.deny` fjerner resten fra modellens kontekst, og appen afviser selv et kald fra en rolle, der ikke må ("Din rolle tillader ikke dette værktøj"), uanset hvad de to andre viste.

**Sådan leveres værktøjerne.** Appen skriver filer i `%APPDATA%\dk.mira.bots\`: `mcp.json` (peger på `mira-mcp.exe`, fælles for alle agenter) og pr. profil `profiles\<id>\settings.json` (hooks, tilladelser, model, effort og statuslinje) og `profiles\<id>\system-prompt.md` (et kort tillæg til agentens systemprompt). Hver agent startes som `claude --settings <profilens settings.json> --mcp-config <mcp.json> --append-system-prompt-file <profilens system-prompt.md> [--model <model>] [--effort <effort>] --session-id <id>`. `mira-mcp.exe` bundles ved siden af `mira-hook.exe` (under `resources`), og `MIRA_BOTS_PIPE`, `MIRA_AGENT_ID` og `MIRA_AGENT_ROLES` følger med til serveren, så et værktøjskald altid havner hos den rigtige agent. Findes `mira-mcp.exe` ikke, skrives `mcp.json` og systemprompten ikke, agenterne kører uden værktøjer (men stadig med profilens settings), og Diagnostik viser "Agentværktøjer utilgængelige: mira-mcp mangler".

**Forhåndsgodkendelse.** Appens egne værktøjer står som `permissions.allow` (`mcp__mira-bots__*`) i profilens settings-fil, ikke i din. Dine egne MCP-servere og dine egne tilladelsesregler virker uændret ved siden af, og `~/.claude` og projekternes `.mcp.json` røres aldrig. Som ekstra sikkerhedsnet tillader appen selv et værktøjskald til `mira-bots`-serveren i tilladelsesflowet, hvis reglen af en eller anden grund ikke virker.

**Systemprompten.** Tillægget (på dansk) består af en fælles del, en rolletekst pr. rolle, profilens prompt-tillæg og et resumé af workspace-reglerne. Den fælles del siger, at opgaverne ligger som filer i `.mira-bots/tickets/`, at agenten skal kalde `mira_submit_for_review` med en opsummering, når en ticket er færdig (uden kaldet står den som "ikke afleveret"), at større arbejde bør dokumenteres med en rapport (`mira_add_report`), at opfølgende arbejde skal oprettes som ny ticket med `mira_create_ticket`, og at agenten ikke selv må røre mappen `.mira-bots/`. Ticket-filen indeholder de samme regler. Er der ingen aflevering, når turen slutter, se "Når agenten er færdig" ovenfor.

## Første gang i en mappe

Første gang `claude` startes i en mappe, spørger Claude Code, om du har tillid til filerne i den. Spørgsmålet vises i agentens terminal i Workplace, og det er dér, du besvarer det.

Indtil du har svaret, afvikler Claude Code ingen hooks fra nogen settings-fil (jf. dokumentationen), så agenten ser ud til at stå på "starter". Efter ca. 15 sekunder uden hook-events viser appen derfor en tekst om at vente på svar i terminalen. Først efter accept begynder hooks at virke, og statusvisningen følger med.

Tip: kør `claude` én gang i en almindelig terminal i `%USERPROFILE%\mira-bots\agents` og accepter. Trusten for en overliggende mappe skal ifølge dokumentationen dække nye undermapper, så du slipper for spørgsmålet i hver ny agentmappe. Det er ikke afprøvet. Appen rører aldrig din `~/.claude` eller `~/.claude.json` og forsøger ikke at omgå spørgsmålet.

## Sådan virker hooks

Appen skriver en egen settings-fil pr. profil, `%APPDATA%\dk.mira.bots\profiles\<id>\settings.json`, og starter hver agent som `claude --settings <profilens fil>` (plus `--mcp-config`, `--append-system-prompt-file` og evt. `--model`/`--effort`, se Agentens værktøjer). Filen indeholder:

- `hooks`: 12 hook-events (inkl. `PostModelSwitch`), der peger på `mira-hook.exe`, som sender hændelser til appen over en named pipe og (kun ved tilladelsesanmodninger) venter på dit svar. Er appen ikke startet, gør `mira-hook.exe` ingenting, og Claude Code påvirkes ikke.
- `permissions.allow`: kun appens egne værktøjer (`mcp__mira-bots__*`) og profilens `extraAllow` (fx git-læseregler hos reviewer).
- `permissions.deny`: de af appens værktøjer, profilens roller ikke må bruge, og profilens `extraDeny` (fx `git commit` og `git push`, også som `git -C <mappe> commit`/`push`, hos reviewer). Nøglen udelades, når intet skal afvises, som hos en specialist med alle roller.
- `model` og `effortLevel`, når profilen har dem (effort `max` gives kun som flag).
- `statusLine`: `mira-hook.exe` uden argumenter. Claude Code kalder den ved statusopdateringer med model og effort på stdin, og appen bruger det til at vise den aktuelle model og effort. Programmet skriver intet tilbage, så statuslinjen i selve terminalen er tom. Appen opdaterer kun visningen, når værdierne ændrer sig.

Før trin 4 hed filen `hooks.json`; den gamle fil fjernes ved første start. Den fælles `settings.json` i `%APPDATA%\dk.mira.bots\` skrives stadig (til diagnostik), men bruges ikke længere til at starte agenter.

Appen sætter `MIRA_BOTS_PIPE`, `MIRA_AGENT_ID` og `MIRA_AGENT_ROLES` i agentens miljø. `MIRA_AGENT_ID` bruges til at koble hook-events til den rigtige agent, også efter `/clear`.

Din egen `~/.claude/settings.json` røres aldrig. Dine eksisterende globale hooks kører stadig ved siden af.

## Diagnostik og log

Fanen Diagnostik i Workplace viser blandt andet Claude Code-sti og -version, om hooks med `args` understøttes, `settings.json`, `mcp.json` og systemprompt-filens stier, om `mira-mcp.exe` er fundet, profilmappens sti og antal indlæste profiler (med en advarsel, hvis en profilfil var ødelagt), pipen, antal modtagne hook-events og det sidste event samt antal værktøjskald, antal værktøjskald med fejl og det sidste værktøjskald (kun værktøjets navn, agenten og om det lykkedes, aldrig indholdet). Diagnostik advarer, når `mira-mcp.exe` eller `settings.json` mangler. Knappen Kopiér lægger det hele på udklipsholderen som tekst til en fejlrapport, og Åbn logmappe åbner mappen med loggen.

Loggen ligger i `%LOCALAPPDATA%\dk.mira.bots\logs\mira-bots.log`. Den roteres ved hver start, og de seneste tre gamle filer gemmes. Sæt `MIRA_LOG=debug` (eller `trace`, `info`, `warn`, `error`) for mere detaljeret log; standard er `info`, og debug giver bl.a. én linje pr. hook-event.

## Compliance og ansvar

- Du er selv ansvarlig for din Claude-plan og for at overholde Anthropics vilkår.
- Appen laver intet login og rører ikke dine credentials. Den læser ikke `~/.claude` og ændrer ikke `claude`-programmet.
- Appen giver Claude Code kun sine egne filer pr. profil: `--settings` (hooks, tilladelser for appens egne værktøjer, profilens model, effort og statuslinje), `--mcp-config` (appens egen MCP-server) og `--append-system-prompt-file` (profilens systemprompt), plus `--model`, `--effort` og `--resume` som almindelige kommandolinje-flag. Den taster aldrig `/model` eller `/effort` i terminalen, bruger ikke `--print`/`-p`, og slår ikke tilladelsestjek fra.
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

Trin 4 tilføjede crate'en `crates/mira-mcp` (binæren `mira-mcp`): en håndskrevet JSON-RPC 2.0-server over stdin/stdout, der kun afhænger af `serde_json` (som `mira-hook`). Den officielle Rust-SDK (`rmcp`) blev fravalgt, fordi den trækker `tokio` og omkring 63 crates med, til fem simple værktøjer (fra trin 5 femten). Den transport, der er fælles med `mira-hook`, er kopieret i stedet for at flyttes til en delt crate, så den Windows-verificerede hook-exe ikke røres; en test holder konstanterne ens. Ingen nye npm-afhængigheder. `cargo test -p mira-mcp` kører serverens egne tests (inkl. en test, der starter den rigtige binær). `npm run build:hook` bygger både `mira-hook` og `mira-mcp`, og `npm run copy:hook` lægger begge i `src-tauri/resources/`.

Trin 5 tilføjede ingen nye npm-pakker og ingen nye eksterne Rust-crates. `src-tauri` bruger nu `mira-mcp` som almindelig afhængighed (rollematrixen `ROLE_TOOLS` bor kun dér), `mira-hook` fik en afgrænset ændring til `statusLine`, og figurerne for planlægger, debugger og specialist, markdown-visningen af rapporter og model-valideringen er håndskrevet. Frontendens node-tests (`npm run test:node`) kører fem scripts: `test-terminal-input.mjs`, `test-bot-core.mjs` (figur-porten mod referencen), `test-markdown.mjs`, `test-models.mjs` og `test-tickets.mjs`.

Appens filer i `%APPDATA%\dk.mira.bots\`: `mcp.json`, `settings.json` og `system-prompt.md` (genskrives ved hver start), `profiles\<id>\settings.json` og `profiles\<id>\system-prompt.md` (genskrives før hver agentstart og ved gem af en profil), `tickets.json` og `tickets\<ticket-id>\reports\` (rapporter). Profilerne selv ligger i `%USERPROFILE%\mira-bots\agents\.mira-bots\profiles\<id>.json` (flyttes senere til projektroden).

Verifikation fra repo-roden (kan køres på Linux; Windows-koden tjekkes ved cross-check):

```
cargo check --workspace --target x86_64-pc-windows-msvc
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
npm run build
npm run test:node
```

Miljøvariabler: `MIRA_CLAUDE_PATH` (sti til `claude`), `MIRA_HOOK_EXE` (sti til `mira-hook`), `MIRA_MCP_EXE` (sti til `mira-mcp`), `MIRA_HOOK_DEBUG=1` (hook-logning på stderr), `MIRA_MCP_DEBUG=1` (logning fra `mira-mcp` på stderr), `MIRA_MCP_TIMEOUT_MS` (kun til test og fejlsøgning: hvor længe `mira-mcp` venter på appen, standard 10000), `MIRA_LOG` (logniveau for appen, standard `info`). `MIRA_BOTS_PIPE`, `MIRA_AGENT_ID` og `MIRA_AGENT_ROLES` sættes af appen selv.

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
10. `mira-hook.exe` og `mira-mcp.exe` findes under `resources/` efter NSIS-installation.
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
33. Med `AUTO_REVIEW_ON_STOP = true` flytter Stop-hooket ticketen til Review (eller Done ved "Spring review over"); Esc midt i turen giver intet Stop, og en API-fejl giver "Turn fejlede" med fungerende "Send igen".
34. `tickets.json` skrives atomisk i `%APPDATA%\dk.mira.bots\` (omdøbning over en eksisterende fil virker, ingen `.tmp` efterlades), og en beskadiget fil omdøbes til `.broken-<tidspunkt>` med advarsel i Diagnostik.
35. Efter genstart står tickets, der var i kø eller i gang, i Backlog med noten "app genstartet", og Review/Done er urørte.
36. Chippen "n i review" i den ikke-fokuserbare island åbner Workplace på fanen Tickets, både når vinduet oprettes og når det allerede er åbent.
37. Stop/Fjern af en agent med kø: alle dens tickets står i Backlog med note, en igangværende aflevering skriver ikke mere i terminalen, og `queueLength` er 0.
38. xterms automatiske svar (Device Attributes, cursor- og fokusrapporter) tæller ikke som brugerinput under ConPTY og udsætter ikke ticket-levering, mens tastetryk, piletaster og indsat tekst (også bracketed paste via Shift+Insert eller højreklik) gør.
39. Claude Code (≥ 2.1.274; researchet mod 2.1.286) starter `mira-mcp.exe` fra `--mcp-config` med en sti med mellemrum og `/` uden `cmd /c`; nøglerne `alwaysLoad` og `timeout` i `mcp.json` accepteres af den installerede version; `/mcp` viser `mira-bots` som connected, og de fem værktøjer er synlige uden `ToolSearch`.
40. `MIRA_BOTS_PIPE` og `MIRA_AGENT_ID` når `mira-mcp.exe` (arv og/eller `${VAR}`-udvidelse); et værktøjskald rammer den rigtige agent, også efter `/clear`.
41. Ingen tilladelsesprompt for `mcp__mira-bots__*` via appens `settings.json` (`permissions.allow`); dine egne MCP-servere og allow-regler virker stadig.
42. `mira_submit_for_review` flytter ticketen til Review med opsummeringen synlig på kortet; Stop uden kald giver "Ikke afleveret"; "Bed om aflevering" taster linjen og Enter, og agenten svarer med et kald; "Send til review" virker som fallback.
43. `--append-system-prompt-file` med æøå virker sammen med `--settings`, `--mcp-config`, `--session-id` og den positionelle prompt, og tillægget overlever `/clear`.
44. Nedlukning: ingen efterladte `mira-mcp.exe` efter `/exit`, Stop af agent eller Afslut; et crash af serveren dræber ikke sessionen, og Diagnostik viser fejlen.
45. Named pipe fra `mira-mcp.exe` (en forbindelse pr. kald): genforsøg ved optaget pipe, handlen lukkes efter svaret, og en frosset app giver en fejl efter 10 sekunder uden at Claude Code hænger.
46. Om din Claude Code-version sender `server/discover` før `initialize`; begge forløb skal ende med at `mira-bots` er connected.
47. `mira_update_status` vises som statuslinje i islanden; et værktøjskald giver "Tænker" med en dansk betegnelse, og en fejl fra et værktøj (fx "Du har ingen ticket i gang") viser ikke rødt.
48. Første start efter opgradering: `hooks.json` er fjernet, `settings.json`, `mcp.json` og `system-prompt.md` findes i `%APPDATA%\dk.mira.bots\`, og Diagnostik viser stierne.
49. Første start efter opgradering: `%USERPROFILE%\mira-bots\agents\.mira-bots\profiles\` indeholder de syv indbyggede profiler, og fanen Agenter viser dem med figurer; Nulstil og Slet virker, og listen opdateres via `profiles-changed`.
50. Start fra en profil: `%APPDATA%\dk.mira.bots\profiles\<id>\settings.json` og `system-prompt.md` findes; `claude` starter med `--settings` på profilens fil og med `--model`/`--effort`, når de er sat; headeren i terminalen viser den valgte model og effort.
51. `permissions.deny` virker: en koder ser ikke `mira_approve_ticket` i `/mcp`-værktøjslisten, en reviewer ikke `mira_assign_ticket`; `MIRA_AGENT_ROLES` når `mira-mcp.exe` (`${VAR}`-udvidelse), så `tools/list` er filtreret.
52. `statusLine` med den citerede sti til `mira-hook.exe` starter uden fejl i terminalen (tom statuslinje), og terminalpanelet viser "Model: claude-… (observeret)" kort efter første svar; `/effort xhigh` skrevet af dig i terminalen ændrer visningen via statuslinjen. Det måles også, om en proces pr. statusopdatering giver mærkbar belastning.
53. "Skift model": agenten genstarter med `--resume <session-id> --model <alias>`, samtalen er bevaret (tidligere beskeder er synlige), `SessionStart` med `source: resume` giver klar, `PostModelSwitch` bekræfter modellen, og køen fortsætter ved næste klar. Der efterlades ingen processer fra den gamle session (ConPTY). "Skift model" på en nystartet agent uden beskeder giver en ny session (`--session-id`) i stedet for `--resume` og ender klar.
54. Reviewer-flow: en koder afleverer, review-filen ligger i reviewerens `.mira-bots\reviews\`, linjen tastes, når revieweren er klar, `git -C "<koderens mappe>" diff .` kører uden tilladelsesspørgsmål, og både `git commit` og `git -C "<koderens mappe>" commit` (samt `push`) afvises; `mira_approve_ticket` sætter Done, `mira_reject_ticket` sætter ticketen forrest hos koderen med runde 1 af 3.
55. Tre afvisninger giver badget "Eskaleret til dig" og ingen ny review-linje, og du kan godkende, afvise eller vælge reviewer manuelt.
56. Reviewer stoppes midt i et review: ticketen står i Review uden reviewer og gives til en anden reviewer, hvis en findes.
57. Koordinator: `mira_create_ticket` med `assignTo`, `mira_assign_ticket`, `mira_spawn_agent` (lofterne overholdes: den 6. arbejdsagent afvises med den danske lofttekst) og `mira_list_agents` virker; en ticket trukket på koordinatoren vises som "Koordineringsopgave".
58. Rapporter: `mira_add_report` skriver `%APPDATA%\dk.mira.bots\tickets\<id>\reports\01-<titel>.md`; "Rapporter (1)" vises på kortet, fold-ud viser markdown med æøå, "Åbn mappe" åbner Stifinder, og sletning af ticketen fjerner mappen.
59. Specialist-figuren (SVG som data-URL med animationer) tegnes i WebView2 på pladsen, i chippen og i editorens forhåndsvisning, i både lyst og mørkt tema; planlægger- og debugger-figurerne vises.
60. En profil med et fuldt model-id (`claude-sonnet-5-5`) starter; et ugyldigt id afvises i editoren, før profilen gemmes; en model, kontoen ikke har adgang til, giver fejl ved første forespørgsel i terminalen (ikke i appen).
61. `mira_submit_for_review` med `report` i ét kald lægger ticketen i Review med rapporten synlig; uden `mira-mcp.exe` starter agenter stadig med profilens settings (uden værktøjer).
62. Settings med 12 hook-events (`PostModelSwitch`) giver ingen fejl i ældre Claude Code-versioner (fra 2.1.139), der ikke kender eventet; det er uverificeret, om ukendte hook-navne ignoreres stille.
63. Kontor-look i WebView2: skriveborde, laptops og inventar tegnes med gradienter og blur-filter fra det delte `<defs>`-svg i både lyst og mørkt tema, skifter live ved Windows-temaskift og viser aldrig "hvide" borde (tegn på, at defs-svg'et ikke er tilgængeligt).
64. Splitteren: træk med mus og touchpad (pointer capture holder også uden for grebet og ved kanten af vinduet), piletaster ±16 px, Home/End, dobbeltklik nulstiller til 360 px (ved 1100×720 giver det mindst 9 synlige terminalrækker, også med en ticket i gang og én i kø); splitteren helt oppe/nede skjuler ikke arbejdsrækkens navneskilte (også med "Lidt mere"), og PTY'en får aldrig under 6 rækker. Er vinduet for lavt til både kontor og terminal (fx vinduets mindstestørrelse 820×540, eller 1100×720 med lang kø, reviews og opstartshint), har terminalen forrang: den bevarer mindst 6 rækker med inputlinjen synlig, kontoret krymper ned til én pladsrække og kan rulles (begge rækker kan rulles frem), og splitteren skjules, når der ikke er noget at flytte; xterm tilpasser sig under trækket uden duplikerede eller forskudte linjer under ConPTY (kendt VS Code-problem microsoft/vscode#247385). Noter samtidig, om `windowsPty` uden `buildNumber` giver manglende reflow på Windows 11.
65. Minimér → gendan: terminalen får korrekt antal rækker og kolonner uden ekstra klik, PTY'en fik aldrig `rows=1`, og det tidligere output er tilbage (afspilning fra ring-bufferen). Tastaturfokus går ved Minimér til "Gendan" og ved Gendan til terminalen (eller til ▁, når agenten er stoppet).
66. Maksimér: strimlen viser alle 8 pladser (3 stab + 5 arbejde) med små figurer og navne (navnene kan læses ved 1100 px bredde; lange navne afkortes med det fulde navn i værktøjstippet), man kan skifte agent fra strimlen, og terminalen tilpasser sig den nye højde.
67. Efter lukning og genstart af den installerede app huskes splitter-højde, terminaltilstand (normal/minimeret/maksimeret) og kontor-detaljer (localStorage i WebView2's user data folder under `%LOCALAPPDATA%\dk.mira.bots`); begge vinduer deler lageret. Åbnes Workplace fra øen, mens terminalen er minimeret, vises den valgte agents terminal i den gemte højde.
68. `prefers-reduced-motion`: med "Animationseffekter" slået fra i Windows (Indstillinger → Tilgængelighed → Visuelle effekter) blinker skærmlinjerne på laptops ikke; uret på væggen går stadig.
69. Tre stabspladser: en tredje stabsagent kan startes fra en tom stabsplads og via koordinatorens `mira_spawn_agent`; den fjerde afvises med "Loft på 3 stabspladser nået"; headeren viser "n/3 stab".
70. Vægur viser Windows' lokale tid og opdateres; med "Lidt mere", 8 figurer og én kørende terminal er der ingen mærkbar UI-belastning.

## Licens og inspiration

Koden er MIT-licenseret (se `LICENSE`). Alle ikoner og andre assets er lavet til dette projekt. Idéen er inspireret af [Coucou](https://github.com/Louis-CFM/coucou); ingen af dets assets eller kode er genbrugt.
