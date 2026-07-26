# REFACTORING — dette structurelle identifiée

Relevé des refactorings restants, établi le 2026-07-25 après six lots déjà
appliqués (voir « Journal » en bas). Chiffres mesurés sur l'arbre à cette date ;
les revérifier avant d'attaquer une tâche, ils bougent à chaque commit.

## Convention

- Identifiant stable (`R1`, `R2`…) : citable dans un commit même après
  réordonnancement.
- `- [ ]` à faire · `- [~]` en cours · `- [x]` fait (déplacer alors la ligne
  dans « Journal », avec le commit).
- **Où** liste des points d'ancrage vérifiés, pas une liste exhaustive.
- Les priorités reflètent le **risque de maintenance**, pas la longueur : un
  fichier long mais linéaire coûte moins qu'une logique dupliquée à deux
  endroits.
- Une tâche qui déplace du code doit rester un déplacement : `just fmt`,
  `just clippy` et `just test` verts, aucun changement de comportement non
  mentionné dans le message de commit.
- Quand une tâche modifie l'architecture, mettre à jour `CLAUDE.md` **et**
  `AGENTS.md`.

---

## Ordre recommandé

Tout est fait. Ce document ne garde que le journal et ce qui a été cherché
sans être trouvé.

---

---

## Cherché et absent

Consigné pour ne pas y revenir :

- **Backends providers** : pas de duplication exploitable entre
  `providers/{graph,gmail,imap}/messages.rs`. Les idiomes diffèrent
  réellement (pagination opaque, labels ⇔ dossiers, RFC 822) et la surface
  commune est déjà `Session`.
- **`settings_view`** : pas de motif « carte de réglages » répété — deux
  occurrences en tout. Les sections sont uniques.
- **Listes virtualisées** : `contacts_view`, `inbox/messages` et
  `calendar_view` mesurent leurs hauteurs différemment par nécessité (une
  ligne type, une par variante, dérivée du viewport). Le tronc commun ferait
  dix lignes.
- **`runtime/protocol.rs`** (1 405 lignes) : de la donnée. `Cmd`/`Evt`
  n'ont rien à factoriser.

---

## Ce qui paierait avant les prochains TODO

- **`TODO.md` P1-5 (règles de filtrage)** — arrivera sur `Evt::NewMessages`
  dans `runtime/mailbox.rs` et passera par `runtime/operations.rs::submit`.
  Rien à refactorer d'abord.

---

## Journal

- **2026-07-25** — Reste de `TODO.md` P1-2 livré, le préalable qui figurait
  ici avec : un lot naît désormais à la soumission (dans `schedule_action`,
  déduit de ses propres commandes) au lieu d'être enregistré à la
  planification puis nettoyé à la main par l'effet d'annulation, et son issue
  compte succès et échecs au lieu d'un booléen.
- **2026-07-25** — `e42cf97` Boucle d'acteur du cache mail réduite à deux
  helpers `answer`/`apply` (`start` 263 → 152 lignes).
- **2026-07-25** — `a8eb7ad` Paires single/bulk fusionnées : `MoveMenu` +
  `MoveScope` pour le menu « déplacer », `MessageState` pour lu/suivi, un seul
  parcours de caches d'en-têtes. Conséquence : une bascule depuis une ligne ou
  un raccourci est désormais *par compte*, comme le lot.
- **2026-07-25** — `4d4c45d` `app.rs` 3 826 → 1 710 lignes, quatre sous-modules
  (`app/{undo,quick_action_state,session,chrome}.rs`).
- **2026-07-25** — `c153abc` `handle_event` redevient une table de routage ;
  `events/{outbox,quick_actions}.rs`.
- **2026-07-25** — `02513f6` Cartes repliables du lecteur rendues depuis un
  seul endroit (`viewer/cards.rs`, `SentCard` + `CardBody`).
- **2026-07-25** — `6042197` Le panneau de réponse devient un `ComposeView`
  sur la surface `Panel` (`ComposeSurface`) : ~1 000 lignes supprimées,
  `PendingCancelEffect::InlineReply` avec. **Non vérifié en exécutant
  l'application** : réponse, envoi avec et sans délai d'annulation, autosave
  de brouillon, détachement en fenêtre, restauration de session d'un panneau
  ouvert.
- **2026-07-25** — `b70267a` (`R1`) `ComposeView::render` 600 → 87 lignes, sept
  méthodes de rendu (`render_header`, `render_banner`, `render_toolbar`,
  `render_ai_panel`, `render_attachment_chips`, `render_body`,
  `render_footer`) ; chacune lit la surface pour laquelle elle rend.
- **2026-07-25** — `510f8ac` (`R2` lot 1) Tests de `blitz_body` sortis vers
  `blitz_body/tests.rs` : 5 906 → 3 271 lignes de code de production, chemins
  de test inchangés.
