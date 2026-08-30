//! Recursive-descent parser over the token stream produced by [`crate::lexer`].
use crate::ast::{Command, List, Redirect, RedirectOp, SimpleCommand, Word};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub msg: String,
}

/// The extracted body of one heredoc. `expand` is false when the delimiter was
/// quoted (`<<'EOF'`), so the body is fed literally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeredocBody {
    pub body: String,
    pub expand: bool,
}

use super::lexer::{Tok, lex};

/// The marker `<<<` target the preprocessor leaves in place of a heredoc.
const HEREDOC_MARKER: &str = "CARSON_HEREDOC";

/// Parse a complete script into a list of top-level commands.
pub fn parse(src: &str) -> Result<List, ParseError> {
    let (cleaned, heredocs) = extract_heredocs(src)?;
    let toks = lex(&cleaned).map_err(|e| ParseError { msg: e.msg })?;
    let mut p = Parser {
        toks,
        pos: 0,
        heredocs,
    };
    let list = p.parse_list(&[], false)?;
    match p.peek() {
        Tok::Eof => Ok(list),
        tok => Err(ParseError {
            msg: format!("unexpected token after command list: {tok:?}"),
        }),
    }
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    heredocs: Vec<HeredocBody>,
}

/// If `w` is a heredoc marker like `CARSON_HEREDOC3`, return the body index.
fn heredoc_marker_index(w: &Word) -> Option<usize> {
    let plain = w.plain()?;
    let rest = plain.strip_prefix(HEREDOC_MARKER)?;
    rest.parse::<usize>().ok()
}

/// Rewrite heredocs (`<<`, `<<-`) into here-string markers and return the
/// script without the body lines plus the extracted bodies. Here-strings
/// (`<<<`) are left alone for the lexer.
///
/// The scan is quote-aware. Each heredoc's body begins after the newline that
/// ends the command line containing the redirect and runs until a line equal
/// to the delimiter (`<<-` strips leading tabs). Multiple heredocs on one line
/// consume their bodies in order.
fn extract_heredocs(src: &str) -> Result<(String, Vec<HeredocBody>), ParseError> {
    let chars: Vec<char> = src.chars().collect();
    // (replace_start, replace_end, body_start, body_end, expand)
    let mut edits: Vec<(usize, usize, usize, usize, bool)> = Vec::new();
    let mut bodies: Vec<HeredocBody> = Vec::new();
    let mut quote: Option<char> = None;
    let mut i = 0;
    let mut pending: Vec<(usize, usize, String, bool, bool)> = Vec::new(); // start,end,delim,expand,strip
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if q == '"' && c == '\\' {
                    i += 1;
                }
                i += 1;
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    i += 1;
                }
                '\\' => {
                    i += 2;
                }
                '$' if i + 1 < chars.len() && chars[i + 1] == '(' => {
                    // `$( ... )` is re-parsed later (command substitution), so
                    // its heredocs must stay intact for that nested parse.
                    // Skip the whole balanced region without rewriting.
                    i += 2;
                    let mut depth = 1usize;
                    while i < chars.len() && depth > 0 {
                        match chars[i] {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            '\'' => {
                                i += 1;
                                while i < chars.len() && chars[i] != '\'' {
                                    i += 1;
                                }
                            }
                            _ => {}
                        }
                        i += 1;
                    }
                }
                '<'
                    if i + 1 < chars.len() && chars[i + 1] == '<' =>
                {
                    if i + 2 < chars.len() && chars[i + 2] == '<' {
                        // `<<<` here-string: leave for the lexer.
                        i += 3;
                        continue;
                    }
                    let start = i;
                    i += 2;
                    let strip = i < chars.len() && chars[i] == '-';
                    if strip {
                        i += 1;
                    }
                    while i < chars.len() && matches!(chars[i], ' ' | '\t') {
                        i += 1;
                    }
                    let (delim, expand) = if i < chars.len() && chars[i] == '\'' {
                        i += 1;
                        let s = i;
                        while i < chars.len() && chars[i] != '\'' {
                            i += 1;
                        }
                        let d: String = chars[s..i].iter().collect();
                        if i < chars.len() {
                            i += 1;
                        }
                        (d, false)
                    } else if i < chars.len() && chars[i] == '"' {
                        i += 1;
                        let s = i;
                        while i < chars.len() && chars[i] != '"' {
                            i += 1;
                        }
                        let d: String = chars[s..i].iter().collect();
                        if i < chars.len() {
                            i += 1;
                        }
                        (d, false)
                    } else {
                        let s = i;
                        while i < chars.len()
                            && !chars[i].is_whitespace()
                            && !matches!(
                                chars[i],
                                ';' | '&' | '|' | '(' | ')' | '<' | '>' | '`'
                            )
                        {
                            i += 1;
                        }
                        let d: String = chars[s..i].iter().collect();
                        (d, true)
                    };
                    if delim.is_empty() {
                        return Err(ParseError {
                            msg: "empty heredoc delimiter".into(),
                        });
                    }
                    pending.push((start, i, delim, expand, strip));
                }
                '\n' if !pending.is_empty() => {
                    let newline = i;
                    let mut cursor = newline + 1;
                    for (start, end, delim, expand, strip) in std::mem::take(&mut pending) {
                        let (body, body_end) =
                            scan_heredoc_body(&chars, cursor, &delim, strip)?;
                        bodies.push(HeredocBody { body, expand });
                        edits.push((start, end, cursor, body_end, expand));
                        cursor = body_end;
                    }
                    i = cursor;
                }
                _ => {
                    i += 1;
                }
            },
        }
    }
    if !pending.is_empty() {
        return Err(ParseError {
            msg: "unterminated heredoc (missing newline)".into(),
        });
    }
    if edits.is_empty() {
        return Ok((src.to_string(), bodies));
    }

    // Rebuild the source, replacing each `<<DELIM` with a marker and dropping
    // the body lines. Apply edits from the end so earlier offsets stay valid.
    let mut out: Vec<char> = Vec::new();
    let mut cursor = 0;
    let mut body_index = 0;
    for (start, end, body_start, body_end, _expand) in edits {
        out.extend_from_slice(&chars[cursor..start]);
        let marker: Vec<char> =
            format!("<<<{HEREDOC_MARKER}{body_index}").chars().collect();
        out.extend_from_slice(&marker);
        out.extend_from_slice(&chars[end..body_start]);
        body_index += 1;
        cursor = body_end;
    }
    out.extend_from_slice(&chars[cursor..]);
    Ok((out.iter().collect(), bodies))
}

