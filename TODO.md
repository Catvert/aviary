# TODO — parité fonctionnelle client e-mail

Écarts identifiés entre Aviary et ce qu'un client e-mail de bureau complet
(Outlook, Gmail, Thunderbird) offre en 2026. Issu d'une revue de l'architecture,
du protocole `Cmd`/`Evt`, de la surface `providers::Session`, des réglages et des
catalogues i18n (2026-07-24).

## Convention

- Chaque tâche porte un identifiant stable (`P1-2`, `P2-7`…) : on peut y référer
  dans un commit ou une conversation même après réordonnancement.
- `- [ ]` à faire · `- [~]` en cours · `- [x]` fait (déplacer alors la ligne dans
  « Journal » en bas, avec le commit).
- **Où** liste les points d'ancrage vérifiés dans le code, pas une liste
  exhaustive des fichiers à toucher.
- Les priorités reflètent la valeur perçue par l'utilisateur final, pas la
  difficulté. `P1` = se sent tous les jours, `P2` = attendu d'un client complet,
  `P3` = confort et interopérabilité.
- Quand une tâche modifie l'architecture, mettre à jour `CLAUDE.md` **et**
  `AGENTS.md`.

---

## P1 — se sent tous les jours

### P1-1 · Recherche hors ligne (FTS5) et recherche riche
- [x] **Lot 1** — Table FTS5 dans le cache mail, alimentée à l'écriture des
      en-têtes/corps, et recherche cache-first. *Fait le 2026-07-24, voir Journal.*
- [x] **Lot 2** — Opérateurs dans le champ, traduits vers la syntaxe de chaque
      provider **et** vers le FTS local. *Fait le 2026-07-24, voir Journal.*
- [x] **Lot 3a** — Portée de recherche (dossier courant / tous les dossiers).
      *Fait le 2026-07-24, voir Journal.*
- [ ] **Lot 3b** — Pagination des résultats (aujourd'hui `limit` fixe, pas de
      « charger plus »). Demande un curseur par source : `$skip` Graph,
      `nextPageToken` Gmail, offset IMAP, `OFFSET` SQL côté cache — et un
      `Evt::MoreSearchResults` avec son bouton dans la liste.
- [x] **Lot 4** — Affichage par pertinence, avec bascule Pertinence/Date dans le
      menu de recherche. *Fait le 2026-07-25, voir Journal.*
- [ ] Extraits surlignés (`snippet()`) dans les lignes de résultat. Nécessite de
      quitter le mode contentless ou de re-rendre l'extrait côté Rust —
      à arbitrer contre le coût en taille d'index.

**Pourquoi** : tous les corps sont déjà en local, la recherche pourrait être
instantanée et fonctionner hors connexion ; elle passe actuellement par un aller-retour
réseau avec une requête brute.
**Où** : `runtime/mail_cache.rs` (schéma, `SCHEMA_VERSION` à bumper) ·
`providers/mod.rs:438` (`Session::search`) · `providers/{graph,gmail,imap}/messages.rs`
(`search`) · `runtime/protocol.rs` (`Cmd::Search`, `Evt::SearchResults`) ·
`ui/inbox/messages.rs` (champ + menu de suggestions déjà en place).

### P1-2 · Regroupement par conversation dans la liste
- [x] **Lots A à F** — `conversation_id` sur `MessageHeader`, regroupement dans la
      liste virtualisée, comptage via le cache, sélection/épinglage au niveau du
      fil, réglage, fils dérivés IMAP. *Fait le 2026-07-25, voir Journal.*
- [x] **Reste** — issue partielle d'une action groupée (certaines opérations
      réussissent, d'autres échouent) et lecture d'un fil à son ouverture.
      *Fait le 2026-07-25, voir Journal.*

### P1-4 · Indésirable et blocage d'expéditeur
- [x] **Lot 1** — « Marquer comme indésirable » / « non indésirable », y compris
      la résolution IMAP de l'alias et l'action masquée sans dossier junk.
      *Fait le 2026-07-25, voir Journal.*
- [x] **Lot 2** — Blocage d'expéditeur, local. *Fait le 2026-07-25, voir Journal.*

### P1-5 · Règles de filtrage à l'arrivée
- [ ] Modèle de règle (conditions expéditeur/destinataire/objet/pièce jointe →
      actions étiquette/déplacement/lu/suivi/suppression).
- [ ] Évaluation sur les nouveaux messages, côté runtime.
- [ ] Onglet de préférences dédié.

