# Instradamento per argomento: un terminale nuovo per un lavoro nuovo

Stato: **rimandata, non implementata** (decisione del 2026-11-22, §12). Scritta il 2026-11-21 su `main` a `3132d05`. Nasce dall'analisi dei costi di cache della stessa data (§2). Gli strumenti della valutazione offline sono in `scripts/topic-routing-eval/`.

## 1. Cosa si costruisce

Quando l'utente invia un messaggio a un terminale Claude con un contesto già grande, Connecto chiede a Jev se il messaggio continua il lavoro in corso o ne apre uno nuovo. Se è nuovo, il messaggio non entra nella conversazione: Connecto apre un terminale identico (stesso strumento, modello, effort, modalità, cartella) e lo invia lì come primo messaggio. Il terminale vecchio resta aperto e fermo.

Dopo una pausa più lunga della durata della cache (un'ora) la cache è già fredda e riprendere costa come ripartire. La regola allora si inverte: si resta solo se Jev è sicuro che il messaggio continua il lavoro.

Lo scopo è uno solo: **ridurre i token riletti dalla cache a ogni chiamata**, che oggi sono la voce di costo principale.

### 1.1 Dentro

- Terminali Claude, messaggi scritti dall'utente: tastiera, incolla, Connecto Chat dal telefono. Conta ogni prompt che arriva al terminale e non è una consegna di Connecto (§5.2).
- Decisione con Jev, soglie configurabili, spostamento con annullamento.
- Impostazione per progetto, spenta per default.
- Registro di ogni decisione nell'audit, con i numeri che servono a misurare il risparmio.

### 1.2 Fuori

- Codex, Pi, Antigravity: economia della cache e hook diversi (§11, Q1).
- Caricare o scaricare skill e server MCP per messaggio. Le definizioni degli strumenti stanno in testa al prefisso in cache: cambiarle a metà sessione invalida tutta la cache e la riscrive a 2×. In più Claude Code le legge solo all'avvio. Se un giorno servirà, si sceglierà il profilo **all'apertura** del terminale nuovo, non a ogni messaggio.
- `/clear` o `/compact` automatici: il terminale nuovo li sostituisce e lascia la cronologia leggibile.
- Avviare sessioni nuove con un timer: un terminale fermo non costa nulla, il costo arriva solo col messaggio successivo, ed è lì che si decide.

## 2. Perché: i numeri

Sessioni Claude di un progetto di sviluppo in due settimane di lavoro tipico (circa un centinaio di sessioni e qualche migliaio di chiamate, script in §9.1). Valori arrotondati:

| Voce | Quota dei token | Peso sul costo* |
|---|---|---|
| Letture dalla cache | ~98% dell'input | ~60% |
| Scritture in cache (tutte a 1 ora) | ~1,5% dell'input | ~20% |
| Uscita | — | ~20% |
| Input non in cache | quasi nulla dell'input | ~0% |

\* Rapporti standard: lettura 0,1×, scrittura con durata 1 ora 2×, uscita 5× rispetto all'input base.

- Circa il 98% dell'input è già letto dalla cache: aumentare i successi non porta quasi nulla.
- La chiamata tipica rilegge **circa 150k token**, una chiamata su dieci più di 250k. Le chiamate sopra 200k sono circa un terzo del totale ma circa **metà delle letture**.
- Una sessione nuova parte da circa 41-46k token.
- Le ricostruzioni dopo più di un'ora ferma sono poche e pesano qualche punto percentuale del costo.

Un messaggio spostato fa risparmiare circa (contesto attuale − 45k) × 0,1 **a ogni chiamata successiva** di quel lavoro. Con il contesto tipico sono ~11k token equivalenti per chiamata. Il costo dello spostamento è una partenza: in media ~8,5k token scritti, perché il prefisso comune è già in cache dagli altri terminali.

## 3. Fatti del repository su cui poggia

- I terminali Claude girano con l'hook `UserPromptSubmit` di Connecto (`crates/connecto-adapter/src/claude.rs`, `hook_entry`), che inoltra il payload al bridge e poi stampa l'inbox (`crates/connecto-hook/src/bin/connecto-hook.rs`, `hook`). Il payload di Claude contiene `prompt`, `session_id` e `transcript_path`.
- Un hook `UserPromptSubmit` può rifiutare il prompt stampando `{"decision":"block","reason":"..."}`: il prompt non entra nella conversazione e l'utente vede la ragione. **Da verificare sulla versione installata** (spike §9.2).
- Il lancio sa già dare a una conversazione nuova il suo primo messaggio (`LaunchCtx::initial_prompt`, passato dopo `--`) e copiare modello ed effort (`--model`, `--effort`). `connecto add` usa lo stesso percorso.
- Jev esiste già: client bloccante `crates/connecto-automation/src/jev.rs`, domande `choice` con `confidence`, chiave per utente nel portachiavi. Il modo di usarlo è fissato da ARCHITECTURE §6: Jev sceglie una strada e non scrive mai testo.
- REQUIREMENTS RTE-01/RTE-02 descrivono già l'instradamento "discussione del lavoro fatto / lavoro nuovo" e impongono che un messaggio non venga mai perso in silenzio.

## 4. La decisione

### 4.1 Quando si chiede a Jev (porta)

Si chiede solo se valgono tutte queste condizioni, altrimenti il messaggio passa senza chiamare Jev:

1. L'impostazione del progetto è accesa e la chiave Jev è configurata.
2. Il terminale è Claude, non è una sessione utility e non è in pausa globale.
3. Il prompt è dell'utente (§5.2) e non comincia con `/` o `!`.
4. **Contesto ≥ 80k token** (`min_context`). È la somma di `input_tokens + cache_read_input_tokens + cache_creation_input_tokens` dell'ultima risposta dell'assistente nel transcript. Sotto questa soglia ripartire non fa risparmiare abbastanza da giustificare la chiamata.
5. Il terminale non coordina lavoro in corso: nessun `--ask` aperto da lui o verso di lui, nessun terminale suo ancora vivo (`belongs to`). Le sessioni di un team restano dove sono.

### 4.2 Cosa riceve Jev

- `state`: le **richieste dell'utente** della conversazione, dalla più vecchia, ciascuna tagliata a 500 caratteri, in tutto al massimo 8.000 caratteri (si tengono le più recenti). Restano fuori i risultati degli strumenti, le risposte dell'assistente, le consegne di Connecto, il testo dell'hook. Tutto passa da `connecto_redact` prima di uscire; se la redazione fallisce non si chiama Jev e il messaggio passa.
- Una domanda `choice`, `topic`, con le opzioni `continues` (il messaggio riguarda, corregge, estende o verifica il lavoro già chiesto) e `new` (un compito indipendente che si capisce senza la conversazione). Il nuovo messaggio va nelle istruzioni della domanda.

### 4.3 La regola

Si guarda il tempo dall'ultima risposta dell'assistente nel transcript:

| Cache | Condizione | Si sposta se |
|---|---|---|
| Calda | pausa < 60 min (`cold_after`) | `choice = new` e `confidence ≥ 0.75` (`move_at`) |
| Fredda | pausa ≥ 60 min | **non** (`choice = continues` e `confidence ≥ 0.75` (`stay_at`)) |

Con la cache calda l'errore costoso è spezzare un lavoro: l'agente nuovo non sa cosa vuol dire "fai lo stesso per X". Per questo si sposta solo con buona sicurezza. Con la cache fredda restare costa una riscrittura completa a 2×, quindi si resta solo con buona sicurezza.

### 4.4 Errori e tempi

- Timeout della chiamata: **3 s** (non i 30 s del client delle automazioni). Errore, timeout, 401/403/429, risposta malformata: il messaggio **passa nel terminale corrente**, come se la funzione fosse spenta, e l'audit lo registra. Non si perde e non si blocca mai un messaggio per colpa di Jev.
- Pausa globale: niente chiamate a Jev (MON-02), il messaggio passa.
- Il ritardo aggiunto riguarda solo i messaggi oltre la porta (§4.1).

## 5. Il flusso

### 5.1 Passi

1. L'utente invia. Claude esegue l'hook `UserPromptSubmit` di Connecto.
2. Il bridge controlla la porta (§4.1). Se il messaggio non passa la porta, l'hook fa quello che fa oggi.
3. Il bridge legge dal transcript le richieste precedenti, il contesto e la pausa, poi chiama Jev su un thread di rete, mai sul thread della UI o della PTY (ARCHITECTURE §5).
4. **Resta**: l'hook fa quello che fa oggi (inoltro e inbox).
5. **Si sposta**:
   - Il bridge apre un terminale con la stessa configurazione: strumento, `--model` ed `--effort` effettivi, modalità permessi, cartella o worktree, progetto, nome `<nome> (2)`. Come `initial_prompt` riceve il testo originale **non redatto** (la redazione vale solo per ciò che va a Jev).
   - L'hook stampa `{"decision":"block","reason":"Moved to claude-39: this looks like a new task. Use 'Send here instead' to bring it back."}`, senza inbox: le notizie restano in coda per il prossimo prompt del terminale vecchio.
   - La UI seleziona il terminale nuovo e mostra sul vecchio una barra con **Send here instead**.
6. **Send here instead**: interrompe e chiude il terminale nuovo, poi scrive il messaggio nel terminale vecchio con un lasciapassare monouso, così la porta non lo valuta di nuovo. Il lavoro già fatto dal terminale nuovo va perso, ed è accettato.

### 5.2 Cosa non è un messaggio dell'utente

Il bridge scrive lui stesso nelle PTY risvegli, richieste `--ask` e risposte. Ogni consegna in volo è registrata nello stato del bridge per terminale, e il bridge la riconosce quando l'hook la riporta: non passa mai dalla porta. Come seconda difesa, un prompt che comincia con `[connecto` non passa dalla porta. Anche il primo messaggio di un terminale appena spostato non ci passa, e comunque il suo contesto è sotto soglia.

### 5.3 Il terminale vecchio

Resta aperto e fermo: la cronologia serve. Non si chiude da solo. Un terminale fermo non costa token.

## 6. Impostazioni (per progetto, `settings.json`, chiave `topic_routing`)

```json
{
  "topic_routing": {
    "enabled": false,
    "min_context": 80000,
    "cold_after_minutes": 60,
    "move_at": 0.75,
    "stay_at": 0.75,
    "timeout_ms": 3000
  }
}
```

Nella UI: Project settings, sezione **TOPIC ROUTING**, con la spunta e la soglia di contesto. Gli altri valori restano nel file.

## 7. Audit e misura

Per ogni messaggio oltre la porta si registra: terminale, contesto, pausa, risposta di Jev (scelta, confidenza, token), decisione, durata, eventuale terminale nuovo, eventuale **Send here instead**. Il testo non si registra.

La pagina del progetto mostra: messaggi spostati, annullati, token di contesto evitati (somma del contesto al momento dello spostamento meno il contesto di partenza del terminale nuovo), errori di Jev.

## 8. Task

| # | Task | Dove |
|---|---|---|
| T1 | Spike: verificare che `decision: block` scarta il prompt e mostra la ragione sulla versione di Claude Code installata; misurare i tempi di Jev su 20 domande reali | §9.2 |
| T2 | Lettore del transcript: richieste dell'utente pulite, contesto dell'ultima risposta, ora dell'ultima risposta. Legge solo la coda per i numeri e salta le righe illeggibili | `connecto-hook` o `connecto-session` |
| T3 | Op del bridge `route_prompt`: porta, chiamata a Jev su thread di rete, regola, risposta `{stay}` o `{moved: id}` | `connecto-session/src/bridge` |
| T4 | Riconoscimento delle consegne del bridge (§5.2) e lasciapassare monouso | `connecto-session/src/bridge/state.rs` |
| T5 | Hook: chiamare `route_prompt` prima dell'inoltro; su spostamento stampare `block` e saltare l'inbox | `connecto-hook/src/bin/connecto-hook.rs` |
| T6 | Clonazione del terminale con `initial_prompt` | `connecto-app` / `connecto-workspace` |
| T7 | UI: selezione del terminale nuovo, barra **Send here instead**, impostazioni del progetto | `connecto-ui` |
| T8 | Audit e contatori (§7) | `connecto-audit` |
| T9 | Valutazione offline (§9.1) prima di accendere la funzione per default | `scripts/` |

## 9. Test

### 9.1 Valutazione offline (cancello)

- Estrarre dai transcript degli ultimi 14 giorni i messaggi dell'utente arrivati con contesto ≥ 80k, ciascuno con le richieste precedenti (~100-200 casi).
- Etichettarli a mano: `continues` o `new`.
- Passarli a Jev e misurare, alle soglie proposte:
  - **precisione di `new` ≥ 95%**: uno spostamento sbagliato rompe il lavoro dell'utente;
  - richiamo di `new`, che dà il risparmio;
  - risparmio stimato: per ogni caso spostato, (contesto − 45k) × chiamate successive della sessione × 0,1.
- Se la precisione non arriva al 95% a 0,75, alzare `move_at` finché ci arriva. Se non ci arriva mai, la funzione non si accende.

### 9.2 Automatici

- Porta: ogni condizione di §4.1 da sola esclude la chiamata (Jev finto non chiamato).
- Regola: le quattro combinazioni calda/fredda × `continues`/`new`, più i casi limite a 0,75.
- Errori di Jev (timeout, 401, 403, 429, 500, malformato) → il messaggio passa nel terminale corrente.
- Consegne del bridge, prompt `[connecto…`, `/`, `!` → mai valutati.
- Spostamento: il terminale nuovo ha modello, effort, modalità e cartella del vecchio e riceve il testo esatto, non redatto. L'hook stampa `block` e non svuota l'inbox.
- **Send here instead**: il terminale nuovo viene chiuso, il messaggio arriva al vecchio una volta sola e non viene rivalutato.
- Pausa: nessuna chiamata, il messaggio passa.
- Redazione: un segreto finto nel prompt non compare nella richiesta a Jev.

### 9.3 Manuali

Una sessione reale oltre 80k: un messaggio che continua resta, uno indipendente si sposta, l'annullamento lo riporta indietro. Lo stesso dopo più di un'ora di pausa.

## 10. Criteri di accettazione

1. Nessun messaggio dell'utente va perso o arriva due volte, in nessun ramo d'errore.
2. Con la funzione spenta, o sotto soglia, il comportamento e il testo dell'hook sono identici a oggi.
3. La valutazione offline rispetta §9.1.
4. Dopo due settimane accesa, le letture dalla cache per chiamata calano in modo misurabile rispetto ai numeri di §2, e gli annullamenti restano sotto il 10% degli spostamenti.

## 11. Domande aperte

- **Q1.** Codex: ha un hook che può scartare il prompt? Il suo costo di cache è simile? Da valutare dopo la v1.
- **Q2.** Il nome del terminale nuovo: `<nome> (2)` o un titolo generato dal messaggio (i titoli esistono già come sessione utility)?
- **Q3.** Se il terminale vecchio, a cache fredda, viene spostato più volte di fila, conviene proporre all'utente di chiuderlo?
- **Q4.** Soglia di contesto: 80k è una stima. Si può ricavare dalla valutazione offline il punto in cui il risparmio supera il costo della chiamata a Jev e il rischio di errore.

## 12. Valutazione del 2026-11-22 e decisione

Fatta di notte, con l'utente che aveva approvato il lavoro tramite il coordinatore (claude-34), prima di costruire qualsiasi parte del runtime. Rivista da un revisore Codex, che ha corretto metodo e aritmetica della prima stesura.

### 12.1 Dati

- `extract.py` sui transcript di `~/.claude/projects/-Users-<user>-workspace-*/*.jsonl`, con i messaggi filtrati per data propria negli ultimi 30 giorni, trova:
  - **N messaggi scritti dall'utente** arrivati con contesto ≥ 80k (alcune centinaia);
  - le consegne di altri agenti, escluse sia come casi sia come storia (§5.2).
- Campione etichettato: 140 casi estratti con la prima versione del filtro. **65** si ritrovano nella popolazione corretta, abbinati in modo univoco per sessione e primi 800 caratteri del messaggio (uno ambiguo è stato scartato). Da qui in avanti un caso è identificato dall'`uuid` del messaggio nel transcript.
  - Etichette `continues`/`new` date da due agenti di etichettatura indipendenti (uno per metà campione, nessuna sovrapposizione né arbitrato), con la regola "nel dubbio, continues". Sono etichette di un modello, non dell'utente.
  - Il campione sovrarappresenta la cache fredda: 27 casi su 65.
- Risultato: **4 `new` su 65, tutti marcati come incerti**; nessun `new` sicuro. Dei 27 casi freddi, 1 è `new`.

### 12.2 Quanto potrebbe valere

- I 4 casi `new` del campione, contati una sola volta per sessione e con lo spostamento supposto perfetto, valgono questo risparmio: (contesto − 45k) × 0,1 su ogni chiamata successiva della sessione, più la riscrittura fredda evitata quando il messaggio arriva dopo più di un'ora. In tutto **circa il 2% delle unità di costo dei 30 giorni**. È il contributo modellato di quei 4 casi, non un tetto sui 30 giorni.
- Moltiplicato per N/65 (circa 4) si arriva a **meno del 10%**. È solo un'**estrapolazione illustrativa**, non un tetto: suppone che il campione rappresenti sia la frequenza dei messaggi nuovi sia il costo rimasto nelle sessioni, e il costo delle sessioni non campionate può spostarla in entrambe le direzioni. Inoltre suppone:
  - router perfetto;
  - l'argomento nuovo occupa tutto il resto della sessione;
  - casi tutti ambigui, che un router tarato su una precisione del 95% in buona parte non sposterebbe;
  - campione non stratificato come la popolazione.
- Per confronto, sugli stessi dati le chiamate sopra 200k portano circa metà delle letture. Quanto se ne recupera compattando prima dipende da quanto la compattazione toglie e per quanto. La stima è in `docs/specs/token-optimization.md` §7 e il pannello la mostra sui dati dell'utente.

### 12.3 Decisione

- Il runtime di §5 **è rimandato, non costruito**. È uno scostamento dal mandato della notte ("costruirlo spento se la valutazione non passa"), segnalato al coordinatore e all'utente: **la decisione finale spetta all'utente**.
- Motivi:
  - la soglia del 95% non si può verificare con 4 positivi, tutti incerti;
  - il guadagno non si può stimare in modo affidabile con questi dati;
  - Jev non si poteva ancora chiamare: il suo segreto non era disponibile;
  - costruirlo vorrebbe dire spedire, anche se spento, codice non validato che blocca prompt e clona terminali.
- Strumenti pronti per la verifica:
  - `extract.py` (filtro per data del messaggio, glob e parametri scritti nel risultato);
  - `jev_eval.py`: passa ogni testo intero dal redattore di Connecto (`connecto-redact`, esempio `redact_texts`) e solo dopo lo taglia, non invia un caso la cui redazione non riesce, usa il timeout di 3 s del runtime e rifiuta le risposte di tipo sbagliato o con confidenza booleana. Il redattore gira fuori dall'app e non conosce i valori del vault: una prova reale richiede `--reviewed-dataset`, cioè un file di casi rivisto o ripulito dall'utente; ha un `--self-test` con un segreto finto in storia e messaggio, valuta come "resta" le risposte non valide e gli errori, riporta la regola calda/fredda con spostamenti, precisione e richiamo, separatamente per tutte le etichette e per quelle sicure.

  Campione ed etichette contengono testo privato e restano fuori dal repository.
- Prima di riconsiderarlo serve:
  - un'etichettatura con arbitrato (idealmente dell'utente) su un campione stratificato come la popolazione;
  - almeno ~20 `new` sicuri;
  - `jev_eval.py` con una precisione ≥ 95% a 0,75.
