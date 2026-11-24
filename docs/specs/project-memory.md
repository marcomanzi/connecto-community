# Memoria di progetto (palazzo della memoria): specifica e stato dell'implementazione

Stato: implementata sul ramo `dev-team/project-memory`, review chiuse, non ancora committata né rilasciata. Task T0-T14 fatti, benchmark compreso (§11.3, misurato il 2026-11-14), T15 da fare (§10). Il documento è stato riallineato al codice il 2026-11-14 e di nuovo il 2026-11-15: dove una frase del piano e il codice divergevano, vale il codice e la frase è cambiata. Base del piano: `main` a `d4d7a3f`.

Questo documento era la consegna per `/dev-team` e ora descrive ciò che è stato costruito: cosa fa, cosa resta fuori, i fatti del repository su cui poggia, l'architettura, i contratti, i task, i test e i criteri di accettazione. Le decisioni prese con l'utente sono in §13; i limiti dichiarati in §16. La documentazione per l'utente è `docs/project-memory.md`.

Non verificato: la build Windows di `connecto-memory` e `connecto-app`. Osservati **una sola volta** con le CLI reali (benchmark, §11.3): il tick reale di 30 s dello scheduler e il percorso completo cattura → ciclo di sviluppo → lettura da un altro terminale → sogno → avviso di versione. Il resto in §16.

**Provare una build di sviluppo accanto all'app installata** (dalle indicazioni del README): `cargo build --release --workspace`, poi `CONNECTO_DATA_DIR="$HOME/connecto-dev-data" target/release/connecto-app`. Con una cartella dati propria la build di prova non tocca i dati dell'app installata. Per accendere e costruire la memoria di un progetto: icona a ingranaggio sulla riga del progetto nella barra laterale (**Project settings**), oppure clic destro sul progetto → **Project settings…**; sezione **PROJECT MEMORY** → spunta **Keep a memory for this project** → **Build the palace from the current project (recommended)**; nella scheda **Initial build** scegliere fonti e limiti, **Preview the plan**, poi **Start the build**. Avanzamento, copertura e note su git restano nella stessa scheda.

---

## 1. Cosa si costruisce

Una funzione **opt-in per progetto**: ogni progetto Connecto ha una memoria propria, organizzata in stanze stabili, che gli agenti Claude e Codex consultano con il comando `connecto memory`. Due cicli la tengono viva, ed entrambi fanno parte della prima versione completa:

1. **Ciclo di sviluppo.** Mentre si lavora, un modello economico legge gli eventi del progetto a piccoli gruppi e scrive **postille**: note brevi con fonte, consultabili subito.
2. **Ciclo di sogno.** A un orario scelto dall'utente, un modello consolida postille ed evidenze nei **ricordi**: verifica nei limiti delle evidenze, deduplica, riorganizza, comprime. Il modello propone operazioni; Connecto le valida e le pubblica in una sola transazione, annullabile.

La memoria migliora il contesto e le procedure che un agente riceve. Non cambia i pesi di alcun modello.

### 1.1 Dentro la prima versione completa

- Archivio SQLite per progetto, fuori dal repository, con relazioni e ricerca testuale (FTS5).
- Stanze fisse e indirizzi persistenti; ricordi strutturati; postille.
- Inizializzazione del palazzo su progetti esistenti (§5.9).
- Uso e raccolta in tutte le sessioni: sviluppo, planning, investigazione, review e conversazioni di progetto (§5.10).
- Cattura eventi ai punti di contatto, coda persistente, raggruppamento, ciclo di sviluppo.
- Scheduler ad app aperta, recupero delle esecuzioni saltate, ciclo di sogno con pubblicazione atomica e rollback.
- Comandi `connecto memory brief | search | read | room | propose | flag | status`.
- Testo di avvio per gli agenti, avviso di nuova versione alle sessioni aperte.
- Configurazione separata dei due cicli (provider, modello, ragionamento, orari, limiti). Provider Claude e Codex, entrambi funzionanti: ogni lavoro LLM che Connecto fa per sé passa da sessioni utility indipendenti dal provider (§2.2, D4), compresi i titoli dei terminali, che ora possono girare anche su Codex (§8.2).
- Integrazione con Pause e Stop Everything.
- Esportazione; vista e impostazioni nell'interfaccia; indicatore di freschezza e arretrati.
- Suite di test e benchmark con/senza memoria.

### 1.2 Fuori (futuro, non in questa consegna)

- Daemon che lavora ad app chiusa o a computer in sospensione.
- Server graph, database esterni.
- Embedding e ricerca semantica (lo schema lascia il posto, nessun codice).
- Sincronizzazione remota o tra macchine; memoria condivisa tra utenti.
- Scrittura automatica in `AGENTS.md` / `CLAUDE.md` (mai, per requisito).
- Tetto di spesa in valuta (manca la telemetria della CLI; vedi §8.4).
- Hook `PostToolUse` per osservare ogni strumento usato dall'agente (vedi §2.5).

---

## 2. Fatti del repository che vincolano il piano

Verificati sul codice il 2026-11-14, **prima** dell'implementazione, e lasciati come scritti allora: descrivono il punto di partenza. Cosa è cambiato da allora: il runner utility è generico tra adapter (2.1, 2.2 → §5.6); Stop Everything annulla le run di memoria (2.4 → §7); SQLite è una dipendenza del solo `connecto-memory` (2.7 → §4.7); lo scheduler esiste (2.3 → §7). Il resto vale ancora.

**2.1 Il runner utility oggi fa una cosa sola.** `crates/connecto-session/src/utility.rs` avvia una sessione Claude nascosta in un PTY, incolla un prompt, aspetta l'hook `Stop` e usa la risposta come titolo di un terminale. È legato ai titoli (`TitleJob`, `Titler`, `clean_title`), ha `RUN_TIMEOUT` di 60 s, `MAX_IN_FLIGHT` 3, e gira con cartella di lavoro = progetto. Gli argomenti sono in `crates/connecto-adapter/src/claude.rs:318`: `--tools ""`, `--setting-sources user`, `--strict-mcp-config`, `--model`, `--effort`, nessun flag di bypass, nessun bridge. È un buon punto di partenza, non un runner generico.

**2.2 Solo Claude, tre modelli.** `UtilityLaunch` è dichiarato "Claude only" (`crates/connecto-adapter/src/spec.rs:93`); `codex.rs` non ha alcun percorso utility. `UtilityModel` è un enum chiuso `Haiku | Sonnet | Opus` (`crates/connecto-core/src/utility.rs`). Non esiste oggi un modo provato di far girare Codex senza strumenti (misurato in S2, §9.1). **Decisione dell'utente (2026-11-14):** non serve. Le sessioni utility diventano indipendenti dal provider: ogni adapter (Claude, Codex, e le CLI future) offre un lancio utility, e Codex è un provider funzionante per entrambi i cicli in questa versione.

**2.3 Non esiste alcuno scheduler.** `connecto-automation` osserva schermi e digita tasti secondo il modello Jev: non pianifica nulla nel tempo. Lo scheduler va costruito.

**2.4 Pause e Stop.** `PauseSwitch` (`crates/connecto-core/src/relay.rs:30`) è globale e ha un'epoca che cresce a ogni pausa. `stop_everything` (`crates/connecto-workspace/src/lib.rs:482`) mette in pausa e ferma i terminali. **Oggi Stop Everything non annulla le run utility**: `Utility::cancel_all` è chiamato solo alla chiusura (`terminals.rs:540-551`). L'integrazione va fatta, non è già presente.

**2.5 Osservabilità parziale.** Gli hook registrati sono: Claude `SessionStart`, `UserPromptSubmit`, `Stop`, `Notification`; Codex `SessionStart`, `UserPromptSubmit` più `notify` per la fine turno. Nessun hook sugli strumenti. Connecto quindi conosce prompt, risposta finale, percorso del log di sessione, inizio e fine turno. **Non** conosce i file modificati né l'esito dei test, se non per ciò che l'agente scrive nella risposta. Una risposta troppo grande arriva come `Reply::Dropped` (`crates/connecto-core/src/events.rs`, `crates/connecto-hook/src/shrink.rs`).

**2.6 Il bridge.** Le operazioni sono un enum `Op` in `crates/connecto-hook/src/bridge.rs` (wire v1, campi ignoti ignorati). L'identità del chiamante viene dall'albero dei processi, mai da una variabile (`ADR 0016`). Limiti: frame di richiesta 256 KiB, corpo di un avviso 3000 byte, avviso 120 byte. Regola scritta in `bridge/render.rs`: i testi che Connecto genera per un agente contengono solo id, parole d'evento e conteggi, mai testo stampato da un terminale.

**2.7 Persistenza.** Tutto è JSON con scritture atomiche sotto `<data>/projects/<id>/` (`settings.json`, `terminals.json`, `history/`, `profiles/`). Nessuna dipendenza SQLite nel workspace. L'id progetto è un hash del percorso reale: **un worktree aperto come progetto a sé è un progetto diverso**.

**2.8 Redazione.** `connecto_redact::redact` non ha limite di lunghezza; `prepare` rifiuta oltre 4096 byte ("denied, never truncated"). Per la memoria si usa `redact`. È un filtro al meglio delle possibilità su forme note, non una prova di assenza. Vale solo per ciò che passa dall'host (§5.6): ciò che una sessione utility legge da sé con i propri strumenti non passa dalla redazione.

**2.9 Modifiche in corso di altri agenti.** Sono non committati `connecto-core/src/terminals.rs`, `connecto-session/src/terminals.rs`, `connecto-session/src/utility.rs`, `connecto-ui/src/app.rs`, `lib.rs`, `tests.rs`, `update_strip.rs` e altri. I task T3 e T9 toccano gli stessi file: partono solo dopo che quel lavoro è committato, oppure in un worktree creato dopo.

**2.10 ADR.** `0001` (solo PTY, niente `claude -p` / `codex exec` / MCP) e `0002` (thread e canali `crossbeam`, niente runtime async) restano rispettati: nessuna modifica proposta. Uso di git da parte dell'host: esiste un solo precedente (`crates/connecto-packages/src/commit.rs`).

**2.11 Regole di qualità.** `unsafe_code`, `unwrap_used`, `expect_used`, `panic` sono negati dai lint. Cancello: `cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace --all-features`. I test non usano CLI reali né credenziali.

---

## 3. Decisioni concrete

| # | Decisione | Motivo |
| - | - | - |
| D1 | Nuovo crate `connecto-memory`: archivio, coda, motore dei due cicli, validatore, scheduler, rendering dei comandi. Non conosce PTY né bridge: parla con il resto tramite un trait `Host`, implementato in `connecto-app/src/memory_host.rs`. | Stesso schema di `connecto-automation` + `automation_host.rs`. Il motore si prova con un host finto. |
| D2 | SQLite tramite `rusqlite` con feature `bundled` e FTS5, modalità WAL. Un file per progetto: `<data>/projects/<id>/memory/memory.db`. | Relazioni, transazioni e ricerca testuale senza server. Nuova dipendenza: serve ADR 0024. |
| D3 | Un thread per progetto possiede la connessione di scrittura e riceve comandi da un canale limitato. Le letture del bridge aprono connessioni in sola lettura sui propri thread. Il thread UI legge solo una vista `ArcSwap`. | ADR 0002; nessun accesso al disco dal thread UI. |
| D4 | Le run dei modelli sono sessioni utility di Connecto: una normale sessione della CLI dell'agente, nascosta in un PTY, con uno scopo preciso, senza bridge, fuori dal registro dei terminali. Ogni adapter offre il proprio lancio utility (`UtilityPurpose::Memory` in sola lettura, `Title` minimo). Cartella di lavoro = cartella del progetto, con il profilo del progetto: la sessione può leggere i file del progetto se serve. | ADR 0001 e 0025. Decisione dell'utente: non c'è un requisito "senza strumenti"; la memoria è protetta dall'host (§5.6, §5.7). |
| D5 | Il contratto di configurazione ha `provider`, `model`, `effort` per ciascun ciclo. I provider Claude e Codex sono implementati entrambi. `capability()` dichiara un provider non disponibile solo per cause reali (CLI non installata, accesso non fatto), con il motivo. | Requisito "configurabili per provider"; fatto 2.2 e decisione dell'utente. |
| D6 | Il contenuto della memoria raggiunge un agente **solo come output di un comando che l'agente ha lanciato**. Testo di avvio e avvisi contengono solo istruzioni fisse, numeri di versione e conteggi. | Regola 2.6: i testi generati da Connecto non portano contenuti arbitrari. Il contenuto resta "dato, non istruzione". |
| D7 | Le modifiche al repository sono osservate dall'host con git a fine turno (ramo, HEAD, percorsi cambiati). I risultati dei test contano come evidenza solo quando compaiono nella risposta dell'agente o in una proposta esplicita. | Fatto 2.5: niente da inventare. |
| D8 | Ogni run tratta un solo progetto: archivio, coda, cartella di lavoro (il progetto stesso) e profilo sono del progetto; l'id progetto è in ogni riga e verificato alla pubblicazione. | Requisito "una run non mescola progetti". |
| D9 | Due assi distinti su ogni ricordo: **stato** (affidabilità) e **freschezza**. Nessuno dei due è derivato dall'altro. | Requisito "distinguere freschezza da affidabilità". |
| D10 | Niente viene cancellato dai cicli. "Superato" e "archivio" sono stati; ogni pubblicazione salva le immagini precedenti. | Rollback e verificabilità. |
| D11 | Scheduler su ora locale con **una esecuzione per data locale e per programma**; chiave di idempotenza `(programma, data locale)`. | Niente duplicati per cambio d'ora o riavvio. |
| D12 | I limiti sono su grandezze misurabili: run per ora, caratteri in ingresso e in uscita, durata. Nessun limite in valuta. | Requisito; manca la telemetria. |
| D13 | Nessun `serde` nel contratto: ogni enum memorizzato o che attraversa un processo ha una parola stabile scritta a mano (`word`/`from_word`; italiano per il contenuto, inglese per le chiavi di `settings.json`). | Come il resto di `connecto-core`: la parola è la forma su disco e sul filo. |
| D14 | Un solo `MemoryControl` serve tutti i progetti aperti; ogni metodo nomina il progetto (`project_id`). | Stesso schema di `PackagesControl`. |
| D15 | Terzo asse **posizione** (`Standing`) accanto a stato e freschezza (§4.8). | Requisito §5.10: proposta ≠ decisione, piano ≠ implementazione, ipotesi ≠ causa accertata, procedura documentata ≠ eseguita. |
| D16 | Git per la memoria è **isolato, non sovrascritto**: repository controllato leggendo i file, configurazione su una lista ammessa, cartella git privata con configurazione generata; altrimenti git non si usa e la lacuna è dichiarata (§5.9). | La configurazione del repository non è considerata affidabile: nulla che possa far eseguire a git un programma o leggere altrove (`memory_git.rs`). |
| D17 | Le dichiarazioni dell'utente dal vivo vengono solo dalla finestra dell'app (§5.11); i prompt di terminale non lo sono mai (`TerminalCredit::Off`, §5.3). | Review: le CLI possono avviare prompt da sole. Deviazione dal piano, §13 DS4, confermata dall'utente il 2026-11-15. Vale anche per i turni delle conversazioni passate nel bootstrap. |

---

## 4. Modello dei dati

### 4.1 Stanze

Fisse, con slug stabile. Non si creano né si rinominano stanze nella prima versione.

| Slug | Contenuto |
| - | - |
| `ingresso` | Il brief. È una **vista generata**, non un deposito: nessun ricordo vi abita. |
| `architettura` | Struttura, componenti, confini, flussi. |
| `officina` | Procedure: come si compila, si prova, si rilascia, si diagnostica. |
| `decisioni` | Scelte con la loro motivazione. |
| `problemi` | Difetti noti, trappole, errori ricorrenti e come evitarli. |
| `lavori-aperti` | Lavori in corso e sospesi. |
| `convenzioni` | Regole di stile e di lavoro. |
| `archivio` | Ricordi superati o non più attivi. Leggibili, esclusi dal brief e, di default, dalla ricerca. |

### 4.2 Identificatori e indirizzi

- Ricordo: `m-` più 8 caratteri base32, generato dall'host, mai riusato.
- Postilla: `p-` più 8 caratteri base32.
- Evidenza: `e-` più numero di sequenza dell'evento.
- Indirizzo: `stanza/slug` (es. `officina/rilascio-macos`). Uno spostamento lascia un **rinvio** dal vecchio indirizzo al nuovo. `read` accetta id o indirizzo.
- Identificatori su una riga (valori di scope, origini, percorsi, nomi di ramo e di terminale): rifiutati se contengono caratteri di controllo, separatori di riga o paragrafo (U+2028, U+2029, NEL), caratteri a larghezza zero, marcatori e override di direzione (bidi) o il BOM. Un percorso `component:` o di un'ancora deve essere relativo al progetto, nella forma di git: niente `/` iniziale, `\`, `:`, segmenti vuoti, `.` o `..`, al massimo 512 byte. Il testo libero (sintesi, dettagli, note) non è soggetto a questa regola: lo cita il renderer (§6).

