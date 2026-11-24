# Agent Token Optimization: specifica

Stato: costruito (2026-11-21, notte), su `main`; vedi §11 per le decisioni prese senza l'utente. Nasce dall'analisi dei costi di cache del 2026-11-21 (`docs/specs/topic-routing.md` §2) e dalla richiesta dell'utente di una sezione **Agent Token Optimization** nelle impostazioni del progetto, con un pulsante che studia le sessioni passate più grandi e taglia ciò che non serve.

## 1. Cosa si costruisce

Una scheda nuova nelle impostazioni del progetto (Project settings → **Tokens**, intestazione **AGENT TOKEN OPTIMIZATION**) con:

1. **Analyze past sessions**: Connecto legge i transcript Claude del progetto degli ultimi 30 giorni e mostra:
   - dove vanno i token;
   - cosa entra in ogni sessione senza essere mai usato (plugin, skill, server MCP, file di memoria);
   - lo studio della sessione più grande, fatto da una sessione utility.
2. **Apply to new terminals**: i tagli scelti valgono per i terminali Claude di questo progetto **avviati dopo**. Non si tocca mai una sessione in corso, perché cambiare strumenti o skill a metà sessione riscrive tutta la cache. Non si modifica nessun file dell'utente.
3. **Before / after misurati**: contesto di partenza mediano prima dei tagli e dopo, misurato sulle sessioni reali avviate in seguito.
4. **Compact earlier** (spento per default): passa `--autocompact <N>` ai terminali nuovi, con il risparmio stimato sui dati dell'utente.

## 2. Fatti misurati che vincolano il disegno (Claude Code 2.1.328, 2026-11-21)

- Il transcript di una sessione interattiva registra all'inizio, come righe `{"type":"attachment","attachment":{...}}`, i blocchi che entrano nel contesto:
  - `skill_listing` (`names`, `content` con una riga `- nome: descrizione` per skill, `skillCount`);
  - `deferred_tools_delta` (`addedNames`, con i tool MCP nella forma `mcp__<server>__<tool>`);
  - `agent_listing_delta` (`addedLines`);
  - `mcp_instructions_delta` (`addedNames`, `addedBlocks`);
  - `instructions` (`files[]` con `path`, `type`, `content`: CLAUDE.md, MEMORY.md…).

  Il formato è interno a Claude Code e non documentato: il parser lo tratta come facoltativo e mostra "not recorded" quando manca.
- Il contesto di una chiamata è `input_tokens + cache_read_input_tokens + cache_creation_input_tokens` nell'`usage` della riga `assistant`. Una stessa `message.id` può comparire su più righe: va contata una volta.
- Misurato con sessioni interattive di prova in un repository (contesto della prima chiamata; i valori assoluti dipendono dai plugin e dalle skill installati, qui contano le differenze):

  | Prova | Contesto | Effetto |
  |---|---|---|
  | Nessun cambiamento | base | tutte le skill e i tool differiti installati |
  | `--settings {"enabledPlugins":{"<plugin>@synced":false}}` | base − ~1,4k | le skill e i tool MCP del plugin in meno. **Funziona** |
  | `--settings {"skillOverrides":{"<skill-a>":"off","<skill-b>":"off"}}` | base − ~0,6k | due skill in meno. **Funziona** |
  | `enabledPlugins` con l'id del marketplace (`<plugin>@<marketplace>`) | base | nessun effetto: un plugin sincronizzato da claude.ai ha id `<nome>@synced` |
  | `env.ENABLE_CLAUDEAI_MCP_SERVERS=false`, `disableClaudeAiConnectors`, `permissions.deny Skill(x)` | ±5 | nessun effetto |
  | `disableBundledSkills: true` | base + ~3,4k | **peggio**: non si usa |
