//! Emoji completion for every textual block in the mail editor.
//!
//! A colon at a token boundary opens BlockInput's cursor-anchored completion
//! menu. Common text emoticons (`:)`, `;)`, `<3`, …) and completed
//! `:shortcode:` forms resolve through the same menu. The catalog uses stable
//! shortcodes for display and hidden French/English keywords for filtering, so
//! it follows the active UI language without maintaining two result sets.

use crate::ui::components::block_input::{BlockCompletionItem, BlockCompletionProvider};
use std::rc::Rc;

const MAX_SUGGESTIONS: usize = 10;
const MAX_QUERY_BYTES: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Emoji {
    glyph: &'static str,
    shortcode: &'static str,
    /// Search-only aliases. Keep both English and accent-free French forms so
    /// a query works independently of the current locale and keyboard layout.
    keywords: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EmojiAlias {
    text: &'static str,
    shortcode: &'static str,
}

// Exact, case-sensitive text emoticons. Colon/semicolon forms may naturally
// follow a word (`Merci:)`); letter-led aliases still require a token boundary.
const EMOJI_ALIASES: &[EmojiAlias] = &[
    EmojiAlias {
        text: ":)",
        shortcode: "smile",
    },
    EmojiAlias {
        text: ":-)",
        shortcode: "smile",
    },
    EmojiAlias {
        text: ":]",
        shortcode: "smile",
    },
    EmojiAlias {
        text: ";)",
        shortcode: "wink",
    },
    EmojiAlias {
        text: ";-)",
        shortcode: "wink",
    },
    EmojiAlias {
        text: ":D",
        shortcode: "grinning",
    },
    EmojiAlias {
        text: ":-D",
        shortcode: "grinning",
    },
    EmojiAlias {
        text: "xD",
        shortcode: "laughing",
    },
    EmojiAlias {
        text: "XD",
        shortcode: "laughing",
    },
    EmojiAlias {
        text: ":(",
        shortcode: "frown",
    },
    EmojiAlias {
        text: ":-(",
        shortcode: "frown",
    },
    EmojiAlias {
        text: ":'(",
        shortcode: "cry",
    },
    EmojiAlias {
        text: ":'-(",
        shortcode: "cry",
    },
    EmojiAlias {
        text: ":P",
        shortcode: "yum",
    },
    EmojiAlias {
        text: ":p",
        shortcode: "yum",
    },
    EmojiAlias {
        text: ":-P",
        shortcode: "yum",
    },
    EmojiAlias {
        text: ":-p",
        shortcode: "yum",
    },
    EmojiAlias {
        text: ":|",
        shortcode: "neutral",
    },
    EmojiAlias {
        text: ":-|",
        shortcode: "neutral",
    },
    EmojiAlias {
        text: ":*",
        shortcode: "kiss",
    },
    EmojiAlias {
        text: ":-*",
        shortcode: "kiss",
    },
    EmojiAlias {
        text: "<3",
        shortcode: "heart",
    },
    EmojiAlias {
        text: ":+1:",
        shortcode: "thumbsup",
    },
    EmojiAlias {
        text: ":-1:",
        shortcode: "thumbsdown",
    },
];

// Popular entries come first: a bare `:` doubles as a useful compact picker.
// This intentionally focuses on mail/conversation use rather than exposing
// every Unicode flag, skin-tone sequence, or obscure symbol.
const EMOJIS: &[Emoji] = &[
    Emoji {
        glyph: "🙂",
        shortcode: "smile",
        keywords: "happy heureux sourire content simple",
    },
    Emoji {
        glyph: "😂",
        shortcode: "joy",
        keywords: "laugh rire larmes tears funny drole mdr lol",
    },
    Emoji {
        glyph: "❤️",
        shortcode: "heart",
        keywords: "love amour coeur rouge red",
    },
    Emoji {
        glyph: "👍",
        shortcode: "thumbsup",
        keywords: "like pouce oui yes ok approve accord +1",
    },
    Emoji {
        glyph: "😉",
        shortcode: "wink",
        keywords: "clin oeil complice",
    },
    Emoji {
        glyph: "🎉",
        shortcode: "tada",
        keywords: "party fete celebration felicitation congrats bravo",
    },
    Emoji {
        glyph: "😍",
        shortcode: "heart_eyes",
        keywords: "love amour yeux coeur adore",
    },
    Emoji {
        glyph: "🤔",
        shortcode: "thinking",
        keywords: "think penser reflexion question hmm",
    },
    Emoji {
        glyph: "😢",
        shortcode: "cry",
        keywords: "sad triste pleure larme tear",
    },
    Emoji {
        glyph: "😭",
        shortcode: "sob",
        keywords: "cry pleurer triste larmes tears",
    },
    Emoji {
        glyph: "🙁",
        shortcode: "frown",
        keywords: "sad triste malheureux unhappy moue",
    },
    Emoji {
        glyph: "😄",
        shortcode: "grinning",
        keywords: "happy heureux sourire grin",
    },
    Emoji {
        glyph: "😃",
        shortcode: "smiley",
        keywords: "happy heureux sourire joie",
    },
    Emoji {
        glyph: "😁",
        shortcode: "grin",
        keywords: "happy heureux sourire dents teeth",
    },
    Emoji {
        glyph: "🤣",
        shortcode: "rofl",
        keywords: "laugh rire sol floor mdr lol drole",
    },
    Emoji {
        glyph: "😅",
        shortcode: "sweat_smile",
        keywords: "relief soulagement sueur nerveux nervous",
    },
    Emoji {
        glyph: "😆",
        shortcode: "laughing",
        keywords: "laugh rire heureux happy xd",
    },
    Emoji {
        glyph: "😋",
        shortcode: "yum",
        keywords: "delicious delicieux langue tongue food manger",
    },
    Emoji {
        glyph: "😎",
        shortcode: "sunglasses",
        keywords: "cool soleil sun lunettes",
    },
    Emoji {
        glyph: "🥳",
        shortcode: "partying",
        keywords: "party fete celebrate anniversaire birthday",
    },
    Emoji {
        glyph: "🤩",
        shortcode: "star_struck",
        keywords: "wow etoile star excite impressed",
    },
    Emoji {
        glyph: "😘",
        shortcode: "kiss",
        keywords: "bisou amour love coeur",
    },
    Emoji {
        glyph: "🤗",
        shortcode: "hug",
        keywords: "calin embrasser soutien support",
    },
    Emoji {
        glyph: "🤭",
        shortcode: "giggle",
        keywords: "rire secret oups oops hand mouth",
    },
    Emoji {
        glyph: "🫡",
        shortcode: "salute",
        keywords: "salut respect yes monsieur",
    },
    Emoji {
        glyph: "🤫",
        shortcode: "shushing",
        keywords: "silence chut secret quiet",
    },
    Emoji {
        glyph: "🤨",
        shortcode: "raised_eyebrow",
        keywords: "doute sceptique skeptical vraiment really",
    },
    Emoji {
        glyph: "😐",
        shortcode: "neutral",
        keywords: "neutre bof blank expression",
    },
    Emoji {
        glyph: "😑",
        shortcode: "expressionless",
        keywords: "sans expression indifferent indifferent",
    },
    Emoji {
        glyph: "🙄",
        shortcode: "roll_eyes",
        keywords: "yeux ciel agace annoyed whatever",
    },
    Emoji {
        glyph: "😬",
        shortcode: "grimacing",
        keywords: "gene awkward ouch dents teeth",
    },
    Emoji {
        glyph: "🤥",
        shortcode: "lying",
        keywords: "mensonge mentir lie pinocchio",
    },
    Emoji {
        glyph: "😌",
        shortcode: "relieved",
        keywords: "soulage calme calm peaceful",
    },
    Emoji {
        glyph: "😴",
        shortcode: "sleeping",
        keywords: "dormir sommeil fatigue tired zzz",
    },
    Emoji {
        glyph: "🤒",
        shortcode: "sick",
        keywords: "malade sick thermometre fever fievre",
    },
    Emoji {
        glyph: "🤕",
        shortcode: "hurt",
        keywords: "blesse bandage douleur hurt",
    },
    Emoji {
        glyph: "🤢",
        shortcode: "nauseated",
        keywords: "malade nausee sick vert green",
    },
    Emoji {
        glyph: "🤯",
        shortcode: "mind_blown",
        keywords: "choque shocked wow incroyable explode",
    },
    Emoji {
        glyph: "😕",
        shortcode: "confused",
        keywords: "confus perdu puzzled question",
    },
    Emoji {
        glyph: "😟",
        shortcode: "worried",
        keywords: "inquiet worry anxious anxieux",
    },
    Emoji {
        glyph: "😱",
        shortcode: "scream",
        keywords: "peur fear horreur shock crie",
    },
    Emoji {
        glyph: "😠",
        shortcode: "angry",
        keywords: "fache colere mad enerve",
    },
    Emoji {
        glyph: "🤬",
        shortcode: "rage",
        keywords: "furieux colere angry swear",
    },
    Emoji {
        glyph: "🥺",
        shortcode: "pleading",
        keywords: "supplie please pitié yeux puppy",
    },
    Emoji {
        glyph: "👎",
        shortcode: "thumbsdown",
        keywords: "dislike pouce non no reject desaccord -1",
    },
    Emoji {
        glyph: "👌",
        shortcode: "ok_hand",
        keywords: "ok parfait perfect accord bien",
    },
    Emoji {
        glyph: "✌️",
        shortcode: "v",
        keywords: "peace paix victoire victory two deux",
    },
    Emoji {
        glyph: "🤞",
        shortcode: "crossed_fingers",
        keywords: "chance luck espoir hope doigts",
    },
    Emoji {
        glyph: "👏",
        shortcode: "clap",
        keywords: "applaudir bravo applause felicitation",
    },
    Emoji {
        glyph: "🙌",
        shortcode: "raised_hands",
        keywords: "bravo celebration mains hourra hooray",
    },
    Emoji {
        glyph: "🙏",
        shortcode: "pray",
        keywords: "merci please prie prayer gratitude mains",
    },
    Emoji {
        glyph: "👋",
        shortcode: "wave",
        keywords: "bonjour salut hello bye revoir main",
    },
    Emoji {
        glyph: "🤝",
        shortcode: "handshake",
        keywords: "accord deal partenariat partner merci",
    },
    Emoji {
        glyph: "💪",
        shortcode: "muscle",
        keywords: "force strength courage fort biceps",
    },
    Emoji {
        glyph: "👀",
        shortcode: "eyes",
        keywords: "regarde look voir attention watch",
    },
    Emoji {
        glyph: "💡",
        shortcode: "bulb",
        keywords: "idee idea lumiere light astuce tip",
    },
    Emoji {
        glyph: "✅",
        shortcode: "check",
        keywords: "fait done oui yes valide complete termine",
    },
    Emoji {
        glyph: "❌",
        shortcode: "x",
        keywords: "non no faux wrong erreur error annule",
    },
    Emoji {
        glyph: "⚠️",
        shortcode: "warning",
        keywords: "attention avertissement danger alerte alert",
    },
    Emoji {
        glyph: "❓",
        shortcode: "question",
        keywords: "question aide help pourquoi why",
    },
    Emoji {
        glyph: "❗",
        shortcode: "exclamation",
        keywords: "important attention urgence urgent",
    },
    Emoji {
        glyph: "🔥",
        shortcode: "fire",
        keywords: "feu hot chaud super tendance lit",
    },
    Emoji {
        glyph: "⭐",
        shortcode: "star",
        keywords: "etoile favori favorite important",
    },
    Emoji {
        glyph: "✨",
        shortcode: "sparkles",
        keywords: "brille sparkle nouveau magic magique propre",
    },
    Emoji {
        glyph: "💯",
        shortcode: "hundred",
        keywords: "cent parfait perfect score vrai true",
    },
    Emoji {
        glyph: "💥",
        shortcode: "boom",
        keywords: "explosion impact wow bombe",
    },
    Emoji {
        glyph: "💜",
        shortcode: "purple_heart",
        keywords: "amour love coeur violet purple",
    },
    Emoji {
        glyph: "💙",
        shortcode: "blue_heart",
        keywords: "amour love coeur bleu blue",
    },
    Emoji {
        glyph: "💚",
        shortcode: "green_heart",
        keywords: "amour love coeur vert green",
    },
    Emoji {
        glyph: "💛",
        shortcode: "yellow_heart",
        keywords: "amour love coeur jaune yellow",
    },
    Emoji {
        glyph: "💔",
        shortcode: "broken_heart",
        keywords: "coeur brise rupture sad triste",
    },
    Emoji {
        glyph: "💌",
        shortcode: "love_letter",
        keywords: "lettre amour love mail coeur",
    },
    Emoji {
        glyph: "🎂",
        shortcode: "birthday",
        keywords: "anniversaire gateau cake bougies fete",
    },
    Emoji {
        glyph: "🎁",
        shortcode: "gift",
        keywords: "cadeau present anniversaire birthday",
    },
    Emoji {
        glyph: "🎈",
        shortcode: "balloon",
        keywords: "ballon fete party anniversaire",
    },
    Emoji {
        glyph: "🥂",
        shortcode: "cheers",
        keywords: "sante toast fete verres celebrate",
    },
    Emoji {
        glyph: "☕",
        shortcode: "coffee",
        keywords: "cafe boisson pause matin morning",
    },
    Emoji {
        glyph: "🍻",
        shortcode: "beer",
        keywords: "biere verre drink sante cheers",
    },
    Emoji {
        glyph: "🍕",
        shortcode: "pizza",
        keywords: "pizza manger food repas",
    },
    Emoji {
        glyph: "🍽️",
        shortcode: "meal",
        keywords: "repas manger food restaurant assiette",
    },
    Emoji {
        glyph: "🚀",
        shortcode: "rocket",
        keywords: "fusee launch lancement rapide fast projet",
    },
    Emoji {
        glyph: "🎯",
        shortcode: "target",
        keywords: "cible objectif goal bullseye focus",
    },
    Emoji {
        glyph: "🏆",
        shortcode: "trophy",
        keywords: "trophee victoire winner gagnant succes success",
    },
    Emoji {
        glyph: "🏅",
        shortcode: "medal",
        keywords: "medaille victoire award prix bravo",
    },
    Emoji {
        glyph: "📌",
        shortcode: "pin",
        keywords: "epingle important location note",
    },
    Emoji {
        glyph: "📅",
        shortcode: "calendar",
        keywords: "calendrier date rendez vous meeting",
    },
    Emoji {
        glyph: "⏰",
        shortcode: "alarm",
        keywords: "alarme heure time rappel reminder reveil",
    },
    Emoji {
        glyph: "⌛",
        shortcode: "hourglass",
        keywords: "attente wait temps time sablier",
    },
    Emoji {
        glyph: "📧",
        shortcode: "email",
        keywords: "mail message courrier envoyer send",
    },
    Emoji {
        glyph: "📩",
        shortcode: "inbox",
        keywords: "boite reception mail message received",
    },
    Emoji {
        glyph: "📢",
        shortcode: "loudspeaker",
        keywords: "annonce announcement megaphone important",
    },
    Emoji {
        glyph: "🔔",
        shortcode: "bell",
        keywords: "cloche notification rappel reminder alerte",
    },
    Emoji {
        glyph: "🔗",
        shortcode: "link",
        keywords: "lien url chaine chain attach",
    },
    Emoji {
        glyph: "📎",
        shortcode: "paperclip",
        keywords: "trombone piece jointe attachment fichier file",
    },
    Emoji {
        glyph: "📝",
        shortcode: "memo",
        keywords: "note ecrire write document crayon",
    },
    Emoji {
        glyph: "📊",
        shortcode: "chart",
        keywords: "graphique stats donnees data croissance",
    },
    Emoji {
        glyph: "💼",
        shortcode: "briefcase",
        keywords: "travail work bureau business pro",
    },
    Emoji {
        glyph: "🔒",
        shortcode: "lock",
        keywords: "verrou securite security prive private",
    },
    Emoji {
        glyph: "🔓",
        shortcode: "unlock",
        keywords: "deverrouille open ouvert public",
    },
    Emoji {
        glyph: "🛠️",
        shortcode: "tools",
        keywords: "outils reparer fix maintenance travail",
    },
    Emoji {
        glyph: "🐛",
        shortcode: "bug",
        keywords: "insecte probleme erreur issue debug",
    },
    Emoji {
        glyph: "🌍",
        shortcode: "earth",
        keywords: "terre monde world globe international",
    },
    Emoji {
        glyph: "☀️",
        shortcode: "sunny",
        keywords: "soleil beau temps meteo weather happy",
    },
    Emoji {
        glyph: "🌧️",
        shortcode: "rain",
        keywords: "pluie meteo weather triste nuage",
    },
    Emoji {
        glyph: "🌱",
        shortcode: "seedling",
        keywords: "plante pousse croissance grow nature vert",
    },
    Emoji {
        glyph: "🐶",
        shortcode: "dog",
        keywords: "chien animal pet puppy",
    },
    Emoji {
        glyph: "🐱",
        shortcode: "cat",
        keywords: "chat animal pet kitten",
    },
    Emoji {
        glyph: "🐦",
        shortcode: "bird",
        keywords: "oiseau animal voler fly aviary",
    },
];

pub(super) fn completion_provider() -> BlockCompletionProvider {
    Rc::new(completions)
}

/// Finds a `:query` ending exactly at the cursor. A boundary is required before
/// the colon so times (`10:30`), URLs, and ordinary words do not open a menu.
fn current_token(before_cursor: &str) -> Option<(usize, &str)> {
    let colon = before_cursor.rfind(':')?;
    if let Some(previous) = before_cursor[..colon].chars().next_back() {
        if previous.is_alphanumeric() || matches!(previous, '_' | ':') {
            return None;
        }
    }
    let query = &before_cursor[colon + 1..];
    if query.len() > MAX_QUERY_BYTES
        || !query
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-' | '+'))
    {
        return None;
    }
    Some((colon, query))
}

fn emoji_for_shortcode(shortcode: &str) -> Option<&'static Emoji> {
    EMOJIS
        .iter()
        .find(|emoji| emoji.shortcode.eq_ignore_ascii_case(shortcode))
}

