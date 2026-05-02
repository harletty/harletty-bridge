# OAMD Metadata Frame Plan

Ce document décrit le chemin exact actuellement implémenté dans `truehd-bridge` pour transformer une charge utile OAMD binaire en `RMetadataFrame` bridge. Il suit le code tel qu'il existe aujourd'hui, sans extrapolation.

## 1. Point d'entrée exact dans le décodeur

Le bridge ne parse pas directement l'OAMD depuis les octets MAT. Le chemin réel est:

1. Le flux TrueHD est extrait puis parsé en `AccessUnit`.
2. Pendant le décodage, on inspecte `access_unit.extra_data.evo_frame`.
3. Pour chaque `evo_payload`:
   - le payload n'est traité comme OAMD que si `evo_payload.evo_payload_id == 11`
   - le sample offset Evolution utilisé par le bridge est `evo_payload.evo_payload_config.smploffst.unwrap_or_default()`
4. Les octets du payload sont ensuite parsés par:
   - [ObjectAudioMetadataPayload::read](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L229)
5. Le champ `evo_sample_offset` du résultat est ensuite rempli côté décodeur:
   - [decode.rs](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/process/decode.rs#L296)

Condition exacte d'entrée OAMD dans le décodeur:

- seulement dans la sous-trame `i == 3`
- seulement si `access_unit.extra_data` existe
- seulement si `extra_data.evo_frame` existe
- seulement pour les payloads Evolution de type `11`

Référence:
- [decode.rs](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/process/decode.rs#L296)

## 2. Parsing binaire de `ObjectAudioMetadataPayload`

Le parseur binaire principal est:

- [ObjectAudioMetadataPayload::read](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L229)

Ordre exact de lecture:

1. `oamd_version`
   - lecture de 2 bits
   - si la valeur lue vaut `3`, on lit 3 bits supplémentaires et on additionne
   - le parseur impose ensuite `assert_eq!(oamd_version, 0)`
   - donc toute version autre que `0` panique ici

2. `object_count_bits`
   - lecture de 5 bits
   - si la valeur lue vaut `31`, on lit 7 bits supplémentaires et on additionne
   - `object_count = object_count_bits + 1`

3. `program_assignment`
   - parsé par [ProgramAssignment::read](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L137)

4. `b_alternate_object_data_present`
   - lecture de 1 bit booléen

5. `oa_element_count`
   - lecture de 4 bits
   - si la valeur lue vaut `15`, lecture de 5 bits supplémentaires et addition

6. Boucle sur `oa_element_count`
   - chaque élément est parsé par [OAElementMD::read](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L356)

## 3. Parsing d'un `OAElementMD`

Ordre exact:

1. `oa_element_id_idx`
   - 4 bits

2. `oa_element_size_bits`
   - via `get_variable_bits_max(4, 4)`

3. `oa_element_size`
   - calculé comme `(oa_element_size_bits + 1) << 3`

4. `pos_start` et `pos_end`
   - `pos_end = pos_start + oa_element_size`
   - sauf si `oa_element_size > reader.available()`
   - dans ce cas:
     - warning `Truncated oa_element_md`
     - `pos_end = reader.available()`

5. Si `b_alternate_object_data_present == true`
   - lecture de `alternate_object_data_id_idx` sur 4 bits

6. `b_discard_unknown_element`
   - 1 bit booléen

7. Dispatch selon `OAElementType::from_u8(oa_element_id_idx)`:
   - `Object` => parse [ObjectElement::read](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L439), stocké dans `state.object_element`
   - `Trim` => parse `TrimElement::read`, stocké dans `state.trim_element`
   - `ExtendObject` => parse `ExtendedObjectElement::read`, stocké dans `state.extended_object_element`
   - tout autre type => warning, pas d'implémentation, contenu ignoré

8. Padding de fin d'élément
   - si `pos_end > pos_current`, le parseur saute `pos_end - pos_current` bits

Conséquence exacte:

- le payload final conserve `object_element`, `trim_element`, `extended_object_element` depuis `state`
- il n'y a pas de tableau de plusieurs `object_element`; le dernier vu pour un type donné écrase l'état précédent

## 4. Parsing de `ObjectElement`

Référence:
- [ObjectElement::read](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L439)

Ordre exact:

1. `md_update_info = MDUpdateInfo::read(reader)`
2. `b_reserved_data_not_present`
   - 1 bit
3. Si `b_reserved_data_not_present == false`
   - lecture de `reserved_data` sur 5 bits
4. `block_count = md_update_info.num_obj_info_blocks`
5. Boucle sur tous les objets `0..state.object_count`
6. Pour chaque objet, boucle sur tous les blocs `0..block_count`
7. Chaque bloc est parsé par [ObjectInfoBlock::read](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L565)

## 5. Parsing de `MDUpdateInfo`

Référence:
- [MDUpdateInfo::read](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L480)

Ordre exact:

1. `sample_offset_code`
   - 2 bits

2. Décodage de `sample_offset`
   - code `0` => `0`
   - code `1` => lecture de `sample_offset_idx` sur 2 bits
     - `0` => `8`
     - `1` => `16`
     - `2` => `18`
     - `3` => `24`
   - code `2` => lecture directe de `sample_offset_bits` sur 5 bits
   - code `3` => `unreachable!()`

3. `num_obj_info_blocks`
   - lecture de 3 bits
   - valeur finale = bits lus + 1

4. Boucle `num_obj_info_blocks`
   - chaque entrée est parsée par [BlockUpdateInfo::read](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L527)

## 6. Parsing binaire exact de `BlockUpdateInfo` et du `ramp_duration`

Référence:
- [BlockUpdateInfo::read](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L527)

Ordre exact:

1. `block_offset_factor_bits`
   - 6 bits

2. `ramp_duration_code`
   - 2 bits

3. Décodage de `ramp_duration`
   - code `0` => `0`
   - code `1` => `512`
   - code `2` => `1536`
   - code `3` =>
     1. lecture de `b_use_ramp_duration_idx` sur 1 bit
     2. si `true`:
        - lecture de `ramp_duration_idx` sur 4 bits
        - valeur lue dans `RAMP_DURATION_LIST`
     3. sinon:
        - lecture directe de `ramp_duration_bits` sur 11 bits

Table exacte `RAMP_DURATION_LIST`:

- `32, 64, 128, 256, 320, 480, 1000, 1001, 1024, 1600, 1601, 1602, 1920, 2000, 2002, 2048`

Important:

- le bridge ne recalcule jamais cette valeur
- il consomme `block_update_info[0].ramp_duration` déjà décodé

## 7. Parsing d'un `ObjectInfoBlock`

Référence:
- [ObjectInfoBlock::read](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L565)

Ordre exact:

1. `b_object_not_active`
   - 1 bit

2. `object_basic_info_status_idx`
   - si `b_object_not_active == true` => `0`
   - sinon si `block_index == 0` => `1`
   - sinon => lecture de 2 bits

3. `prev_object_basic_info`
   - bloc 0 => `ObjectBasicInfo::default()`
   - sinon => `state.prev_object_basic_info.clone()`

4. `object_basic_info`
   - status `0` => `default`
   - status `1` ou `3` => parse `ObjectBasicInfo::read(...)`
   - sinon => reprise du précédent

5. `b_object_in_bed_or_isf`
   - calculé comme `object_index < state.program_assignment.beds_or_isf_count()`

6. `object_render_info_status_idx`
   - si `b_object_not_active == true` => `0`
   - sinon si `b_object_in_bed_or_isf == false`
     - bloc 0 => `1`
     - sinon => lecture de 2 bits
   - sinon => `0`

7. `prev_object_render_info`
   - bloc 0 => `ObjectRenderInfo::default()`
   - sinon => `state.prev_object_render_info.clone()`

8. `object_render_info`
   - status `0` => `default`
   - status `1` ou `3` => parse `ObjectRenderInfo::read(...)`
   - sinon => reprise du précédent

9. `b_additional_table_data_exists`
   - 1 bit
   - si `true`:
     - `additional_table_data_size = (reader.get_n::<u32>(4)? + 1) << 3`
     - saut de `additional_table_data_size` bits

## 8. Parsing utile de `ObjectRenderInfo`

Référence:
- [ObjectRenderInfo::read](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L762)

Ce qui influence directement les événements bridge:

1. `object_render_info_bits`
   - si status `1` => `15`
   - sinon lecture de 4 bits

2. Si bit `1` actif:
   - `b_differential_position_specified`
     - bloc 0 => toujours `false`
     - sinon lecture d'un bit
   - si différentiel:
     - `x += signed(3) / 62.0`
     - `y += signed(3) / 62.0`
     - `z += signed(3) / 15.0`
   - sinon position absolue:
     - `x = u6 / 62.0`
     - `y = u6 / 62.0`
     - `sign_z` sur 1 bit
     - `z = u4 / 15.0 * sign_z`
   - `b_object_distance_specified`
   - si `true`:
     - `b_object_at_infinity`
     - si `false`: `distance_factor_idx` sur 4 bits
   - la distance n'est pas reconstruite plus loin: commentaire exact `TODO: parse distance`

3. Si bit `4` actif:
   - lecture de `object_size`
   - c'est cette taille qui alimente ensuite le `spread` bridge

4. En fin de fonction:
   - `b_object_snap` est toujours lu sur 1 bit, même si aucun bit de `object_render_info_bits` n'est actif

## 9. Construction des positions DAMF utilisées par le bridge

Référence:
- [get_damf_pos](/home/user/dev/spatial-renderer/harletty-bridge/truehd/src/structs/oamd.rs#L280)

Étapes exactes:

1. Initialisation `damf_pos = vec![vec![]; object_count]`
2. Si `object_element` existe:
   - pour chaque objet
   - pour chaque bloc
   - push de `block.object_render_info.pos3d`
3. Si `extended_object_element` existe:
   - pour chaque objet/bloc
   - addition des composantes de précision étendue à la position déjà présente
4. Normalisation finale sur toutes les positions:
   - `x = (clamp(x, 0..1) - 0.5) * 2.0`
   - `y = (0.5 - clamp(y, 0..1)) * 2.0`
   - `z = clamp(z, -1..1)`

Important:

- le bridge consomme cette position normalisée
- si la structure `extended_object_element` ne correspond pas en taille aux positions déjà présentes, le code suppose implicitement que l'indexation est valide

## 10. Construction d'une `RMetadataFrame`

Référence:
- [build_metadata_frame](/home/user/dev/spatial-renderer/harletty-bridge/src/lib.rs#L1211)

Ordre exact:

1. `events = extract_events(oamd, evo_base)`
2. `bed_index_vec`
   - pris depuis `oamd.program_assignment.bed_assignment.first()`
   - sinon vide
3. `bed_indices`
   - conversion des indices lit/beds via `speaker_to_id`
4. `name_updates`
   - construits seulement pour les événements présents
   - avec cache `name_key_cache`
5. `ramp_duration`
   - pris depuis `oamd.object_element`
   - puis `md_update_info.block_update_info.first()`
   - sinon `0`
6. construction finale:
   - `sample_pos = frame_sample_pos`
   - `ramp_duration = valeur ci-dessus`

Point important:

- `RMetadataFrame.sample_pos` n'utilise pas `evo_sample_offset`
- `extract_events` utilise `evo_base`
- donc la frame metadata et les événements qu'elle contient ne portent pas exactement la même base temporelle

## 11. Conditions exactes de rejet dans `extract_events`

Référence:
- [extract_events](/home/user/dev/spatial-renderer/harletty-bridge/src/lib.rs#L1274)

Le bridge retourne immédiatement une liste vide dans les cas suivants:

1. `oamd.object_element.is_none()`
2. `object_element.md_update_info.num_obj_info_blocks != 1`
3. `oamd.program_assignment.bed_assignment.len() != 1`
4. `oamd.program_assignment.num_isf_objects != 0`

Ces cas ne font pas tous la même chose:

- cas 1: retour vide sans log
- cas 2, 3, 4: retour vide avec `log::warn!`

## 12. Construction exacte des événements bridge

Toujours dans [extract_events](/home/user/dev/spatial-renderer/harletty-bridge/src/lib.rs#L1274):

Pré-calculs:

1. `sample_offset = object_element.md_update_info.sample_offset as u64`
2. `ramp_duration = object_element.md_update_info.block_update_info[0].ramp_duration as u32`
3. `sample_pos = base_sample_pos + sample_offset`
4. `pos_vec = oamd.get_damf_pos()`
5. `bed_index_vec = first bed_assignment -> to_index_vec()`, sinon vide

Boucle exacte sur `i in 0..object_count`:

1. Si `object_element.object_data.get(i)` est absent
   - compteur `missing_object_data += 1`
   - objet ignoré

2. Si `object_blocks.first()` est absent
   - compteur `empty_object_blocks += 1`
   - objet ignoré

3. Calcul de `id`
   - si `b_object_in_bed_or_isf == true`
     - on lit `bed_index_vec.get(i)`
     - si absent:
       - compteur `bed_index_oob += 1`
       - objet ignoré
     - sinon `id = speaker_to_id(bed_idx) as u32`
   - sinon
     - `id = (i + 10 - bed_index_vec.len()) as u32`

4. Calcul de `(has_pos, pos, spread)`
   - si objet dynamique (`!b_object_in_bed_or_isf`)
     - lecture de `render = object_data.object_render_info`
     - lecture de `pos_vec.get(i).and_then(|raw_blocks| raw_blocks.first())`
     - cas `Some(raw)` avec `raw.len() >= 3`
       - `has_pos = true`
       - `pos = [raw[0], raw[1], raw[2]]`
       - `spread = (render.object_size[0] * 180.0).clamp(0.0, 180.0)`
     - cas `Some(_)` mais longueur < 3
       - `has_pos = false`
       - `pos = [0.0; 3]`
       - `spread = 0.0`
     - cas `None`
       - compteur `missing_damf_pos += 1`
       - `has_pos = false`
       - `pos = [0.0; 3]`
       - `spread = 0.0`
   - si bed / isf
     - `has_pos = false`
     - `pos = [0.0; 3]`
     - `spread = 0.0`

5. Push final de `REvent`
   - `id`
   - `sample_pos`
   - `has_pos`
   - `pos`
   - `gain_db = object_data.object_basic_info.object_gain`
   - `spread`
   - `ramp_duration`

## 13. Warnings non bloquants en fin d'extraction

Après la boucle objets, warnings éventuels si compteurs non nuls:

- `missing_object_data`
- `empty_object_blocks`
- `bed_index_oob`
- `missing_damf_pos`

Dans ces cas:

- la frame n'est pas rejetée globalement
- seuls les objets concernés sont omis, ou bien gardés sans position selon le cas

## 14. Résumé strict des conditions qui gouvernent `ramp_duration`

Le `ramp_duration` bridge d'une trame suit exactement cette règle:

1. l'OAMD doit avoir un `object_element`
2. `build_metadata_frame` prend le premier `block_update_info`
3. si ce premier bloc n'existe pas, la valeur de la frame vaut `0`
4. `extract_events` suppose au contraire que `num_obj_info_blocks == 1` et lit `block_update_info[0]`
5. chaque `REvent` reçoit la même valeur `ramp_duration` pour toute la trame

Donc, dans l'implémentation actuelle:

- pas de support multi-bloc côté événements
- pas de `ramp_duration` par objet
- pas de `ramp_duration` distinct par bloc exporté
- la source unique est `object_element.md_update_info.block_update_info[0].ramp_duration`

## 15. Schéma des embranchements

```text
EvoFrame
└─ evo_payloads[]
   └─ evo_payload_id == 11 ?
      ├─ non -> ignoré
      └─ oui
         ├─ smploffst = evo_payload_config.smploffst.unwrap_or_default()
         ├─ ObjectAudioMetadataPayload::read(bytes)
         │  ├─ oamd_version
         │  │  ├─ 2 bits
         │  │  ├─ == 3 ? -> lire 3 bits en plus
         │  │  └─ version finale != 0 -> panic
         │  ├─ object_count_bits
         │  │  ├─ 5 bits
         │  │  └─ == 31 ? -> lire 7 bits en plus
         │  ├─ program_assignment
         │  ├─ b_alternate_object_data_present
         │  ├─ oa_element_count
         │  │  ├─ 4 bits
         │  │  └─ == 15 ? -> lire 5 bits en plus
         │  └─ boucle oa_element_count
         │     └─ OAElementMD::read
         │        ├─ oa_element_id_idx
         │        ├─ oa_element_size_bits
         │        ├─ b_alternate_object_data_present ?
         │        │  ├─ oui -> lire alternate_object_data_id_idx
         │        │  └─ non
         │        ├─ b_discard_unknown_element
         │        └─ type oa_element_id_idx
         │           ├─ Object -> ObjectElement::read
         │           │  ├─ MDUpdateInfo::read
         │           │  │  ├─ sample_offset_code
         │           │  │  │  ├─ 0 -> sample_offset = 0
         │           │  │  │  ├─ 1 -> lire sample_offset_idx (2 bits): 8|16|18|24
         │           │  │  │  └─ 2 -> lire sample_offset_bits (5 bits)
         │           │  │  ├─ num_obj_info_blocks = read(3 bits) + 1
         │           │  │  └─ boucle num_obj_info_blocks
         │           │  │     └─ BlockUpdateInfo::read
         │           │  │        ├─ block_offset_factor_bits (6 bits)
         │           │  │        ├─ ramp_duration_code (2 bits)
         │           │  │        └─ ramp_duration
         │           │  │           ├─ 0 -> 0
         │           │  │           ├─ 1 -> 512
         │           │  │           ├─ 2 -> 1536
         │           │  │           └─ 3
         │           │  │              ├─ b_use_ramp_duration_idx == 1
         │           │  │              │  └─ ramp_duration_idx (4 bits) -> LUT[16]
         │           │  │              └─ b_use_ramp_duration_idx == 0
         │           │  │                 └─ ramp_duration_bits (11 bits)
         │           │  ├─ b_reserved_data_not_present ?
         │           │  │  ├─ non -> lire reserved_data (5 bits)
         │           │  │  └─ oui
         │           │  └─ boucle objets × blocs
         │           │     └─ ObjectInfoBlock::read
         │           │        ├─ b_object_not_active ?
         │           │        │  ├─ oui -> basic_status = 0 ; render_status = 0
         │           │        │  └─ non
         │           │        ├─ block_index == 0 ?
         │           │        │  ├─ oui -> basic_status = 1
         │           │        │  └─ non -> lire basic_status (2 bits)
         │           │        ├─ basic_status
         │           │        │  ├─ 0 -> default
         │           │        │  ├─ 1|3 -> ObjectBasicInfo::read
         │           │        │  └─ 2 -> reprendre précédent
         │           │        ├─ object_index < beds_or_isf_count ?
         │           │        │  ├─ oui -> b_object_in_bed_or_isf = true ; render_status = 0
         │           │        │  └─ non
         │           │        ├─ block_index == 0 ?
         │           │        │  ├─ oui -> render_status = 1
         │           │        │  └─ non -> lire render_status (2 bits)
         │           │        ├─ render_status
         │           │        │  ├─ 0 -> default
         │           │        │  ├─ 1|3 -> ObjectRenderInfo::read
         │           │        │  └─ 2 -> reprendre précédent
         │           │        └─ b_additional_table_data_exists ?
         │           │           ├─ oui -> lire taille + skip
         │           │           └─ non
         │           ├─ Trim -> TrimElement::read
         │           ├─ ExtendObject -> ExtendedObjectElement::read
         │           └─ autre -> warning + skip padding
         ├─ oamd.evo_sample_offset = smploffst
         └─ build_metadata_frame
            ├─ events = extract_events(oamd, evo_base)
            │  ├─ object_element absent ? -> events vides
            │  ├─ num_obj_info_blocks != 1 ? -> warning + events vides
            │  ├─ bed_assignment.len() != 1 ? -> warning + events vides
            │  ├─ num_isf_objects != 0 ? -> warning + events vides
            │  ├─ sample_pos = evo_base + sample_offset
            │  ├─ ramp_duration = block_update_info[0].ramp_duration
            │  └─ boucle objets
            │     ├─ object_data absent ? -> skip
            │     ├─ premier bloc absent ? -> skip
            │     ├─ bed/isf ?
            │     │  ├─ oui -> id = speaker_to_id(bed_idx)
            │     │  └─ non -> id = i + 10 - bed_count
            │     ├─ dynamique ?
            │     │  ├─ oui -> prendre pos_vec[i][0] si dispo
            │     │  └─ non -> has_pos = false
            │     └─ push REvent { id, sample_pos, pos, gain_db, spread, ramp_duration }
            ├─ bed_indices depuis premier bed_assignment
            ├─ name_updates depuis events
            └─ RMetadataFrame
               ├─ sample_pos = frame_sample_pos
               └─ ramp_duration = first block_update_info ramp_duration || 0
```
