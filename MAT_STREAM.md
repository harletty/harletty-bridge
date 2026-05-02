# Reconstruction du flux MAT

Ce document décrit l'algorithme utilisé par `src/mat.rs` pour transformer un payload IEC 61937 de type `0x16` en suite de chunks MAT exploitables par l'étape suivante.

Le mot "décodage" n'est pas tout à fait le bon ici. Cette étape ne décompresse pas l'audio. Elle fait surtout trois choses:

- vérifier qu'on reçoit bien une trame MAT valide
- retirer les marqueurs MAT qui ne font pas partie du payload utile
- réassembler les chunks utiles, même quand un chunk traverse un marqueur MAT

L'objectif est de produire un flux binaire identique à ce que verrait l'extracteur si les marqueurs MAT n'existaient pas.

## Vue d'ensemble

Chaque burst IEC 61937 TrueHD contient une frame MAT de taille fixe:

- taille totale: `61424` octets
- code de début MAT: `20` octets au tout début
- code du milieu MAT: `12` octets à l'offset `30708`
- code de fin MAT: `16` octets à l'offset `61408`

Entre ces codes, le contenu utile est une suite de chunks. Chaque chunk:

- commence par un header de `2` octets
- ce header donne la taille du chunk
- le payload du chunk peut contenir n'importe quelle valeur, y compris `00 00`
- un chunk peut traverser le code du milieu ou le code de fin

Le parseur doit donc travailler avec deux repères en même temps:

- la position absolue dans la frame MAT
- la position dans le buffer en cours

Ces deux repères ne doivent jamais être désynchronisés.

## Entrée et sortie

Entrée:

- un payload IEC 61937 complet déjà extrait du burst SPDIF
- ce payload commence normalement par le code de début MAT

Sortie:

- zéro, un ou plusieurs morceaux de chunk
- chaque morceau est déjà réordonné par mots de 16 bits

Le parseur est "incrémental" au sens où on appelle `next_chunk()` plusieurs fois après un `push_payload()`.

## Idée générale

L'algorithme fonctionne comme une petite machine à états:

1. attendre un payload
2. vérifier le code de début MAT
3. lire le reste de la frame comme une suite de chunks
4. sauter les codes MAT quand on arrive exactement à leur position
5. si un chunk traverse un code MAT, émettre la partie avant le code, puis reprendre immédiatement la continuation après ce code

## Les états

`WaitingForPayload`

- aucun payload courant
- `next_chunk()` renvoie `None`

`VerifyingMatStart`

- on vient de recevoir un payload
- on vérifie que ses `20` premiers octets correspondent bien au code de début MAT

`ReadingPayload`

- on parcourt le contenu utile
- on garde:
  - `bytes_remaining`: ce qu'il reste à lire dans la frame
  - `mat_position`: position absolue du prochain octet utile dans la frame MAT
  - `middle_code_skipped`: vrai si le code du milieu a déjà été sauté
  - `end_code_skipped`: vrai si le code de fin a déjà été sauté

En plus de l'état principal, on garde `pending_chunk_bytes`:

- `None` s'il n'y a pas de chunk en cours
- `Some(n)` si un chunk a été coupé par un code MAT et qu'il reste `n` octets à sortir

## Étape 1: vérifier le code de début

À la réception d'un nouveau payload:

1. copier le payload dans le buffer interne
2. vérifier que les `20` premiers octets sont exactement le code de début MAT
3. si ce n'est pas le cas:
   - vider l'état courant
   - signaler une erreur
4. sinon:
   - avancer le curseur de `20` octets
   - initialiser `mat_position = 20`
   - passer à l'état `ReadingPayload`

Cette vérification est importante, car tout le reste repose sur l'hypothèse que les offsets MAT sont corrects.

## Étape 2: retirer les codes MAT internes

Pendant la lecture du payload:

- si `mat_position == 30708`, vérifier si le code du milieu est présent
- si `mat_position == 61408`, vérifier si le code de fin est présent

Si le code attendu est bien là:

- avancer le curseur de la longueur du code
- avancer `mat_position` de la même longueur
- diminuer `bytes_remaining` de la même longueur
- marquer le code comme déjà sauté

Point essentiel:

- on ne saute un code que si `mat_position` est exactement à la position du code
- on ne doit jamais "drainer" un code situé plus loin dans le buffer

Autrement dit:

- le parseur ne retire jamais des octets "au milieu" du buffer
- il retire seulement ce qui se trouve exactement à la tête logique de la lecture

Cet invariant évite de casser tous les calculs d'offset suivants.

## Étape 3: continuation d'un chunk coupé par un code MAT

C'est le point le plus sensible.

Si `pending_chunk_bytes` est défini, cela veut dire:

- un chunk a commencé avant
- on en a déjà sorti une première partie
- la suite du chunk se trouve immédiatement après un code MAT

Dans ce cas, il faut traiter cette continuation avant toute autre logique.

Plus précisément:

1. calculer combien d'octets de cette continuation on peut lire maintenant
2. si un autre code MAT se trouve encore à l'intérieur de cette continuation, couper à cet endroit
3. sortir cette portion
4. mettre à jour `pending_chunk_bytes`
5. si le chunk n'est toujours pas terminé, reprendre plus tard après le prochain code MAT