fn has_token_boundary(value: &str, start: usize) -> bool {
    value[..start]
        .chars()
        .next_back()
        .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_')
}

fn current_alias(before_cursor: &str) -> Option<(usize, &'static Emoji)> {
    EMOJI_ALIASES.iter().find_map(|alias| {
        // `len()` is measured in bytes. Subtracting the alias length first can
        // therefore land inside the final Unicode scalar of ordinary text
        // (for example `"coût "`) and make the following slice panic. Let the
        // UTF-8-aware string API validate the suffix before deriving its byte
        // offset.
        let prefix = before_cursor.strip_suffix(alias.text)?;
        let start = prefix.len();

        // Avoid matching the letter-led forms inside a word (for example the
        // final `XD` in an identifier) and `<3` inside a numeric comparison.
        let first = alias.text.chars().next()?;
        if (first.is_alphanumeric() || first == '<') && !has_token_boundary(before_cursor, start) {
            return None;
        }

        emoji_for_shortcode(alias.shortcode).map(|emoji| (start, emoji))
    })
}

fn current_closed_shortcode(before_cursor: &str) -> Option<(usize, &'static Emoji)> {
    let without_closing_colon = before_cursor.strip_suffix(':')?;
    let start = without_closing_colon.rfind(':')?;
    if !has_token_boundary(before_cursor, start) {
        return None;
    }
    let shortcode = &without_closing_colon[start + 1..];
    if shortcode.is_empty()
        || shortcode.len() > MAX_QUERY_BYTES
        || !shortcode
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-' | '+'))
    {
        return None;
    }
    emoji_for_shortcode(shortcode).map(|emoji| (start, emoji))
}