/// Read a heredoc body starting at `from` until a line matching `delim`.
/// Returns the body text (each line kept, delimiter line excluded) and the
/// index just past the delimiter line's newline. `<<-` strips leading tabs
/// from every body line and from the delimiter line.
fn scan_heredoc_body(
    chars: &[char],
    from: usize,
    delim: &str,
    strip: bool,
) -> Result<(String, usize), ParseError> {
    let mut cursor = from;
    let mut body: String = String::new();
    while cursor <= chars.len() {
        let line_end = chars[cursor..]
            .iter()
            .position(|&c| c == '\n')
            .map(|p| cursor + p)
            .unwrap_or(chars.len());
        let line: String = chars[cursor..line_end].iter().collect();
        let stripped = if strip {
            line.trim_start_matches('\t')
        } else {
            line.as_str()
        };
        if stripped == delim {
            return Ok((body, (line_end + 1).min(chars.len())));
        }
        body.push_str(stripped);
        if line_end < chars.len() {
            body.push('\n');
        }
        if line_end >= chars.len() {
            break;
        }
        cursor = line_end + 1;
    }
    Err(ParseError {
        msg: format!("unterminated heredoc (delimiter `{delim}` not found)"),
    })
}

const KEYWORDS: &[&str] = &["if", "then", "elif", "else", "fi", "for", "in", "do", "done", "while", "until", "{", "}"];

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }

    fn advance(&mut self) -> Tok {
        let tok = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        tok
    }

    fn err(&self, msg: impl Into<String>) -> ParseError {
        ParseError { msg: msg.into() }
    }

    /// A command boundary: the next token ends the enclosing construct.
    fn at_end(&self, stop: &[&str], stop_rparen: bool) -> bool {
        match self.peek() {
            Tok::Eof => true,
            Tok::RParen if stop_rparen => true,
            Tok::Word(w) => {
                if let Some(plain) = w.plain() {
                    stop.contains(&plain)
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn consume_separator(&mut self) -> bool {
        match self.peek() {
            Tok::Semi | Tok::Newline | Tok::Amp => {
                self.advance();
                true
            }
            _ => false,
        }
    }

    fn skip_separators(&mut self) {
        while self.consume_separator() {}
    }

    fn parse_list(&mut self, stop: &[&str], stop_rparen: bool) -> Result<List, ParseError> {
        let mut list = Vec::new();
        loop {
            self.skip_separators();
            if self.at_end(stop, stop_rparen) {
                break;
            }
            let cmd = self.parse_andor(stop_rparen)?;
            list.push(cmd);
            if !self.consume_separator() {
                break;
            }
        }
        Ok(list)
    }

    fn parse_andor(&mut self, stop_rparen: bool) -> Result<Command, ParseError> {
        let mut lhs = self.parse_pipeline(stop_rparen)?;
        loop {
            let (and, is) = match self.peek() {
                Tok::And => (true, true),
                Tok::Or => (false, true),
                _ => (false, false),
            };
            if !is {
                break;
            }
            self.advance();
            let rhs = self.parse_pipeline(stop_rparen)?;
            lhs = Command::AndOr {
                lhs: Box::new(lhs),
                and,
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_pipeline(&mut self, stop_rparen: bool) -> Result<Command, ParseError> {
        let negated = self.consume_plain("!");
        let first = self.parse_command(stop_rparen)?;
        if !matches!(self.peek(), Tok::Pipe) {
            return Ok(match (negated, first) {
                (false, cmd) => cmd,
                (true, Command::Simple(s)) => Command::Pipeline {
                    negated: true,
                    stages: vec![s],
                },
                (true, other) => {
                    return Err(self.err(format!("cannot negate {}", describe(&other))))
                }
            });
        }
        let mut stages = match first {
            Command::Simple(s) => vec![s],
            other => {
                return Err(self.err(format!(
                    "unsupported command in pipeline: {}",
                    describe(&other)
                )))
            }
        };
        while matches!(self.peek(), Tok::Pipe) {
            self.advance();
            let stage = self.parse_command(stop_rparen)?;
            match stage {
                Command::Simple(s) => stages.push(s),
                other => {
                    return Err(self.err(format!(
                        "unsupported command in pipeline: {}",
                        describe(&other)
                    )))
                }
            }
        }
        Ok(Command::Pipeline { negated, stages })
    }

    fn parse_command(&mut self, stop_rparen: bool) -> Result<Command, ParseError> {
        match self.peek().clone() {
            Tok::Word(w) => match w.plain() {
                Some("if") => self.parse_if(stop_rparen),
                Some("for") => self.parse_for(stop_rparen),
                Some("while") => self.parse_while(stop_rparen, false),
                Some("until") => self.parse_while(stop_rparen, true),
                Some("{") => self.parse_brace(stop_rparen),
                _ => self.parse_simple(stop_rparen),
            },
            Tok::LParen => {
                self.advance();
                let body = self.parse_list(&[], true)?;
                if !matches!(self.peek(), Tok::RParen) {
                    return Err(self.err("expected ')'"));
                }
                self.advance();
                Ok(Command::Subshell(body))
            }
            _ => Err(self.err("expected a command")),
        }
    }

    fn parse_if(&mut self, stop_rparen: bool) -> Result<Command, ParseError> {
        self.advance(); // if
        let cond = self.parse_list(&["then"], stop_rparen)?;
        if !self.consume_plain("then") {
            return Err(self.err("expected 'then'"));
        }
        let then = self.parse_list(&["elif", "else", "fi"], stop_rparen)?;
        let mut elifs = Vec::new();
        let mut els = None;
        loop {
            if self.consume_plain("elif") {
                let c = self.parse_list(&["then"], stop_rparen)?;
                if !self.consume_plain("then") {
                    return Err(self.err("expected 'then'"));
                }
                let b = self.parse_list(&["elif", "else", "fi"], stop_rparen)?;
                elifs.push((c, b));
            } else if self.consume_plain("else") {
                els = Some(self.parse_list(&["fi"], stop_rparen)?);
            } else {
                break;
            }
        }
        if !self.consume_plain("fi") {
            return Err(self.err("expected 'fi'"));
        }
        Ok(Command::If {
            cond,
            then,
            elifs,
            els,
        })
    }

    fn parse_for(&mut self, stop_rparen: bool) -> Result<Command, ParseError> {
        self.advance(); // for
        let var = match self.peek().clone() {
            Tok::Word(w) => match w.plain() {
                Some(name) if valid_name(name) => name.to_string(),
                _ => return Err(self.err("expected a variable name after 'for'")),
            },
            _ => return Err(self.err("expected a variable name after 'for'")),
        };
        self.advance();
        let mut words = Vec::new();
        if self.consume_plain("in") {
            loop {
                match self.peek() {
                    Tok::Word(w) => {
                        words.push(w.clone());
                        self.advance();
                    }
                    Tok::Semi | Tok::Newline | Tok::Eof => break,
                    _ => return Err(self.err("unexpected token in 'for' word list")),
                }
            }
        }
        self.skip_separators();
        if !self.consume_plain("do") {
            return Err(self.err("expected 'do'"));
        }
        let body = self.parse_list(&["done"], stop_rparen)?;
        if !self.consume_plain("done") {
            return Err(self.err("expected 'done'"));
        }
        Ok(Command::For { var, words, body })
    }

    fn parse_while(
        &mut self,
        stop_rparen: bool,
        until: bool,
    ) -> Result<Command, ParseError> {
        self.advance(); // while/until
        let cond = self.parse_list(&["do"], stop_rparen)?;
        if !self.consume_plain("do") {
            return Err(self.err("expected 'do'"));
        }
        let body = self.parse_list(&["done"], stop_rparen)?;
        if !self.consume_plain("done") {
            return Err(self.err("expected 'done'"));
        }
        Ok(Command::While { cond, body, until })
    }

    fn parse_brace(&mut self, stop_rparen: bool) -> Result<Command, ParseError> {
        self.advance(); // {
        let body = self.parse_list(&["}"], stop_rparen)?;
        if !self.consume_plain("}") {
            return Err(self.err("expected '}'"));
        }
        Ok(Command::Brace(body))
    }

    fn parse_simple(&mut self, _stop_rparen: bool) -> Result<Command, ParseError> {
        let mut assignments = Vec::new();
        let mut words = Vec::new();
        let mut redirects = Vec::new();
        loop {
            match self.peek().clone() {
                Tok::Word(w) => {
                    if let Some((name, value)) = split_assignment(&w) {
                        if words.is_empty() {
                            assignments.push((name, value));
                            self.advance();
                            continue;
                        }
                    }
                    // A reserved word here is a leftover terminator that the
                    // list parser missed; treat it as the end of this command.
                    if let Some(plain) = w.plain()
                        && KEYWORDS.contains(&plain)
                    {
                        break;
                    }
                    words.push(w);
                    self.advance();
                }
                Tok::Redir(r) => {
                    self.advance();
                    // `2>&1` carries its target fd in the token; no word follows.
                    let is_dup = matches!(r, super::lexer::RedirectTok::Dup { .. });
                    let target = if is_dup {
                        Word::lit("")
                    } else {
                        match self.peek().clone() {
                            Tok::Word(w) => {
                                self.advance();
                                w
                            }
                            _ => return Err(self.err("expected a file after redirect")),
                        }
                    };
                    // A here-string whose target is a heredoc marker resolves
                    // to the heredoc body extracted by the preprocessor.
                    if matches!(r, super::lexer::RedirectTok::HereString { .. })
                        && let Some(idx) = heredoc_marker_index(&target)
                    {
                        if let Some(hb) = self.heredocs.get(idx) {
                            redirects.push(Redirect {
                                fd: 0,
                                op: RedirectOp::Heredoc {
                                    body: hb.body.clone(),
                                    expand: hb.expand,
                                },
                                target: Word::lit(""),
                            });
                            continue;
                        }
                        return Err(self.err("unknown heredoc marker"));
                    }
                    redirects.push(make_redirect(r, target));
                }
                _ => break,
            }
        }
        if assignments.is_empty() && words.is_empty() && redirects.is_empty() {
            return Err(self.err("expected a command"));
        }
        Ok(Command::Simple(SimpleCommand {
            assignments,
            words,
            redirects,
        }))
    }

    fn consume_plain(&mut self, s: &str) -> bool {
        if let Tok::Word(w) = self.peek()
            && w.plain() == Some(s)
        {
            self.advance();
            true
        } else {
            false
        }
    }
}

fn make_redirect(r: super::lexer::RedirectTok, target: Word) -> Redirect {
    use super::lexer::RedirectTok;
    match r {
        RedirectTok::In { fd } => Redirect {
            fd,
            op: RedirectOp::In,
            target,
        },
        RedirectTok::Out { fd, append } => Redirect {
            fd,
            op: if append { RedirectOp::Append } else { RedirectOp::Out },
            target,
        },
        RedirectTok::Dup { fd, to } => Redirect {
            fd,
            op: RedirectOp::Dup(to),
            target,
        },
        RedirectTok::HereString { fd } => Redirect {
            fd,
            op: RedirectOp::HereString,
            target,
        },
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Split a leading `NAME=value` assignment word into (name, value-word).
fn split_assignment(word: &Word) -> Option<(String, Word)> {
    let first = word.parts.first()?;
    let crate::ast::WordPart::Lit(head) = first else {
        return None;
    };
    let eq = head.find('=')?;
    let name = &head[..eq];
    if !valid_name(name) {
        return None;
    }
    let mut value_parts = Vec::new();
    if eq + 1 < head.len() {
        value_parts.push(crate::ast::WordPart::Lit(head[eq + 1..].to_string()));
    }
    value_parts.extend_from_slice(&word.parts[1..]);
    Some((name.to_string(), Word { parts: value_parts }))
}

fn describe(cmd: &Command) -> &'static str {
    match cmd {
        Command::Simple(_) => "simple command",
        Command::AndOr { .. } => "&&/|| chain",
        Command::Pipeline { .. } => "pipeline",
        Command::If { .. } => "if",
        Command::For { .. } => "for",
        Command::While { .. } => "while",
        Command::Subshell(_) => "( )",
        Command::Brace(_) => "{ }",
    }
}