### 4.3 Ricordo

| Campo | Note |
| - | - |
| `id`, `address`, `room` | Stabili. |
| `kind` | `fatto`, `procedura`, `decisione`, `problema`, `lavoro`, `convenzione`. |
| `standing` | Cosa afferma (§4.8). Ogni `kind` ha il proprio insieme; il default è il più debole. |
| `summary` | Una o due frasi, al massimo 400 caratteri. È ciò che brief e ricerca mostrano. |
| `details` | Testo completo, al massimo 16 KiB. |
| `scope` | `project`, oppure `component:<percorso>`, `branch:<nome>`, `worktree:<percorso>`. Almeno uno, anche più d'uno. Ogni voce porta `gone`: il ramo o il worktree non esiste più (§4.6). |
| `status` | `candidato`, `verificato`, `da_verificare`, `superato`, `conflitto`. |
| `freshness` | `last_confirmed_ms`, `anchors_changed` (bool), `open_annotations` (conteggio). |
| `anchors` | File a cui il ricordo si riferisce: percorso relativo al progetto più hash del blob (40 o 64 cifre esadecimali) al momento dell'osservazione. |
| `sources` | Almeno una evidenza. Obbligatorio. |
| `relations` | `sostituisce`, `contraddice`, `dipende_da`, `vedi_anche`, `deriva_da`. |
| `created_at`, `updated_at`, `version` | Versione della memoria in cui è cambiato l'ultima volta. |

### 4.4 Postilla

Scritta dal ciclo di sviluppo o da un agente (`propose`, `flag`). Sempre visibile subito.

| Campo | Note |
| - | - |
| `id`, `created_at`, `seq` | `seq` crescente per progetto. |
| `type` | `integrazione`, `correzione`, `contraddizione`, `task`. |
| `target` | Id del ricordo a cui si riferisce, oppure nessuno (nota nuova) con stanza suggerita. |
| `text` | Il contenuto, al massimo 8 KiB. |
| `scope` | Come per i ricordi. |
| `sources` | Evidenze. Obbligatorio. |
| `verification` | `non_verificata`, `verificata_da_evidenza`, `dichiarata_dall_utente`. |
| `origin` | `ciclo_sviluppo`, `agente:<terminale>`, `utente`. Il terminale viene dal bridge, mai dall'agente. |
| `memory_kind`, `standing` | Che tipo di ricordo diventerebbe e cosa afferma, quando noto. Non erano nel piano: portano le distinzioni di §5.10 dal ciclo di sviluppo al sogno. Una correzione o contraddizione non afferma alcuna posizione. |
| `absorbed_in` | Vuoto finché un sogno non la assorbe; poi la versione di memoria. |

Una `correzione` o `contraddizione` deve nominare il ricordo a cui si riferisce; una nota nuova non può avere come stanza `ingresso`.

### 4.5 Evidenza e provenienza

Ogni evidenza registra **chi** l'ha prodotta e **di che tipo** è:

| `kind` | Origine |
| - | - |
| `dichiarazione_utente` | Una dichiarazione fatta dall'utente nella finestra di Connecto, scheda **Memories** (§5.11). **Non** un prompt di terminale (§5.3) né un turno di una conversazione passata (§5.9). Il piano diceva "prompt scritto dall'utente": cambiato, §13 DS4. |
| `osservazione_codice` | Fatto git rilevato dall'host: ramo, HEAD, percorsi cambiati, hash di blob. |
| `esito_riportato` | Esito di test o comando **come riportato** nella risposta di un agente. |
| `affermazione_agente` | Ogni altra affermazione in una risposta, e ogni prompt di terminale. |
| `proposta_agente` | Testo di `propose` o `flag`. |
| `documento_progetto` | File del progetto letto dall'host durante il bootstrap (documentazione, manifest, ADR, codice), con il suo hash di blob. Non era nel piano: aggiunto perché una procedura documentata non passi per eseguita. |

Campi comuni: progetto, terminale, sessione dello strumento, turno, ramo e HEAD al momento, puntatore al log di sessione (percorso e posizione), file con hash (per osservazioni e documenti), estratto redatto, e `exposed_to`: l'elenco degli id di ricordi e postille che il registro `served` aveva già mostrato a quella sessione (o terminale) prima che producesse l'evidenza.

**Regola contro l'autoconferma** (`connecto-core/src/memory/confirm.rs`, applicata dall'host, mai chiesta al modello). Le funzioni ricevono la **discendenza** del soggetto: il ricordo stesso e ogni ricordo o postilla da cui è nato (postille assorbite, ricordi fusi o derivati).