enum ActiveCompletion<'a> {
    Search { start: usize, query: &'a str },
    Exact { start: usize, emoji: &'static Emoji },
}

fn active_completion(before_cursor: &str) -> Option<ActiveCompletion<'_>> {
    if let Some((start, emoji)) = current_alias(before_cursor) {
        return Some(ActiveCompletion::Exact { start, emoji });
    }
    if let Some((start, emoji)) = current_closed_shortcode(before_cursor) {
        return Some(ActiveCompletion::Exact { start, emoji });
    }
    current_token(before_cursor).map(|(start, query)| ActiveCompletion::Search { start, query })
}

fn visible_aliases(emoji: &Emoji) -> String {
    EMOJI_ALIASES
        .iter()
        .filter(|alias| alias.shortcode == emoji.shortcode && !alias.text.ends_with(':'))
        .take(2)
        .map(|alias| alias.text)
        .collect::<Vec<_>>()
        .join(" · ")
}

fn match_score(emoji: &Emoji, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let query = query.to_lowercase();
    let shortcode = emoji.shortcode.to_lowercase();
    let words = emoji.keywords.split_ascii_whitespace();
    if shortcode == query || words.clone().any(|word| word == query) {
        Some(0)
    } else if shortcode.starts_with(&query) {
        Some(1)
    } else if words.clone().any(|word| word.starts_with(&query)) {
        Some(2)
    } else if shortcode.contains(&query) {
        Some(3)
    } else if emoji.keywords.contains(&query) {
        Some(4)
    } else {
        None
    }
}

