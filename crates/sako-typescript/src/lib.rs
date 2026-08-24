// SPDX-License-Identifier: BSD-3-Clause

use std::error::Error;
use std::fmt;
use std::path::Path;

use deno_ast::{
    EmitOptions, MediaType, ModuleKind, ModuleSpecifier, ParseParams, SourceMapOption,
    TranspileModuleOptions, TranspileOptions, parse_program,
};

pub const MAXIMUM_TYPESCRIPT_SOURCE_BYTES: usize = 16 * 1024 * 1024;
pub const MAXIMUM_JAVASCRIPT_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputModuleKind {
    Esm,
    CommonJs,
}

#[derive(Debug)]
pub struct TypeScriptError(String);

impl fmt::Display for TypeScriptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for TypeScriptError {}

pub fn is_typescript_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "ts" | "mts" | "cts" | "tsx"
            )
        })
}

pub fn transpile(
    path: &Path,
    source: &str,
    output_kind: OutputModuleKind,
) -> Result<String, TypeScriptError> {
    if source.len() > MAXIMUM_TYPESCRIPT_SOURCE_BYTES {
        return Err(TypeScriptError(format!(
            "{}: TypeScript source exceeds the {} byte limit",
            path.display(),
            MAXIMUM_TYPESCRIPT_SOURCE_BYTES
        )));
    }
    if is_declaration_path(path) {
        return Err(TypeScriptError(format!(
            "{}: TypeScript declaration files are not executable",
            path.display()
        )));
    }
    let media_type = media_type(path).ok_or_else(|| {
        TypeScriptError(format!(
            "{}: unsupported TypeScript file extension",
            path.display()
        ))
    })?;
    let specifier = ModuleSpecifier::from_file_path(path).map_err(|_| {
        TypeScriptError(format!(
            "{}: cannot create a TypeScript module URL",
            path.display()
        ))
    })?;
    let parsed = parse_program(ParseParams {
        specifier,
        text: source.into(),
        media_type,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|error| TypeScriptError(error.to_string()))?;
    let emitted = parsed
        .transpile(
            &TranspileOptions::default(),
            &TranspileModuleOptions {
                module_kind: Some(match output_kind {
                    OutputModuleKind::Esm => ModuleKind::Esm,
                    OutputModuleKind::CommonJs => ModuleKind::Cjs,
                }),
            },
            &EmitOptions {
                source_map: SourceMapOption::None,
                inline_sources: false,
                ..EmitOptions::default()
            },
        )
        .map_err(|error| TypeScriptError(format!("{}: {error}", path.display())))?
        .into_source()
        .text;
    if emitted.len() > MAXIMUM_JAVASCRIPT_OUTPUT_BYTES {
        return Err(TypeScriptError(format!(
            "{}: transpiled JavaScript exceeds the {} byte limit",
            path.display(),
            MAXIMUM_JAVASCRIPT_OUTPUT_BYTES
        )));
    }
    Ok(emitted)
}

fn is_declaration_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name.ends_with(".d.ts") || name.ends_with(".d.mts") || name.ends_with(".d.cts")
        })
}

fn media_type(path: &Path) -> Option<MediaType> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase()
        .as_str()
    {
        "ts" => Some(MediaType::TypeScript),
        "mts" => Some(MediaType::Mts),
        "cts" => Some(MediaType::Cts),
        "tsx" => Some(MediaType::Tsx),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transpiles_types_enums_and_namespaces() {
        let source = r#"
interface User { name: string }
enum Mode { Fast = "fast" }
namespace Values { export const answer: number = 42; }
class Box { constructor(public value: number) {} }
const user = { name: "Sako" } satisfies User;
console.log(user.name, Mode.Fast, Values.answer, new Box(1).value);
"#;
        let output = transpile(
            Path::new("C:/project/main.ts"),
            source,
            OutputModuleKind::Esm,
        )
        .unwrap();
        assert!(!output.contains("interface User"));
        assert!(!output.contains("satisfies User"));
        assert!(output.contains("Mode"));
        assert!(output.contains("Values"));
        assert!(output.contains("this.value = value"));
    }

    #[test]
    fn transpiles_tsx_and_commonjs_syntax() {
        let tsx = transpile(
            Path::new("C:/project/view.tsx"),
            "const view = <div id=\"sako\">ok</div>; export default view;",
            OutputModuleKind::Esm,
        )
        .unwrap();
        assert!(tsx.contains("React.createElement"));

        let cjs = transpile(
            Path::new("C:/project/value.cts"),
            "type Value = number; const value: Value = 42; module.exports = value;",
            OutputModuleKind::CommonJs,
        )
        .unwrap();
        assert!(!cjs.contains("type Value"));
        assert!(cjs.contains("module.exports = value"));
    }

    #[test]
    fn reports_typescript_syntax_locations() {
        let error = transpile(
            Path::new("C:/project/broken.ts"),
            "const value: = 42;",
            OutputModuleKind::Esm,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("broken.ts"), "{message}");
        assert!(message.contains("1:"), "{message}");
    }

    #[test]
    fn rejects_declaration_files_as_runtime_modules() {
        let error = transpile(
            Path::new("C:/project/types.d.ts"),
            "export interface Value { answer: number }",
            OutputModuleKind::Esm,
        )
        .unwrap_err();
        assert!(error.to_string().contains("not executable"));
    }
}
