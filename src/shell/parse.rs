//! Parser for the `Sako.$` shell language (a POSIX sh subset, like `Bun.$`).
//!
//! The input is a tagged template: text segments with interpolated values in
//! between. Interpolations are never re-parsed as shell syntax; each one is an
//! opaque word fragment (or several words, for arrays).

/// One input unit: a character of template text or an interpolation slot.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Item {
  Ch(char),
  Val(usize),
}

#[derive(Debug, Clone)]
pub enum Part {
  /// Unquoted text: subject to tilde and glob expansion.
  Lit(String),
  /// Quoted or escaped text: taken literally.
  Quoted(String),
  /// `$NAME` / `${NAME}`; the flag says whether it was inside double quotes.
  Var(String, bool),
  /// Interpolated JS value `${value}` (by index).
  Interp(usize),
  /// `$(...)` command substitution.
  Subst(Box<Script>, bool),
}

#[derive(Debug, Clone, Default)]
pub struct Word(pub Vec<Part>);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fd {
  Stdin,
  Stdout,
  Stderr,
  /// `&>` / `&>>`: stdout and stderr.
  Both,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RedirOp {
  In,
  Out,
  Append,
}

#[derive(Debug, Clone)]
pub enum Redirect {
  File { fd: Fd, op: RedirOp, target: Word },
  /// `2>&1` (from 2 to 1) or `1>&2` / `>&2` (from 1 to 2).
  Dup { from: u8, to: u8 },
}

#[derive(Debug, Clone, Default)]
pub struct Command {
  pub assigns: Vec<(String, Word)>,
  pub words: Vec<Word>,
  pub redirects: Vec<Redirect>,
}

#[derive(Debug, Clone)]
pub struct Pipeline(pub Vec<Command>);

#[derive(Debug, Clone)]
pub struct AndOr {
  pub first: Pipeline,
  /// (is_and, pipeline): `&&` when true, `||` when false.
  pub rest: Vec<(bool, Pipeline)>,
}

#[derive(Debug, Clone, Default)]
pub struct Script(pub Vec<AndOr>);

pub fn parse(texts: &[String], nvals: usize) -> Result<Script, String> {
  let mut items = Vec::with_capacity(texts.iter().map(|t| t.len()).sum::<usize>() + nvals);
  for (i, t) in texts.iter().enumerate() {
    items.extend(t.chars().map(Item::Ch));
    if i < nvals {
      items.push(Item::Val(i));
    }
  }
  let mut p = Parser { it: &items, pos: 0 };
  let script = p.script(false)?;
  if p.pos < items.len() {
    return Err(p.unexpected());
  }
  Ok(script)
}

struct Parser<'a> {
  it: &'a [Item],
  pos: usize,
}

const OPERATOR_CHARS: &str = "|&;<>()\n";