- `--autocompact <auto|tokens>` esiste (`claude --help`): finestra di auto-compattazione per un solo avvio, almeno 100k. Con modelli a 1M la compattazione di default arriva verso 967k: è per questo che le sessioni lunghe arrivano a 300-400k.
- ADR 0001/0025: niente `claude -p` nel prodotto. Le misure vengono dai transcript, il lavoro LLM passa da una sessione utility.

## 3. Analisi (deterministica, nessuna chiamata a modelli)

Modulo nuovo `crates/connecto-session/src/token_audit.rs`, solo funzioni pure più la lettura dei file. È testabile con transcript finti in `tests/fixtures`.

### 3.1 Quali transcript

- Le cartelle profilo Claude del progetto: quella di sistema (`~/.claude`) quando il progetto usa la configurazione di sistema, altrimenti tutte le generazioni `profiles/claude/g*`.
- Dentro ciascuna, `projects/<cartella>/` dove `<cartella>` è il percorso del progetto con ogni carattere non alfanumerico sostituito da `-`. Esempio: `/Users/x/workspace/connecto` → `-Users-x-workspace-connecto`.
- Solo i file `*.jsonl` di primo livello (niente sottocartelle di subagent) modificati negli ultimi 30 giorni. Si scartano le sessioni con meno di 3 chiamate.
- Si legge riga per riga, saltando quelle illeggibili. Limite di 200 MB letti in tutto: oltre, si prendono le sessioni più recenti e lo si dice.

### 3.2 Numeri per sessione e totali

Per sessione:
- chiamate, contesto della prima chiamata, picco, letture, scritture, uscita;
- **ricostruzioni fredde**: chiamate con scrittura > 20k e maggiore della lettura, dopo più di 60 minuti dalla precedente;
- compattazioni (righe con `isCompactSummary` o `subtype` di compattazione, se presenti).

Totali:
- quota di input letta dalla cache;
- contesto mediano per chiamata;
- **quota delle letture da chiamate sopra 200k**;
- contesto di partenza mediano;
- costo relativo con i rapporti 0,1× lettura, 2× scrittura a 1 ora, 1,25× scrittura a 5 minuti, 5× uscita. Si mostrano percentuali, non dollari.

### 3.3 Cosa entra senza essere usato

Dagli attachment iniziali della sessione **più recente** si ricava cosa entra oggi:
- skill: nome e caratteri della sua riga;
- server MCP: tool e caratteri;
- istruzioni MCP;
- agenti;
- file di memoria.

Dalle sessioni dei 30 giorni si ricava cosa è stato usato:
- `tool_use` con nome `Skill` (`input.skill`);
- messaggi utente `<command-name>/x</command-name>`;
- `tool_use` `mcp__<server>__*`;
- `Agent`/`Task` con `subagent_type`.

Proposte, ciascuna con i token stimati (caratteri / 4, dichiarato come stima) e le prove ("used 0 times in 23 sessions"):
- **Plugin** mai usato (nessuna sua skill `plugin:*` e nessun suo server MCP `plugin_<plugin>_*`): spegnerlo per questo progetto. L'id va risolto in **tutti** gli id esistenti per quel nome:
  - `<nome>@synced` se compare nei manifest sotto `<profilo>/plugins/synced/*/manifest.json`;
  - `<nome>@<marketplace>` da `<profilo>/plugins/installed_plugins.json` e dalle chiavi `enabledPlugins` dei settings utente, progetto e locali.

  Mettere a `false` un id che non esiste è innocuo.
- **Skill** mai usata, non di plugin: `skillOverrides: "off"`. Non si propongono mai le skill dei pacchetti di Connecto (`dev-team`, `daily-team`, `brainstorm-team` e quelle in `crates/connecto-packages/builtin`): sono funzioni di Connecto.
- **Server MCP** non di plugin mai usato: **solo indicazione**. Il disattivare per progetto vive in `~/.claude.json` (`/mcp`), che Connecto non scrive. Si spiega come farlo.
- **File di memoria** sopra 2.000 caratteri: **solo indicazione**, con dimensione e percorso.

