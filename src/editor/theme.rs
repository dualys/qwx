use crossterm::style::Color;

pub const FG_DEFAULT: Color = Color::Rgb {
    r: 212,
    g: 212,
    b: 212,
};
pub const BG_BLACK: Color = Color::Rgb { r: 0, g: 0, b: 0 };

// --- PALETTE DU FINDER ---
pub const FINDER_BORDER: Color = Color::White;
pub const FINDER_TEXT_MUTED: Color = Color::White;
// Pour les éléments inactifs/secondaires
pub const FINDER_DIR_COLOR: Color = Color::Blue;
pub const FINDER_FILE_COLOR: Color = Color::White;
pub const FINDER_ACTIVE_SELECT: Color = Color::White;
pub const CONSTANTS: Color = Color::Rgb {
    r: 79,
    g: 193,
    b: 255,
};

// VS Code Dark+ syntax palette
pub const NAMESPACES: Color = Color::Rgb {
    r: 78,
    g: 201,
    b: 176,
};
pub const DIRECTIVES: Color = Color::Rgb {
    r: 197,
    g: 134,
    b: 192,
};
pub const COMMENTS: Color = Color::Rgb {
    r: 106,
    g: 153,
    b: 85,
};

pub const TYPES: Color = Color::Rgb {
    r: 78,
    g: 201,
    b: 176,
};

pub const FUNCTIONS: Color = Color::Rgb {
    r: 220,
    g: 220,
    b: 170,
};

pub const KEYWORDS: Color = Color::Rgb {
    r: 86,
    g: 156,
    b: 214,
};
pub const STRINGS: Color = Color::Rgb {
    r: 206,
    g: 145,
    b: 120,
};
const NUMBERS: Color = Color::Rgb {
    r: 181,
    g: 206,
    b: 168,
};

pub const UI_BORDER_INACTIVE: Color = Color::Rgb { r: 0, g: 0, b: 0 };

// Un violet cosmique très doux pour mettre en valeur le panneau actif sans agresser l'œil
pub const UI_BORDER_ACTIVE: Color = Color::Rgb {
    r: 130,
    g: 110,
    b: 190,
};

// Un gris-bleu fantôme pour les textes d'information (pourcentages, numéro de vue)
pub const UI_TEXT_MUTED: Color = Color::Rgb {
    r: 90,
    g: 100,
    b: 120,
};

// Pour la barre Menu : un fond sombre et un texte clair, fini le vert fluo !
pub const UI_DMENU_BG: Color = Color::Rgb { r: 0, g: 255, b: 0 };
pub const UI_DMENU_FG: Color = Color::Rgb { r: 0, g: 0, b: 0 };

pub fn get_color_for_capture(capture_name: &str) -> Color {
    match capture_name {
        "constant" | "constant.builtin" | "constant.language" | "constant.character.escape" => {
            CONSTANTS
        }

        "namespace" => NAMESPACES,

        "attribute" | "keyword.directive" => DIRECTIVES,
        "comment" | "line_comment" | "block_comment" => COMMENTS,

        "type" | "primitive_type" | "type.builtin" | "type.deprecated" => TYPES,

        "function" | "function.macro" | "function.method" | "method" => FUNCTIONS,

        "keyword" | "keyword.function" | "keyword.return" | "keyword.operator"
        | "keyword.control" => KEYWORDS,

        "string" | "string_literal" | "string.special" | "string.escape" | "character" => STRINGS,

        "number" | "integer" | "float" | "boolean" => NUMBERS,

        "operator" | "punctuation.bracket" | "punctuation.delimiter" => FG_DEFAULT,

        "variable" | "variable.parameter" | "variable.builtin" | "property" => FG_DEFAULT,

        _ => FG_DEFAULT,
    }
}