- `dichiarazione_utente`, `osservazione_codice` e `documento_progetto` sono sempre indipendenti.
- `affermazione_agente`, `esito_riportato` e `proposta_agente` sono indipendenti solo se la sessione non aveva visto nulla della discendenza.
- Le evidenze indipendenti contano **una volta per origine**: una sessione utente o agente (le sessioni sconosciute condividono un'origine), lo stesso contenuto osservato dei file (o lo stesso HEAD quando nessun file è nominato), lo stesso documento. Una sessione che si ripete è una conferma sola.
- `verificato` richiede almeno una conferma indipendente; una posizione più forte del default richiede un'evidenza indipendente di un tipo ammesso (§4.8).

Il sogno e il bootstrap applicano una regola **più stretta** (§5.7): lì un'evidenza di agente conta per `verificato` o per una posizione più forte solo se la sua sessione non aveva visto **nulla** della memoria, e le fonti proprie di un ricordo, o delle postille che ha assorbito, non lo confermano.

### 4.6 Regole sulla verità

- Il codice descrive il comportamento **attuale**; l'utente definisce quello **voluto**; le decisioni spiegano il **perché**. Nessuna di queste fonti vince sempre sulle altre: se divergono, il ricordo va in `conflitto` con entrambe le fonti visibili.
- Un file ancorato che cambia hash accende `anchors_changed` e può portare a `da_verificare`. **Non** rende il ricordo falso né `superato`.
- Un ramo cancellato, schiacciato o ribasato segna lo scope `branch` come "ramo non più presente" (`ScopeEntry.gone`; i comandi stampano `(branch no longer present)`). Il ricordo resta. La promozione a scope `project` e l'archiviazione richiedono un'evidenza (dichiarazione dell'utente, contenuto ritrovato sul ramo principale tramite percorso e hash, o decisione registrata): la sola raggiungibilità di un commit non basta in nessuna direzione.

### 4.7 Tabelle

`meta` (versione schema, versione memoria, id progetto, stato interno del motore), `room`, `memory`, `memory_scope`, `memory_anchor`, `relation`, `address_redirect`, `evidence`, `source_link` (ricordo o postilla ↔ evidenza), `annotation`, `event` (coda), `batch`, `run`, `publication` (una riga per versione pubblicata: istante, ciclo, operazioni, versione annullata, run), `revision` (immagini precedenti per versione), `served` (cosa è stato mostrato a quale sessione), `schedule_state`, `dream_staging`, `bootstrap`, `bootstrap_source`, `bootstrap_checkpoint`, più `memory_fts` e `annotation_fts` (FTS5).

**Versione dello schema: 2.** La migrazione 2 aggiunge solo gli indici di discendenza (`memory_merged_into`, `annotation_absorbed_into`). Aprire in scrittura applica in una transazione le migrazioni mancanti; un database di una build più recente è rifiutato e non toccato. Ogni tabella porta `project_id`, controllato da un trigger contro quello in `meta` (D8).

**Fusioni.** Un ricordo fuso in un altro resta come riga non viva (`live = 0`, `merged_into`); il suo indirizzo diventa un rinvio e le letture seguono la catena delle fusioni (un ciclo fa fallire la lettura).

**Scritture verificate.** Ogni riga scritta da una pubblicazione o da un annullamento viene riletta e confrontata; poi un controllo su tutto l'archivio verifica che nessuna verifica o posizione resti senza supporto (§5.5, punto 7).

Posizione: `<data>/projects/<id>/memory/` con `memory.db`, `exports/` e `resume-noted` (le conversazioni già avvisate, §6.1). Le run lavorano nella cartella del progetto (D4). La configurazione sta in `<data>/projects/<id>/settings.json`, sotto una chiave `memory`.

### 4.8 Posizione (`Standing`)

Cosa un ricordo o una postilla afferma, separato da quanto è affidabile (stato) e da quanto è recente (freschezza).

| `kind` | Posizioni (la prima è il default) | Cosa serve per le altre |
| - | - | - |
| `decisione`, `convenzione` | `proposta`, `accettata`, `scartata` | `accettata`/`scartata`: `dichiarazione_utente` o `documento_progetto` |
| `lavoro` | `pianificato`, `in_corso`, `implementato` | `in_corso`: nulla; `implementato`: `osservazione_codice` o `dichiarazione_utente` |
| `fatto`, `problema` | `ipotesi`, `accertato` | `accertato`: un'evidenza indipendente di qualsiasi tipo |
| `procedura` | `documentata`, `eseguita` | `eseguita`: `dichiarazione_utente` o `esito_riportato` |

Il bootstrap è più stretto (§5.9): lì `accertato` e `verificato` richiedono `dichiarazione_utente` o `osservazione_codice`, e un documento rende `accettata` una decisione solo se è un registro di decisioni.

Una nota di un agente (`propose`, `flag`) ha sempre la posizione più debole del suo tipo. Una posizione che le evidenze non sostengono è **abbassata** alla più debole, non rifiutata (§5.7).

---

## 5. Architettura

```
terminali ──eventi hook──▶ registro (connecto-session)
                               │  MemoryEvent (canale limitato, mai bloccante)
                               ▼
bridge `connecto memory` ─▶ ┌─────────────────────────────────────────┐
                            │ connecto-memory: un thread per progetto │
UI (vista ArcSwap) ◀──────  │  ingest → coda → batch                  │
                            │  ciclo sviluppo ─▶ postille             │
scheduler (tick) ────────▶  │  ciclo sogno ─▶ validatore ─▶ pubblica  │
                            └───────────────┬─────────────────────────┘
                                            │ trait Host
                                            ▼
                          connecto-app/memory_host.rs
                    (runner utility, git, Pause, segreti, orologio)
```

### 5.1 Trait `Host` (in `connecto-memory`)

```rust
pub trait Host: Send + Sync {
    /// Una chiamata al modello. Blocca fino a risposta, scadenza o annullamento.
    fn run_model(&self, job: &ModelJob, deadline: Instant, cancel: &AtomicBool)
        -> Result<ModelReply, RunError>;
    /// Cosa questo provider/modello sa fare qui, o perché no.
    fn capability(&self, choice: &ModelChoice) -> Capability;
    /// Stato git della cartella del progetto. `None` se non è un repository.
    fn git_snapshot(&self, project: &Path) -> Option<GitSnapshot>;
    fn paused(&self) -> bool;
    fn pause_epoch(&self) -> u64;
    /// Valori segreti noti, da togliere per primi.
    fn known_secrets(&self) -> Vec<String>;
    fn now(&self) -> WallClock;          // UTC, ora locale, data locale, giorno
    fn wake(&self);                      // la vista è cambiata
    // Con default, aggiunti per motore e bootstrap: projects, load_settings,
    // save_settings, read_logged_reply, file_blobs, branches, cancel_runs,
    // project_files, reads_files, read_project_file, git_history, git_diff,
    // past_conversations. Tutti in sola lettura sul progetto.
}
```

`ModelJob { project_id, cycle, choice: ModelChoice { provider, model, effort }, prompt }`: nessuna cartella di lavoro nel job, l'host la ricava dal progetto (D4). `ModelReply { text, source: Hook | SessionLog, chars_in, chars_out, duration }`. `RunError`: `Timeout`, `Cancelled`, `Unavailable`, `HeldUntrusted { reason }`, `ModelError`, `Failed`. `GitSnapshot { branch, head, changed: Vec<ChangedFile> }`, dove `ChangedFile { path, blob, deleted }`: `blob: None` con `deleted: false` vuol dire *sconosciuto*, mai cancellato. Tutto finto nei test (`connecto-memory/src/testing.rs`).

### 5.2 Contratto verso UI e bridge (in `connecto-core/src/memory/`)

Il modulo è una cartella: `settings.rs`, `ids.rs`, `record.rs`, `confirm.rs`, `command.rs`, `bridge.rs`, `control.rs`. Nessun `serde` (D13).

```rust
pub trait MemoryControl: Send + Sync {
    fn view(&self) -> Arc<MemoryView>;                 // non blocca
    fn set_settings(&self, project_id: &str, settings: MemorySettings) -> Result<(), String>;
    fn plan_bootstrap(&self, project_id: &str, options: BootstrapOptions) -> Result<(), String>;
    fn bootstrap(&self, project_id: &str, options: BootstrapOptions) -> Result<(), String>;
    fn cancel_bootstrap(&self, project_id: &str);
    fn resume_bootstrap(&self, project_id: &str) -> Result<(), String>;
    fn run_dream_now(&self, project_id: &str) -> Result<(), String>;
    fn revert(&self, project_id: &str, version: u64) -> Result<(), String>;
    fn export(&self, project_id: &str, format: ExportFormat) -> Result<(), String>;
    fn request_read(&self, project_id: &str, request: ReadRequest) -> Result<u64, String>;
    fn user_statement(&self, project_id: &str, statement: UserStatement) -> Result<u64, String>;
    fn delete_memory(&self, project_id: &str) -> Result<(), String>;
}
```

Ogni comando ritorna subito e non tocca il disco: il lavoro gira sul thread del progetto e il risultato compare in una vista successiva. Gli errori di argomento tornano subito come `Err`; i fallimenti successivi in `ProjectMemoryView::last_error`. `NoMemory` è il controllo vuoto che rifiuta tutto, per costruire la UI senza motore.

`MemoryView`: un `ProjectMemoryView` per progetto aperto più le capacità dei provider (con i modelli e i ragionamenti offerti). Per progetto: impostazioni, abilitata o no, versione, conteggi per stanza e per stato, postille e contraddizioni aperte, **freschezza** (`considered_through_ms`, istante dell'ultimo evento considerato), **arretrati** (eventi in coda, età del più vecchio, batch falliti, eventi persi a canale pieno, ultimo motivo), stato e ultima run di ciascun ciclo, prossimo sogno, date saltate, bootstrap (stato, avanzamento, copertura, lacune, piano), run (al più 100) e versioni, letture (al più 8), dichiarazioni dell'utente con il loro esito (§5.11), ultima esportazione, ultimo resoconto di sogno o annullamento (`last_report`), nota su git (`git_note`, perché git non è usato o lo è solo in parte, §5.9), ultimo errore. `RunOutcome`: `Running`, `Annotated`, `Published`, `NoChange`, `HeldByPause`, `Held { reason }`, `Cancelled`, `Interrupted`, `Failed`. Le letture pesanti (ricerca, un ricordo, una stanza) passano da `request_read` con risposta pubblicata nella vista.

Lato bridge, `MemoryBridge` (`command`, `enabled`, `take_version_notice`) riceve un `MemoryCaller { terminal, tool_session, branch }` riempito dal server del bridge dall'albero dei processi, mai dagli argomenti dell'agente.

### 5.3 Cattura degli eventi

Punti di contatto, tutti già osservabili (fatto 2.5):

| Punto | Cosa produce |
| - | - |
| Prompt accettato | `affermazione_agente`: mai `dichiarazione_utente` (sotto). |
| Fine turno | Risposta come `affermazione_agente`; subito dopo, fotografia git dall'host. |
| Differenza git tra due fotografie | `osservazione_codice` con ramo, HEAD, percorsi cambiati. |
| Chiusura o uscita di un terminale | Evento di fine sessione con puntatore al log. |
| `memory propose` / `memory flag` | `proposta_agente`, **priorità alta** (scritta dal comando stesso, §6). |

Si catturano solo i terminali agente (Claude, Codex) del progetto, a ogni turno, senza filtri su diff, commit o strumenti (§5.10). Le shell sono escluse; le sessioni utility non entrano mai nel registro.

**Chi ha scritto un prompt.** Nessun prompt scritto in un terminale è registrato automaticamente come dichiarazione dell'utente (`TerminalCredit::Off` in `connecto-session/src/input_ledger.rs`, default e unica modalità collegata: `memory_host.rs` imposta la cattura con `set_sink`). Motivo, trovato in review: le CLI possono emettere da sole l'hook del prompt (`/loop` e run programmate di Claude, avvisi di fine di un task in background consegnati come prompt, hook `Stop` o plugin che avviano un nuovo prompt, Codex che continua da solo, comandi slash che si espandono in più prompt), e un tale hook può portare qualsiasi testo; nessuna regola su byte e hook distingue il suo testo da quello dell'utente. Quindi l'input da terminale non può provare che una persona ha scritto qualcosa. Ogni prompt di terminale diventa `affermazione_agente`, con l'etichetta "origin not known: not the user" (o "typed by Connecto for another agent or an automation, not the user") nel prompt del ciclo: può diventare una postilla o una proposta, mai un'approvazione. Le dichiarazioni dell'utente si registrano solo dalla finestra dell'app (§5.11). È una deviazione dal piano: §13, DS4.

Il registro dell'input resta attivo per ordine e conteggi:

- **Un solo ingresso nel PTY** (`InputGate::write`): ogni byte per un terminale del registro passa da lì con la sua fonte (`Window`, `Remote`, `Connecto`, `Protocol`), sotto un lock; il registro vede esattamente i byte che il PTY riceve, nel suo ordine.
- **`TerminalCredit::Quarantine`** resta nel codice come modalità non di default, documentata: accredita all'utente un prompt solo se il terminale era "pulito" (nessuna scrittura estranea, conteggio delle possibili sottomissioni per eccesso). È corretta **solo se lo strumento non emette mai un hook di prompt da solo**; il test `autonomous_hooks_defeat_quarantine` mostra che quell'ipotesi, se falsa, la rompe.
- Un'intestazione `[connecto …]` vince sempre: `[connecto notice]` è di Connecto, ogni altra di un agente.

Regole deterministiche, applicate prima di ogni modello:

- **Esclusione degli eventi interni.** Le run di memoria non sono terminali del registro e non hanno bridge: non producono eventi e non possono chiamare `connecto memory`. Gli avvisi di Connecto, i prompt che sono un comando `connecto memory` e le risposte la cui prima riga inizia con `[connecto memory` sono salvati con stato `interno`, senza evidenza, e non vanno mai a un modello. Così non nascono cicli autoindotti.
- **Duplicati.** Chiave = SHA-256 di (terminale, sessione dello strumento, inizio sessione, turno, tipo, testo redatto); per le osservazioni di codice, della fotografia e della precedente. Un duplicato non entra in coda e non salva evidenze.
- **Prompt oltre 64 KiB.** L'hook porta il prompt tagliato (`MAX_EVENT_TEXT`). Il prompt è salvato come "contenuto non disponibile", con il puntatore al log, senza testo e **senza evidenza**: mai come testo tagliato.
- **Risposta `Dropped`** (oltre 256 KiB). L'evento conserva il puntatore al log di sessione; il testo si legge da lì a pezzi. Se il log non è leggibile, l'evento resta "contenuto non disponibile" e non genera note inventate.
- **Fotografia git.** Ramo trattenuto se la redazione lo cambierebbe, se ha caratteri di controllo o supera 255 byte; HEAD solo se esadecimale da 7 a 64 cifre; percorsi tenuti solo se relativi al progetto e invariati dalla redazione. File senza hash noto (oltre il limite di 500 hash, o hash fallito) non sono osservati e il testo lo dice; un'ancora è "cambiata" solo per un hash diverso noto o una cancellazione confermata.
- **Redazione all'ingresso.** `sanitize` più `redact` con i segreti noti prima di scrivere su disco. Se la redazione non può girare, si salva il solo fatto dell'evento, con l'estratto "(text withheld: redaction could not run)". I segreti noti sono i valori di variabili d'ambiente dall'aspetto segreto (nome che finisce in `_API_KEY`, `_TOKEN`, `_SECRET` o contiene `PASSWORD`, valore di almeno 8 caratteri), più i valori dei segreti personali della cassaforte (ADR 0027), in forma esatta e codificata. Le chiavi interne di Connecto (dispositivo, relay, Jev) non vengono lette. Limite accettato: la redazione copre ciò che l'host raccoglie, conserva e consegna, non le letture che una sessione utility fa da sé né il log di sessione grezzo che la CLI lascia nel proprio profilo (§5.6).
- L'invio dal registro alla memoria non blocca mai un terminale: canale limitato, e se è pieno si incrementa un contatore visibile in `status` ("lost when full").

### 5.4 Ciclo di sviluppo

1. Gli eventi entrano nella tabella `event` con stato `in_coda`.
2. Una finestra di raggruppamento (default **3600 s**, un'ora: il benchmark ha mostrato che 120 s produce molti run piccoli, ognuno con circa 22K token di ingresso fissi, configurabile; 0 = subito) parte dal più vecchio evento pronto. Una proposta esplicita o un evento con priorità la chiude subito.
3. Il batch contiene il **delta** (al più 200 eventi) più i **ricordi pertinenti**, trovati con FTS5 sui termini del delta e sui percorsi toccati, entro un quarto del tetto e al più 15.000 caratteri; se il prompt non entra, si tolgono prima i ricordi.
4. Se il batch supera il tetto di ingresso, il tetto decide quanti eventi entrano e il resto fa il batch seguente: nessuna evidenza è riassunta o tagliata dal programma. Un singolo evento più grande del tetto va da solo, in run consecutive marcate `(part i of n)`; il batch è completo solo quando ogni parte ha una risposta. Un tetto sotto la parte fissa del prompt più 1.000 caratteri non può girare: il ciclo lo segnala ("raise it to at least … characters") invece di superarlo.
5. Il modello risponde con JSON: una lista di postille, anche vuota. Una lista vuota è un esito valido. Un evento `proposta_agente` è già una postilla con la sua evidenza (§6): il ciclo cita l'evidenza esistente, non ne crea un'altra, e chiede al modello solo ciò che manca.
6. L'host valida (§5.7) e inserisce le postille in una transazione che marca anche gli eventi come `considerati`. Ogni evento pertinente è quindi considerato esattamente una volta. **Avanzamento per parti**: un batch in più run mette in staging le postille validate di ogni parte finita (`engine.devbatch.<id>` in `meta`, con una firma del materiale); un nuovo tentativo, una sospensione, una pausa, il limite orario o un riavvio riprendono dalla prima parte non finita. L'inserimento finale resta una transazione unica.
7. **`RunGate` dentro ogni scrittura.** Ogni transazione che porta output del modello (parte, completamento, fallimento, sospensione) ricontrolla pausa, epoca di pausa e annullamento subito prima del commit: una pausa arrivata mentre la scrittura aspettava nella coda del writer la ferma ancora, e il batch torna in coda senza consumare tentativi.
8. Fallimento: il batch torna in coda con ritardo crescente (30 s, raddoppiando, al più 30 min); dopo N tentativi (default 3) è `fallito` e conta negli arretrati, con il motivo in `status`. Scadenza per run: 120 s.
9. **CLI non ancora fidata nella cartella del progetto.** Se la sessione utility si ferma sulla schermata di fiducia della CLI, il runner la riconosce dal testo dello schermo prima di digitare e chiude subito la run con un motivo visibile: «Claude has not been trusted for this project folder yet: open a Claude terminal in the project once and accept» (e l'equivalente per Codex). La run è **sospesa** (`RunOutcome::Held`), non fallita: non consuma tentativi e non viene ritentata in ciclo. Il motivo compare in `status` e nella UI con l'indicazione di cosa fare. Il batch sospeso riparte dopo 15 minuti, o un minuto dopo che un terminale del progetto è stato usato di nuovo (l'istante della sospensione sta in memoria: dopo un riavvio l'attesa ricomincia). Connecto non risponde mai alla domanda al posto dell'utente e non modifica la configurazione della CLI. Vale per ogni run di memoria: sogno (§5.5) e bootstrap (§5.9) compresi, con i tempi di ripresa propri.

Una run alla volta per progetto e per ciclo. Mai durante una Pausa. Limite di run per ora (default 20), contato per parte e controllato prima del batch e tra le parti: oltre, la coda cresce e l'indicatore lo mostra. Ogni run registra caratteri in ingresso e in uscita, durata ed esito.

### 5.5 Ciclo di sogno

1. **Avvio**: scheduler, o "Sogna ora". Il sogno tiene il `PublicationGate` del progetto dall'inizio alla fine (`Engine::publication_gate`): sogno, bootstrap e annullamento di versione non pubblicano mai insieme. Si fissa il **cutoff**: massimo `seq` di eventi e postille in quell'istante. Ciò che arriva dopo resta intatto per il sogno seguente.
2. **Pacchetto di evidenze**, raccolto dal programma: ricordi attivi, postille fino al cutoff, evidenze collegate con i loro estratti, fotografia git corrente, ancore con hash attuale. Diviso **per stanza** se supera il tetto (un gruppo troppo grande va in parti consecutive marcate, di almeno 500 caratteri); una passata finale leggera ("final pass across rooms") sull'indice di tutti i ricordi tratta spostamenti e doppioni tra stanze. **Pacchetti per frammento**: ogni sessione può citare come fonti solo gli id dei blocchi che ha ricevuto. Una sessione nuova per parte; scadenza per sessione 600 s, entro il tempo rimasto del sogno.
3. **Proposta del modello**: solo operazioni strutturate.

   | Operazione | Effetto |
   | - | - |
   | `create` | Nuovo ricordo, con stanza, scope e fonti. |
   | `update` | Cambia sintesi, dettagli, scope o ancore. |
   | `merge` | Più ricordi in uno; gli altri diventano rinvii. |
   | `supersede` | Segna `superato` e collega il sostituto. |
   | `set_status` | Cambia lo stato, con le evidenze che lo giustificano. |
   | `move` | Cambia stanza o indirizzo; lascia il rinvio. |
   | `archive` | Sposta in `archivio`. |
   | `link` | Aggiunge una relazione. |
   | `absorb` | Dichiara una postilla assorbita da un'operazione. |
   | `keep` | Lascia una postilla aperta, con il motivo. |

4. **Validazione dell'host** (§5.7). Ogni sessione è validata contro il proprio pacchetto **e contro lo stato cumulativo** delle sessioni precedenti già in staging. Le operazioni non valide sono scartate una per una e registrate; quelle valide sono redatte e messe in staging (`dream_staging`). Una risposta del tutto illeggibile fa fallire l'intero sogno senza effetti.
5. **Riesame e pubblicazione atomica**: l'intera lista ordinata delle operazioni in staging è giudicata di nuovo contro l'archivio attuale (`operations::recheck`) e si pubblica solo il suo esito finale (`Final`). Una postcondizione abbassa ciò che a quel punto non ha più supporto ("verificato without independent support at the final check: lowered to da_verificare", "standing … without support at the final check: lowered to …"). Una sola transazione scrive le immagini precedenti in `revision`, applica le operazioni, marca assorbite solo le postille con `absorb` e `seq ≤ cutoff`, rilegge ogni riga scritta, ricontrolla verifiche e posizioni su tutto l'archivio, scrive `publication` e incrementa la versione. Un'operazione che l'archivio rifiuta comunque è tolta con il motivo e il resto si pubblica; nulla è mai pubblicato a metà. Se la transazione fallisce, la memoria resta alla versione precedente.
6. **Staging e conti della run.** Il `RunGate` (pausa, epoca di pausa, annullamento) e il limite di tempo sono controllati dentro ogni transazione di staging e dentro la pubblicazione, fino al commit. Il limite di tempo si misura dall'inizio del sogno ("the time limit of N minutes was reached"). I conti della run (riga `run`, cutoff, date del programma) seguono in una transazione propria: prima di pubblicare si scrive un record `dream.pending.<run>`, e `scheduler::reconcile`, a ogni controllo e a ogni avvio di sogno, completa una run interrotta tra le due transazioni che ha già una riga `publication`. Annullato, sospeso da una pausa, sospeso per fiducia, fallito: nulla è pubblicato e lo staging è scartato; lo staging lasciato da un processo fermato è scartato al riavvio.
7. **Rollback**: `revert(versione)` ripristina le immagini precedenti e riapre le postille assorbite da quella versione, come nuova versione. Le postille arrivate dopo non sono toccate. È rifiutato se una versione successiva ha cambiato lo stesso ricordo. I ricordi creati da quella versione sono ritirati, mai cancellati. Quando le evidenze rimaste non sostengono più una verifica o una posizione, l'annullamento la **abbassa** (`da_verificare` o la posizione più debole; per le postille `non_verificata` o nessuna posizione) invece di rifiutare; poi ricontrolla tutto l'archivio e rilegge ogni riga.

**Resoconto del sogno** (`DreamReport`): sessioni, caratteri, durata, operazioni rifiutate con il motivo, aggiustate (anche dalla postcondizione), postille assorbite e lasciate aperte, ricordi abbassati da un annullamento. La vista ne porta un riassunto (`DreamReport` in `connecto-core`: run, versione, rifiutate, aggiustate, abbassate) come `last_report` dell'ultimo sogno o annullamento; la UI lo mostra come **LAST DREAM REPORT** in **Overview** e in **Runs and versions** ("N refused · N weakened · N lowered", con i dettagli e un pulsante per aprire ogni soggetto abbassato).

Se le evidenze non bastano, il ricordo resta o diventa `da_verificare`. Il sogno lavora sul pacchetto che il programma raccoglie; la sessione **può** leggere file del progetto per controllare un'affermazione. Non scrive il progetto (§5.6). Ciò che legge da sé non è un'evidenza finché non entra nel pacchetto di una run: le fonti valide restano quelle del pacchetto (§5.7). Una CLI non ancora fidata sospende la run come al §5.4, punto 9; un sogno programmato sospeso riprova dopo 5 minuti (§7), un "Sogna ora" sospeso non è ritentato da solo.

### 5.6 Protezione della memoria e del progetto

Decisione dell'utente (2026-11-14): non esiste un requisito "nessuno strumento". La protezione sta in quattro punti:

1. **La memoria la protegge l'host.** Il modello propone solo operazioni strutturate; l'host le valida e le pubblica (§5.7, invariato). Nessuna sessione utility scrive nell'archivio.
2. **Lancio in sola lettura, per quanto ogni CLI lo consente**, mantenendo la lettura dei file: Claude con `--tools "Read,Grep,Glob"`, `--setting-sources user`, `--strict-mcp-config`, nessun flag di bypass né `--permission-mode`; Codex con `--sandbox read-only`, `--ask-for-approval never`, `-c mcp_servers={}`, `-c web_search="disabled"`. Il modello `default` non passa alcun flag di modello. È un irrobustimento al meglio per adapter, **non** una condizione per essere un provider. I titoli usano il lancio minimo (Claude `--tools ""`; Codex lo stesso lancio della memoria, perché non ha un lancio senza strumenti).
3. **Nessun bridge, nessun terminale del registro, nessun evento** verso la cattura della memoria: il pane parte senza bridge, le variabili `CONNECTO_BRIDGE_SOCK` e `CONNECTO_TERMINAL` non arrivano alla sessione; una sessione utility non produce eventi e non può chiamare `connecto memory`.
4. **Il prompt** dichiara lo scopo e che il progetto non va modificato, né vanno eseguiti test o comandi; chiede di lavorare dal pacchetto e di non aprire percorsi esclusi dall'utente. Sono istruzioni, non garanzie.

**Il runner** (`connecto-session/src/utility_run.rs`, un solo `UtilityRunner` per l'app, al più 2 run di memoria in volo in tutto; i titoli hanno il proprio limite di 3 per progetto):

- **Verifica esatta del prompt** per le run di memoria: il testo arrivato deve coincidere con quello inviato, salvo spazi bianchi. Fino a 64 KiB fa fede l'hook; oltre, o quando l'hook l'ha tagliato, decide il log di sessione (per Claude togliendo l'involucro `<pasted_content>`; per Codex leggendo i messaggi utente del log). Un prompt tagliato fa fallire la run ("the prompt reached … cut off"). I titoli usano la verifica più larga di prima.
- **Risposta accettata solo da un turno completo**: per la memoria il log di sessione si legge sempre, anche se l'hook porta il testo, e deve segnare il turno finito (Claude: `stop_reason` `end_turn` o `stop_sequence`, quindi un turno fermato al limite di token è rifiutato; Codex: `task_complete`). Un errore della CLI (`API Error`, `isApiErrorMessage`, evento `error`, `turn_aborted`) è un errore, non un output. Il log si rilegge per 2 s.
- **Mai ridigitare senza prova** (`SubmitUncertain`): se dopo l'Invio il compositore è vuoto ma né hook né log confermano il prompt, il runner aspetta invece di ridigitare; un prompt ancora nel compositore dopo 6 s non riceve un secondo Invio. Un log leggibile che non mostra il prompt porta a una sola nuova digitazione.
- **Schermata di fiducia**: riconosciuta dal testo dello schermo prima di digitare; la run è sospesa con il motivo, mai risposta (§5.4, punto 9).
- **Scadenze per job**: titoli 60 s, sviluppo 120 s, sessione di sogno 600 s, sessione di bootstrap 15 min, sempre entro il limite di tempo del ciclo.
- **Annullamento**: Stop Everything e la chiusura chiamano `cancel_all`, che chiude ogni pane in volo.
- `capability()` dichiara un provider disponibile solo se la CLI risulta collegata per il progetto; i motivi visibili sono "… is not installed", "… is not logged in for this project", "…'s login could not be confirmed", "… could not be set up: …". Si ricalcola ogni 300 s.

Detto chiaramente: per una CLI senza modalità di sola lettura, la proprietà "non modifica il progetto" poggia sul solo prompt. Per Codex la sandbox `read-only` blocca le scritture locali ma i comandi in lettura possono girare, e restano strumenti remoti fuori dalla sandbox (S2, §9.1). È il compromesso accettato con la decisione.

**Redazione ed esclusioni: dove valgono e dove no.**

- **(a) Ciò che l'host controlla**: i pacchetti che il programma prepara per una run, e tutto ciò che la memoria conserva, pubblica, esporta o serve con `connecto memory`. Qui valgono redazione (§2.8) ed esclusioni; la risposta del modello è redatta prima di essere salvata.
- **(b) Ciò che una sessione utility fa da sé**: se legge un file del progetto con i propri strumenti, il contenuto grezzo va al modello e finisce nel log di sessione della CLI, nel profilo del provider, prima e fuori da ogni redazione dell'host; le esclusioni del bootstrap non vincolano queste letture. È esattamente ciò che può fare un normale terminale di quella CLI nel progetto. **Limite accettato**, non una garanzia.

### 5.7 Validatore (host)

Rifiuta, registrando il motivo:

- JSON non valido, campi ignoti, campi obbligatori mancanti, tipi errati, lunghezze oltre i limiti;
- id inesistenti, non vivi o di un altro progetto;
- fonti assenti, o fonti non presenti nel pacchetto consegnato a quella sessione;
- `set_status: verificato` senza almeno un'evidenza indipendente (§4.5);
- cancellazioni (non esiste l'operazione), stanze inesistenti, indirizzi duplicati;
- `absorb` di una postilla con `seq` oltre il cutoff;
- una posizione che non si applica al tipo di ricordo;
- testo che la redazione non riesce a pulire o che lascia vuoto.

**Forma dei campi.** Il prompt del sogno e del bootstrap mostra la forma di ogni campo, e i campi lista come liste (l'esempio porta `"scope":["project"]`). Una sola stringa di scope valida al posto della lista è letta come lista di un elemento e registrata fra gli aggiustamenti ("scope: one scope string was given; read as a one-element list (scope is a list, e.g. [\"project\"])"). Un rifiuto nomina il campo e la forma attesa, per esempio "scope: not a list (it must be a list of scopes, e.g. [\"project\"])". La correzione viene da un difetto trovato dalle run reali (§11.3). Il validatore del ciclo di sviluppo non ha questa normalizzazione: lì una stringa di scope singola è rifiutata ("scope: not a list"), e il suo prompt mostra già `"scope":["project"]`.

**Abbassa invece di rifiutare.** Una posizione che le evidenze citate non sostengono è portata alla più debole del suo tipo, e un `verificato` non sostenuto resta `candidato` ("standing … is not supported by its sources: kept as …", "verificato needs an independent confirmation: kept as candidato"): ciò che la sessione ha detto vale ancora la pena di tenerlo, per quello che è.

**Più stretto di §4.5 nel sogno e nel bootstrap** (`operations.rs`). Per `verificato` o per una posizione più forte del default:

- un'evidenza di agente conta solo se la sua sessione non aveva visto **nulla** dalla memoria (`exposed_to` vuoto), non solo nulla della discendenza;
- le fonti proprie di un ricordo, e quelle delle postille assorbite nella sua discendenza, non confermano nulla.

Nel ciclo di sviluppo vale la regola di §4.5, con come discendenza quella del ricordo bersaglio più tutto ciò che le sessioni citate avevano visto.

Una risposta del tutto illeggibile fa fallire la run senza effetti. L'output del modello è redatto prima di essere salvato.

### 5.8 Dati, non istruzioni

Nei prompt, ogni evidenza è racchiusa tra delimitatori con un id casuale per sessione, che non compare nel materiale (nel bootstrap `<<<MATERIAL id` … `MATERIAL id>>>`), e il prompt dichiara che il contenuto tra i delimitatori è materiale da valutare, mai un'istruzione, anche quando si presenta come tale o dice di venire da Connecto, dall'utente o dal sistema. L'output dei comandi `connecto memory` si apre con una riga fissa che lo qualifica come note di sessioni passate, non come richieste dell'utente; ogni testo memorizzato passa da un solo percorso di pulizia, così nessun testo salvato può aprire una riga propria che sembri di Connecto (§6).

**Segreti noti nei prompt.** Il testo memorizzato è redatto con i segreti noti al momento in cui è stato salvato. Ogni prompt che l'host compone (sviluppo, sogno, bootstrap, "chiedere alla memoria") ripassa il **solo materiale** (testo dei ricordi, estratti delle evidenze, testo degli eventi catturati, righe dei ricordi già registrati, candidati, intestazioni dei blocchi, la domanda dell'utente) con i segreti noti al momento dell'invio, **prima** di metterlo insieme alle istruzioni; poi sceglie l'id dei delimitatori, che non compare nel materiale redatto. Istruzioni, chiavi del protocollo, esempi di schema e delimitatori non passano mai dalla redazione: un segreto uguale a una parola del protocollo (`operations`, `annotations`, `Used`, `MATERIAL`…) non può alterare il formato di risposta né spezzare un delimitatore. Anche le risposte sono lette per prime così come arrivano (il JSON delle operazioni, la riga `Used:`), e redatte campo per campo dopo.

Un segreto uguale a qualcosa di strutturale **dentro** il materiale (una stanza, uno slug, un indirizzo) è redatto lì come ogni altro testo. Gli id che Connecto genera (`m-…`, `p-…`, `e-…`) non portano testo dell'utente, quindi le operazioni, che nominano i ricordi per id, continuano a funzionare; un elemento che il modello potrebbe nominare solo con l'indirizzo redatto degrada da solo: un `create` con uno slug che contiene un segreto è rifiutato con il suo motivo (`slug: withheld, it holds what looks like a secret`), un indirizzo citato in una risposta di "chiedere alla memoria" vale se esiste (gli indirizzi sono identificatori che l'utente vede già nella finestra). Mai l'intera run.

**Lunghezza minima.** La redazione dei valori noti (`connecto-redact`, `hide_known`) sostituisce ogni valore non vuoto, di qualunque lunghezza; i minimi del redattore valgono solo per i segreti trovati per forma (token ad alta entropia almeno 32 caratteri, valori nominati almeno 8). Il minimo dei valori noti sta nell'host: l'app registra come segreti noti solo i valori di almeno 8 caratteri delle variabili d'ambiente che finiscono in `_API_KEY`, `_TOKEN`, `_SECRET` o contengono `PASSWORD` (`env_secrets`, `connecto-app/src/memory_host.rs`); quindi "a" o "id" non diventano mai segreti noti.

---

### 5.9 Inizializzazione su progetti esistenti (obbligatoria nella prima versione)

*Sezione riallineata al codice il 2026-11-15, dopo la chiusura delle review.*

All'attivazione l'interfaccia offre **Build the palace from the current project (recommended)** oppure **Start empty**. L'inizializzazione è avviabile anche dopo, dalla scheda **Initial build**. Usa per default il provider e il modello del ciclo di sogno: non introduce un terzo modello obbligatorio. Prima dell'avvio **Preview the plan** mostra, per classe di fonte, cosa leggerebbe e cosa è escluso, i caratteri e il numero stimato di run, senza alcun costo monetario.

Opzioni (`BootstrapOptions`): classi di fonti (tutte per default), esclusioni (percorsi o glob con `*`, `**`, `?`), giorni di storia (default 90, 1-3650), caratteri totali letti (default 2.000.000, minimo **12.000**, massimo 100.000.000), minuti (default 60, 1-1440). Ogni run resta entro il tetto di ingresso del sogno; se quel tetto è troppo piccolo per una run del bootstrap, l'avvio è rifiutato con il motivo.

**Fonti** (`SourceClass`): documentazione e istruzioni, manifest, struttura dei componenti, codice pertinente, configurazione dei test, stato git e modifiche locali, storia recente, conversazioni Connecto. Con git si elencano i file tracciati e i non tracciati non ignorati; senza git, una visita che non segue collegamenti. Le conversazioni sono quelle dei terminali chiusi del progetto (`history/`), lette dal log di sessione dello strumento nel profilo del progetto.

**Mai letti, qualunque scelta:**

- file di credenziali (`.env`, `.env.*`, `*.env`; estensioni `.pem .key .p12 .pfx .crt .cer .der .asc .gpg .kdbx`; `id_rsa*`, `id_ed25519*`, `id_ecdsa*`, `id_dsa*`; `.npmrc .netrc .pypirc .pgpass .htpasswd .git-credentials`; nomi che iniziano con `credentials` o `secrets.`), e ogni percorso che la redazione cambierebbe o che contiene un segreto noto: di questi non arriva nemmeno il nome;
- cartelle di dipendenze e build (`.git`, `node_modules`, `target`, `dist`, `build`, `out`, `vendor`, `.venv`, `venv`, `__pycache__` e simili), file di lock, binari (per estensione o perché non UTF-8 / con byte NUL);
- file oltre **1 MiB** (una dimensione sconosciuta conta come oltre);
- nulla attraverso un collegamento: su Unix ogni componente del percorso è aperto relativo al genitore con `O_NOFOLLOW` e il file è controllato come regolare. Fuori da Unix il contenuto dei file **non è letto** (lacuna `FilesNotRead`): solo nomi, stato git e storia.

Altri tetti: 20.000 file elencati, diff non committato al più 2 MiB (oltre, scartato intero con `InputLimit`, mai tagliato), 500 commit, log di sessione letti al più 64 MiB in tutto. Il testo libero (testi di file, messaggi, conversazioni) può **nominare** percorsi esclusi: l'esclusione vale per il contenuto dei file, non per le parole che altri testi contengono.

**Git isolato** (`connecto-app/src/memory_git.rs`, `memory_private.rs`, `memory_host.rs`). Principio: isolare, non sovrascrivere. Prima di qualsiasi comando git per un progetto, Connecto trova il repository e lo controlla leggendo i file da sé; se qualcosa potrebbe far eseguire a git un programma o leggere fuori dal repository, git non è usato per quel progetto e il bootstrap lo dichiara (lacuna `GitNotUsable`, motivo in `git_note`, §5.2).

- **Ricerca.** Si sale dal percorso reale del progetto fino al primo `.git`, mai oltre la cartella home (che non è mai presa: un repository di dotfiles catturerebbe ogni progetto). La home è una sola regola, quella di git sulla piattaforma (`git_home`), usata sia qui sia per trovare la configurazione globale. Il `.git` più vicino decide: uno rifiutato non fa passare a uno più esterno. Un progetto in una sottocartella (monorepo) restringe ogni comando alla propria sottocartella con un pathspec letterale; i percorsi fuori sono scartati.
- **Struttura.** `.git` è una cartella vera con `HEAD` regolare, `objects/` e `refs/` veri; niente `commondir`, `gitdir`/`gitfile`, `objects/info/alternates`, `reftable`; oppure il puntatore di un worktree collegato verificato nei due sensi. Tutta la cartella `.git` è visitata senza seguire collegamenti (collegamenti, socket, FIFO o dispositivi rifiutati; si salta solo `modules/`). La visita spende da un'unica dotazione (`Budget`): ogni cartella aperta e ogni voce letta costa un'unità, al più 600.000 unità (circa 300.000 voci) e 5 s; esaurita la dotazione, git è rifiutato ("its git folder is too large to check whole"): incompleto vale rifiutato.
- **Configurazione.** Il file del repository (al più 256 KiB e 2.000 righe) deve rispettare una grammatica stretta (ASCII, niente `\`, CR, NUL, chiavi e valori semplici), e la lista di chiavi letta da Connecto deve coincidere con quella di `git config --no-includes --list -z --file` (senza configurazione di sistema e globale). Ogni chiave deve essere nella lista ammessa (`ALLOWED`: chiavi `core.*` di contabilità locale, `extensions.objectformat`/`worktreeconfig`, `remote.*`, `branch.*`, `user.*`, `pull.*`, `push.*`, `init.defaultbranch`, `color.*`, `advice.*`, `column.ui`), con le sole grafie esatte accettate (booleani `true`/`false`, `core.bare = false`, …). Qualsiasi altra chiave (filtri, LFS compreso, `textconv`, diff esterno, fsmonitor, hook, pager, editor, gpg, credenziali, ssh, proxy, alias, `core.worktree`, file di esclusioni o attributi, `submodule.*`, estensioni come il partial clone), ogni `include`/`includeIf`, un file illeggibile: git non è usato.
- **Cartella git privata** sotto `<data>/memory-git/` (mai nel temporaneo di sistema né nel progetto). Su Unix `0700`, file `0600` creati nuovi, controllata prima di ogni uso (cartella vera, dell'utente, nessun accesso a gruppo o altri, nessuna cartella sopra scrivibile da altri salvo sticky); su Windows si controlla solo che sia una cartella vera. Contiene una configurazione generata: le chiavi "semantiche" (`core.autocrlf`, `eol`, `safecrlf`, `filemode`, `symlinks`, `trustctime`, `checkstat`, `ignorecase`, `precomposeunicode`, `protectntfs`, `protecthfs`, `longpaths`, `quotepath`, `abbrev`) copiate, con la precedenza di git, dalla configurazione globale (entrambi i file, `$XDG_CONFIG_HOME/git/config` o `~/.config/git/config` e `~/.gitconfig`, letti da git con `--no-includes`; home risolta come fa git, su Windows `HOME`, poi `HOMEDRIVE`+`HOMEPATH`, poi `USERPROFILE`) < repository < `config.worktree` del worktree attivo. Una chiave globale con una grafia non accettata, un file globale che è un collegamento o un include non seguito: la lacuna `GlobalGitSettings`. La **configurazione di sistema non è mai letta**. `HEAD` è staccato sul commit risolto; l'indice (al più 256 MiB, formato riconosciuto) è copiato; gli oggetti veri si leggono con `GIT_OBJECT_DIRECTORY`.
- **Ciclo di vita.** Una cartella privata si riusa finché i suoi input non cambiano e si toglie quando il progetto si ferma o la memoria si spegne; mai ricorsivamente: nomi noti, uno per uno, senza seguire collegamenti. Su Unix i resti di un crash (processo non più vivo) si tolgono all'avvio successivo; **fuori da Unix restano** (§16).
- **Comandi.** Ambiente svuotato salvo `PATH`, `HOME` (impostata alla home di git), cartelle temporanee e poche variabili di sistema Windows; opzioni globali `--no-pager --no-replace-objects --no-lazy-fetch --no-optional-locks --literal-pathspecs`; `GIT_DIR`, `GIT_WORK_TREE`, `GIT_OBJECT_DIRECTORY`, `GIT_INDEX_FILE` espliciti; `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL` al dispositivo nullo; e come seconda linea 16 sovrascritture via `GIT_CONFIG_COUNT` (fsmonitor, hook, pager, askPass, sshCommand, file di esclusioni e attributi, helper delle credenziali, diff esterno, `gpg.program`, `log.showSignature`, `protocol.allow=never`, `submodule.recurse=false`). Dopo il legame `rev-parse` deve risolvere la stessa work tree e la cartella privata. 10 s e 32 MiB di output per comando.
- **Storia in tre passi** con una cornice non falsificabile: `rev-list` (ogni riga un id esadecimale di 40 o 64 cifre), `cat-file --batch` (ogni record deve aprirsi con lo stesso id, `commit` e la dimensione esatta), `diff-tree --stdin -z --name-status --no-textconv --no-ext-diff --ignore-submodules=all`. Un output non compreso è un fallimento registrato, mai indovinato.
- **Repository annidati e sottomoduli** non sono mai visitati: ogni cartella con il proprio `.git`, o nominata da un `path =` di `.gitmodules`, è un confine escluso (lacuna `NestedRepositories`; fuori da Unix `.gitmodules` non è letto, quindi si riconoscono solo le cartelle con `.git`). Un sottomodulo inizializzato (`submodule.*` nella configurazione) fa rifiutare git per l'intero repository.
- **Ripiego.** Git rifiutato o un comando fallito: si usa la visita delle cartelle che non segue collegamenti, con la lacuna `GitNotUsable` dichiarata. Anche questa visita spende da una dotazione dello stesso tipo: al più 1.000.000 di unità di lavoro e 10 s, e al più 20.000 file; esaurita, l'elenco è troncato e dichiarato incompleto.

**Ruoli dai log degli strumenti** (`connecto-adapter/src/past_log.rs`), con regole che falliscono chiuse: un messaggio è "digitato" solo se non porta marcatori sintetici (promemoria di sistema, comandi locali, output di hook, `[connecto `, istruzioni di `AGENTS.md`, contesto d'ambiente, inizi come `Caveat:` o la ripresa di una conversazione compattata) e, per Claude, se non ha flag `is*`, risultati di strumenti, hook o `userType` diverso da `external`; per Codex, se non ha campi ignoti e il tipo è `plain`. Si tengono solo i turni completi. Il compito d'apertura di un terminale avviato da un altro agente è "iniettato". Ogni turno di una conversazione passata, anche un messaggio digitato, è `affermazione_agente`: un turno passato **non è mai** una dichiarazione dell'utente, come dal vivo (§5.3, DS4; `bootstrap_packet.rs`, `turn_kind`). Quando le conversazioni contengono messaggi digitati, il bootstrap lo dichiara con la lacuna `past_user_provenance`: "Past conversations: messages typed there are not taken as your statements (nothing proves who typed them); to accept or confirm something, use the Memories tab". Una conversazione senza log leggibile si legge dalla trascrizione dello schermo, ogni riga come affermazione di agente (lacuna `RolesUnknown`). Comandi e output di `connecto memory` e avvisi `[connecto notice]` sono scartati.

**Tipi di evidenza.** Il testo di un file, codice compreso, e il diff non committato sono `documento_progetto`; struttura e stato git sono `osservazione_codice`. Di un commit, i file cambiati sono `osservazione_codice` (con il commit come HEAD), il messaggio è `affermazione_agente` ("its message, written by its author"): un'affermazione del suo autore, non un fatto. Il bootstrap è più stretto di §4.8: `accertato` e `verificato` richiedono un fatto osservato dall'host stesso (`osservazione_codice`: un percorso esiste, un hash di blob, ramo o HEAD, i percorsi cambiati da un commit) o una `dichiarazione_utente`, quindi un file **non può stabilire un fatto**; `accettata`/`scartata` richiedono una `dichiarazione_utente` o un `documento_progetto` che sia un registro di decisioni. Il bootstrap stesso non crea mai `dichiarazione_utente`, e con la cattura `Off` l'app non ne crea da un terminale: l'unica che può trovare è una già fatta nella scheda **Memories** (§5.11). La riconciliazione controlla però solo il tipo di evidenza, non la provenienza dalla finestra (`is_window_statement`). Registro di decisioni: file d'istruzioni, cartella ADR/RFC, nome che inizia con `adr-`, `adr_`, `decision-`, `rfc-`. Ciò che non è sostenuto è abbassato alla posizione più debole o da `verificato` a `candidato`.

Percorso: inventario e piano di copertura → pacchetti di evidenze limitati (parti da 500 a 30.000 caratteri, divise a fine riga, mai tagliate; redazione di ogni parte; se la redazione fallisce, della fonte non resta nulla; delimitatori `MATERIAL` con nonce, §5.8) → estrazione con modello, una sessione nuova per pacchetto → riconciliazione → validazione → pubblicazione atomica della prima versione e del brief. Ogni ricordo ha una fonte; procedure documentate non sono esecuzioni verificate, possibili motivazioni dedotte dal codice restano ipotesi. Le conversazioni storiche includono planning e investigazioni, non solo sessioni con diff.

**Copertura.** La UI mostra fonti analizzate, escluse e ancora da analizzare, e le lacune (`CoverageGap`): `no_git`, `no_conversations`, `history_window`, `input_limit`, `time_limit`, `user_exclusions`, `changed_since_start`, `roles_unknown`, `global_git_settings`, `nested_repositories`, `git_not_usable`, `files_not_read`, `past_user_provenance`, ciascuna con il testo per l'utente. Accanto alla copertura, la sezione **GIT** mostra `git_note`: "Git is not used for this project: …" (rifiutato al legame), "Git is partly used for this project: a command failed recently (…)" (legato, ma un comando è fallito di recente), o "Git is used with limits: …" (impostazioni globali non prese). Il completamento della run non implica conoscenza esaustiva del progetto, e la UI lo dice.

**Staging per pacchetto e pubblicazione.** Ogni pacchetto è validato (`operations::validate`) e messo in staging con il suo checkpoint e le misure della run in una transazione che ricontrolla il `RunGate` prima del commit: una pausa o uno stop la annullano. Alla fine: riconciliazione, poi `operations::recheck` → `Final`, e si pubblica esattamente `Final::publication`; su una versione base cambiata si riprova, un elemento rifiutato dall'archivio è tolto e la lista giudicata di nuovo. Il bootstrap non scrive `last_report`.

**Limite di tempo.** Alla scadenza l'estrazione si ferma, le fonti in attesa sono segnate `remaining:time_limit`, si aggiunge la lacuna `TimeLimit` e si **pubblica ciò che è stato analizzato**; lo stato finale è `Completed`. Il limite si conta dall'inizio di ogni avvio o ripresa (non è salvato): una ripresa ha di nuovo il limite intero, e l'attesa del `PublicationGate` o di una pausa conta dentro il limite. Ogni sessione ha al più 15 minuti.

**Stato e ripresa.** Il bootstrap ha ID persistente, manifest di fonti con hash/ambito e checkpoint per pacchetto (`bootstrap`, `bootstrap_source`, `bootstrap_checkpoint`). Stati: non iniziato, in coda, in corso, interrotto, fallito, completato. Non esiste uno stato "in pausa": l'attesa di una pausa, di una run sospesa per fiducia o del `PublicationGate` si mostra come **in coda**. Annullamento (dall'utente o da Stop Everything) → interrotto, riprendibile dall'ultimo checkpoint. Chiusura dell'app o memoria spenta: il bootstrap resta in corso e **riprende da solo al riavvio** dal primo pacchetto non finito, con i tentativi azzerati. Fallito dopo 3 fallimenti dello stesso pacchetto (o per un provider non disponibile), riprendibile. Un crash durante la pubblicazione si riconosce dalla storia delle versioni: nulla è pubblicato due volte; un fallimento non pubblica una memoria parziale come completa. Una sessione sospesa per fiducia non consuma tentativi e riprova dopo 15 minuti (§5.4, punto 9). Il bootstrap può partire prima che l'utente abbia mai aperto un terminale di quella CLI nel progetto. Avviarne uno mentre un altro è in corso è rifiutato; uno nuovo segna fallito quello vecchio non finito ("replaced by a newer initial build").

La cattura degli eventi ordinari parte dall'attivazione. Durante il bootstrap la cattura e il ciclo di sviluppo continuano; le postille scritte nel frattempo restano aperte (il cutoff è fissato alla preparazione) e la pubblicazione iniziale le preserva. Bootstrap, sogno e annullamenti tengono lo stesso `PublicationGate`: non pubblicano mai insieme; "Sogna ora" è rifiutato mentre gira un sogno o un bootstrap. Fonti mutate dalla fotografia iniziale sono segnalate da ricontrollare (`ChangedSinceStart`), senza scartare gli eventi nuovi. La pubblicazione controlla la versione base e si ferma su una pausa al commit. Prima della prima pubblicazione gli agenti vedono le postille disponibili e lo stato «initialisation in progress … This is NOT a complete brief», non un brief apparentemente completo.

Ripetere l'analisi su memoria già popolata è una riconciliazione incrementale, mai un reset: nessuna fusione, sostituzione, spostamento o archiviazione; una creazione su un ricordo esistente gli aggiunge solo fonti; ricordi accettati, scartati o verificati tengono testo e posizione, nessuna posizione è abbassata e nessuno stato scende a `candidato`. Uno stato esistente **può** però passare a `da_verificare` o `conflitto`, e il controllo finale su tutto l'archivio può ancora abbassare una verifica che non ha più supporto (§5.5). Le postille non sono assorbite. La cancellazione rimane un'azione separata. Un progetto senza git o senza cronologia Connecto può essere inizializzato dai file disponibili, dichiarando questi limiti.

### 5.10 Memoria in tutte le conversazioni del progetto

«Ciclo di sviluppo» indica il ciclo di lavoro diurno: comprende implementazione, planning, investigazione, debugging, review e conversazioni sul progetto. Nessun filtro richiede un diff, un commit o l'uso di strumenti per considerare un turno pertinente. Istruzioni di consultazione, comandi e raccolta sono disponibili a tutte le sessioni Claude/Codex collegate al progetto; non si modificano retroattivamente le conversazioni e non si presume accesso a chat esterne a Connecto.

Prima di pianificare o investigare, l'agente recupera decisioni, vincoli, lavori aperti e indagini pertinenti con lo stesso budget usato durante l'implementazione. Al termine propone ciò che aggiunge valore: requisiti, decisioni, alternative scartate con motivazione, domande aperte, evidenze investigative, ipotesi e prossimi passi. Una discussione senza novità può produrre zero postille.

Stati semantici espliciti, realizzati come asse **posizione** (§4.8): proposta ≠ decisione accettata; piano ≠ implementazione; ipotesi ≠ causa accertata; procedura documentata ≠ eseguita. Una proposta di un agente non prova l'approvazione dell'utente: una nota di agente ha sempre la posizione più debole del suo tipo. Quando manca una fonte diretta per l'approvazione, la posizione resta quella più debole. Il sogno conserva queste distinzioni. Anche il bootstrap applica tali regole alle conversazioni pregresse.

Poiché un prompt di terminale non vale mai come dichiarazione dell'utente (§5.3), ciò che l'utente dice in una conversazione arriva alla memoria come evidenza senza attribuzione e può diventare una proposta; approvare, rifiutare o correggere si fa dalla finestra (§5.11).

### 5.11 Dichiarazioni dell'utente dalla finestra

L'unica via a una `dichiarazione_utente` dal vivo (`connecto-core/src/memory/control.rs`, `connecto-memory/src/user_statement.rs`, `connecto-ui/src/memory/statement.rs`). La provenienza è certa: gli agenti possono scrivere nei terminali ma non cliccare l'app, `MemoryControl` è chiamato solo dalla UI, e **nessun verbo di `connecto memory` vi arriva**.

- `UserStatement { kind, about, room, text, seen }`, con `UserStatementKind::{Accept, Reject, Correct, Note}`. `about` (un ricordo o una postilla aperta) è obbligatorio salvo per `Note`; `room` solo senza `about`, mai `ingresso`; testo obbligatorio per `Correct` e `Note`, facoltativo (un motivo) per `Accept` e `Reject`, al più `MAX_STATEMENT_BYTES` = 8.096 byte: gli 8 KiB di una postilla meno i 96 byte della frase fissa che lo precede. Il limite è lo stesso in `UserStatement::check`, nel motore e nel contatore della UI; un testo più lungo è rifiutato, mai tagliato.
- **Legame con ciò che si è visto** (`Seen`): con `about` serve l'impronta di ciò che era sullo schermo (versione e contenuto). Il soggetto si carica per id esatto, senza seguire rinvii o fusioni; se non è più vivo e uguale (testo, posizione o stato cambiati, fuso, assorbito, riportato a un'altra versione anche con lo stesso contenuto; il controllo è un'impronta SHA-256 dei campi mostrati più la versione) la dichiarazione è rifiutata, "changed since you saw it: review it and confirm again", e nulla è scritto.
- `MemoryControl::user_statement(project_id, statement) -> Result<u64, String>`: gli errori di forma tornano subito; poi il risultato arriva in modo asincrono in `ProjectMemoryView.statements` (`StatementState`: in attesa, poi registrato o rifiutato). Si tengono al più 16 risultati, quelli in attesa sempre. Se un `Accept`/`Reject` registrato non si può applicare, l'esito lo dice e la postilla resta aperta. `StatementRefusal`: `Changed`, `AlreadyStated` (la stessa dichiarazione è già una postilla aperta), `Busy` (un sogno, un annullamento o un bootstrap stanno pubblicando), `NotFound`, `Disabled`, `Invalid`, `Other`. Nulla è scritto in caso di rifiuto; la UI tiene la bozza.
- **Una transazione** del writer, sotto il `PublicationGate`: un'evidenza `dichiarazione_utente` con la provenienza della finestra (nessun terminale, sessione o log, `exposed_to` vuoto; l'estratto è una frase fissa sul soggetto, la sintesi vista, citata e redatta, e le parole dell'utente redatte) e una postilla con origine `utente` e verifica `dichiarata_dall_utente`.
- `Accept` e `Reject` su un ricordo `superato` o in `archivio` sono **rifiutati** (si agisce su quello che lo ha sostituito); `Correct` e `Note` su di esso sono registrati. `MemoryDetail.superseded_by` porta i sostituti, e `connecto memory read` stampa la riga `  superseded by: m-…`.
- **Effetto immediato per `Accept` e `Reject` su un ricordo**: applicato attraverso il giudizio finale del sogno (`recheck` → `Final` → pubblicazione), come nuova versione con ciclo `dream` e nessuna run. Se il giudizio o l'archivio non lo permettono, nulla è applicato e la postilla resta aperta per il sogno seguente.

  | Dichiarazione | `decisione`, `convenzione` | `fatto`, `problema` | `lavoro`, `procedura` |
  | - | - | - | - |
  | Accept su un ricordo | `accettata`, `verificato` | `accertato`, `verificato` | posizione invariata, `verificato` |
  | Reject su un ricordo | `scartata` (stato invariato) | `conflitto`, posizione abbassata alla più debole, `contraddizione` aperta | come a sinistra |
  | Correct | `correzione` aperta | idem | idem |
  | Note | `integrazione` aperta | idem | idem |

  Accettare un piano o una procedura li conferma come sono: l'utente non ha detto che il lavoro è finito o la procedura eseguita, quindi nessuna posizione sale. Su una postilla aperta la dichiarazione è una nuova postilla sullo stesso bersaglio, senza effetto immediato; `Accept` di una nota per un ricordo nuovo porta la posizione che l'utente sostiene, `Reject` di una decisione proposta porta `scartata`. La dichiarazione applicata non entra nella discendenza del ricordo: non è mai l'autoconferma che la regola di §4.5 impedisce.
- Nessun evento in coda: il ciclo di sviluppo non vede la dichiarazione, quindi non la conta due volte. È ammessa **durante una Pausa** (è un atto dell'utente, non una run) e non avvia run.
- **UI** (scheda **Memories**): sotto un ricordo o una postilla aperta, pulsanti con parole per tipo e posizione (per esempio "Accept this decision"/"Reject this decision", "Confirm this plan is right", "Confirm this procedure is right", "This is wrong", "Correct this …"), ciascuno con una riga che dice esattamente cosa fa il motore (per esempio "It stays a plan: nothing marks the work started or implemented"); su una postilla aperta: "Recorded as your judgment on this note; the next dream applies it. Nothing changes at once.", più "Add a note…". Un rifiuto chiede conferma ("Record, as your statement: …?"). Le parole dei pulsanti e le righe di spiegazione (`statement.rs`) dicono solo ciò che il motore fa; un giudizio su una postilla dice che è registrato per il sogno seguente. Una bozza è legata al soggetto come era: se il soggetto cambia o non è più mostrato, la bozza è sospesa con il motivo e il testo resta. La UI dice: "What you state here is recorded as yours. What agents or automations type into terminals is not."

### 5.12 Chiedere alla memoria

Decisione dell'utente (2026-11-15): dalla finestra della memoria l'utente fa una domanda sulla memoria del progetto e un modello economico risponde **dai ricordi memorizzati**. Le letture del modello **non sono limitate fisicamente** (l'utente non vuole impedire all'agente di leggere file): il prompt gli chiede di rispondere solo dalla memoria data e di dire chiaramente quando la memoria non contiene la risposta, invece di indagare il codice.

- Contratto (`connecto-core/src/memory/control.rs`): `MemoryControl::ask(project_id, AskRequest { question, choice }) -> Result<u64, String>`; `ProjectMemoryView.asks: Vec<AskEntry>` (dalla più vecchia, al più `MAX_ASKS` = 8, solo in memoria), `AskEntry { id, question, choice, asked_ms, state }`, `AskState::{Pending, Answered { text, used, version }, Failed(String)}`. `used` sono gli indirizzi `stanza/slug` citati che esistono. `ask_choices()` offre un modello economico per fornitore, dai modelli di utilità: Claude `haiku`/`low`, Codex sul modello della sua configurazione (`default`)/`low`.
- Rifiutata con memoria spenta, in Pausa, domanda vuota o oltre `MAX_ASK_BYTES` = 2.000 byte, scelta del modello non valida, o con `MAX_PENDING_ASKS` = 2 domande in attesa.
- Materiale (`connecto-memory/src/ask.rs`), composto dall'host con le stesse pagine, la stessa pulizia e la stessa redazione di `connecto memory`: il brief dell'ingresso (600 token stimati), la ricerca della domanda (800), quella di al più 3 parole chiave (300 ciascuna), e al più 4 ricordi interi (1.500 ciascuno) con indirizzo, tipo, posizione, stato e fonti. In tutto al più 8.300 token stimati (circa 33.000 caratteri, più le righe `altro:`), più una legenda delle parole di posizione e stato e circa 1.500 caratteri fissi. Domanda e memoria stanno tra delimitatori con un nonce; la memoria è dato, mai istruzione. Il modello chiude con una riga `Used: …`.
- Sessione: una sessione di utilità ordinaria (`UtilityRunner`, ADR 0025) con l'avvio in sola lettura dei cicli, etichetta `ask`, scadenza 90 s; Stop Everything la annulla; una risposta arrivata dopo l'inizio di una Pausa è scartata. Le sessioni di utilità non hanno la presa di cattura: la domanda non diventa mai conversazione del progetto.
- La risposta è testo non fidato: redatta, caratteri di controllo e invisibili tolti, ogni riga che comincerebbe con `[connecto` diventa `(connecto`, al più 6.000 caratteri.
- **Una risposta non è un ricordo**: nessuna evidenza, postilla o riga `served` (è l'utente che legge la sua memoria attraverso un modello, come le altre letture della finestra); nessuna riga `run` (non è un ciclo: non compare in Run e versioni e non conta nel tetto di run all'ora del ciclo di sviluppo).

## 6. Consultazione: `connecto memory`

Una nuova variante `Op::Memory { verb, args, text }` sul wire v1. L'identità del chiamante viene dal bridge, quindi `propose` e `flag` hanno una fonte certa. Un build precedente risponde "operazione sconosciuta": il comando lo dice in chiaro.

| Comando | Cosa fa | Budget di default |
| - | - | - |
| `memory brief` | Orientamento: versione, freschezza, convenzioni e procedure principali, lavori aperti, contraddizioni aperte, per lo scope corrente. | 600 token |
| `memory search <testo> [--room R] [--scope S] [--all]` | Ricerca testuale su ricordi e postille. | 800 token |
| `memory read <id\|indirizzo>` | Ricordo completo: dettagli, fonti, relazioni, postille collegate. | 1500 token |
| `memory room <stanza>` | Indice della stanza: id, indirizzo, sintesi, stato. | 800 token |
| `memory propose [--type T] [--about <id>] [--room R] [--scope S]` | Crea una postilla dal testo su stdin. Priorità alta nel ciclo di sviluppo. | — |
| `memory flag <id> [--type contraddizione\|correzione]` | Postilla collegata a un ricordo, motivo su stdin. | — |
| `memory status` | Abilitata, versione, freschezza, arretrati, ultima run di ciascun ciclo, prossimo sogno. | 200 token |

Il parser è unico per i due lati (`connecto-core/src/memory/command.rs`): `connecto-hook` legge la riga di comando, il motore la riceve sul filo come parola del verbo, argomenti e testo. Gli errori sono frasi fisse: non ripetono mai ciò che l'agente ha scritto.

Regole dell'output (`connecto-memory/src/render.rs`, `commands.rs`):

- `--budget <n>` cambia il budget (da 50 a 20.000). I token sono **stimati** (caratteri / 4): è scritto nell'output.
- **Il budget copre le voci e le righe `altro:`.** Niente è tagliato a metà: una voce entra intera o è elencata come `altro: <id> <sintesi breve> (read: …)`, con il comando per leggerla (e un `--budget` sufficiente quando la voce da sola supera il budget). Le righe `altro:` possono superare il budget al più del 25%; quelle che non entrano più sono contate in "altro: N more not listed".
- **Vista unica.** Ogni ricordo mostrato porta accanto le sue postille aperte. Una postilla `contraddizione` compare **subito sotto** il ricordo che contraddice (marcata `!`), con origine e fonti, in ogni comando che mostra quel ricordo, brief compreso; nel brief i ricordi contraddetti vengono prima, sotto `== open contradictions ==`. Una riga `altro:` di un ricordo porta con sé i marcatori delle sue contraddizioni aperte (`[! contraddizione p-…, sources: …]`), oppure mostra il solo id senza sintesi. Non esiste un'opzione per nasconderle.
- Ogni voce mostra separati tipo, **posizione**, **stato** e **freschezza** (ultima conferma, ancore cambiate, numero di postille aperte), e le fonti per tipo; uno scope di un ramo sparito porta `(branch no longer present)`.
- Ogni `brief`, `search`, `read`, `room` registra in `served` **tutto ciò che ha stampato**, voci intere, righe `altro:` ed estratti di evidenza (un estratto registra il ricordo di cui è fonte), prima di consegnare la pagina; se la registrazione fallisce, la pagina non è consegnata. Registrare troppo è la direzione sicura. `status` non mostra contenuti e non registra nulla.
- `brief` e `search` lasciano fuori l'archivio (`search --all` lo include); `brief` mostra i ricordi dell'intero progetto e del ramo corrente del chiamante. Durante la prima costruzione il brief elenca solo le postille e dice "This is NOT a complete brief".
- **`propose` e `flag`**: testo vuoto o oltre 8 KiB rifiutato, mai tagliato ("Nothing was stored"). Stanza di default `decisioni`, `lavori-aperti` per `--type task`; scope di default `project`. La nota ha origine `agente:<terminale>`, verifica `non_verificata` e **sempre la posizione più debole del suo tipo**: solo l'utente può fare di una proposta una decisione, e lo fa dalla scheda **Memories** (§5.11); l'output del comando dice "It stays a proposal until the user confirms it". Evento con priorità, evidenza `proposta_agente` e postilla sono scritti **in una sola transazione**; la stessa nota inviata di nuovo dalla stessa sessione (stessi terminale, sessione, ramo, verbo, tipo, bersaglio, scope e testo redatto) è riconosciuta dalla chiave dell'evento e salvata una volta sola.
- **Segreti**: il testo di `propose`/`flag` è redatto con i segreti noti e le regole a pattern prima di essere salvato; ogni output è ripassato contro i segreti noti prima di essere stampato.
- **Un solo percorso di pulizia** per ogni testo memorizzato: `clean` per un valore su una riga (scope, origini, terminali, rami, percorsi, motivi), `quoted` per il testo libero, con ogni riga che inizia con `  > `. Entrambi trattano ogni tipo di a capo come uno e tolgono controlli, escape e formattazione invisibile: nessuna riga stampata inizia con `[connecto` se non l'ha scritta Connecto.
- Memoria non abilitata: ogni comando risponde con una riga sola, "project memory is not enabled for this project (Project settings > Memory turns it on)."
- **Codici di uscita** di `connecto memory`: 0 quando l'app ha risposto (la risposta può dire che nulla è stato salvato), 1 quando l'app ha rifiutato il comando o è più vecchia del comando, 2 per un errore d'uso visto dal comando stesso (testo su stdin assente o oltre 8 KiB compresi), 3 quando l'app non è raggiungibile.

### 6.1 Avvio e avvisi

- **Testo di avvio** (`bridge/render.rs`): quando la memoria è abilitata, una sezione fissa in più che elenca i comandi e dice di eseguire `connecto memory brief` prima di iniziare un lavoro. Nessun contenuto di memoria nel testo (D6).
- **Sessioni riprese** avviate prima dell'abilitazione: si usa il meccanismo `resume_note` esistente, con una nota fissa, **una sola volta per conversazione**, alla prima ripresa con la memoria accesa. Le conversazioni già avvisate sono elencate in `memory/resume-noted` (al più 2000 id); se l'elenco non si può scrivere, niente nota, piuttosto che una a ogni avvio.
- **Nuova versione**: dopo una pubblicazione, ogni terminale aperto riceve un avviso nella casella del bridge: `[connecto notice] project memory updated to version 14`. Compare al successivo `UserPromptSubmit` di quel terminale. **Nessun terminale viene svegliato**: l'avviso aspetta il prossimo prompt. Più versioni in attesa si fondono in un solo avviso. La prima richiesta per un terminale registra solo la versione corrente, senza avviso: il terminale eseguirà comunque `brief`.
- Un'app più vecchia del comando risponde "operazione sconosciuta"; il comando stampa che l'app non ha la memoria di progetto e va aggiornata.

---

## 7. Scheduler

`connecto-memory/src/scheduler.rs`: codice semplice sopra l'archivio e un `WallClock`.

- Thread unico nell'app, tick ogni 30 s, più un controllo all'avvio e quando l'orologio di parete si sposta di oltre 60 s rispetto a quello monotono (ritorno dalla sospensione).
- Programma per progetto (`dream`): giorni della settimana, ora locale `HH:MM`, durata massima, condizione minima (default: almeno un evento o una postilla dopo l'ultimo cutoff). Nessun giorno scelto: mai in programma, "Sogna ora" funziona ancora.
- **Idempotenza**: per ogni `(programma, data locale)` esiste al più una run. Data e ora locali si confrontano come valori di calendario, mai come istanti: un'ora che il cambio d'ora ripete trova la data già presa, un'ora che salta è superata dal primo tick successivo; una sola esecuzione per data in entrambi i casi.
- **Recupero**: le date dovute senza run, al più **31 giorni** indietro e **mai prima dell'abilitazione** (l'istante in cui lo scheduler ha visto il programma per la prima volta), producono **una sola** run cumulativa: vale per la data più recente e registra le altre come recuperate da quella.
- **Condizione minima non soddisfatta**: le date sono chiuse come fatte, senza run.
- **Esito e data**: una run pubblicata, senza cambiamenti o **fallita** conta come la run di quella data. Sospesa per fiducia: la data non è fatta e si riprova **5 minuti** dopo, come recupero. Sospesa da una pausa, annullata, interrotta: la data aspetta e il primo controllo dopo la ripresa la avvia.
- **Riavvio a metà**: una run `in_corso` trovata all'avvio è marcata `interrotta`; lo staging è scartato; il recupero la rilancia una volta. Una run interrotta che ha già una riga `publication` è invece completata da `scheduler::reconcile` (§5.5, punto 6).
- **Pause**: nessuna run parte in pausa e "Sogna ora" è rifiutato. Una run in corso controlla pausa ed epoca di pausa dentro ogni transazione fino al commit: se sono cambiate, non pubblica e torna in attesa.
- **Stop Everything**: annulla le run di memoria in corso (`Engine::cancel_runs`, registrato con `on_stop_everything`; Stop Everything mette anche in pausa). Gli eventi restano in coda. Nulla riparte finché l'utente non riprende.
- **Chiusura dell'app**: annulla le run, ferma i thread; lo stato su disco resta coerente perché ogni scrittura è una transazione. Il bootstrap riprende al riavvio (§5.9).
- Nulla gira ad app chiusa. L'interfaccia lo dice accanto all'orario e nella scheda delle run.

---

## 8. Configurazione e limiti

### 8.1 Impostazioni per progetto (`settings.json`, chiave `memory`)

```json
{
  "enabled": false,
  "development": {
    "provider": "claude", "model": "haiku", "effort": "low",
    "group_seconds": 3600, "max_runs_per_hour": 20,
    "max_input_chars": 60000, "max_attempts": 3
  },
  "dream": {
    "provider": "claude", "model": "sonnet", "effort": "medium",
    "days": ["mon","tue","wed","thu","fri"], "time": "02:30",
    "max_minutes": 30, "max_input_chars": 200000, "min_new_items": 1
  }
}
```

Intervalli (fuori intervallo il valore è riportato dentro): `group_seconds` 0-3600, `max_runs_per_hour` 1-600, `max_input_chars` 5.000-2.000.000 (sviluppo) e 5.000-5.000.000 (sogno), `max_attempts` 1-20, `max_minutes` 1-240, `min_new_items` 0-10.000. Il minimo di **5.000** caratteri vale per entrambi i cicli: la parte fissa di un prompt di ciclo è di circa 4.200 caratteri. Ogni ciclo controlla anche il proprio minimo reale (parte fissa più 1.000 caratteri per lo sviluppo, più 500 per il sogno) e lo segnala invece di superare il tetto. Il bootstrap ha il proprio minimo di 12.000 (§5.9). Modello ed effort sono parole libere (1-64 caratteri tra lettere ASCII, cifre e `. _ : - [ ]`, mai all'inizio `-`), perché diventano argomenti della CLI.

Lettura e scrittura in `connecto-session/src/project_settings.rs`. Ogni campo ricade sul proprio default da solo; una scelta di modello si prende intera o per niente. Le altre chiavi del file sono conservate. **Un `settings.json` corrotto o illeggibile non viene sovrascritto**: il salvataggio è rifiutato e il file resta com'è ("the project's settings.json is not a JSON object; it was left unchanged"); solo un file assente parte dai default. In lettura, un file illeggibile dà i default, cioè memoria spenta.

I valori numerici erano punti di partenza: lo spike S1 li ha confermati.

**Esito S1** (`project-memory.work/spikes.md`): `max_input_chars` 60000 e 200000 confermati. 300000 caratteri sono arrivati interi al modello; il limite reale è il contesto del modello, non l'incolla. Condizione per 200000: oltre 64 KiB l'hook `UserPromptSubmit` porta il prompt tagliato (`MAX_EVENT_TEXT`), quindi il runner verifica l'arrivo dal log di sessione (T3). Il `RUN_TIMEOUT` fisso di 60 s diventa una scadenza per job (valori in ADR 0025): una run è rimasta senza `Stop` per 25 minuti durante un'interruzione di rete.

### 8.2 Modelli

`ModelChoice { provider, model: String, effort: String }` sostituisce l'enum chiuso per le run di memoria. Provider disponibili: Claude e Codex. L'elenco offerto in UI viene da `capability()`, che segna un provider non disponibile solo per cause reali (CLI non installata, accesso non fatto), con il motivo. Offerte: Claude `haiku`, `sonnet`, `opus`, `default` con effort `low`, `medium`, `high`, `xhigh`, `max`; Codex `default` (il modello configurato in Codex) con `low`, `medium`, `high`, `xhigh` (`max` è passato a Codex come `xhigh`). `default` non passa alcun flag di modello.

Decisione dell'utente (2026-11-14): ogni provider che Connecto sa far girare è disponibile per ogni funzione utility, titoli compresi. L'enum `UtilityModel` dei titoli ha quindi una variante in più, `Codex` (parola `codex`), che usa il modello di default di Codex; un titolo gira sul provider che l'impostazione nomina.

### 8.3 Opt-in

Disattivata di default. Si attiva in Impostazioni del progetto → **PROJECT MEMORY** → **Keep a memory for this project**. All'attivazione si creano le stanze e si offre la costruzione iniziale dal progetto esistente (§5.9), oppure la partenza vuota esplicita. Disattivare ferma cattura e cicli e lascia i dati.

Cancellare la memoria è un'azione separata, con conferma: la memoria viene spenta e salvata come spenta, poi si tolgono per nome solo `memory.db` (con `-wal`, `-shm`, `-journal`) e le esportazioni scritte da Connecto; le cartelle solo se vuote, nulla ricorsivamente. Su Unix la cancellazione è a prova di collegamento (`openat` con `O_NOFOLLOW`, `unlinkat` per nome): un collegamento a qualsiasi livello rifiuta la cancellazione o è tolto come collegamento, mai attraversato. **Fuori da Unix la cancellazione è rifiutata**: la memoria è spenta, i file chiusi, e l'utente riceve il percorso della cartella da togliere a mano. Le trascrizioni delle run di memoria nel profilo della CLI non sono toccate (ADR 0025), e la conferma nella UI lo dice: "Memory runs also leave their transcripts in the CLI's own session logs (Claude, Codex). Deleting the memory does not remove those."

### 8.4 Cosa si misura e cosa no

Si misura e si mostra: numero di run, caratteri in ingresso e in uscita, durata, esito (scheda **Runs and versions**, al più 100 run nella vista). **Non** si mostra una spesa in valuta e non si promette un tetto economico: la CLI non dà telemetria affidabile. Nessun risparmio di token è affermato da nessuna parte senza le misure del benchmark (§11.3).

### 8.5 Esportazione

`export` scrive in `<data>/projects/<id>/memory/exports/`: un JSON completo `memory-<ms>.json` (ricordi, postille, evidenze, relazioni, rinvii, versioni; formato versione 1) e una cartella Markdown `memory-<ms>/` con un file per stanza. Ogni esportazione ha nomi propri: nulla di precedente è sovrascritto. Si esporta solo testo memorizzato, che è già redatto. Mai dentro il repository: il motore accetta una cartella scelta, ma la UI di questa versione scrive solo nella cartella dati.

---

## 9. Spike preliminari (cancello prima di T3 e T6)

Brevi, con esito scritto in un ADR. Non spostano nulla fuori dalla versione: decidono parametri e forma.

| Spike | Domanda | Esito atteso |
| - | - | - |
| **S1** Costo e limiti del runner | Tempo di avvio per run; dimensione massima di un prompt incollato che arriva intero (il runner verifica già `arrived_whole`); dimensione massima di risposta prima di `Reply::Dropped`; lettura della risposta dal log di sessione; comportamento con cartella di lavoro nuova (richiesta di fiducia; non più pertinente, D4). | Valori per `max_input_chars`, timeout, e la scelta fra una sessione nuova per batch (default) o riuso. Il costo di avvio va **misurato**, non presunto. |
| **S2** Codex senza strumenti | Esiste un lancio Codex in PTY che non può scrivere né eseguire, con fine turno via `notify`? | Esito: no. Non più un requisito per decisione dell'utente (§13, DS1): Codex è un provider con lancio in sola lettura al meglio (§5.6). |
| **S3** SQLite | `rusqlite` `bundled` + FTS5 compila su macOS e Windows, peso del binario, licenze compatibili con AGPL-3.0. | ADR 0024. |

ADR scritti: **0024** archivio SQLite della memoria; **0025** run di memoria su sessioni utility indipendenti dal provider (estende, non contraddice, 0001); **0026** scheduler ad app aperta.

### 9.1 Esiti (2026-11-14)

Dati grezzi in `project-memory.work/spikes.md`; decisioni in ADR 0024, 0025, 0026.

- **S1.** Una run nuova costa circa 3,2 s (0,65 s al compositore, 1,25 s di incolla e attesa, ~1,2 s di modello per una risposta minima) e circa 17.300 token di ingresso fissi, in cache. Sessione nuova per batch: confermata. Prompt fino a 300.000 caratteri arrivano interi; oltre 64 KiB la verifica va fatta sul log di sessione. Il testo della risposta compare nel log da 1 a 120 ms dopo l'hook `Stop`: lettura con breve ritentativo. Una risposta `API Error: …` è un errore, non un output.
- **S1, cartella di lavoro.** Due problemi della cartella separata non sono più pertinenti: il rifiuto di `validate_project` dentro la cartella dati e il profilo scelto dalla cartella; le run lavorano nella cartella del progetto con il suo profilo (D4). La domanda di fiducia resta possibile: la cartella del progetto non è fidata finché l'utente non vi ha accettato la CLI almeno una volta (per esempio un bootstrap prima di ogni terminale). Una sessione nascosta vi si blocca (misurato): gestione in §5.4, punto 9.
- **S2.** Negativo: Codex 0.167.1 non ha un modo di partire con zero strumenti. Anche il lancio più ristretto lascia `functions.exec`, la richiesta di input e gli strumenti per avviare altri agenti; con sandbox `read-only` i comandi girano ma le scritture locali sono bloccate. **Decisione dell'utente (2026-11-14):** non è più richiesto; Codex è un provider funzionante con lancio in sola lettura al meglio (§5.6, ADR 0025). Fine turno via `notify`: arriva due volte per prompt (prima per il turno interno che genera il titolo), il runner deve riconoscere il turno giusto.
- **S3.** Positivo su macOS: `rusqlite` 0.42 `bundled` (SQLite 3.54.1, FTS5) compila e il test passa; circa +1,8 MB per architettura; licenze MIT/Apache-2.0 e dominio pubblico, compatibili con AGPL-3.0. Windows **non verificato** da qui: va provato con il workflow `release-build`.

---

## 10. Task per /dev-team

Owner secondo la tabella del team. `core` è il contratto: lo cambia chi coordina, insieme ai due lati. Ogni task ha un revisore Codex del proprio lato.

| # | Task | Owner | Dipende da | Stato |
| - | - | - | - | - |
| **T0** | Spike S1, S2, S3 e ADR 0024-0026. | `strong-backend` | — | fatto |
| **T1** | Contratti in `connecto-core/src/memory/` (era `memory.rs` nel piano): tipi di §4, `MemoryControl`, `MemoryView`, `MemorySettings`, `ModelChoice`. Lettura e scrittura della chiave `memory` in `project_settings.rs`. | `strong-backend` (contratto concordato con il frontend prima di scrivere) | T0 | fatto |
| **T2** | Crate `connecto-memory`: schema e migrazioni, thread per progetto, archivio (ricordi, postille, evidenze, relazioni, rinvii, revisioni, FTS), connessioni in sola lettura. | `strong-backend` | T1 | fatto |
| **T3** | Sessioni utility generiche tra adapter (Claude e Codex): un lancio utility per adapter (sola lettura al meglio, nessun bridge), un runner riusabile estratto da `utility.rs` (realizzato come `UtilityRunner` in `utility_run.rs`) (prompt lungo con verifica dal log oltre 64 KiB, timeout per job, risposta da hook o da log con breve ritentativo, `API Error` come errore, fine turno Codex via `notify` sul turno giusto, schermata di fiducia riconosciuta come run sospesa con motivo tipizzato (§5.4, punto 9), annullamento), `capability()` per provider con motivo. Cartella di lavoro = progetto. I titoli passano dal nuovo runner senza cambiare comportamento (e ora possono girare su Codex, §8.2). Assorbe il vecchio T3b. | `strong-backend` | T0, e §2.9 risolto | fatto |
| **T4** | Cattura eventi: emissione dal registro (prompt, fine turno, chiusura), fotografia git nell'host, redazione, deduplica, esclusione degli interni, coda persistente. | `strong-backend` | T2 | fatto |
| **T5** | `connecto-app/src/memory_host.rs`: implementazione di `Host`, avvio e arresto dei thread per progetto, collegamento a Pause e Stop Everything (compreso l'annullamento che oggi manca). | `backend` | T3, T4 | fatto |
| **T6** | Ciclo di sviluppo: finestra, batch, ricordi pertinenti, divisione oltre il tetto, prompt, validatore delle postille, ritentativi, limiti. | `strong-backend` | T4, T5 | fatto |
| **T7** | Bridge: `Op::Memory`, i sette comandi, rendering a budget, vista unica con contraddizioni accanto, registro `served`, testo di avvio, `resume_note`. | `backend` (rendering) + `strong-backend` (wire e identità) | T2 | fatto |
| **T8** | Scheduler: tick, programma, idempotenza per data locale, recupero, run interrotte, pausa. | `strong-backend` | T5 | fatto |
| **T9** | Ciclo di sogno: cutoff, pacchetto, divisione per stanza, operazioni, validatore completo, staging, pubblicazione atomica, rollback, avviso di versione. | `strong-backend` | T6, T7, T8 | fatto |
| **T9b** | Bootstrap (§5.9): inventario e selezione fonti, manifest, checkpoint, estrazione, riconciliazione, pubblicazione, ripresa e integrazione con coda diurna. | `strong-backend` | T4, T5, T9 | fatto (git isolato rifatto in review, §5.9) |
| **T10** | UI: bootstrap con fonti, limiti, copertura, avanzamento e ripresa; sezione Memoria nelle impostazioni del progetto (opt-in, due cicli, orari, limiti, capacità dei provider); indicatore di freschezza e arretrati; elenco delle run; "Sogna ora"; annulla versione; esporta. | `frontend`; `strong-frontend` per la vista delle run e lo stato | T1 (contratto), poi T9 per i dati reali | fatto |
| **T11** | Vista memoria nell'interfaccia: stanze, ricordo con postille accanto, stato e freschezza distinti, fonti. | `strong-frontend` | T10 | fatto |
| **T12** | Esportazione JSON e Markdown. | `backend` | T2 | fatto |
| **T13** | Suite di robustezza (§11.2) e harness di benchmark (§11.3). | `strong-backend` | T9 | suite di robustezza fatta; harness e misure del benchmark in corso in un altro task |
| **T14** | README e documentazione utente: cosa fa, cosa non fa (app chiusa, costi), come si consulta. | `backend` | T9, T10 | fatto (questo aggiornamento, `docs/project-memory.md`, README) |
| **T15** | `/simplify` sul cambiamento finito. | `strong-finalizer` | tutto | da fare |

Milestone interne, ciascuna consegnabile e verificabile, tutte parte della stessa versione:

- **M1 Fondamenta**: T0-T2, T12. Archivio leggibile ed esportabile, senza modelli. **Fatta.**
- **M2 Consultazione**: T7, T10 (impostazioni). Gli agenti leggono e propongono; le postille esplicite funzionano. **Fatta.**
- **M3 Ciclo di sviluppo**: T3-T6. **Fatta.**
- **M4 Ciclo di sogno e avvio su progetti esistenti**: T8, T9, T9b, T11. **Fatta.**
- **M5 Chiusura**: T13-T15. **In corso**: benchmark e T15 restano.

T7 e T10 potevano procedere in parallelo a T3-T6 dopo T2.

---

## 11. Test

### 11.1 Per componente

Host finto, modello finto, orologio finto. Nessuna CLI reale nei test ordinari.

- Archivio: migrazioni, vincoli, rinvii d'indirizzo, FTS, lettura concorrente durante una scrittura.
- Validatore: una tabella di casi per ogni regola di §5.7.
- Adapter: per ciascun adapter, il lancio utility attesta la sola lettura che quella CLI consente (Claude: `Read,Grep,Glob`, nessun bypass; Codex: `--sandbox read-only`, `--ask-for-approval never`, niente MCP, ricerca web spenta) e l'assenza di bridge.
- Bridge: analisi di `Op::Memory`, comandi a memoria spenta, budget.
- UI: harness egui con finti, come per gli altri pannelli.

Le run con le CLI reali sono in `connecto-session/tests/utility_real.rs`, ignorate per default e lanciate a mano, un provider alla volta, con il login reale. Sono le sole prove con CLI reali: un percorso completo memoria-CLI-memoria con le CLI reali non è stato provato (§16).

### 11.2 Robustezza (obbligatoria)

Tracciabilità di ogni riga di §11.2 e §11.2b verso i test che la provano: `docs/specs/project-memory.work/robustness.md`. Dopo T13, su 39 righe: 38 coperte, 1 coperta in parte (della riga sulla fiducia, "Connecto non tocca la configurazione della CLI"), 0 mancanti, 0 fallite; nessun test aggiunto ha trovato un difetto. Comprende uccisioni reali del processo dentro una scrittura aperta.

| Area | Cosa si prova |
| - | - |
| Crash e atomicità | Arresto del processo in ogni punto della pubblicazione: alla riapertura la memoria è alla versione vecchia o a quella nuova, mai a metà. |
| Idempotenza | Stesso evento consegnato due volte: una sola riga. Stesso batch ritentato: nessuna postilla doppia. |
| Riavvio | Coda, batch in corso e staging sopravvivono o sono scartati secondo le regole; nessun evento perso. |
| Pause | Nessuna run parte in pausa; una pausa durante una run impedisce la pubblicazione; alla ripresa il lavoro riparte. |
| Stop Everything | Le run in corso sono annullate; la coda resta. |
| Cutoff | Postille create durante un sogno restano aperte e intatte dopo la pubblicazione. |
| Concorrenza | `propose` mentre un sogno pubblica; due progetti aperti con run contemporanee. |
| Progetti | Una run non legge né scrive righe di un altro progetto; un id estraneo nelle operazioni è rifiutato. |
| Rami | Ramo cancellato, schiacciato, ribasato: il ricordo resta, lo scope è annotato, nessuna promozione o archiviazione automatica. |
| Contraddizioni | La postilla che contraddice compare accanto al ricordo in `brief`, `search`, `read`, `room` e nella UI. |
| Cicli di evidenze | Un agente legge X e lo ripete: X non diventa `verificato`. Output di `connecto memory` non rientra come evento. |
| Risposte malformate | JSON rotto, operazioni ignote, id inventati, fonti inventate, risposta vuota, risposta enorme, risposta `Dropped`. |
| Limiti | Batch oltre il tetto diviso senza perdite; evento oltre il tetto passato per parti; `propose` troppo lungo rifiutato; limite di run per ora rispettato. |
| Segreti | Chiavi e token seminati in prompt, risposte e file del progetto non compaiono nelle superfici controllate dall'host: archivio, pacchetti consegnati alle run, output dei comandi `connecto memory`, esportazioni, vista della UI. Le letture fatte dalla sessione con i propri strumenti e il log grezzo della CLI sono fuori da questa prova (§5.6). |
| Fiducia | CLI mai autorizzata nella cartella: la run è sospesa con il motivo tipizzato, non consuma tentativi, non gira in ciclo, Connecto non risponde alla domanda né tocca la configurazione della CLI; dopo la fiducia il lavoro riprende. |
| Scheduler | Cambio d'ora nei due versi, riavvio a ridosso dell'orario, tre giorni saltati: sempre una run per data, una sola run di recupero. |
| Osservabilità | Nessun evento "test passati" o "decisione" esiste senza una risposta o una proposta che lo contenga. |

### 11.2b Bootstrap e conversazioni senza codice (obbligatori)

Anche queste righe sono tracciate in `robustness.md`, con i conteggi di §11.2.

- Progetto esistente: documentazione, file e conversazioni producono ricordi con fonti e brief iniziale; test soltanto documentati non diventano verificati.
- Fonti mancanti, progetto senza git, cronologia assente, file esclusi, segreti, input oltre i limiti: copertura dichiarata senza dati inventati.
- Crash/ripresa, annullamento, doppio avvio, nuovo bootstrap su palazzo popolato, fonti cambiate e postille arrivate durante la run: niente perdita/duplicazione o pubblicazione incoerente.
- Sessione di planning senza diff: consulta la memoria e produce una postilla; proposta non approvata non diventa decisione.
- Investigazione senza modifiche: salva evidenze e ipotesi distinguendole; un agente successivo le recupera.
- Review o chat senza novità: nessun obbligo di generare rumore; nessuna dipendenza da commit o strumenti per attivare l'elaborazione.

### 11.3 Benchmark

Un harness ripetibile, fuori dal cancello ordinario (usa CLI reali, lanciato a mano):

- Un insieme fisso di compiti su un repository di prova, eseguiti **con** e **senza** memoria, stesso modello e stesse istruzioni.
- Misure: esito del compito, numero di turni, caratteri in ingresso e in uscita, durata; e **il lavoro dei due cicli sommato** al totale del lato "con memoria".
- Qualità del recupero: per domande con risposta nota, se `search` e `brief` riportano il ricordo giusto nei primi risultati.
- Ripetizione degli errori: compiti con una trappola già registrata in `problemi`; si conta quante volte l'agente ci ricade.
- Il risultato è una tabella in `docs/specs/project-memory.work/benchmark.md`. Nessuna soglia di miglioramento è promessa in anticipo: il benchmark serve a decidere i default.

**Eseguito il 2026-11-14** (18:13-18:46 e, dopo la correzione del prompt del sogno, 18:57-19:01), risultati in `docs/specs/project-memory.work/benchmark.md`. Harness: `crates/connecto-app/src/memory_bench.rs`, test `#[ignore]` lanciati a mano sul percorso reale del prodotto (motore, registro, bridge, hook, PTY). Ogni numero viene da una o poche run: **nulla è statisticamente significativo** e nessun risparmio è affermato. Riassunto di ciò che il file mostra:

- **Compiti.** Cinque compiti (planning, investigazione, implementazione, trappola, compito che dipende da una decisione precedente), una sessione nuova ciascuno, Claude `haiku`, **n = 1 per compito**. Con memoria e senza: **5/5 risolti in entrambi i casi**. Token delle sole sessioni di compito: circa 1,00 M contro 0,94 M in ingresso. Contando il lavoro dei due cicli (7 run di sviluppo e 1 sogno, misurati prima della correzione del prompt), il lato con memoria ha usato circa 1,20 M token in ingresso e 36 K in uscita, contro 0,94 M e 6 K: **più token, stesso esito**. Il costo per run dei cicli è dominato dall'overhead fisso di ogni sessione CLI (circa 22 K token in ingresso per un prompt di sviluppo di circa 6 K caratteri). Il lato senza memoria non era privo di ricordo tra sessioni: per T5 l'agente ha letto le trascrizioni di Claude stesso.
- **Difetto trovato dalle run reali e corretto.** Lo schema del prompt del sogno non mostrava che `scope` è una lista: ogni `create` con `"scope":"project"` era rifiutata (25 di 31 operazioni rifiutate per il primo sogno Haiku, 28 di 37 per Codex). Correzione: ogni campo mostra la sua forma, i campi lista come liste, l'esempio porta `"scope":["project"]`, una stringa di scope singola valida è letta come lista di un elemento e registrata come aggiustata, i rifiuti nominano campo e forma attesa (§5.7).
- **Sogni dopo la correzione**, stesso stato (versione 4, 5 postille aperte), una sessione ciascuno: Claude haiku/low 31,3 s, 4 applicate, 1 rifiutata; Codex default/low 30,4 s, 12 applicate, 0 rifiutate; Claude sonnet/medium (il default) 12,5 s, 3 applicate, 0 rifiutate; nessuna aggiustata. Non confrontabile con il primo sogno (5 postille contro circa 23), e lo stato più pesante non è stato rieseguito.
- **Bootstrap dopo la correzione** (progetto B): una run, 35,3 s, 12 operazioni applicate, 0 rifiutate; **0 evidenze `dichiarazione_utente`**: i turni delle conversazioni passate sono diventati solo `affermazione_agente`.
- **Recupero** alle quantità di default, 8 domande con risposta nota: hit@1 5/8, hit@3 6/8, presente nel brief 7/8.
- **Trappola.** Esecuzioni fallite di `build.py` per sessione: 0/3 con la trappola registrata in `problemi`, 1 per sessione (3/3) senza memoria. n = 3 per lato, e il confronto è "memoria con la trappola registrata" contro "nessuna memoria", non contro "memoria senza la trappola".
- **Tempi** contro le scadenze di default: sviluppo 7,8-19,1 s (mediana 12,5) contro 120 s; sogno 12,5-90,1 s per sessione contro 600 s; bootstrap 37,6 s contro 15 min. Nessuna scadenza né tetto di ingresso si è avvicinato al limite.
- **Tick reale di 30 s osservato una volta**: un sogno programmato alle 18:42 è partito 14,9 s dopo, con una sola run per la data.
- **Percorso end-to-end osservato una volta**: cattura → 7 run di sviluppo da sole → lettura di una postilla da un altro terminale (uscita 0) → sogno pubblicato → `[connecto notice] project memory updated to version 2` al prompt seguente di un terminale aperto, nessun terminale svegliato; la coda sopravvive al riavvio.
- **Prompt lungo**: 75.464 caratteri, hook tagliato, nessun testo salvato, nessuna nota scritta.
- **Non misurati**: più di una run per compito; Codex come agente dei compiti; un vero `Reply::Dropped`; le cause di taint per evento; cambi d'ora, recupero di più giorni o riavvio vicino all'orario con CLI reali; altre finestre di raggruppamento; costo in valuta; Windows. Il sogno `sonnet`/`medium` è stato misurato solo dopo la correzione, una volta.

Nessun documento afferma un risparmio di token o di turni: il costo aggiunto della memoria si dichiara solo come misurato per run (caratteri in ingresso e in uscita, durata), mostrato nella UI e riassunto qui.

---

## 12. Criteri di accettazione

Stato alla scrittura: il criterio 16 è soddisfatto (§11.3); il criterio 15 va verificato sul ramo finito (questo aggiornamento non l'ha eseguito); per il criterio 14, Codex ha girato con le CLI reali nel benchmark per entrambi i cicli, una run ciascuno. Il criterio 8 vale nella forma più stretta di §5.7.

1. La memoria è spenta di default; attivarla per un progetto non tocca gli altri né il repository.
2. Nessun file viene scritto nel repository; `AGENTS.md` e `CLAUDE.md` non sono mai modificati.
3. Un agente in un terminale ottiene `brief`, `search`, `read`, `room`, `status` e può creare postille con `propose` e `flag`; l'output rispetta il budget senza tagli a metà.
4. Una postilla esplicita è leggibile da un altro terminale entro pochi secondi dalla creazione.
5. Dopo un turno concluso, il ciclo di sviluppo considera gli eventi entro la finestra configurata più il tempo di run, senza bloccare alcun terminale; può non produrre note.
6. `status` e la UI mostrano freschezza e arretrati, distinti dallo stato dei ricordi.
7. Una contraddizione aperta è visibile accanto al ricordo in ogni vista.
8. Ogni ricordo e ogni postilla ha almeno una fonte; nessun ricordo diventa `verificato` per sola ripetizione di un agente che lo aveva letto.
9. Il sogno parte all'orario scelto con l'app aperta; saltato, viene recuperato una volta alla riapertura; non si duplica.
10. Il sogno pubblica in modo atomico; "annulla versione" riporta lo stato precedente; le postille arrivate dopo il cutoff restano.
11. Le run di memoria non hanno bridge e partono in sola lettura per quanto ogni CLI lo consente: lo attestano test sugli argomenti di lancio, per ciascun adapter.
12. Pause impedisce nuove run e nuove pubblicazioni; Stop Everything annulla le run in corso.
13. Le sessioni aperte ricevono l'avviso di nuova versione al prompt seguente, senza essere svegliate.
14. I due cicli si configurano separatamente per provider, modello e ragionamento; Claude e Codex funzionano entrambi; un provider non disponibile (CLI non installata, accesso non fatto) è indicato come tale, con il motivo.
15. La suite §11.2 passa; il cancello ordinario (`fmt`, `clippy -D warnings`, `cargo test --workspace --all-features`) passa.
16. Il benchmark §11.3 è stato eseguito e il risultato è scritto. **Fatto** (2026-11-14, `benchmark.md`): n = 1 per compito, non significativo statisticamente; stesso esito con e senza memoria, più token con memoria contando i cicli.
17. L'esportazione produce JSON e Markdown completi.
18. Un progetto esistente può costruire il palazzo iniziale con copertura esplicita, fonti, ripresa e preservazione degli eventi concorrenti (§5.9).
19. Planning, investigazione e review senza diff consultano e alimentano la memoria; proposte, decisioni e implementazioni restano distinte (§5.10).

---

## 13. Differenze sostanziali dai requisiti e decisioni prese

**Decisioni dell'utente del 2026-11-14:**

- **Nessun requisito "senza strumenti".** Una run di memoria può leggere i file del progetto; la memoria la protegge il validatore dell'host (§5.6, §5.7).
- **Sessioni utility indipendenti dal provider.** Tutto il lavoro LLM che Connecto fa per sé passa da sessioni utility: normali sessioni della CLI dell'agente, con uno scopo preciso, nella cartella del progetto, che possono leggere file (D4, ADR 0025).
- **Ogni provider per ogni funzione.** Ogni provider che Connecto sa far girare (oggi Claude e Codex) è disponibile per ogni funzione utility, titoli dei terminali compresi (§8.2).

**DS1. Provider Codex per i due cicli. Risolta (2026-11-14)** dalle decisioni sopra. Codex è un provider funzionante in questa versione, con lancio in sola lettura al meglio (§5.6). Esito di S2, per memoria: Codex non può partire con zero strumenti (§9.1).

**DS2. "Risultati test" e "modifiche repo" hanno un'osservabilità più stretta di quanto i requisiti lascino intendere.** Senza hook sugli strumenti, le modifiche si vedono solo come differenza git a fine turno, e i test solo se l'agente li riporta o li propone. Implementata come da piano: nessun hook `PostToolUse` in questa versione. Le modifiche sono `osservazione_codice` dalla fotografia git a fine turno; i test sono `esito_riportato`, non fatti verificati, e un `esito_riportato` sostiene `eseguita` solo da una sessione indipendente (§4.5, §4.8). L'hook resta fra i lavori futuri (§1.2).

**DS3. Il brief non è iniettato all'avvio: l'agente lo chiede.** I requisiti dicono "breve orientamento all'avvio". Implementata come da piano: il testo di avvio contiene l'istruzione fissa di eseguire `connecto memory brief` prima di ogni lavoro, e i comandi, mai il contenuto (D6). Motivo: la regola del bridge per cui i testi generati da Connecto non portano contenuto arbitrario, e il fatto che un contenuto nel prompt di sistema avrebbe l'autorità di un'istruzione. Conseguenza: un agente che ignora l'istruzione parte senza orientamento. L'iniezione richiederebbe una deroga esplicita a quella regola.

**DS4. I prompt di terminale non sono dichiarazioni dell'utente. Decisa durante l'implementazione, segnalata all'utente e confermata dall'utente il 2026-11-15** ("ok keep terminal credit off, go ahead"). Il piano (§4.5) diceva che un prompt scritto dall'utente conta come `dichiarazione_utente`. La review ha trovato che le CLI possono emettere da sole l'hook del prompt, quindi l'input di un terminale non prova che una persona abbia scritto (§5.3). Nel codice: `TerminalCredit::Off` per default, ed è l'unica modalità collegata; `Quarantine` resta come modalità documentata non di default, con l'ipotesi che richiede e un test che ne mostra il limite. Le dichiarazioni dell'utente passano dalla finestra (§5.11). Conseguenza: quello che l'utente scrive in un terminale arriva come evidenza non attribuita e può diventare una proposta; per approvare serve un clic nella scheda **Memories**. Il bootstrap segue la stessa regola: un turno di una conversazione passata non è mai una dichiarazione dell'utente (§5.9).

**DS5. Regola contro l'autoconferma più stretta nel sogno e nel bootstrap.** Implementata così, ed è più stretta della formulazione originale di §4.5: per `verificato` o una posizione più forte, un'evidenza di agente conta solo se la sua sessione non aveva visto **nulla** dalla memoria, non solo nulla di quel ricordo; e le fonti proprie di un ricordo, o delle postille che ha assorbito, non lo confermano (§5.7). Effetto: meno affermazioni raggiungono `verificato` per via di agenti; nessuna per ripetizione. Segnalata all'utente il 2026-11-15 perché si discosta dal testo del piano; nessuna obiezione, ma non è un'approvazione esplicita.

---

## 14. Correzioni minori incluse

Tutte realizzate:

- "Ingresso" è una vista generata, non una stanza che contiene ricordi; `room ingresso` stampa il brief.
- L'integrazione con Stop Everything è costruita: annulla le run di memoria in corso (§7).
- Lo scheduler è un componente nuovo, non un'estensione delle automazioni (§7).
- Il runner utility è generico tra adapter (Claude e Codex); il comportamento dei titoli non è cambiato, salvo che ora possono girare anche su Codex (§8.2).
- SQLite è una dipendenza nuova del solo `connecto-memory` (ADR 0024); la build Windows non è verificata.
- Le run di memoria lavorano nella cartella del progetto, in sola lettura per quanto la CLI lo consente; la memoria la protegge il validatore dell'host.
- Evidenze troppo grandi: divisione in più run, mai riassunto o taglio da parte del programma.
- Token nei budget: stima dichiarata come tale.
- La regola contro l'autoconferma è meccanica (registro `served` + validatore), non affidata al modello; nel sogno e nel bootstrap è più stretta di quanto il piano chiedesse (§5.7).
- L'avviso di versione usa la casella del bridge al prompt successivo: nessun risveglio.
- Limiti economici sostituiti da limiti su run, caratteri e durata.
- Uno scheduler con chiave `(programma, data locale)` evita i duplicati da cambio d'ora.
- I valori iniziali (20 run/ora, tetti di caratteri) sono stati confermati dallo spike S1; la finestra di raggruppamento, inizialmente 120 s, è stata portata a 3600 s dopo il benchmark (decisione dell'utente, 2026-11-15).

Emerse durante l'implementazione:

- Un `settings.json` corrotto o illeggibile non viene più sovrascritto al salvataggio (§8.1).
- La posizione (§4.8) e il tipo di evidenza `documento_progetto` (§4.5) si sono aggiunti al modello dei dati.
- Revert e sogno abbassano le affermazioni non più sostenute invece di rifiutare (§5.5).

## 15. Decisioni aperte minori

Hanno un default nel codice; cambiarle non sposta l'architettura.

- **Worktree aperti come progetti separati** hanno memorie separate, perché l'id progetto è l'hash del percorso. Default: così. Un collegamento "usa la memoria del progetto X" è lavoro futuro. Nota: `/dev-team` lavora in worktree ma dai terminali del progetto principale, quindi ricade nella memoria del progetto principale.
- **Modello di default del sogno**: `sonnet`, ragionamento medio. Lo conferma, o lo cambia, il benchmark.

Chiuse:

- **Sessione nuova per ogni batch** del ciclo di sviluppo: deciso da S1, sessione nuova; vale anche per ogni parte del sogno e per ogni pacchetto del bootstrap.
- **Chi può annullare una versione**: solo l'utente dalla UI; nessun comando per gli agenti.

## 16. Limiti noti

Dichiarati, non difetti nascosti. La documentazione per l'utente li ripete. Le misure (caratteri, durata, token, esiti) stanno nel benchmark (§11.3), non qui.

- **Nulla gira ad app chiusa.** Né i cicli né il bootstrap; il bootstrap riprende al riavvio, un sogno saltato si recupera una volta (al più 31 giorni indietro).
- **Nessun prompt di terminale né turno di una conversazione passata è una dichiarazione dell'utente** (§5.3, §5.9, DS4, confermata dall'utente il 2026-11-15). Approvare, rifiutare e correggere si fa dalla scheda **Memories** (§5.11).
- **Windows:**
  - la cancellazione della memoria è rifiutata: la memoria è spenta e l'utente riceve il percorso della cartella da togliere a mano;
  - il bootstrap non legge il contenuto dei file (solo nomi, stato git e storia, lacuna `FilesNotRead`), perché lì un collegamento non si può rifiutare in modo sicuro; `.gitmodules` non è letto;
  - le cartelle git private lasciate da un crash non sono tolte all'avvio successivo;
  - `connecto-memory` e `connecto-app` non sono mai stati compilati per Windows, né il benchmark vi è stato eseguito.
- **Repository per cui git non è usato** (si ripiega sulla visita delle cartelle, lacuna `GitNotUsable`): una chiave di configurazione fuori dalla lista ammessa (LFS e ogni filtro, hook, `include`, sottomoduli inizializzati, partial clone, …) o una grafia non accettata; una cartella `.git` che non si visita entro circa 300.000 voci o 5 s; un indice oltre 256 MiB o in un formato non riconosciuto; `alternates`, `commondir`, `reftable`; un comando che fallisce. Senza git il bootstrap non ha storia né modifiche locali.
- **Configurazione git di sistema non letta**; della globale solo le chiavi semantiche, con le grafie accettate e senza include (lacuna `GlobalGitSettings`): git può confrontare i file in modo diverso da quello dell'utente. Se LFS è configurato solo a livello globale, i file LFS si leggono come puntatori.
- **Prompt tagliati.** Un prompt oltre 64 KiB arriva tagliato dall'hook: la memoria lo salva come "contenuto non disponibile", senza testo né evidenza (osservato una volta nel benchmark).
- **Conversazioni senza log leggibile**: la trascrizione dello schermo vale solo come affermazioni di agente (`RolesUnknown`).
- **Esclusioni e testo libero.** Le esclusioni del bootstrap valgono per il contenuto dei file; un testo libero (documentazione, messaggi, conversazioni) può nominare un percorso escluso.
- **Solo i segreti personali della cassaforte sono usati per la redazione** (ADR 0027): le chiavi interne di Connecto no; per il resto, le variabili d'ambiente dall'aspetto segreto e le regole a pattern.
- **Ciò che una sessione legge da sé è fuori dalla redazione** e dalle esclusioni: va grezzo al modello e nel log della CLI (§5.6, ADR 0025).
- **Le trascrizioni delle run di memoria** restano nelle cartelle di sessione del profilo della CLI; cancellazione ed esportazione non le toccano, e la conferma della cancellazione lo dice.
- **Un "Sogna ora" sospeso per fiducia** non è ritentato da solo.
- **Osservati una sola volta con le CLI reali** (§11.3): il tick reale di 30 s dello scheduler; il percorso completo cattura → ciclo di sviluppo → lettura da un altro terminale → sogno → avviso; Codex per i due cicli; il sogno `sonnet`/`medium`.
- **Non verificati:** la build Windows; un vero `Reply::Dropped` (oltre 256 KiB); cambi d'ora, recupero di più giorni e riavvio vicino all'orario con CLI reali (provati solo con l'orologio finto); Codex come agente dei compiti; più di una run per compito.