Un elemento usato anche una sola volta non viene proposto. Tutte le proposte partono deselezionate tranne plugin e skill mai usati in almeno 5 sessioni.

## 4. Studio della sessione più grande (sessione utility)

- La sessione più grande è quella con più letture dalla cache nei 30 giorni.
- Connecto ne costruisce un **riassunto compatto**, sotto i 12.000 caratteri:
  - per ogni messaggio dell'utente: i primi 200 caratteri, il contesto prima e dopo il turno e i tool usati;
  - i 15 output di tool più grandi (nome, dimensione, primi 120 caratteri del comando o del percorso);
  - pause oltre 60 minuti, compattazioni, picco.

  Il testo passa da `connecto_redact` prima di uscire.
- Una sessione utility (scopo nuovo `UtilityPurpose::TokenStudy`, accesso `Minimal`, senza tool) riceve il riassunto e risponde **solo JSON**: `{"findings":[{"title":"…","evidence":"…","advice":"…"}]}`, al massimo 6, tutti su abitudini (output grandi da delegare, argomenti da separare, sessioni da chiudere, pause lunghe). Il modello è quello scelto per i titoli dei terminali nel progetto (o il default delle sessioni utility).
- Se la sessione fallisce o il JSON non è valido, l'analisi deterministica resta e si mostra "study unavailable" con il motivo. Lo studio è testo da leggere: non cambia niente da solo.

## 5. Applicare

### 5.1 Impostazione del progetto

Chiave `token_optimization` in `<data>/projects/<id>/settings.json`, scritta col modello di `providers` (`rewrite` + `merge`):

```json
{
  "token_optimization": {
    "plugins_off": ["product-management@synced"],
    "skills_off": ["dataviz"],
    "autocompact": null,
    "applied_at": "2026-11-21T23:50:00Z",
    "before": { "start_context_median": 42614, "sessions": 12 }
  }
}
```

### 5.2 Al lancio

Solo terminali Claude normali, mai le sessioni utility. In `Claude::launch_as` / `settings()` il file `--settings` che Connecto già scrive riceve:
- `"enabledPlugins": {"<id>": false, …}`;
- `"skillOverrides": {"<nome>": "off", …}`;
- con `autocompact` impostato, gli argomenti `--autocompact <N>`.

`--settings` è la fonte "flag", sopra utente, progetto e locale, quindi vince. `LaunchCtx` riceve un campo nuovo con queste liste, riempito in `launch.rs` da `token_optimization`.

### 5.3 Regole

- **Never mid-session**: il pulsante dice "applies to Claude terminals started from now on; running terminals keep their setup".
- **Undo all** svuota le liste. Vale anch'esso dai terminali nuovi.
- `--resume` di una conversazione esistente riceve le impostazioni attuali. La prima chiamata dopo la ripresa riscrive comunque la cache se è fredda; se è calda, il prefisso cambia e la cache si riscrive una volta. È accettato, e la nota della UI lo dice.

## 6. Before / after

- **Before**: al momento di Apply si salva il contesto di partenza mediano delle ultime 10 sessioni (`before`).
- **After**: mediana del contesto di partenza delle sessioni con il primo timestamp dopo `applied_at`. Con 0 sessioni si mostra "start a new terminal to measure". Con 1-2 sessioni si mostra il numero con "(n sessions)".
- Si mostra anche, dagli attachment della prima sessione dopo, la conferma che skill e tool tagliati non compaiono più ("confirmed in the transcript"), oppure un avviso se compaiono ancora.

## 7. Compact earlier

