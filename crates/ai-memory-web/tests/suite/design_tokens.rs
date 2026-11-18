//! The Memory Archive palette reaches templates only through token utilities
//! (`text-muted`, `bg-tint`, `data-hue`, ...), which follow the theme on
//! their own. A stock Tailwind shade or a `dark:` color pair in a template
//! would bypass the tokens and drift from the site (docs/ui-design-system.md).

use std::path::Path;

const STOCK_PALETTES: [&str; 22] = [
    "slate", "gray", "zinc", "neutral", "stone", "red", "orange", "amber", "yellow", "lime",
    "green", "emerald", "teal", "cyan", "sky", "blue", "indigo", "violet", "purple", "fuchsia",
    "pink", "rose",
];
const COLOR_UTILITIES: [&str; 8] = [
    "text-",
    "bg-",
    "border-",
    "divide-",
    "ring-",
    "placeholder-",
    "outline-",
    "fill-",
];

fn templates() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("templates")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|ext| ext == "html") {
                let text = std::fs::read_to_string(&path).unwrap();
                out.push((path.display().to_string(), text));
            }
        }
    }
    assert!(out.len() > 20, "templates not found");
    out
}

/// A class token such as `text-slate-500`, `hover:bg-blue-600/40` or
/// `dark:text-amber-300`: a color utility on a stock palette shade.
fn stock_shade(token: &str) -> bool {
    let bare = token.rsplit(':').next().unwrap_or(token);
    COLOR_UTILITIES.iter().any(|utility| {
        bare.strip_prefix(utility).is_some_and(|rest| {
            STOCK_PALETTES.iter().any(|palette| {
                rest.strip_prefix(palette).is_some_and(|shade| {
                    shade
                        .strip_prefix('-')
                        .is_some_and(|n| n.chars().next().is_some_and(|c| c.is_ascii_digit()))
                })
            })
        })
    })
}

#[test]
fn templates_use_palette_tokens_not_stock_shades() {
    let mut offenders = Vec::new();
    for (path, text) in templates() {
        for token in text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
            if stock_shade(token) {
                offenders.push(format!("{path}: {token}"));
            }
        }
    }
    assert!(offenders.is_empty(), "{offenders:#?}");
}

#[test]
fn templates_carry_no_literal_colors() {
    let mut offenders = Vec::new();
    for (path, text) in templates() {
        for line in text.lines() {
            // The browser chrome color is set before CSS loads, so these two
            // meta tags repeat the --ma-bg values by necessity.
            if line.contains("name=\"theme-color\"") {
                continue;
            }
            for attribute in ["fill=\"#", "stroke=\"#", "color:#", "background:#"] {
                if line.contains(attribute) {
                    offenders.push(format!("{path}: {}", line.trim()));
                }
            }
        }
    }
    assert!(offenders.is_empty(), "{offenders:#?}");
}

#[test]
fn the_guard_recognises_stock_shades() {
    for token in [
        "text-slate-500",
        "dark:bg-slate-900",
        "hover:text-blue-600",
        "bg-amber-950/30",
    ] {
        assert!(stock_shade(token), "{token}");
    }
    for token in [
        "text-muted",
        "bg-tint",
        "text-green",
        "bg-green-soft",
        "text-amber-ink",
        "text-sm",
        "border-b",
    ] {
        assert!(!stock_shade(token), "{token}");
    }
}
