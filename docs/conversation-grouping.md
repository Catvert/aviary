# Design — regroupement par conversation dans la liste (TODO P1-2)

État : **implémenté le 2026-07-25** (lots A à F). Rédigé le 2026-07-25.

Ce document reste le compte rendu des décisions : les questions ouvertes ont
été tranchées (section « Questions à trancher »), et les écarts entre la
proposition et le code livré sont notés au fil du texte.

## Objectif

Un échange de douze messages occupe douze lignes dans la liste. Le fil n'existe
aujourd'hui qu'à l'ouverture d'un message (`viewer/mod.rs`,
`thread_newer_messages`). On veut une ligne par conversation, repliable, avec un
compteur — le comportement d'Outlook, Gmail et Thunderbird.

## État des lieux

Trois constats changent la nature de la tâche. **P1-2 n'est pas un chantier
d'interface : c'est d'abord un travail de modèle et de providers.**

1. **`MessageHeader` ne porte pas de `conversation_id`.** Seul `Message` — le
   corps complet — l'expose (`model.rs`). La liste ne manipule que des
   `MessageHeader` : elle ne connaît aucun fil, et aucun regroupement n'est
   possible sans ajouter ce champ.
2. **Graph ne le demande même pas.** `conversationId` est absent de
   `MESSAGE_SELECT` (`providers/graph/messages.rs`), donc il n'arrive pas dans
   les listings. Une ligne à ajouter, mais il faut y penser.
3. **Gmail l'a déjà** : `threadId` est désérialisé et actuellement marqué
   `#[allow(dead_code)]`. Gratuit.
4. **IMAP n'a rien.** `conversation_id` y est toujours `None` et `list_thread`
   renvoie un fil vide, avec un commentaire assumant la limite. Le protocole
   n'expose pas d'identifiant de fil : il faudrait le dériver.

## Contraintes

- La liste est **virtualisée** : l'arbre sections → lignes est aplati en
  `MsgEntry` de hauteur connue, un représentant par variante étant mesuré hors
  écran (`message_list_model`). Toute nouvelle variante visuelle ajoute une
  mesure — et une variante oubliée décale toutes les lignes.
- La liste est **paginée** : elle ne contient que les messages déjà chargés.
- L'inbox est **unifiée** : plusieurs comptes cohabitent dans la même liste.
- Les lignes sont **réutilisées** par l'historique d'expéditeur, les Contacts et
  le Kanban (`message_row`). Le regroupement ne doit pas les contaminer.

## Décisions proposées

### Ce qui me semble non discutable

**Clé de groupe = `(account_id, conversation_id)`.** Les identifiants de fil ne
sont pas comparables d'un compte à l'autre ; une collision fusionnerait deux
échanges sans rapport.

**Un groupe se place à la date de son message le plus récent**, ce qui décide à
la fois de l'ordre des groupes et de la section « jour » à laquelle il
appartient. Un fil qui s'étale sur trois jours apparaît donc une seule fois, au
jour de sa dernière activité.

**Un groupe est non lu si au moins un de ses messages l'est**, et son compteur
affiche le nombre de non-lus, pas le total. C'est ce qui rend la ligne
actionnable.

**Un groupe d'un seul message reste une ligne ordinaire** — pas de chevron, pas
de compteur. Sinon la boîte se remplit de faux groupes.

**L'état replié/déplié est persistant en session**, comme
`collapsed_message_sections` aujourd'hui.

**Le regroupement ne s'applique qu'à la liste de messages principale.**
L'historique d'expéditeur, les Contacts et le Kanban restent plats.

### Le point le plus inconfortable : les groupes sont partiels

La liste ne contient que les messages chargés. Un fil de douze messages dont la
page courante n'en contient que trois s'affichera « 3 » — un chiffre faux.

Trois issues :

- **(a) Assumer le partiel.** Le compteur reflète ce qui est chargé, et
  l'ouverture montre le fil complet (le lecteur sait déjà le faire). Simple,
  honnête, mais un compteur qui change en faisant défiler surprend.
- **(b) Interroger le provider** pour le nombre réel de messages du fil. Une
  requête par fil visible : inacceptable.
- **(c) Compter dans le cache local.** `folder_messages` connaît tous les
  messages déjà vus, souvent bien plus que la page courante. Le compteur devient
  « ce que je sais du fil », stable au défilement, sans coût réseau.

**Je recommande (c)**, avec repli sur (a) quand le cache ne sait rien. Cela
suppose une requête d'agrégation par page de liste, pas par fil — à mesurer,
mais l'index existe déjà (`folder_messages_order`).

### IMAP : dégradation ou heuristique ?

IMAP n'a pas de fil. Deux voies :

- **Dégradation gracieuse** : pas de regroupement pour les comptes IMAP, la
  liste reste plate. Honnête, zéro risque, mais l'utilisateur IMAP ne bénéficie
  de rien — et il voit ses autres comptes groupés dans la même liste unifiée.
- **Fil dérivé localement** : récupérer `References` / `In-Reply-To` dans le
  `FETCH` du listing (ils tiennent dans la même requête, coût quasi nul) et
  reconstruire les fils par chaînage, avec repli sur le sujet normalisé
  (`Re:`/`Fwd:` retirés). C'est l'algorithme que tout client IMAP applique.
  Imparfait — deux échanges sans lien au même sujet peuvent fusionner — mais
  c'est ce à quoi les gens sont habitués.

**Je recommande le fil dérivé**, en lot séparé : le chaînage par `References`
est fiable, et c'est le repli par sujet qui est risqué. On peut livrer le
chaînage seul et décider ensuite si le repli par sujet vaut ses faux positifs.

