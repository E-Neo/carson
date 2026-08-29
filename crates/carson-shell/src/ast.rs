/// Syntax tree for the carson shell.
///
/// Only the subset of bash the tool needs is modelled here; anything outside
/// it is rejected at parse time with a clear message.

/// One word as written in the script, made of literal and expandable parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub parts: Vec<WordPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WordPart {
    /// Plain literal text (unquoted or single-quoted).
    Lit(String),
    /// Literal text inside double quotes: no word splitting / globbing.
    Quoted(String),
    /// `$NAME` (or `$?`) outside quotes: value is word-split.
    Var(String),
    /// `$NAME` inside double quotes: value is not split.
    VarQuoted(String),
    /// `$(...)`: raw inner source, parsed recursively by the parser.
    SubRaw(String),
    /// `$(...)` inside double quotes: no word splitting on the result.
    SubQuotedRaw(String),
}

impl Word {
    pub fn lit(s: impl Into<String>) -> Self {
        Word {
            parts: vec![WordPart::Lit(s.into())],
        }
    }

    /// True when the word is a single unquoted literal (assignments and
    /// reserved-word detection only apply to those).
    pub fn plain(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [WordPart::Lit(s)] => Some(s.as_str()),
            _ => None,
        }
    }
}

/// A redirect attached to a simple command. The fd defaults to 1 for output
/// and 0 for input when omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub fd: u32,
    pub op: RedirectOp,
    pub target: Word,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectOp {
    /// `fd<word`
    In,
    /// `fd>word`
    Out,
    /// `fd>>word`
    Append,
    /// `fd>&N`: duplicate to another fd.
    Dup(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
    pub assignments: Vec<(String, Word)>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

/// A command. Compound bodies are lists; a list is a sequence of commands run
/// in order, separated by `;`, newlines or `&`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Simple(SimpleCommand),
    AndOr {
        lhs: Box<Command>,
        and: bool,
        rhs: Box<Command>,
    },
    Pipeline {
        negated: bool,
        stages: Vec<SimpleCommand>,
    },
    If {
        cond: Vec<Command>,
        then: Vec<Command>,
        elifs: Vec<(Vec<Command>, Vec<Command>)>,
        els: Option<Vec<Command>>,
    },
    For {
        var: String,
        words: Vec<Word>,
        body: Vec<Command>,
    },
    While {
        cond: Vec<Command>,
        body: Vec<Command>,
        until: bool,
    },
    /// `( ... )`: run in a subshell (state snapshot).
    Subshell(Vec<Command>),
    /// `{ ...; }`: run in the current shell.
    Brace(Vec<Command>),
}

pub type List = Vec<Command>;
