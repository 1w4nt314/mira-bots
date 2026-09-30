# mira-bots

Et lille statusvindue ("island") øverst på skærmen, der viser dine Claude Code-agenter og lader dig svare Tillad/Afvis, når en agent beder om lov til at bruge et værktøj. Appen kører Claude Code CLI i baggrunden med dit eget abonnement.

## Status: trin 1 — ikke brugbart endnu

Det her er første trin. Målet er at bevise kæden fra ende til anden, ikke at være et færdigt værktøj.

Det virker (når det er testet på Windows, se nedenfor):

- Island-vindue, top-centreret på hovedskærmen, der folder ud ved hover.
- Start af agenter: vælg en mappe, så kører `claude` i den.
- Statusvisning pr. agent (starter, klar, tænker, læser, redigerer, kører, afventer tilladelse, afsluttet).
- Tillad / Afvis / "Altid for denne agent" på tilladelsesanmodninger.

Det findes ikke endnu: workplace, tickets, terminalvisning, MCP, systembakke, autostart.

## Krav

- Windows 10 version 1809 eller nyere.
- Nyeste Claude Code installeret og logget ind. `claude` skal kunne findes som `%USERPROFILE%\.local\bin\claude.exe` eller i `PATH`. Ellers sæt miljøvariablen `MIRA_CLAUDE_PATH` til den fulde sti.
- WebView2 (følger med Windows 11; installeren henter den på Windows 10).

## Sådan virker hooks

Appen skriver sin egen `hooks.json` i `%APPDATA%\dk.mira.bots\` og starter hver agent som `claude --settings <den fil>`. Hooks-filen peger på `mira-hook.exe`, som sender hændelser til appen over en named pipe og (kun ved tilladelsesanmodninger) venter på dit svar. Er appen ikke startet, gør `mira-hook.exe` ingenting, og Claude Code påvirkes ikke.

Din egen `~/.claude/settings.json` røres aldrig. Dine eksisterende globale hooks kører stadig ved siden af.

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

Kræver Rust (stable), Node 22 og, på Linux, Tauris systembiblioteker (`libwebkit2gtk-4.1-dev` m.fl.). `npm run tauri dev` kræver Windows.

```
npm ci
npm run tauri dev         # kun Windows
```

Verifikation fra repo-roden (kan køres på Linux; Windows-koden tjekkes ved cross-check):

```
cargo check --workspace --target x86_64-pc-windows-msvc
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
npm run build
```

Miljøvariabler: `MIRA_CLAUDE_PATH` (sti til `claude`), `MIRA_HOOK_EXE` (sti til `mira-hook`), `MIRA_HOOK_DEBUG=1` (hook-logning på stderr), `RUST_LOG` (applog). `MIRA_BOTS_PIPE` sættes af appen selv.

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

## Licens og inspiration

Koden er MIT-licenseret (se `LICENSE`). Alle ikoner og andre assets er lavet til dette projekt. Idéen er inspireret af [Coucou](https://github.com/Louis-CFM/coucou); ingen af dets assets eller kode er genbrugt.