## Questions tranchées

1. **Activé par défaut.** `default_group_by_conversation()` renvoie `true`, de
   sorte qu'une installation neuve et une installation mise à jour se
   comportent pareil. **Préférences → Boîte → Liste des messages** le désactive.
2. **Un clic ouvre, le chevron déplie.** La ligne de groupe *est* le message le
   plus récent du fil : elle passe par le même `message_row_inner` que
   n'importe quelle ligne, donc actions rapides, survol et sélection sont ceux
   des autres lignes, sans duplication. Le chevron arrête la propagation du
   clic. **Révisé le 2026-07-25** : le menu contextuel d'une ligne *repliée*
   agit en revanche sur le fil (supprimer, archiver, déplacer, lu/non lu,
   épingler), comme le font Gmail et Outlook — supprimer « le fil » et n'en
   retirer que le dernier message était le seul endroit où la ligne cessait de
   représenter la conversation. Les étiquettes restent au message : elles
   alimentent le Kanban, une par fil y déposerait une carte par message.
3. **Recherche à plat.** Replier les résultats en fils masquerait la ligne qui
   a précisément répondu à la requête.
4. **L'épinglage porte sur le fil.** Un membre épinglé maintient toute la
   conversation en haut, y compris les réponses arrivées ensuite. Épingler ne
   marque que le membre le plus récent ; dépingler doit en revanche effacer
   *tous* les membres, sinon un marquage ancien ré-épinglerait le fil en
   silence.
5. **Fil dérivé pour IMAP**, sans repli par sujet — voir ci-dessus.

## Découpage proposé

| Lot | Contenu | Risque |
|---|---|---|
| **A** | `conversation_id` sur `MessageHeader` ; `conversationId` ajouté au `MESSAGE_SELECT` de Graph ; Gmail branché ; bump `SCHEMA_VERSION` (les en-têtes déjà en cache n'ont pas le champ) | Faible, mais touche les trois backends |
| **B** | Regroupement dans `build_message_list_entries` : variante `MsgEntry::Group`, lignes enfants indentées, mesure des nouvelles variantes, repli/dépli persistant | **Le cœur du risque** — c'est la liste virtualisée |
| **C** | Comptage via le cache (option **c** ci-dessus) | Faible |
| **D** | Sélection multiple, actions groupées, lu/non-lu au niveau du fil | Moyen |
| **E** | Réglage d'activation + migration du comportement | Faible |
| **F** | Fils dérivés IMAP (`References`/`In-Reply-To`) | Moyen, isolable |

Le lot B est celui qui justifie de faire P1-2 tôt : chaque fonctionnalité
ajoutée à la liste virtualisée le renchérit.

### Ce qui a été livré, et ce qui a bougé

- **A** — `conversation_id` a été **déplacé** de `Message` vers `MessageHeader`
  plutôt que dupliqué : deux champs auraient fini par diverger.
- **A + F** — l'identifiant IMAP dérivé est le `Message-ID` **racine** (tête de
  `References`), pas un hachage : il se calcule à partir des seuls en-têtes
  d'un message, donc deux pages d'un même listing s'accordent sans état
  partagé. La dérivation a été livrée avec le lot A, `list_thread` avec F.
- **B** — les hauteurs ne sont plus quatre `Option<Pixels>` mais une table
  indexée par `MsgEntryVariant`, dérivée des champs sur lesquels le rendu
  branche. C'est la réponse directe au risque « hauteurs de lignes » ci-dessous.
- **B** — `navigable_message_targets` (j/k) est désormais **reconstruit à
  partir des entrées** au lieu de recalculer les sections en parallèle. Les
  deux calculs ne pouvaient que diverger. La navigation saute la ligne-résumé
  d'un groupe déplié, qui ferait autrement passer deux fois par son message le
  plus récent, et Shift+clic suit l'ordre affiché
  (`ordered_visible_message_references`).
- **C** — seul le **total** vient du cache. L'état non-lu se calcule sur les
  membres chargés : mélanger un compte de cache et une mise à jour optimiste de
  l'UI donnerait un affichage brièvement faux à chaque « marquer comme lu ».
- **F** — `list_thread` ne visite que la boîte de réception et les envoyés. Il
  tourne à chaque ouverture de message.
- **Livré ensuite (2026-07-25)** : ouvrir un fil replié marque ses membres
  chargés comme lus — dans `open_message` et dans le différé de
  `open_message_debounced`, pour que j/k ne lise pas les fils qu'il survole —
  et l'issue **partielle** d'une action groupée est devenue un état explicite
  (succès et échecs comptés, première erreur retenue, un seul toast). Voir le
  journal de `TODO.md`.

## Risques identifiés

- **Hauteurs de lignes.** Une variante visuelle non mesurée décale silencieusement
  toute la liste. Chaque nouvelle variante (`Group`, ligne enfant, groupe
  déplié en fin de section) doit avoir son représentant dans
  `message_list_model`.
- **Compteurs de non-lus.** Le compteur du dossier vient du provider, celui du
  groupe est local : ils peuvent diverger visuellement.
- **Sélection multiple.** `selected_messages` est un ensemble de `MessageRef` ;
  sélectionner un groupe doit y verser ses membres **chargés**, pas le fil
  entier — sans quoi une action groupée porterait sur des messages absents de
  l'écran.
- **Suppression et déplacement d'un fil** produisent N opérations dans l'outbox.
  Les fenêtres d'annulation groupées existent (`bulk_*`), mais l'annulation
  partielle (certaines réussissent, d'autres échouent) reste à penser.
