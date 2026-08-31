//! Tokenizer for the carson shell.
//!
//! Produces a flat token stream; the parser walks it. Quoting, parameter
//! expansion and command substitution are resolved enough to split the input
//! into words, but the actual expansion happens later.
use crate::ast::{Word, WordPart};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectTok {
    In {
        fd: u32,
    },
    Out {
        fd: u32,
        append: bool,
    },
    Dup {
        fd: u32,
        to: u32,
    },
    /// `<<<word`: here-string feeding `word\n` to stdin.
    HereString {
        fd: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    Word(Word),
    Semi,
    Amp,
    Pipe,
    And,
    Or,
    LParen,
    RParen,
    Newline,
    Redir(RedirectTok),
    Eof,
}

/// A lexing error with a position and a human-readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub pos: usize,
    pub msg: String,
}

struct Lexer {
    src: Vec<char>,
    pos: usize,
}

/// Tokenize `src` into a stream ending with `Eof`.
pub fn lex(src: &str) -> Result<Vec<Tok>, LexError> {
    let mut lexer = Lexer {
        src: src.chars().collect(),
        pos: 0,
    };
    let mut out = Vec::new();
    while !matches!(lexer.peek(), None) {
        out.push(lexer.next()?);
    }
    out.push(Tok::Eof);
    Ok(out)
}