impl Parser<'_> {
  fn peek(&self) -> Option<Item> {
    self.it.get(self.pos).copied()
  }

  fn peek_ch(&self) -> Option<char> {
    match self.peek() {
      Some(Item::Ch(c)) => Some(c),
      _ => None,
    }
  }

  fn ch_at(&self, offset: usize) -> Option<char> {
    match self.it.get(self.pos + offset) {
      Some(Item::Ch(c)) => Some(*c),
      _ => None,
    }
  }

  fn starts_with(&self, s: &str) -> bool {
    s.chars().enumerate().all(|(i, c)| self.ch_at(i) == Some(c))
  }

  fn unexpected(&self) -> String {
    match self.peek() {
      Some(Item::Ch('\n')) => "syntax error: unexpected newline".into(),
      Some(Item::Ch(c)) => format!("syntax error: unexpected '{c}'"),
      Some(Item::Val(_)) => "syntax error: unexpected interpolation".into(),
      None => "syntax error: unexpected end of input".into(),
    }
  }

  /// Skips blanks and backslash-newline continuations.
  fn skip_blanks(&mut self) {
    loop {
      match self.peek_ch() {
        Some(' ' | '\t' | '\r') => self.pos += 1,
        Some('\\') if self.ch_at(1) == Some('\n') => self.pos += 2,
        _ => return,
      }
    }
  }

  /// Skips blanks, newlines, `;` and comments between commands.
  fn skip_separators(&mut self) {
    loop {
      self.skip_blanks();
      match self.peek_ch() {
        Some('\n' | ';') => self.pos += 1,
        Some('#') => self.skip_comment(),
        _ => return,
      }
    }
  }

  fn skip_comment(&mut self) {
    while let Some(item) = self.peek() {
      if item == Item::Ch('\n') {
        return;
      }
      self.pos += 1;
    }
  }

  fn script(&mut self, nested: bool) -> Result<Script, String> {
    let mut items = Vec::new();
    loop {
      self.skip_separators();
      match self.peek_ch() {
        None if self.peek().is_none() => break,
        Some(')') if nested => break,
        _ => {}
      }
      items.push(self.and_or()?);
      self.skip_blanks();
      match self.peek_ch() {
        Some(';' | '\n') => self.pos += 1,
        Some('#') => self.skip_comment(),
        Some(')') if nested => break,
        Some('&') => return Err("background jobs (&) are not supported".into()),
        None if self.peek().is_none() => break,
        _ => return Err(self.unexpected()),
      }
    }
    Ok(Script(items))
  }

  fn and_or(&mut self) -> Result<AndOr, String> {
    let first = self.pipeline()?;
    let mut rest = Vec::new();
    loop {
      self.skip_blanks();
      let is_and = if self.starts_with("&&") {
        true
      } else if self.starts_with("||") {
        false
      } else {
        break;
      };
      self.pos += 2;
      self.skip_separators_newlines_only();
      rest.push((is_and, self.pipeline()?));
    }
    Ok(AndOr { first, rest })
  }

  fn skip_separators_newlines_only(&mut self) {
    loop {
      self.skip_blanks();
      if self.peek_ch() == Some('\n') {
        self.pos += 1;
      } else {
        return;
      }
    }
  }

  fn pipeline(&mut self) -> Result<Pipeline, String> {
    let mut cmds = vec![self.command()?];
    loop {
      self.skip_blanks();
      if self.peek_ch() == Some('|') && self.ch_at(1) != Some('|') {
        self.pos += 1;
        self.skip_separators_newlines_only();
        cmds.push(self.command()?);
      } else {
        break;
      }
    }
    Ok(Pipeline(cmds))
  }

  fn command(&mut self) -> Result<Command, String> {
    let mut cmd = Command::default();
    loop {
      self.skip_blanks();
      match self.peek() {
        None => break,
        Some(Item::Ch(c)) => {
          if c == '#' {
            self.skip_comment();
            break;
          }
          if let Some(r) = self.redirect()? {
            cmd.redirects.push(r);
            continue;
          }
          if c == '(' {
            return Err("subshells ( ... ) are not supported".into());
          }
          if OPERATOR_CHARS.contains(c) {
            break;
          }
        }
        Some(Item::Val(_)) => {}
      }
      let word = self.word()?;
      if cmd.words.is_empty()
        && let Some((name, value)) = split_assignment(&word)
      {
        cmd.assigns.push((name, value));
      } else {
        cmd.words.push(word);
      }
    }
    if cmd.words.is_empty() && cmd.assigns.is_empty() && cmd.redirects.is_empty() {
      return Err(self.unexpected());
    }
    Ok(cmd)
  }

  /// Parses a redirection operator and its target, if one starts here.
  fn redirect(&mut self) -> Result<Option<Redirect>, String> {
    let ops: &[(&str, Option<(Fd, RedirOp)>, Option<(u8, u8)>)] = &[
      ("&>>", Some((Fd::Both, RedirOp::Append)), None),
      ("&>", Some((Fd::Both, RedirOp::Out)), None),
      ("2>&1", None, Some((2, 1))),
      ("1>&2", None, Some((1, 2))),
      (">&2", None, Some((1, 2))),
      ("2>>", Some((Fd::Stderr, RedirOp::Append)), None),
      ("2>", Some((Fd::Stderr, RedirOp::Out)), None),
      ("1>>", Some((Fd::Stdout, RedirOp::Append)), None),
      ("1>", Some((Fd::Stdout, RedirOp::Out)), None),
      (">>", Some((Fd::Stdout, RedirOp::Append)), None),
      (">", Some((Fd::Stdout, RedirOp::Out)), None),
      ("0<", Some((Fd::Stdin, RedirOp::In)), None),
      ("<", Some((Fd::Stdin, RedirOp::In)), None),
    ];
    for (op, file, dup) in ops {
      if !self.starts_with(op) {
        continue;
      }
      self.pos += op.chars().count();
      if let Some((from, to)) = dup {
        return Ok(Some(Redirect::Dup { from: *from, to: *to }));
      }
      let (fd, op) = file.unwrap();
      self.skip_blanks();
      let target = self.word()?;
      if target.0.is_empty() {
        return Err("syntax error: expected a file name after redirection".into());
      }
      return Ok(Some(Redirect::File { fd, op, target }));
    }
    Ok(None)
  }

  fn word(&mut self) -> Result<Word, String> {
    let mut parts: Vec<Part> = Vec::new();
    let push_lit = |parts: &mut Vec<Part>, c: char| match parts.last_mut() {
      Some(Part::Lit(s)) => s.push(c),
      _ => parts.push(Part::Lit(c.to_string())),
    };
    let push_quoted = |parts: &mut Vec<Part>, s: &str| match parts.last_mut() {
      Some(Part::Quoted(q)) => q.push_str(s),
      _ => parts.push(Part::Quoted(s.to_string())),
    };
    while let Some(item) = self.peek() {
      match item {
        Item::Val(i) => {
          self.pos += 1;
          parts.push(Part::Interp(i));
        }
        Item::Ch(c) => {
          if c == ' ' || c == '\t' || c == '\r' || OPERATOR_CHARS.contains(c) {
            break;
          }
          match c {
            '\'' => {
              self.pos += 1;
              let mut s = String::new();
              loop {
                match self.peek() {
                  Some(Item::Ch('\'')) => {
                    self.pos += 1;
                    break;
                  }
                  Some(Item::Ch(c)) => {
                    s.push(c);
                    self.pos += 1;
                  }
                  Some(Item::Val(i)) => {
                    // An interpolation inside quotes stays one fragment.
                    if !s.is_empty() {
                      push_quoted(&mut parts, &s);
                      s.clear();
                    }
                    parts.push(Part::Interp(i));
                    self.pos += 1;
                  }
                  None => return Err("syntax error: unterminated single quote".into()),
                }
              }
              push_quoted(&mut parts, &s);
            }
            '"' => {
              self.pos += 1;
              self.double_quoted(&mut parts)?;
            }
            '\\' => {
              self.pos += 1;
              match self.peek() {
                Some(Item::Ch('\n')) => self.pos += 1,
                Some(Item::Ch(c)) => {
                  self.pos += 1;
                  push_quoted(&mut parts, &c.to_string());
                }
                Some(Item::Val(_)) => {}
                None => push_quoted(&mut parts, "\\"),
              }
            }
            '$' => {
              if let Some(p) = self.dollar(false)? {
                parts.push(p);
              } else {
                push_lit(&mut parts, '$');
              }
            }
            c => {
              self.pos += 1;
              push_lit(&mut parts, c);
            }
          }
        }
      }
    }
    Ok(Word(parts))
  }

  fn double_quoted(&mut self, parts: &mut Vec<Part>) -> Result<(), String> {
    let mut s = String::new();
    // An empty "" must still produce a (empty) word.
    let mut produced = false;
    let flush = |parts: &mut Vec<Part>, s: &mut String| {
      match parts.last_mut() {
        Some(Part::Quoted(q)) => q.push_str(s),
        _ => parts.push(Part::Quoted(std::mem::take(s))),
      }
      s.clear();
    };
    loop {
      match self.peek() {
        None => return Err("syntax error: unterminated double quote".into()),
        Some(Item::Ch('"')) => {
          self.pos += 1;
          break;
        }
        Some(Item::Ch('\\')) => {
          self.pos += 1;
          match self.peek_ch() {
            Some(c @ ('$' | '`' | '"' | '\\')) => {
              s.push(c);
              self.pos += 1;
            }
            Some('\n') => self.pos += 1,
            _ => s.push('\\'),
          }
        }
        Some(Item::Ch('$')) => {
          if let Some(p) = self.dollar(true)? {
            if !s.is_empty() {
              flush(parts, &mut s);
            }
            parts.push(p);
            produced = true;
          } else {
            s.push('$');
          }
        }
        Some(Item::Ch(c)) => {
          s.push(c);
          self.pos += 1;
        }
        Some(Item::Val(i)) => {
          if !s.is_empty() {
            flush(parts, &mut s);
          }
          parts.push(Part::Interp(i));
          produced = true;
          self.pos += 1;
        }
      }
    }
    if !s.is_empty() || !produced {
      flush(parts, &mut s);
    }
    Ok(())
  }

  /// Parses `$NAME`, `${NAME}`, `$?` or `$(...)` at the current `$`.
  /// Returns None (consuming nothing) when `$` is literal.
  fn dollar(&mut self, quoted: bool) -> Result<Option<Part>, String> {
    let next = self.ch_at(1);
    match next {
      Some('{') => {
        self.pos += 2;
        let mut name = String::new();
        loop {
          match self.peek_ch() {
            Some('}') => {
              self.pos += 1;
              break;
            }
            Some(c) => {
              name.push(c);
              self.pos += 1;
            }
            None => return Err("syntax error: unterminated ${".into()),
          }
        }
        if !is_name(&name) && name != "?" {
          return Err(format!("bad substitution: ${{{name}}}"));
        }
        Ok(Some(Part::Var(name, quoted)))
      }
      Some('(') => {
        self.pos += 2;
        let script = self.script(true)?;
        if self.peek_ch() != Some(')') {
          return Err("syntax error: unterminated $(".into());
        }
        self.pos += 1;
        Ok(Some(Part::Subst(Box::new(script), quoted)))
      }
      Some('?') => {
        self.pos += 2;
        Ok(Some(Part::Var("?".into(), quoted)))
      }
      Some(c) if c == '_' || c.is_ascii_alphabetic() => {
        self.pos += 1;
        let mut name = String::new();
        while let Some(c) = self.peek_ch() {
          if c == '_' || c.is_ascii_alphanumeric() {
            name.push(c);
            self.pos += 1;
          } else {
            break;
          }
        }
        Ok(Some(Part::Var(name, quoted)))
      }
      Some(c) if c.is_ascii_digit() => {
        self.pos += 2;
        Ok(Some(Part::Var(c.to_string(), quoted)))
      }
      _ => {
        if !quoted {
          self.pos += 1;
        } else {
          self.pos += 1;
        }
        Ok(None)
      }
    }
  }
}

pub fn is_name(s: &str) -> bool {
  let mut chars = s.chars();
  matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
    && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// `NAME=value...` at the start of a command is an assignment.
fn split_assignment(word: &Word) -> Option<(String, Word)> {
  let Some(Part::Lit(first)) = word.0.first() else { return None };
  let eq = first.find('=')?;
  let name = &first[..eq];
  if !is_name(name) {
    return None;
  }
  let mut parts = Vec::with_capacity(word.0.len());
  let rest = &first[eq + 1..];
  if !rest.is_empty() {
    parts.push(Part::Lit(rest.to_string()));
  }
  parts.extend(word.0[1..].iter().cloned());
  Some((name.to_string(), Word(parts)))
}