Règle critique:

- il ne faut jamais appliquer la logique "ignorer les `00 00` de tête" avant de traiter une continuation

Pourquoi:

- au début d'une continuation, `00 00` peut être un vrai contenu du chunk
- si on le retire, on décale tout le chunk et on le corrompt

C'est une régression réelle déjà rencontrée.

## Étape 4: ignorer le padding entre deux chunks

Quand il n'y a pas de continuation en cours, le parseur peut rencontrer des mots `00 00` entre deux chunks.

Dans ce cas:

- tant qu'on voit `00 00` à la position courante
- et qu'on n'est pas dans une continuation de chunk
- on avance de `2` octets

Ce padding est ignoré seulement entre deux chunks.

Il ne faut pas confondre ce padding avec:

- des zéros faisant partie d'un chunk
- ou des zéros situés au début d'une continuation après un code MAT

## Étape 5: lire le header d'un nouveau chunk

Quand on n'est pas dans une continuation:

1. lire `2` octets
2. les interpréter en `u16` little-endian
3. garder les `12` bits de poids faible
4. multiplier par `2`

En formule:

```text
chunk_size = (raw & 0x0FFF) << 1
```

Si `chunk_size == 0`:

- considérer le header comme invalide
- avancer de `2` octets
- continuer la lecture

## Étape 6: sortir le chunk, éventuellement en plusieurs morceaux

Une fois `chunk_size` connu:

1. déterminer combien d'octets sont disponibles avant le prochain code MAT
2. ne sortir que cette partie
3. si le chunk n'est pas complet:
   - mémoriser le reste dans `pending_chunk_bytes`
4. sinon:
   - passer au chunk suivant

Ce comportement permet de gérer naturellement les cas suivants:

- chunk entièrement avant le code du milieu
- chunk qui traverse le code du milieu
- chunk qui traverse le code de fin
- chunk qui serait coupé plusieurs fois

## Réordonnancement des octets

Quand le parseur sort une portion de chunk, il échange les octets par paires de `2`:

```text
[a, b, c, d] -> [b, a, d, c]
```

Ce réordonnancement est fait par `copy_swapped_words()`.

L'idée n'est pas de modifier le contenu logique du chunk, mais de remettre les mots de `16` bits dans l'ordre attendu par l'étape suivante.

## Invariants à respecter

Une réimplémentation correcte doit préserver ces règles:

1. `mat_position` doit toujours représenter la position MAT du premier octet encore non consommé.
2. Toute avancée réelle dans le buffer doit avancer `mat_position` du même montant, sauf réinitialisation complète.
3. Un code MAT ne peut être sauté que si la lecture se trouve exactement à sa position.
4. Une continuation de chunk a priorité sur la détection d'un nouveau header.
5. Le padding `00 00` ne peut être ignoré qu'entre deux chunks, jamais au début d'une continuation.
6. La taille d'un chunk reste la taille logique du chunk complet, même si sa sortie est fractionnée.

## Pseudo-code

```text
push_payload(payload):
  vérifier le code de début
  position = 20
  bytes_remaining = taille_payload - 20
  pending_chunk_bytes = none

next_chunk():
  tant qu'il reste des données:
    si on est exactement sur un code MAT:
      sauter ce code
      continuer

    si pending_chunk_bytes existe:
      sortir immédiatement la suite du chunk
      si chunk incomplet:
        mémoriser le reste
      retourner cette portion

    ignorer les mots 00 00 de padding entre chunks

    lire le header du prochain chunk
    calculer chunk_size
    sortir au plus chunk_size octets, sans traverser un code MAT
    si chunk incomplet:
      pending_chunk_bytes = taille_restante
    retourner cette portion

  retourner None
```

## Symptômes d'une implémentation incorrecte

Quand l'algorithme est faux, les symptômes typiques sont:

- chunks de taille zéro ou invalides
- début de chunk correct puis corruption après le code du milieu
- disparition de séquences `00 00` pourtant attendues dans le payload
- relecture d'une continuation comme si c'était un nouveau header
- décalage du flux binaire après un code MAT

En pratique, cela se traduit vite par:

- peu ou pas de frames décodées
- débit audio inférieur au temps réel
- buffer de sortie qui se vide

## Comment valider une réimplémentation

Le minimum utile:

- un test où un chunk reste entièrement avant le code du milieu
- un test où un chunk traverse le code du milieu
- un test où la continuation commence par `00 00`
- un test où le code de fin est sauté correctement
- un test où un payload sans code de début valide est rejeté

Une bonne validation consiste aussi à comparer:

- le brut lu sur le pipe
- le flux MAT reconstruit
- puis à vérifier que Linux et Windows donnent le même résultat binaire à durée égale

## Résumé

L'algorithme ne "décode" pas vraiment MAT. Il reconstruit un flux binaire propre à partir d'une frame MAT encapsulée.

Le coeur du problème est simple:

- retirer les codes MAT sans perdre l'alignement
- ne jamais confondre padding entre chunks et payload réel
- reprendre correctement un chunk coupé par un code MAT

Si ces trois points sont respectés, le flux reconstruit reste stable et l'étape suivante peut travailler normalement.