- Scelte: **Auto (Claude's default)**, 150k, 200k, 300k. Default Auto, cioè nessun argomento.
- Accanto, la stima: "Calls above 200k used X% of your cache reads in the last 30 days". Con la scelta attiva: "Estimated saving: ~Y% of reads; compaction summarizes earlier turns, so agents may lose detail from long sessions."
- Stima: per ogni chiamata sopra la soglia T, risparmio ≈ (contesto − (contesto − T + 50k)). Si sottrae una compattazione stimata ogni volta che una sessione supera T: lettura di T più 10k di uscita.

## 8. UI

`crates/connecto-ui/src/project_settings.rs`: nuovo `ProjectTab::Tokens`, etichetta **Tokens**; il pannello vive in un modulo nuovo `crates/connecto-ui/src/token_opt.rs`. Il modello è quello di `memory`: azioni in `Out`, controllo dietro un trait (`TokenControl`) con `view()` non bloccante (snapshot in `ArcSwap`), lavoro su un thread, ridisegno ogni 500 ms mentre è occupato.

Sezioni del pannello:
1. Intestazione **AGENT TOKEN OPTIMIZATION** e una frase su cosa fa.
2. **Analyze past sessions**, con avanzamento ("reading 23 sessions…", "studying the biggest session…").
3. **Where your tokens go**: i numeri di §3.2 in poche righe.
4. **Loaded but never used**: lista con caselle, token stimati e prove. Poi **Apply to new terminals** e **Undo all**.
5. **Before / after** (§6).
6. **Compact earlier** (§7).
7. **Biggest session study** (§4).

## 9. Audit

`append_nowait` con `source: "token_optimization"`, azioni `analyze`, `apply`, `undo`, `study`. Nel record vanno il progetto e il riepilogo dei conteggi, mai testo dei transcript.

## 10. Test

- Parser: transcript finti con attachment, usage duplicati per `message.id`, righe rotte, sessioni sotto 3 chiamate, file vecchi.
- Proposte: plugin usato una volta → non proposto; skill di Connecto → mai proposta; risoluzione degli id `@synced` e del marketplace.
- Lancio: `enabledPlugins` e `skillOverrides` nel file `--settings` e `--autocompact` solo quando impostati; mai nelle sessioni utility; con le liste vuote il file è identico a oggi.
- Before/after con `applied_at`.
- Impostazione: lettura e scrittura, chiavi sconosciute conservate.
- Studio: JSON valido, non valido, fallimento della sessione → l'analisi resta.
- UI: test del pannello come quelli di `memory` dove esistono.

## 11. Decisioni prese senza l'utente (notte del 2026-11-21)

- Niente modifica ai file dell'utente: tutto passa dal `--settings` per terminale di Connecto, ed è reversibile.
- Server MCP non di plugin e file di memoria: solo indicazioni.
- Compact earlier spento per default: risparmia di più, ma toglie dettagli alle sessioni lunghe, e la scelta spetta all'utente.
- Spostare l'id del terminale in fondo al testo di avvio (suggerimento 5 dell'analisi) **non si fa**: vale circa lo 0,3-0,5% del costo e cambierebbe un testo verificato dai test di specifica.
- **Apply aggiunge, non sostituisce**: i tagli scelti si uniscono a quelli già attivi (che non compaiono più tra le proposte, perché le sessioni nuove non li caricano). **Undo all** svuota entrambe le liste e cancella `applied_at` e `before`, ma lascia la scelta di Compact earlier, che ha i suoi pulsanti.
- Una scrittura in cache senza la divisione `ephemeral_1h` / `ephemeral_5m` conta come scrittura a 1 ora (2×), cioè il caso delle sessioni interattive.
- Un plugin di cui non si trova nessun id (per esempio le skill `anthropic-skills:*`, che non hanno né manifest sincronizzato né voce in `installed_plugins.json` o in `enabledPlugins`) **non si propone**: non c'è modo sicuro di spegnerlo.
- Gli agenti caricati si contano in "cosa entra" (numero e token stimati) ma non si propongono: §3.3 non dà un modo per spegnerli.
- La "sessione più recente" da cui si legge cosa entra è la più recente con almeno 3 chiamate che registra gli attachment. Le sessioni utility di Connecto stanno nella stessa cartella, ma sono quasi sempre sotto le 3 chiamate e quindi escluse; non si distinguono in altro modo.
- Un uso dentro una sessione con meno di 3 chiamate **conta comunque come uso**: le sessioni corte escono solo dai numeri, non dalle prove d'uso.
- Le skill dei pacchetti di Connecto sono i tre nomi `dev-team`, `daily-team`, `brainstorm-team`; un test verifica che coincidano con `crates/connecto-packages/builtin/claude/.claude/skills`.
- Le skill incluse in Claude Code (`init`, `code-review`, `loop`…) si propongono come ogni altra skill non di plugin, come dice §3.3, e partono selezionate dopo 5 sessioni senza uso. L'utente le vede una per una prima di Apply.
- Compattazioni di una sessione: il massimo tra le righe `system` con `subtype: "compact_boundary"` e le righe con `isCompactSummary`, così una compattazione registrata in entrambi i modi conta una volta.
- Stima di Compact earlier (§7): ogni sessione si ripercorre chiamata per chiamata; quando il contesto, tolto quanto già compattato, supera T, si conta una compattazione che lascia 50k, e ogni chiamata successiva legge `min(quanto tolto, sua lettura)` in meno. Una compattazione reale registrata nel transcript azzera quanto tolto: da lì la sessione vera è già piccola. Ogni compattazione costa una lettura di T più 10k di uscita, convertiti in letture col rapporto 5 / 0,1 = 50. Il risparmio netto, mai sotto zero, è in percentuale delle letture.
- Lo studio (`UtilityPurpose::TokenStudy`) è **strict** come la memoria: fallisce se non si prova che il prompt è arrivato intero. Il modello è quello delle impostazioni utility dell'app (quelle dei titoli, che sono globali e non per progetto), con un limite di 5 minuti; Stop Everything lo annulla.
- Lo stesso transcript in due generazioni del profilo si legge una volta sola (la copia più recente). Il limite di 200 MB vale sempre: se il file più recente da solo lo supera, se ne leggono solo i primi 200 MB e la UI lo dice. Una riga oltre 32 MB si salta senza tenerla in memoria.
- I risultati dell'analisi stanno solo in memoria: alla riapertura di Connecto si rifà Analyze (pochi secondi: circa un centinaio di transcript in un secondo su un Mac recente). Restano salvate solo le impostazioni (`token_optimization`).
- Audit: `actor: "connecto"`, `source: "token_optimization"`, azioni `token_optimization.analyze|apply|undo|study`, motivo solo con conteggi. Il cambio di Compact earlier si registra come `apply` con motivo `autocompact <N|auto>`, senza un'azione nuova.
- Un plugin conta come **usato** se in una qualsiasi sessione dei 30 giorni si usa una sua skill o un suo agente (`<plugin>:x`) o un suo server MCP (`plugin_<plugin>_*`), anche se oggi quel server non è più caricato.
- La conferma del dopo (§6): ogni inventario registrato nella prima sessione dopo i tagli (`skill_listing`, tool differiti) si controlla; qualunque cosa tagliata vi compaia dà l'avviso. Senza riscontri, "confirmed" solo se c'erano tutti gli inventari che potevano mostrarla (la `skill_listing` per skill e plugin, i tool differiti per i plugin); altrimenti "not recorded". Le righe dei subagenti (`isSidechain`) non contano come compattazioni della sessione.
- Apply con niente di nuovo (tutto già tagliato) non cambia nulla: `applied_at` e `before` restano quelli del primo Apply, e il pulsante è spento.
- Gli estratti dello studio (prompt, comando o percorso) passano **interi**, con gli a capo, da `connecto_redact`; solo dopo si mettono su una riga e si tagliano a 200 / 120. Così un segreto a cavallo del taglio, su più righe (un blocco PEM) o noto al vault sparisce intero. Nell'audit lo studio fallito ha solo una categoria fissa ("the utility session failed", "the reply was not the JSON asked for"), mai il testo dell'errore.