impl Lexer {
    fn next(&mut self) -> Result<Tok, LexError> {
        self.skip_blank();
        let Some(c) = self.peek() else {
            return Ok(Tok::Eof);
        };
        if c == '#' {
            while let Some(c) = self.peek() {
                if c == '\n' {
                    break;
                }
                self.pos += 1;
            }
            return self.next();
        }
        if c.is_ascii_digit()
            && let Some(tok) = self.try_digit_redirect()?
        {
            return Ok(tok);
        }
        match c {
            '\n' => {
                self.pos += 1;
                Ok(Tok::Newline)
            }
            ';' => {
                self.pos += 1;
                Ok(Tok::Semi)
            }
            '&' => {
                self.pos += 1;
                Ok(if self.eat('&') { Tok::And } else { Tok::Amp })
            }
            '|' => {
                self.pos += 1;
                Ok(if self.eat('|') { Tok::Or } else { Tok::Pipe })
            }
            '(' => {
                self.pos += 1;
                Ok(Tok::LParen)
            }
            ')' => {
                self.pos += 1;
                Ok(Tok::RParen)
            }
            '<' => {
                self.pos += 1;
                if self.eat('<') {
                    // `<<` heredocs are rewritten away by the parser's
                    // preprocessor; a bare `<<` here means a stray one.
                    if self.eat('<') {
                        Ok(Tok::Redir(RedirectTok::HereString { fd: 0 }))
                    } else {
                        Err(LexError {
                            pos: self.pos,
                            msg: "unterminated heredoc".into(),
                        })
                    }
                } else {
                    Ok(Tok::Redir(RedirectTok::In { fd: 0 }))
                }
            }
            '>' => {
                self.pos += 1;
                if self.eat('>') {
                    Ok(Tok::Redir(RedirectTok::Out {
                        fd: 1,
                        append: true,
                    }))
                } else if self.eat('&') {
                    let to_start = self.pos;
                    while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                        self.pos += 1;
                    }
                    let to: u32 = self.src[to_start..self.pos]
                        .iter()
                        .collect::<String>()
                        .parse()
                        .unwrap_or(0);
                    Ok(Tok::Redir(RedirectTok::Dup { fd: 1, to }))
                } else {
                    Ok(Tok::Redir(RedirectTok::Out {
                        fd: 1,
                        append: false,
                    }))
                }
            }
            _ => Ok(Tok::Word(self.scan_word()?)),
        }
    }

    fn skip_blank(&mut self) {
        loop {
            match self.peek() {
                Some(' ') | Some('\t') => self.pos += 1,
                Some('\\') if self.peek_at(1) == Some('\n') => self.pos += 2,
                _ => break,
            }
        }
    }

    /// A leading digit run directly followed by a redirect operator becomes an
    /// fd-qualified redirect (`2>`, `2>>`, `2>&1`); otherwise `None` and the
    /// digit run is lexed as an ordinary word.
    fn try_digit_redirect(&mut self) -> Result<Option<Tok>, LexError> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        let fd: u32 = self.src[start..self.pos]
            .iter()
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        let tok = match self.peek() {
            Some('>') => {
                self.pos += 1;
                let append = self.eat('>');
                Some(Tok::Redir(RedirectTok::Out { fd, append }))
            }
            Some('<') => {
                self.pos += 1;
                Some(Tok::Redir(RedirectTok::In { fd }))
            }
            Some('&') if self.peek_at(1) == Some('>') => {
                self.pos += 2;
                let to_start = self.pos;
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.pos += 1;
                }
                let to: u32 = self.src[to_start..self.pos]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0);
                Some(Tok::Redir(RedirectTok::Dup { fd, to }))
            }
            _ => None,
        };
        if tok.is_none() {
            self.pos = start;
        }
        Ok(tok)
    }

    fn scan_word(&mut self) -> Result<Word, LexError> {
        let mut parts: Vec<WordPart> = Vec::new();
        let mut lit = String::new();
        loop {
            let Some(c) = self.peek() else {
                break;
            };
            if c == ' '
                || c == '\t'
                || c == '\n'
                || c == ';'
                || c == '&'
                || c == '|'
                || c == '('
                || c == ')'
                || c == '<'
                || c == '>'
            {
                break;
            }
            match c {
                '\'' => {
                    self.pos += 1;
                    let start = self.pos;
                    while let Some(c) = self.peek() {
                        if c == '\'' {
                            break;
                        }
                        self.pos += 1;
                    }
                    let content: String = self.src[start..self.pos].iter().collect();
                    if !self.eat('\'') {
                        return Err(LexError {
                            pos: self.pos,
                            msg: "unterminated single quote".into(),
                        });
                    }
                    lit.push_str(&content);
                }
                '"' => {
                    self.pos += 1;
                    loop {
                        let Some(c) = self.peek() else {
                            return Err(LexError {
                                pos: self.pos,
                                msg: "unterminated double quote".into(),
                            });
                        };
                        match c {
                            '"' => {
                                self.pos += 1;
                                break;
                            }
                            '\\' => match self.peek_at(1) {
                                Some(q @ ('"' | '\\' | '$')) => {
                                    lit.push(q);
                                    self.pos += 2;
                                }
                                Some('\n') => {
                                    self.pos += 2;
                                }
                                _ => {
                                    lit.push('\\');
                                    self.pos += 1;
                                }
                            },
                            '$' => {
                                self.flush(&mut lit, &mut parts);
                                parts.push(self.scan_dollar(true)?);
                            }
                            _ => {
                                lit.push(c);
                                self.pos += 1;
                            }
                        }
                    }
                }
                '\\' => {
                    self.pos += 1;
                    if let Some(c) = self.peek() {
                        if c != '\n' {
                            lit.push(c);
                            self.pos += 1;
                        }
                    }
                }
                '$' => {
                    self.flush(&mut lit, &mut parts);
                    parts.push(self.scan_dollar(false)?);
                }
                _ => {
                    lit.push(c);
                    self.pos += 1;
                }
            }
        }
        self.flush(&mut lit, &mut parts);
        if parts.is_empty() {
            // Only reachable for empty quoting like `''` or `""`.
            parts.push(WordPart::Lit(String::new()));
        }
        Ok(Word { parts })
    }

    fn flush(&self, lit: &mut String, parts: &mut Vec<WordPart>) {
        if !lit.is_empty() {
            parts.push(WordPart::Lit(std::mem::take(lit)));
        }
    }

    /// Scan a `$`-expansion: `$NAME`, `${NAME}`, `$?` or `$(...)`.
    fn scan_dollar(&mut self, quoted: bool) -> Result<WordPart, LexError> {
        self.pos += 1; // consume '$'
        let Some(c) = self.peek() else {
            return Ok(WordPart::Lit('$'.to_string()));
        };
        match c {
            '(' => {
                self.pos += 1;
                let inner = self.scan_command_sub()?;
                Ok(if quoted {
                    WordPart::SubQuotedRaw(inner)
                } else {
                    WordPart::SubRaw(inner)
                })
            }
            '{' => {
                self.pos += 1;
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c == '}' {
                        break;
                    }
                    self.pos += 1;
                }
                let name: String = self.src[start..self.pos].iter().collect();
                if !self.eat('}') {
                    return Err(LexError {
                        pos: self.pos,
                        msg: "unterminated ${...}".into(),
                    });
                }
                Ok(if quoted {
                    WordPart::VarQuoted(name)
                } else {
                    WordPart::Var(name)
                })
            }
            c if c.is_ascii_alphanumeric() || c == '_' => {
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let name: String = self.src[start..self.pos].iter().collect();
                Ok(if quoted {
                    WordPart::VarQuoted(name)
                } else {
                    WordPart::Var(name)
                })
            }
            '?' | '#' | '0' => {
                self.pos += 1;
                let name = c.to_string();
                Ok(if quoted {
                    WordPart::VarQuoted(name)
                } else {
                    WordPart::Var(name)
                })
            }
            _ => Ok(WordPart::Lit('$'.to_string())),
        }
    }

    /// Scan `$( ... )` to the matching close paren, tracking nested parens and
    /// quotes. Returns the inner source text.
    fn scan_command_sub(&mut self) -> Result<String, LexError> {
        let start = self.pos;
        let mut depth = 1;
        let mut quote: Option<char> = None;
        loop {
            let Some(c) = self.peek() else {
                return Err(LexError {
                    pos: self.pos,
                    msg: "unterminated $(".into(),
                });
            };
            match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    } else if q == '"' && c == '\\' {
                        self.pos += 1;
                    }
                }
                None => match c {
                    '\'' | '"' => quote = Some(c),
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            let inner: String = self.src[start..self.pos].iter().collect();
                            self.pos += 1; // consume ')'
                            return Ok(inner);
                        }
                    }
                    '\\' => {
                        self.pos += 1;
                    }
                    _ => {}
                },
            }
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.src.get(self.pos + off).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
}