- **2026-07-25** — `9f60a91` (`R2` lot 2) `blitz_body.rs` 3 271 → 934 lignes
  (l'état partagé), cinq modules par rôle : `element`, `bands`, `events`,
  `paint`, `actor`. Les types et leurs impls restent dans le parent, donc
  aucune visibilité de champ n'a changé ; les fonctions appelées par un frère
  sont `pub(super)` et importées nommément.
- **2026-07-25** — `292e8c2` (`R3`) `viewer/mod.rs` 2 896 → 1 353 lignes, trois modules :
  `attachments_panel.rs` (panneau + récupérations), `translation.rs`
  (traduction éphémère) et `header.rs` (tout ce qui est au-dessus du corps).
  `render_viewer_pane` 670 → 130 lignes : les sections de l'en-tête sont des
  méthodes et `ViewerChrome` porte les réglages et les deux seuils de largeur
  qu'elles partageaient par variables locales.
- **2026-07-25** — `f68c18c` (`R4`) `message_row_inner` 643 → 398 lignes : menu contextuel
  et menu « déplacer » dans `inbox/message_menu.rs` (483 lignes, avec les deux
  tests de hiérarchie de dossiers). Le reste de la ligne n'a **pas** été
  redécoupé : c'est un seul arbre d'éléments linéaire, et le seul point de
  coupe demanderait d'inventer un objet de paramètres pour une ligne dont le
  rendu ne doit pas bouger d'un pixel (clé de variante).
- **2026-07-25** — `da02073` (`R5`) `block_editor.rs` 3 671 → 2 621 lignes, trois modules :
  `proofreading.rs` (584), `history.rs` (319), `tables.rs` (229). Les types que
  le parent porte en champs (`Snapshot`) restent visibles par `pub(super)` ; les
  méthodes déplacées le sont aussi, les modules frères les appelant déjà.
- **2026-07-25** — `f26741f` (`R6`, `AviaryApp`) 80 → 61 champs : `Scrolls`/`ScrollPane`
  (poignée + mouvement de molette appariés, la réinitialisation de session
  passe de dix lignes à une), `QuickActionState`, `BulkCompletions`,
  `ViewerTranslationState` et `AttachmentFetches` — ces deux derniers chez le
  lecteur qui les possède. Renommages mécaniques (128 accès), aucun changement
  de comportement.
- **2026-07-25** — `b13b1fb` (`R6`, `MailboxState`) 39 → 26 champs : `MailSearchState`
  (requête, portée, tri, résultats, historique, menu) et `MailPagination`.
  `MailboxSession` (le type serde de `session.json`) garde ses noms de champs :
  les regrouper aurait changé le format du fichier. Les autres champs restent à
  plat — c'est un seul domaine, et les grouper n'exposerait aucun invariant.
- **2026-07-25** — `ea62160` (`R7`) `runtime::run` 639 → 585 lignes. Le découpage prévu en
  `dispatch_mail`/`dispatch_calendar`/… **n'a pas été fait** : le `match` unique
  est ce qui force le compilateur à couvrir chaque nouveau `Cmd`. Le découper
  demandait soit un chaînage `Option<Cmd>` (plus d'exhaustivité), soit les
  variantes listées deux fois avec un `unreachable!()` par groupe. Ce qui est
  parti, ce sont les corps inline qui n'étaient pas de l'aiguillage : les trois
  commandes de cache vers `mail_cache::{apply_limit,clear_and_report,
  report_stats}` et le rapport d'image collée à côté de son téléchargement.
- **2026-07-25** — `8496325` (`R8`) `render_block` 499 → 122 lignes : une méthode par
  variante (`render_text_block`, `render_list_block`, `render_table_block`,
  `render_image_block`, `render_html_block`,
  `render_original_message_block`) et `BlockMetrics` pour les trois tailles
  dérivées du zoom que toutes partageaient.
- **2026-07-25** — `9fedb84`, `f9032d1`, `6d07aca` (`R8`) Les six onglets de
  préférences découpés par section : `ai.rs` 307 → 71, `correction.rs` 273 → 11,
  `appearance.rs` 370 → 59, `calendars.rs` 347 → 128, `accounts.rs` 467 → 74,
  `quick_actions.rs` 585 → 34. Chaque section relit les réglages dont elle a
  besoin au lieu d'hériter d'un préambule commun.
  **Non découpé** : `render_quick_action_editor` (596 lignes) — son formulaire
  est une chaîne plate de `.child(…)`, les grouper demanderait des conteneurs
  qui changeraient les espacements.
- **2026-07-25** — `e28f81e` (`R8`) `inbox/messages.rs` : `render_mail_search`
  350 → 45 (trois méthodes qui prennent le panneau et le rendent),
  `render_bulk_message_toolbar` 252 → 45 (deux méthodes rendant des
  `Vec<AnyElement>` passés à `.children`), `render_messages_pane` 372 → 121.
- **2026-07-25** — `R8` `calendar_view::render_week_row` 329 → ~95
  (`week_day_cells`, `week_event_chips`) et
  `kanban_view::render_kanban_column` 266 → 151 (`render_kanban_column_header`).