**Pourquoi** : les quick actions sont entièrement manuelles.
**Où** : `runtime/mailbox.rs` (`Evt::NewMessages` est le point d'entrée) ·
`runtime/operations.rs` (`submit`, déjà idempotent et rejouable) ·
`ui/settings_view/` (l'onglet quick actions sert de modèle d'UI).

### P1-6 · Snooze / reporter à plus tard
- [x] Reporter un message (ce soir, demain, semaine prochaine, date libre),
      filtre « Reportés » et réveil à l'échéance. *Fait le 2026-07-25, voir
      Journal.* L'échéance n'est **pas** passée par l'outbox comme le prévoyait
      cette entrée — voir le Journal pour pourquoi.

### P1-7 · Désabonnement des listes (`List-Unsubscribe`)
- [ ] Lire `List-Unsubscribe` / `List-Unsubscribe-Post` (RFC 8058) à l'ouverture.
- [ ] Bandeau « Se désabonner » dans le lecteur, `mailto:` ou POST en un clic,
      confirmation avant tout envoi.

**Où** : `providers/{graph,gmail,imap}/messages.rs` (extraction des en-têtes) ·
`model.rs` (champ sur `Message`) · `ui/viewer/mod.rs` (bandeau).

### P1-8 · Actions de dossier
- [ ] « Marquer tout le dossier comme lu ».
- [ ] « Vider la corbeille » / « Vider les indésirables », avec confirmation.

**Où** : `ui/inbox/folders.rs` (menus contextuels déjà en place) ·
`providers/mod.rs` + backends.

---

## P2 — attendu d'un client complet

### Rédaction et envoi

- [ ] **P2-1 · Envoi programmé** à date/heure choisie (aujourd'hui seul
      `send_delay_secs` existe, de l'ordre de la seconde).
      *Où* : `runtime/operation_store.rs` (`execute_at`) · `ui/compose.rs`.
- [ ] **P2-2 · Alias / identités multiples** par compte : écrire depuis une autre
      adresse que celle du compte.
      *Où* : `ui/settings.rs` (`AccountSettings`) · `ui/compose.rs` (`compose-from`) ·
      `providers/mod.rs` (`OutgoingMessage::from`).
- [ ] **P2-3 · Accusé de réception** (demande à l'envoi, réponse à la réception)
      et **marqueur d'importance/priorité**.
- [ ] **P2-4 · Transfert en pièce jointe** (`message/rfc822`), en plus du transfert
      en ligne actuel.
- [ ] **P2-5 · Garde-fou « pièce jointe oubliée »** : détecter les mentions de type
      « ci-joint / en pièce jointe / attached » sans fichier attaché avant `SendMail`.
      *Effort faible, valeur élevée.*
      *Où* : `ui/compose.rs`, `ui/viewer/reply.rs`.
- [ ] **P2-6 · Réponse automatique / absence du bureau**, exposée par Graph
      (`automaticRepliesSetting`) et Gmail (`settings/vacation`) ; masquée pour IMAP.

### Sécurité et confidentialité

- [ ] **P2-7 · S/MIME et/ou OpenPGP** : vérification de signature, déchiffrement,
      signature et chiffrement à l'envoi, gestion des clés.
      *Chantier lourd — à arbitrer selon la cible (grand public vs pro/souveraineté).*
- [ ] **P2-8 · Indicateurs d'authentification** SPF/DKIM/DMARC dans l'en-tête du lecteur.
- [ ] **P2-9 · Détection de lien trompeur** (texte affiché ≠ URL cible), avertissement
      avant ouverture.
      *Où* : `ui/blitz_body/net.rs` (le `NavigationProvider` intercepte déjà les clics d'ancre).
- [ ] **P2-10 · Images distantes par expéditeur** : liste d'expéditeurs de confiance,
      au lieu du seul réglage global `show_remote_images`.
      *Où* : `ui/settings.rs`, `ui/blitz_body/net.rs`.
- [ ] **P2-11 · Protection des données au repos** : le cache mail et `session.json`
      sont en clair (les jetons sont en 0600, les mots de passe IMAP au keyring).
      Chiffrement au repos et/ou verrouillage applicatif après inactivité.

### Comptes et connectivité

- [ ] **P2-12 · Autoconfiguration IMAP** : ISPDB Mozilla, autodiscover, enregistrements
      SRV — aujourd'hui serveur/port/TLS sont saisis à la main.
      *Où* : `auth/imap.rs` (`ImapConfig`) · `ui/auth_view.rs`.
- [ ] **P2-13 · IMAP IDLE** (push) et connexion persistante : le modèle actuel est
      *connect → login → op → logout* à chaque appel, avec polling ≥ 60 s.
      *Où* : `providers/imap/connect.rs` · `runtime/mailbox.rs` (`Cmd::SetAutoRefresh`).
- [ ] **P2-14 · Abonnement aux dossiers IMAP** (`SUBSCRIBE`/`LSUB`) et **affichage du
      quota** de boîte.
- [ ] **P2-15 · Proxy configurable** (HTTP/SOCKS) pour tous les appels réseau.

### Contacts

- [ ] **P2-16 · Contacts en écriture** : créer, modifier, supprimer.
      Nécessite d'élargir les scopes OAuth (`auth/microsoft.rs::SCOPE`,
      `auth/google.rs::SCOPE`) — **impose une ré-authentification de tous les comptes**.
- [ ] **P2-17 · Champs riches** : téléphone, société, photo, notes (aujourd'hui
      `Contact { name, email, score }` seulement).
- [ ] **P2-18 · Groupes / listes de distribution**.
      *Où* : `providers/mod.rs:564` (`list_people`) · `providers/{graph,gmail}/people.rs` ·
      `ui/contacts_view.rs`.

### Calendrier

- [ ] **P2-19 · Événements récurrents** en création et édition (la récurrence n'est
      lue que dans les flux iCal via `calcard`).
      *Où* : `providers/mod.rs` (`NewCalendarEvent`) · `ui/event_compose.rs`.
- [ ] **P2-20 · Rappels / alertes** avant le début d'un événement.
- [ ] **P2-21 · Disponibilité des participants** (free/busy) et recherche de créneau.
- [ ] **P2-22 · Calendriers multiples par compte**, calendriers partagés/délégués
      (un seul calendrier par défaut aujourd'hui).
- [ ] **P2-23 · Fuseau horaire explicite** par événement.
- [ ] **P2-24 · « Créer un événement depuis ce message »**.

---

## P3 — confort et interopérabilité

- [ ] **P3-1 · Import/export `.eml` et `.mbox`** : enregistrer un message, ouvrir un
      `.eml` déposé, migrer depuis Thunderbird.
- [ ] **P3-2 · Export/import de contacts vCard**.
- [ ] **P3-3 · Sauvegarde et restauration des préférences** depuis l'UI.
- [ ] **P3-4 · Recherche dans le message affiché** (Ctrl+F), y compris en mode Fidèle.
- [ ] **P3-5 · Tri de la liste** (date, expéditeur, objet, taille, non-lus d'abord) et
      sens de tri — l'ordre est toujours antichronologique.
- [ ] **P3-6 · Glisser-déposer message → dossier** : le drag&drop natif n'est câblé que
      pour le kanban (`ui/kanban_view.rs:1423`) et le calendrier (`ui/calendar_view.rs:1848`).
- [ ] **P3-7 · Dossiers virtuels / recherches enregistrées** et filtres rapides
      (« non lus », « avec pièce jointe »).
- [ ] **P3-8 · Accessibilité** : `blitz-dom` est compilé sans la feature `accessibility`
      et gpui sans accesskit → aucun support lecteur d'écran. Bloquant en contexte
      public ou grande entreprise.
- [ ] **P3-9 · Packaging et mise à jour** : AppImage / Flatpak / `.deb`, mise à jour
      automatique. Le tray reste Linux-only (`ksni`).

---

## Dette et cohérence documentaire

- [ ] **D-1 · Clés i18n orphelines** : les préfixes `view-*` (`view-add`, `view-rename`,
      `view-duplicate`, `view-reset-layouts`…) et `pane-*` n'ont aucun code
      correspondant — vestiges du chantier « vues personnalisées / docking ».
      Soit implémenter, soit supprimer. La CI ne les détecte pas : elle ne compare que
      les jeux de clés `fr.json` / `en.json`.
- [ ] **D-2 · `Cmd::FetchInlineImage`** existe côté runtime, `#[allow(dead_code)]`, émis
      par aucune vue. Le brancher (collage de markdown contenant des images `http:`)
      ou le retirer.
- [ ] **D-3 · Dérive `CLAUDE.md` / `AGENTS.md`** : les deux documents ont divergé.
      Les resynchroniser à la prochaine modification architecturale.

---

## Ordre d'attaque recommandé

1. **P1-5** (règles) — capitalise directement sur P1-4 lot 2 : la liste de
   blocage *est* une règle à condition unique, appliquée au même endroit
   (`on_new_messages`) par le même chemin (`send_batch_now`). Le modèle de règle
   généralise ses deux moitiés, et la liste de blocage devrait s'y résorber
   plutôt que coexister avec.
2. **P1-1 lot 3b** (pagination des résultats de recherche) — le seul reste d'un
   chantier par ailleurs terminé.
3. **P2-12, P2-13** (autoconfig + IDLE) — sinon IMAP reste le parent pauvre des
   trois backends.
4. **P2-7** (S/MIME/OpenPGP) en dernier, et seulement si la cible le justifie.

---

## Journal

### 2026-07-25 · P1-6 · Reporter à plus tard — fait

« Reporter à… » dans le menu d'une ligne, le lecteur et la barre groupée (ce
soir, demain, la semaine prochaine, une date), un filtre « Reportés » dans la
liste, et un réveil à l'échéance.

**Le message ne bouge pas.** L'entrée ci-dessus prévoyait de passer par
l'`execute_at` de l'outbox durable ; c'est la conception qui a été écartée.
Reporter par déplacement demande un dossier dédié et un déplacement retour, or
**IMAP change l'identifiant d'un message déplacé** : `UID MOVE` en émet un
nouveau et le repli COPY+EXPUNGE ne le renvoie pas du tout, si bien que
l'opération de retour perdrait sa cible. Une échéance est donc un état côté
Aviary, rangé à côté de l'épinglage (`AccountSettings.snoozed_messages`) et pour
la même raison : aucun provider n'expose de report que les trois backends
pourraient partager. Rien ne part au réseau au moment du report — rien à
retenter, rien à annuler, rien à réconcilier — et l'identifiant reste le même.
Le prix est assumé : un message reporté l'est pour Aviary seul, un autre client
le voit toujours dans la boîte.

**Le réveil est un tick de 30 s**, pas un minuteur armé sur la prochaine
échéance. Un message reporté à demain matin n'a pas besoin de la seconde près, et
un tick n'a rien à ré-armer quand l'ensemble des échéances — ou l'horloge —
bouge sous lui. La première passe précède le premier tick : une échéance tombée
pendant qu'Aviary était fermé est déjà due au démarrage. Le réveil marque le
message non lu, ce qui est l'objet même du report, silencieusement et par lot
comme la lecture d'un fil.

**L'échéance remplace la date de réception sur la ligne**, elle ne s'y ajoute
pas : les hauteurs sont indexées par `MsgEntryVariant` et un élément de plus
dans cette colonne serait une variante visuelle que rien ne mesure — exactement
le décalage silencieux contre lequel `CLAUDE.md` met en garde. Elle ne s'affiche
de toute façon que sous le filtre, un message reporté n'étant dans aucune autre
liste, et c'est là que « quand revient-il » est la colonne qui compte.

Deux impasses fermées : le bouton de filtre reste tant que le filtre est actif
(le dernier message réveillé ne doit pas emporter la sortie avec lui), et le
filtre se retire de lui-même quand plus rien n'est reporté.

### 2026-07-25 · P1-4 · Indésirable et blocage d'expéditeur — fait

**Lot 1 — indésirable.** Menu du lecteur, menu contextuel d'une ligne (variantes
de fil comprises) et barre groupée, tous par `move_message_with_undo` : retrait
optimiste, fenêtre d'annulation et agrégation de lot viennent avec, exactement
comme l'archivage. Trois points à retenir :

- **Le classement en indésirable ne transmet pas de dossier source**, pour la
  raison qui vaut déjà pour l'archivage : Graph et IMAP l'ignorent, mais pour
  Gmail la source est le label à retirer, et passer le dossier affiché
  retirerait *ce* label en laissant le message distribué tout en le marquant
  spam. L'action inverse est le seul endroit où une source *est* transmise —
  Gmail a besoin qu'on lui retire `SPAM` explicitement, sans quoi le message
  revient dans la boîte en restant classé indésirable chez lui.
- **IMAP résout l'alias comme celui d'archive.** Son `move_message` ne
  connaissait que `archive` ; `junkemail` est désormais cherché de la même
  façon, contre la liste de dossiers du compte, puisque IMAP déduit les noms
  bien connus des attributs `LIST` au lieu de se les faire dire.
- **Pas de dossier indésirable, pas d'entrée.** Un serveur IMAP peut n'avoir
  aucune boîte `\Junk` ni rien qui y ressemble, et l'échec de résolution est une
  erreur sur laquelle l'utilisateur ne peut rien. La disponibilité se lit sur la
  liste de dossiers plutôt que sur le provider, ce qui vaut pour les trois sans
  cas particulier.

**Lot 2 — blocage d'expéditeur, local.** L'entrée ci-dessus citait
`blockedSenders` de Graph : il n'existe pas en v1.0, et les filtres Gmail sont
derrière `gmail.settings.basic`. Pousser une règle côté serveur imposerait donc
une ré-authentification de **tous** les comptes — le coût que P2-16 signale déjà
comme bloquant — sans rien apporter à IMAP, qui n'a pas cette surface. La liste
est donc appliquée par Aviary, identiquement sur les trois backends ; le prix,
écrit dans les Préférences, est qu'elle ne tourne que pendant qu'Aviary est
ouvert.

Elle est **globale** et non par compte, contrairement aux signatures ou aux
colonnes Kanban : c'est Aviary qui l'applique et non un serveur, donc son unité
est la personne qui utilise Aviary, pas l'une de ses boîtes. Seule l'adresse est
comparée, en minuscules — le nom affiché appartient à l'expéditeur — et un
`From` sans adresse n'est jamais bloqué, sinon deux en-têtes illisibles se
bloqueraient l'un l'autre.

Les arrivées sont classées silencieusement et sans fenêtre d'annulation, comme
la lecture d'un fil : la décision a été prise une fois, au blocage, et un toast
par spam la reprendrait à chaque message. Bloquer nettoie aussi les messages
chargés de cet expéditeur — ne bloquer que l'avenir laisserait à l'écran ceux
qui ont motivé le blocage — et de celui-là seulement, le toast le nommant et
comptant ce qui a bougé.

### 2026-07-25 · Bloc « signature » et menu contextuel d'un fil — fait

Deux demandes hors liste, notées ici pour la traçabilité.

**Le menu contextuel d'une ligne de fil repliée agit sur le fil** — supprimer,
archiver, déplacer, lu/non lu, épingler passent par les variantes groupées, le
libellé portant le nombre de messages concernés (« Supprimer le fil (3) ») :
c'est ce qui est *chargé* sous la ligne, donc ce qui va réellement partir. Q2
du design disait que la ligne empruntait le menu des autres lignes ; c'était le
dernier endroit où elle cessait de représenter la conversation. Restent au
message seul : ouvrir, répondre, transférer, actions rapides — qui n'ont pas de
sens à l'échelle d'un fil — et **les étiquettes**, parce qu'une étiquette
alimente le Kanban et qu'en poser une sur un fil y déposerait une carte par
message. Une ligne dépliée redevient ordinaire : ses membres sont à l'écran,
chacun avec son menu.

**La signature insérée est un bloc à part entière** (`BlockKind::Signature`),
plus un `RawHtml` anonyme ni des paragraphes versés dans le document. Dissoute,
rien ne disait où elle commençait ni finissait : impossible de la nommer, de la
remplacer, et une signature HTML importée s'affichait en « fragment HTML ». Le
bloc porte l'identifiant de la signature, son nom et son **HTML rendu une fois
à l'insertion** — un brouillon ne doit pas changer parce que la signature a été
modifiée dans les Préférences depuis, la même raison qui fait qu'`OriginalMessage`
porte son propre HTML. Il est rendu opaque, par le moteur qui rend les corps
reçus, sous un en-tête « Signature · <nom> » et un sélecteur des signatures du
compte (plus « Aucune ») ; le composer offre le même sélecteur dans sa barre
d'outils pour un brouillon qui n'en a pas. Non éditable en ligne, et c'est le
choix qui rend le reste simple : une signature s'édite dans **Préférences →
Signatures**, si bien qu'en changer est un remplacement et non la fusion de deux
versions à moitié retouchées. À l'envoi, le fragment est injecté dans
`<div class="aviary-signature">`, comme les autres clients marquent la leur.

Limite alors connue — rouvrir un brouillon enregistré chez le provider ne
reconstituait pas le bloc — levée le jour même, voir l'entrée ci-dessous.

### 2026-07-25 · Réouvrir un brouillon reconstruit ses blocs — fait

Un brouillon enregistré chez le provider revient en HTML, et il était rendu au
composer par la conversion Markdown du lecteur (`convert_email_html` →
`markdown_to_blocks`). Cette conversion est faite pour *lire* du courrier : elle
déplie les tableaux de mise en page, jette les classes qu'Aviary pose sur ses
propres structures et réécrit les `cid:` vers le schéma `bytes://` du lecteur.
Un brouillon écrit ici en revenait donc en suite de paragraphes, sa signature
dissoute parmi eux et son message d'origine aplati.

`blocks/html.rs` parcourt désormais le DOM (`scraper`, déjà là pour les
réparations Outlook) et reconstruit le document : titres, listes imbriquées,
citations, code, tableaux, séparateurs, images inline avec leur largeur, et les
marques (gras, italique, barré, souligné, liens, code) réécrites dans le
Markdown que le modèle de blocs range dans son texte — y compris quand le client
d'origine les exprime en CSS sur un `<span>`, ce que font Outlook et Gmail. Les
conteneurs de présentation sont transparents ; **ce que l'éditeur ne sait pas
tenir reste opaque au lieu d'être aplati** : la signature redevient un
`BlockKind::Signature`, la citation un `OriginalMessage`, et un tableau dont les
cellules ont leur propre structure un `RawHtml`. Un tableau à cellule unique est
au contraire une contrainte de largeur, pas une donnée : son contenu ressort en
blocs.

Deux conventions étrangères sont reconnues, parce qu'un brouillon peut avoir été
écrit ailleurs : `gmail_signature`/`gmail_quote`, et le `<div id="divRplyFwdMsg">`
d'Outlook Web — qui n'enveloppe pas ce qu'il cite mais le laisse en *frères*,
d'où la règle « tout ce qui suit est la citation ».

**L'identité de la signature voyage** : `build_html_body` pose
`data-aviary-signature-id` sur le `<div class="aviary-signature">`. Seul
l'identifiant part — le nom est la formulation de l'utilisateur et reste sur la
machine, le composer le retrouve dans les réglages du compte. Si le provider a
retiré l'attribut, la signature est reconnue à son **texte visible** ; à défaut
le bloc reste une « Signature importée », toujours remplaçable par le sélecteur,
ce qui est l'essentiel. `saving_a_reopened_draft_produces_the_same_html_again`
verrouille l'idempotence : un brouillon repris chaque jour ne doit pas
s'emboîter d'un niveau à chaque enregistrement.

Limites assumées : le texte est importé tel quel, sans échappement Markdown
(l'éditeur affiche sa propre syntaxe, un `\` visible devant chaque `*` serait
pire que le risque, que les règles de flanking de CommonMark rendent rare) ; le
CSS au-delà des trois propriétés qui portent une marque est perdu, un document
de blocs n'ayant pas où le mettre.

### 2026-07-25 · P1-2 (reste) · Issue partielle et lecture d'un fil — fait

Les deux points laissés ouverts par le regroupement par conversation.

**Un lot n'aboutit pas, il aboutit en partie.** L'outbox garde une ligne par
message : supprimer trente messages, c'est trente réponses indépendantes, chacune
libre de réussir, d'échouer définitivement ou d'être différée. L'agrégation
existante les résumait par un booléen `failed`, si bien que le cas qui se produit
réellement — sept déplacés, trois refusés — n'était *dicible* nulle part : le lot
se terminait en silence et le seul retour restait le dernier toast d'erreur
individuel, écrasé par le précédent sur la même clé de notification.
`PendingBulkCompletion` compte donc les deux côtés et retient la **première**
erreur, verbatim : un décompte sans cause ne donne rien sur quoi agir, huit
messages de provider empilés ne se lisent pas. Trois issues à l'arrivée — tout
passe, rien ne passe, le mélange — un seul toast, le résumé en titre et les mots
du provider dessous.

**Le lot naît à la soumission, pas à la planification.** C'est ce qui règle
l'annulation, qui était le vrai trou : une action annulée pendant sa fenêtre
n'envoie jamais ses commandes, donc son lot n'a plus rien à attendre. Il était
auparavant enregistré dès la planification et nettoyé à la main par l'effet
d'annulation, ce qui obligeait `PendingCancelEffect::MessagesRemoved` à
transporter ses références et son identifiant de lot. `schedule_action` est
désormais le seul endroit où un lot commence, juste avant l'envoi, et **la liste
des messages se déduit des commandes elles-mêmes** — les deux ne peuvent plus
diverger. Conséquence gratuite : les lots lu/suivi, qui n'étaient pas agrégés du
tout, le sont maintenant, et un lot dont les mutations échouent ne produit plus
vingt toasts.

**Le filet mémoire passe de 60 s à 20 min.** Il expirait avant les réponses qu'il
devait agréger : l'outbox retente jusqu'à huit fois derrière un backoff plafonné
à cinq minutes, soit une dizaine de minutes de traîne légitime, après quoi les
dernières réponses redevenaient une notification par message. C'est un garde-fou
de mémoire, pas une échéance.

**Ce qui ne se répète plus** : le premier différé d'un lot parle (au pluriel), les
suivants se taisent — ils disent tous la même chose et l'outbox les rejouera tous ;
et un lot ne recharge l'arborescence de dossiers et le listing qu'une fois, à sa
dernière réponse, au lieu d'une fois par message.

**Ouvrir un fil le marque lu.** La ligne repliée porte la marque de non-lu du fil
entier : n'en lire que le message le plus récent la laissait en gras avec plus
rien à lire à l'écran. Le marquage vit dans `open_message` et dans le différé de
`open_message_debounced`, **pas dans le gestionnaire de clic** : j/k qui traverse
la liste ne doit pas lire les fils qu'il survole, et c'est précisément ce que le
différé décide déjà. Silencieux et sans fenêtre d'annulation, comme la lecture du
message ouvert que le runtime applique déjà.

Deux limites assumées. Seuls les **membres chargés** sont marqués lus — ce que la
ligne représente est ce qui est chargé sous elle ; marquer un message que
personne ne peut voir serait inexplicable. Et le menu contextuel d'une ligne de
fil continue de porter sur son seul message : la ligne de groupe *est* le message
le plus récent (décision Q2), c'est la case à cocher qui sélectionne le fil.

### 2026-07-25 · P1-2 · Regroupement par conversation — fait

Une ligne par fil, dépliable, avec un compteur. Réponses aux cinq questions
laissées ouvertes par `docs/conversation-grouping.md` : activé par défaut, un
clic ouvre le message le plus récent (le chevron déplie), les résultats de
recherche restent à plat, l'épinglage porte sur le fil, IMAP obtient des fils
dérivés.

**Ce n'était pas un chantier d'UI.** `MessageHeader` ne portait aucun
identifiant de fil — seul `Message`, le corps complet, en avait un — alors que
la liste ne manipule que des en-têtes. Le champ a donc été *déplacé* plutôt que
dupliqué : une seule source de vérité. Graph désérialisait déjà
`conversationId` mais ne le demandait pas dans son `$select` ; Gmail avait son
`threadId` en `#[allow(dead_code)]`.

**IMAP n'a pas de fil, on le dérive.** L'identifiant est le `Message-ID`
*racine*, que la RFC 5322 place en tête de `References`. Ce choix est ce qui
tient sous pagination : il se calcule à partir des seuls en-têtes d'un message,
donc la page 2 dérive le même identifiant que la page 1 sans état partagé — ce
qu'un union-find sur les chaînes `In-Reply-To` ne pourrait pas garantir. Pas de
repli sur le sujet normalisé : fusionner silencieusement deux échanges « Re:
contrat » sans rapport est pire que les laisser séparés.

**Le compteur vient du cache, pas de la liste.** Une page chargée contient
quelques dizaines de messages là où un fil s'étale bien au-delà ; compter à
l'écran ferait grimper le compteur au fil du défilement. `conversation_totals`
agrège sur `folder_messages` et exclut les fils d'un seul message. `store_headers`
et cette requête empruntent le même canal d'acteur, donc les comptes voient
toujours la page stockée juste avant.

**Le risque annoncé était les hauteurs de lignes** : une variante visuelle que
personne ne mesure décale toute la liste. Plutôt que d'ajouter des cas aux
quatre `Option<Pixels>` écrits à la main, les hauteurs sont désormais indexées
par `MsgEntryVariant`, dérivée des champs mêmes sur lesquels le rendu branche —
une nouvelle variante se mesure donc elle-même.

Deux ordres devaient suivre l'écran plutôt que la liste d'en-têtes :
`navigable_message_targets` (j/k) est reconstruit à partir des entrées au lieu
de recalculer les sections à la main, et saute la ligne-résumé d'un groupe
déplié — sinon la navigation passerait deux fois par son message le plus
récent. Shift+clic utilise `ordered_visible_message_references`, le regroupement
faisant remonter les messages anciens sous leur cadet.

**Limite assumée** : `list_thread` IMAP ne visite que la boîte de réception et
les envoyés. Il tourne à chaque ouverture de message ; un fil dont les réponses
ont été classées ailleurs revient plus court, plutôt que de faire payer un
parcours complet des dossiers à chaque ouverture.

### 2026-07-25 · P1-1 lot 4 · Tri des résultats — fait

Bascule **Pertinence / Date** dans le menu de recherche, à côté de la portée,
persistée en session. Pertinence par défaut.

Une limite à connaître : « pertinence » veut dire **ordre d'arrivée**, pas
classement global. Les résultats viennent de plusieurs sources — l'index local
d'abord, classé par `bm25`, puis une réponse par compte — et leurs scores ne
sont pas comparables d'un backend à l'autre. Chaque source contribue son propre
ordre meilleur-d'abord, les suivantes s'ajoutent à la suite. Un résultat
provider objectivement meilleur qu'un résultat du cache reste donc sous lui ;
les classer ensemble demanderait un score qu'aucun backend n'expose.

Conséquence assumée : revenir de Date à Pertinence **relance la recherche**, le
tri par date ayant détruit l'ordre d'arrivée que rien n'enregistre.

### 2026-07-24 · P1-1 lot 3a · Portée de recherche — fait

Sélecteur « Tous les dossiers » / « <dossier courant> » dans le menu du champ
de recherche, persisté en session (c'est une habitude, pas un choix par
requête). Changer la portée relance la recherche en cours, plutôt que de
laisser à l'écran des résultats qui ne correspondent plus à ce qui est affiché
à côté d'eux.

`Cmd::Search` porte désormais un `SearchScope` explicite plutôt qu'un
`folder_id: Option<String>` : dans tout le reste du code, `None` veut dire
« boîte de réception », pas « tous les dossiers ». Un `Option` aurait rendu les
deux indiscernables — d'où `SearchScope::{Account, Folder(Option<String>)}`,
qui conserve la convention existante à l'intérieur de la variante.

Par backend :

- **Cache** : `EXISTS` sur `folder_messages`, pas une jointure — un message
  peut appartenir à plusieurs dossiers (étiquettes Gmail) et une jointure le
  renverrait une fois par appartenance.
- **Graph** : `/me/mailFolders/{id}/messages`.
- **Gmail** : restriction par `labelIds`, via `with_folder_labels` qui réajoute
  `INBOX` pour une catégorie (un onglet Gmail est l'intersection des deux).
- **IMAP** : corrige un défaut existant — la recherche partait toujours dans
  INBOX, même en consultant un autre dossier. Une recherche « tous les
  dossiers » parcourt maintenant les boîtes dans **une seule session**
  (un aller-retour par boîte, pas une connexion), INBOX d'abord, plafonnée à
  `MAX_SEARCHED_FOLDERS = 12` : IMAP n'a pas de recherche inter-dossiers, et un
  serveur à cent boîtes bloquerait la requête. L'index local, lui, n'a pas
  cette limite et répond en premier.

### 2026-07-24 · P1-1 lot 2 · Opérateurs de recherche — fait

`src/search_query.rs` analyse le champ **une seule fois** en `SearchQuery`
(donnée pure), et chaque consommateur en fait son dialecte : FTS5, `q` Gmail,
KQL Graph, `SEARCH` IMAP. C'est ce qui garantit que `de:alice` veut dire la même
chose selon que le cache ou le provider répond.

Opérateurs FR **et** EN, quelle que soit la langue de l'interface (on colle des
requêtes, on garde ses habitudes d'un client à l'autre) : `de:`/`from:`,
`à:`/`to:`, `objet:`/`subject:`, `avec:pj`/`has:attachment`,
`est:non-lu`/`is:unread`, `est:suivi`/`is:flagged`, `avant:`/`before:`,
`depuis:`/`after:`, dates en `AAAA-MM-JJ`, `JJ/MM/AAAA`, `7j`, `hier`…

Deux règles tiennent la conception :

- **Un opérateur inconnu reste du texte libre.** `truc:machin` et `Re: contrat`
  sont cherchés, pas rejetés — un champ de recherche qui refuse la saisie est
  pire qu'un champ qui cherche ce qu'on a tapé. Une date illisible
  (`avant:jamais`) ne disparaît pas silencieusement non plus.
- **Aucun backend n'est cru sur parole.** Chacun laisse tomber ce que son
  dialecte n'exprime pas — Graph refuse `$search` combiné à un `$filter` de
  date, IMAP n'a pas de prédicat de pièce jointe — donc `SearchQuery::matches`
  est réappliqué à chaque résultat. Sans ce garde-fou, `avec:pj` renverrait
  tranquillement des messages sans pièce jointe.

Ajout d'une colonne `recipients` à l'index (d'où `SCHEMA_VERSION` `"4"` → `"5"`)
pour `à:`. Elle n'est peuplée qu'à l'ouverture d'un message : une liste de
dossier ne porte pas les destinataires. C'est peu gênant — sur du courrier reçu,
le destinataire, c'est vous — mais à savoir.

Les opérateurs sont documentés dans le menu du champ de recherche, visible tant
qu'on n'a rien tapé : une syntaxe que personne ne voit n'existe pas.

### 2026-07-24 · P1-1 lot 1 · Recherche hors ligne (FTS5) — fait

Index plein texte `messages_fts` dans le cache mail (`SCHEMA_VERSION` `"3"` →
`"4"`, donc cache reconstruit au premier lancement) et recherche cache-first :
`Cmd::Search` renvoie les résultats locaux immédiatement, puis réconcilie avec
le provider. Hors ligne, la recherche fonctionne et ne remonte plus d'erreur.

Choix structurants :

- **Index contentless** (`content=''`) : FTS5 ne garde que l'index inversé,
  jamais une seconde copie du texte — décisif sous un quota de 500 Mo. Adressé
  par `messages.rowid`, stable au travers des upserts `ON CONFLICT DO UPDATE`.
- **Suppression par trigger SQL**, indexation explicite en Rust. Les messages
  sont supprimés depuis six endroits (message, dossier, compte, purge, clear,
  réattribution d'id au déplacement) : un `DELETE` oublié aurait laissé des
  résultats impossibles à ouvrir. L'indexation, elle, reste en Rust car le corps
  doit d'abord être nettoyé de ses cibles de liens.
- **Ce qui est indexé, et quand** : lister un dossier indexe sujet + expéditeur
  + aperçu (la plupart des messages ne sont jamais ouverts, c'est leur seule
  chance) ; ouvrir un message ajoute son corps ; l'éviction retire le corps mais
  garde l'en-tête cherchable.
- **La saisie n'atteint jamais `MATCH` telle quelle.** `objet:` serait lu comme
  un filtre de colonne inexistante et ferait échouer toute la requête : chaque
  terme est cité, le dernier reçoit un `*` (recherche au fil de la frappe).
- `remove_diacritics 2` : « reunion » trouve « Réunion ».

Non fait, et assumé : le tri d'affichage reste chronologique (bm25 ne décide que
*quels* résultats survivent au `limit`) — voir lot 4.

### 2026-07-24 · P1-3 · Archiver en un clic — fait

Bouton dans la barre du lecteur, entrée de menu contextuel, bouton dans la barre
de sélection multiple, raccourcis `Ctrl+E` et `e` (Vim), tous documentés dans
**Préférences → Clavier**. Passe par `move_message_with_undo`, donc retrait
optimiste immédiat et fenêtre d'annulation comme les autres mutations.

Deux points à retenir pour la suite :

- Les trois backends savaient déjà résoudre la cible littérale `"archive"`. Elle
  est désormais nommée (`providers::ARCHIVE_FOLDER_ALIAS`) au lieu d'être répétée
  en dur : Graph la traite comme un dossier well-known, Gmail la traduit en
  retrait de `INBOX` (il n'a pas de dossier Archive), IMAP résout la boîte
  `\Archive` et échoue avec un message traduit si le serveur n'en a pas.
- **L'archivage ne transmet jamais de dossier source.** Graph et IMAP l'ignorent,
  mais pour Gmail la source est le label à retirer : passer le dossier affiché
  retirerait *ce* label en laissant le message dans la boîte de réception —
  l'inverse d'un archivage. Verrouillé par
  `gmail::messages::tests::archiving_drops_inbox_whatever_the_source_folder_is`.

L'action de notification de bureau, qui envoyait un `Cmd::MoveMessage` brut,
emprunte maintenant le même chemin et gagne l'annulation.
