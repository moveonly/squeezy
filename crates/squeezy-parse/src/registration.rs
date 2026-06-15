use std::collections::{HashMap, hash_map::Entry};

use squeezy_core::{LanguageKind, Result, SqueezyError};
use tree_sitter::Parser;

#[derive(Clone, Copy)]
struct LanguageRegistration {
    kind: LanguageKind,
    display_name: &'static str,
    grammar: fn() -> tree_sitter::Language,
}

const LANGUAGE_REGISTRATIONS: &[LanguageRegistration] = &[
    LanguageRegistration {
        kind: LanguageKind::C,
        display_name: "C",
        grammar: c_language,
    },
    LanguageRegistration {
        kind: LanguageKind::CSharp,
        display_name: "C#",
        grammar: csharp_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Cpp,
        display_name: "C++",
        grammar: cpp_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Dart,
        display_name: "Dart",
        grammar: dart_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Go,
        display_name: "Go",
        grammar: go_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Java,
        display_name: "Java",
        grammar: java_language,
    },
    LanguageRegistration {
        kind: LanguageKind::JavaScript,
        display_name: "JavaScript",
        grammar: javascript_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Jsx,
        display_name: "JSX",
        grammar: jsx_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Kotlin,
        display_name: "Kotlin",
        grammar: kotlin_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Php,
        display_name: "PHP",
        grammar: php_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Python,
        display_name: "Python",
        grammar: python_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Ruby,
        display_name: "Ruby",
        grammar: ruby_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Rust,
        display_name: "Rust",
        grammar: rust_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Scala,
        display_name: "Scala",
        grammar: scala_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Swift,
        display_name: "Swift",
        grammar: swift_language,
    },
    LanguageRegistration {
        kind: LanguageKind::TypeScript,
        display_name: "TypeScript",
        grammar: typescript_language,
    },
    LanguageRegistration {
        kind: LanguageKind::Tsx,
        display_name: "TSX",
        grammar: tsx_language,
    },
];

#[derive(Default)]
pub(crate) struct ParserPool {
    parsers: HashMap<LanguageKind, Parser>,
}

impl ParserPool {
    pub(crate) fn parser_for_language(&mut self, language: LanguageKind) -> Result<&mut Parser> {
        match self.parsers.entry(language) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let parser = parser_for_language_kind(language)?;
                Ok(entry.insert(parser))
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.parsers.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn contains_language(&self, language: LanguageKind) -> bool {
        self.parsers.contains_key(&language)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.parsers.len()
    }
}

pub(crate) fn parser_for_language_kind(language: LanguageKind) -> Result<Parser> {
    let registration = registration_for_kind(language)
        .ok_or_else(|| SqueezyError::Parse(format!("unsupported parser language {language:?}")))?;
    let mut parser = Parser::new();
    let grammar = (registration.grammar)();
    parser.set_language(&grammar).map_err(|err| {
        SqueezyError::Parse(format!(
            "failed to load {} grammar: {err}",
            registration.display_name
        ))
    })?;
    Ok(parser)
}

pub(crate) fn language_for_kind(language: LanguageKind) -> Option<tree_sitter::Language> {
    registration_for_kind(language).map(|registration| (registration.grammar)())
}

fn registration_for_kind(language: LanguageKind) -> Option<&'static LanguageRegistration> {
    LANGUAGE_REGISTRATIONS
        .iter()
        .find(|registration| registration.kind == language)
}

fn csharp_language() -> tree_sitter::Language {
    tree_sitter_c_sharp::LANGUAGE.into()
}

fn go_language() -> tree_sitter::Language {
    tree_sitter_go::LANGUAGE.into()
}

fn rust_language() -> tree_sitter::Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn java_language() -> tree_sitter::Language {
    tree_sitter_java::LANGUAGE.into()
}

fn kotlin_language() -> tree_sitter::Language {
    tree_sitter_kotlin_ng::LANGUAGE.into()
}

pub(crate) fn scala_language() -> tree_sitter::Language {
    tree_sitter_scala::LANGUAGE.into()
}

fn swift_language() -> tree_sitter::Language {
    tree_sitter_swift::LANGUAGE.into()
}

fn python_language() -> tree_sitter::Language {
    tree_sitter_python::LANGUAGE.into()
}

fn ruby_language() -> tree_sitter::Language {
    tree_sitter_ruby::LANGUAGE.into()
}

fn javascript_language() -> tree_sitter::Language {
    tree_sitter_javascript::LANGUAGE.into()
}

fn jsx_language() -> tree_sitter::Language {
    tree_sitter_javascript::LANGUAGE.into()
}

fn php_language() -> tree_sitter::Language {
    // PHP files routinely interleave inline HTML and `<?php ... ?>` blocks
    // even when the file is "pure PHP", so the mixed-template grammar is the
    // single right choice for the workspace. Pure-PHP files still parse fine
    // under `LANGUAGE_PHP` (the leading `<?php` is just another `php_tag`).
    tree_sitter_php::LANGUAGE_PHP.into()
}

fn typescript_language() -> tree_sitter::Language {
    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
}

fn tsx_language() -> tree_sitter::Language {
    tree_sitter_typescript::LANGUAGE_TSX.into()
}

fn c_language() -> tree_sitter::Language {
    tree_sitter_c::LANGUAGE.into()
}

fn cpp_language() -> tree_sitter::Language {
    tree_sitter_cpp::LANGUAGE.into()
}

fn dart_language() -> tree_sitter::Language {
    tree_sitter_dart::LANGUAGE.into()
}
