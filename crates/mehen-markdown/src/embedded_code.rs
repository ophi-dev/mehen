// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! §9.4 embedded-code adjustment.
//!
//! For every supported fenced code block — `rust`, `ts`/`tsx`, `py`, `go`,
//! `rb`, `c`, `kotlin`, `pwsh`/`powershell` — run the fence body through
//! the source-language analysis pipeline and accumulate:
//!
//! ```text
//! embedded_volume = Σ 0.20 * sqrt(volume_c)
//!                 + 0.50 * cognitive_c
//!                 + 0.10 * loc_c
//! ```
//!
//! The dispatch is decoupled from this crate via [`set_embedded_dispatch`]:
//! the markdown crate doesn't depend on the per-language analyzers
//! directly. `mehen-engine` supplies a callback that maps a fence-language
//! code + body to numeric volume/cognitive/sloc.

use std::sync::OnceLock;

use crate::document::MarkdownDocument;

/// Languages a fenced code block can declare. Mirrors the pre-1.0
/// `LANG` enum, but kept local to this crate so we don't depend on
/// `mehen::langs` at compile time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FenceLanguage {
    Rust,
    Python,
    Typescript,
    Tsx,
    Go,
    Ruby,
    Kotlin,
    Java,
    Powershell,
    C,
    CSharp,
    Php,
}

/// Metrics extracted from one fenced code block. Returned by the
/// dispatch callback registered through [`set_embedded_dispatch`].
#[derive(Clone, Copy, Debug, Default)]
pub struct EmbeddedFenceMetrics {
    pub volume: f64,
    pub cognitive_sum: f64,
    pub sloc: f64,
}

type DispatchFn = fn(FenceLanguage, String) -> Option<EmbeddedFenceMetrics>;

static DISPATCH: OnceLock<DispatchFn> = OnceLock::new();

/// Register the embedded-code dispatch callback.
///
/// Called by `mehen_engine::init_markdown` at startup so the Markdown
/// analyzer can drive the language registry for fence bodies.
pub fn set_embedded_dispatch(f: DispatchFn) {
    let _ = DISPATCH.set(f);
}

/// Public entry: analyze every fenced code block whose language maps to a
/// supported [`FenceLanguage`] and sum the §9.4 contributions.
pub(crate) fn embedded_volume(document: &MarkdownDocument) -> f64 {
    let mut total = 0.0;
    for block in document
        .code_blocks
        .iter()
        .filter(|block| block.is_fenced())
    {
        let lang = block.language.as_deref().and_then(map_fence_to_lang);
        if let Some(lang) = lang {
            let mut body = block.content.clone();
            if matches!(lang, FenceLanguage::Php) {
                let leading = body.trim_start();
                if !leading.starts_with("<?php") && !leading.starts_with("<?=") {
                    body.insert_str(0, "<?php\n");
                }
            }
            total += analyze_fence(lang, body);
        }
    }
    total
}

fn analyze_fence(lang: FenceLanguage, body: String) -> f64 {
    let Some(dispatch) = DISPATCH.get() else {
        return 0.0;
    };
    let Some(m) = dispatch(lang, body) else {
        return 0.0;
    };
    let v = if m.volume.is_finite() && m.volume > 0.0 {
        0.20 * m.volume.sqrt()
    } else {
        0.0
    };
    let c = if m.cognitive_sum.is_finite() {
        0.50 * m.cognitive_sum
    } else {
        0.0
    };
    let l = if m.sloc.is_finite() {
        0.10 * m.sloc
    } else {
        0.0
    };
    v + c + l
}

fn map_fence_to_lang(info: &str) -> Option<FenceLanguage> {
    let head = info
        .split([' ', '\t', ','])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    Some(match head.as_str() {
        "rust" | "rs" => FenceLanguage::Rust,
        "python" | "py" => FenceLanguage::Python,
        "typescript" | "ts" => FenceLanguage::Typescript,
        "tsx" | "jsx" => FenceLanguage::Tsx,
        "javascript" | "js" => FenceLanguage::Typescript,
        "go" => FenceLanguage::Go,
        "ruby" | "rb" => FenceLanguage::Ruby,
        "kotlin" | "kt" | "kts" => FenceLanguage::Kotlin,
        "java" => FenceLanguage::Java,
        "powershell" | "pwsh" | "ps1" => FenceLanguage::Powershell,
        "c" => FenceLanguage::C,
        // `cs` and `csharp` are the tags GitHub and dotnet docs use; `csx` marks
        // a script fence, which Roslyn's `compilation_unit` accepts via
        // `global_statement`.
        "csharp" | "cs" | "c#" | "csx" => FenceLanguage::CSharp,
        "php" => FenceLanguage::Php,
        _ => return None,
    })
}