fn matching_emojis(query: &str) -> Vec<&'static Emoji> {
    let mut matches: Vec<(u8, usize, &Emoji)> = EMOJIS
        .iter()
        .enumerate()
        .filter_map(|(index, emoji)| match_score(emoji, query).map(|score| (score, index, emoji)))
        .collect();
    matches.sort_by_key(|(score, index, _)| (*score, *index));
    matches
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(_, _, emoji)| emoji)
        .collect()
}

fn completions(source: &str, offset: usize) -> Vec<BlockCompletionItem> {
    let mut offset = offset.min(source.len());
    while !source.is_char_boundary(offset) {
        offset = offset.saturating_sub(1);
    }
    let Some(active) = active_completion(&source[..offset]) else {
        return Vec::new();
    };
    let (start, matches) = match active {
        ActiveCompletion::Search { start, query } => (start, matching_emojis(query)),
        ActiveCompletion::Exact { start, emoji } => (start, vec![emoji]),
    };
    matches
        .into_iter()
        .map(|emoji| {
            let aliases = visible_aliases(emoji);
            let shortcut = if aliases.is_empty() {
                format!(":{}:", emoji.shortcode)
            } else {
                format!(":{}: · {aliases}", emoji.shortcode)
            };
            BlockCompletionItem {
                range: start..offset,
                label: emoji.glyph.into(),
                detail: tr!("compose-emoji-completion-detail", {
                    shortcode: shortcut
                }),
                replacement: emoji.glyph.into(),
                on_accept: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        active_completion, current_alias, current_closed_shortcode, current_token, matching_emojis,
        visible_aliases, ActiveCompletion,
    };

    #[test]
    fn recognizes_colon_token_only_at_a_boundary() {
        assert_eq!(current_token(":sou"), Some((0, "sou")));
        assert_eq!(current_token("Bonjour :coeur"), Some((8, "coeur")));
        assert_eq!(current_token("( :thumbs_up"), Some((2, "thumbs_up")));
        assert_eq!(current_token("10:30"), None);
        assert_eq!(current_token("https://example.test"), None);
        assert_eq!(current_token("mot:smile"), None);
        assert_eq!(current_token(":smile maintenant"), None);
    }

    #[test]
    fn filters_with_french_and_english_keywords() {
        assert_eq!(matching_emojis("sourire")[0].shortcode, "smile");
        assert_eq!(matching_emojis("laugh")[0].shortcode, "joy");
        assert_eq!(matching_emojis("coeur")[0].shortcode, "heart");
        assert_eq!(matching_emojis("piece")[0].shortcode, "paperclip");
    }

    #[test]
    fn ranks_exact_and_shortcode_prefix_matches_first() {
        assert_eq!(matching_emojis("heart")[0].shortcode, "heart");
        assert_eq!(matching_emojis("thumbsdown")[0].shortcode, "thumbsdown");
        assert!(matching_emojis("introuvable").is_empty());
    }

    #[test]
    fn bare_colon_returns_a_compact_popular_picker() {
        let matches = matching_emojis("");
        assert_eq!(matches.len(), 10);
        assert_eq!(matches[0].shortcode, "smile");
        assert_eq!(matches[1].shortcode, "joy");
    }

    #[test]
    fn recognizes_common_text_emoticons() {
        assert_eq!(current_alias(":)").unwrap().1.shortcode, "smile");
        assert_eq!(current_alias("Merci:-)").unwrap().1.shortcode, "smile");
        assert_eq!(current_alias(";)").unwrap().1.shortcode, "wink");
        assert_eq!(current_alias(":D").unwrap().1.shortcode, "grinning");
        assert_eq!(current_alias(":('").map(|(_, emoji)| emoji.shortcode), None);
        assert_eq!(current_alias(":'(").unwrap().1.shortcode, "cry");
        assert_eq!(current_alias("<3").unwrap().1.shortcode, "heart");
        assert_eq!(current_alias("prefixXD"), None);
        assert_eq!(current_alias("x<3"), None);
    }

    #[test]
    fn alias_detection_is_safe_around_multibyte_text() {
        assert_eq!(current_alias("Le coût "), None);
        assert_eq!(current_alias("Bien sûr"), None);
        assert_eq!(current_alias("Bien sûr :) "), None);
        assert_eq!(
            current_alias("Bien sûr :)").map(|(_, emoji)| emoji.shortcode),
            Some("smile")
        );
    }

    #[test]
    fn completed_shortcode_remains_selectable() {
        assert_eq!(
            current_closed_shortcode("Bonjour :smile:").unwrap().1.glyph,
            "🙂"
        );
        assert!(current_closed_shortcode("mot:smile:").is_none());
        assert!(matches!(
            active_completion(":)"),
            Some(ActiveCompletion::Exact { emoji, .. }) if emoji.shortcode == "smile"
        ));
    }

    #[test]
    fn exposes_the_most_useful_aliases_in_details() {
        let smile = matching_emojis("smile")[0];
        assert_eq!(visible_aliases(smile), ":) · :-)");
    }
}
