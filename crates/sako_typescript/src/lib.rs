//! TypeScript support for Sako: turns `.ts` / `.mts` / `.cts` / `.tsx` into
//! JavaScript with [oxc](https://oxc.rs) (type stripping plus lowering of
//! TS-only syntax such as `enum`, `namespace` and parameter properties; TSX
//! also gets the JSX transform). No type checking, and no lowering of modern
//! JavaScript: V8 runs it as is.
//!
//! [`transpile`] also returns a [`LineMap`] so the runtime can report stack
//! trace positions in the original TypeScript.

use oxc::allocator::Allocator;
use oxc::codegen::{Codegen, CodegenOptions};
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;
use oxc::transformer::{TransformOptions, Transformer};
use std::path::Path;

/// Whether the file needs [`transpile`] before V8 can run it.
pub fn is_typescript(path: &Path) -> bool {
  matches!(path.extension().and_then(|e| e.to_str()), Some("ts" | "mts" | "cts" | "tsx"))
}

pub struct Output {
  pub code: String,
  pub line_map: LineMap,
}

/// Maps generated (line, column) positions back to the TypeScript source.
/// All values are 0-based.
#[derive(Default, Clone)]
pub struct LineMap {
  /// Per generated line: (generated column, source line, source column),
  /// sorted by generated column.
  lines: Vec<Vec<[u32; 3]>>,
}

impl LineMap {
  /// The source position for a generated position (the closest mapping at or
  /// before `col` on that line).
  pub fn lookup(&self, line: u32, col: u32) -> Option<(u32, u32)> {
    let tokens = self.lines.get(line as usize)?;
    let i = tokens.partition_point(|t| t[0] <= col);
    let t = if i == 0 { tokens.first()? } else { &tokens[i - 1] };
    Some((t[1], t[2]))
  }

  /// Compact binary form (for on-disk caches).
  pub fn to_bytes(&self) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend((self.lines.len() as u32).to_le_bytes());
    for line in &self.lines {
      out.extend((line.len() as u32).to_le_bytes());
      for t in line {
        for v in t {
          out.extend(v.to_le_bytes());
        }
      }
    }
    out
  }

  pub fn from_bytes(bytes: &[u8]) -> Option<LineMap> {
    let mut words = bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
    let n = words.next()? as usize;
    let mut lines = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
      let len = words.next()? as usize;
      let mut line = Vec::with_capacity(len.min(1 << 20));
      for _ in 0..len {
        line.push([words.next()?, words.next()?, words.next()?]);
      }
      lines.push(line);
    }
    Some(LineMap { lines })
  }
}

/// Transpiles TypeScript to JavaScript. Errors are rendered with the
/// offending source lines.
pub fn transpile(source: &str, path: &Path) -> Result<Output, String> {
  let allocator = Allocator::default();
  let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
  let parsed = Parser::new(&allocator, source, source_type).parse();
  if parsed.diagnostics.has_errors() {
    return Err(render(parsed.diagnostics, source));
  }
  let mut program = parsed.program;

  let semantic = SemanticBuilder::new().with_excess_capacity(2.0).with_enum_eval(true).build(&program);
  if semantic.diagnostics.has_errors() {
    return Err(render(semantic.diagnostics, source));
  }
  let scoping = semantic.semantic.into_scoping();

  // Default options: TypeScript and JSX only; no ES-version lowering.
  let options = TransformOptions::default();
  let transformed = Transformer::new(&allocator, path, &options).build_with_scoping(scoping, &mut program);
  if transformed.diagnostics.has_errors() {
    return Err(render(transformed.diagnostics, source));
  }

  let generated = Codegen::new()
    .with_options(CodegenOptions { source_map_path: Some(path.to_path_buf()), ..CodegenOptions::default() })
    .build(&program);
  let mut line_map = LineMap::default();
  if let Some(map) = generated.map {
    for t in map.get_tokens() {
      let line = t.get_dst_line() as usize;
      if line_map.lines.len() <= line {
        line_map.lines.resize_with(line + 1, Vec::new);
      }
      line_map.lines[line].push([t.get_dst_col(), t.get_src_line(), t.get_src_col()]);
    }
    for line in &mut line_map.lines {
      line.sort_unstable_by_key(|t| t[0]);
    }
  }
  Ok(Output { code: generated.code, line_map })
}

fn render(diagnostics: oxc::diagnostics::Diagnostics, source: &str) -> String {
  diagnostics
    .into_iter()
    .map(|d| d.render_with_source_code(source.to_string()))
    .collect::<Vec<_>>()
    .join("\n")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn strips_types_and_lowers_ts_syntax() {
    let src = "enum Color { Red, Green }\n\
               interface P { x: number }\n\
               class A { constructor(public n: number) {} }\n\
               export const f = (p: P): number => p.x + new A(1).n + Color.Green;\n";
    let out = transpile(src, Path::new("t.ts")).unwrap();
    assert!(!out.code.contains("interface"));
    assert!(!out.code.contains(": number"));
    assert!(out.code.contains("Color"));
    assert!(out.code.contains("this.n = n"));
  }

  #[test]
  fn reports_syntax_errors() {
    let err = transpile("const x: = 1;", Path::new("bad.ts")).err().unwrap();
    assert!(!err.is_empty());
  }

  #[test]
  fn line_map_round_trip() {
    let src = "type T = string;\n\nconst a: T = 'x';\nthrow new Error(a);\n";
    let out = transpile(src, Path::new("m.ts")).unwrap();
    let line = out.code.lines().position(|l| l.contains("throw")).unwrap() as u32;
    assert_eq!(out.line_map.lookup(line, 0).map(|p| p.0), Some(3));
    let back = LineMap::from_bytes(&out.line_map.to_bytes()).unwrap();
    assert_eq!(back.lookup(line, 0), out.line_map.lookup(line, 0));
  }
}
