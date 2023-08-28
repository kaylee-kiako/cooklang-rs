//! Cooklang parser
//!
//! Grammar:
//! ```txt
//! recipe     = Newline* (line line_end)* line? Eof
//! line       = metadata | section | step
//! line_end   = soft_break | Newline+
//! soft_break = Newline !Newline
//!
//! metadata   = MetadataStart meta_key Colon meta_val
//! meta_key   = (!(Colon | Newline) ANY)*
//! meta_value = (!Newline ANY)*
//!
//! section    = Eq+ (section_name Eq*)
//! sect_name  = (!Eq ANY)*
//!
//! step       = TextStep? (component | ANY)*
//!
//! component  = c_kind modifiers? c_body note?
//! c_kind     = At | Hash | Tilde
//! c_body     = c_close | c_long | Word
//! c_long     = c_l_name c_alias? c_close
//! c_l_name   = (!(Newline | OpenBrace | Or) ANY)*
//! c_alias    = Or c_l_name
//! c_close    = OpenBrace Whitespace? Quantity? Whitespace? CloseBrace
//!
//! modifiers  = modifier+
//! modifier   = (At (OpenParen Eq? Tilde? Int CloseParen)?) | And | Plus | Minus | Question
//!
//! note       = OpenParen (!CloseParen ANY)* CloseParen
//!
//! quantity   = num_val Whitespace !(unit_sep | auto_scale | val_sep) unit
//!            | val (val_sep val)* auto_scale? (unit_sep unit)?
//!
//! unit       = (!CloseBrace ANY)*
//!
//! val_sep    = Whitespace Or Whitespace
//! auto_scale = Whitespace Star Whitespace
//! unit_sep   = Whitespace Percent Whitespace
//!
//! val        = num_val | text_val
//! text_val   = (Word | Whitespace)*
//! num_val    = mixed_num | frac | range | num
//! mixed_num  = Int Whitespace frac
//! frac       = Int Whitespace Slash Whitespace Int
//! range      = num Whitespace Minus Whitespace Num
//! num        = Float | Int
//!
//!
//! ANY        = { Any token }
//! ```
//! This is more of a guideline, there may be edge cases that this grammar does
//! not cover but the pareser does.

mod block_parser;
mod metadata;
mod quantity;
mod section;
mod step;
mod token_stream;

use std::{borrow::Cow, collections::VecDeque};

use thiserror::Error;

use crate::{
    ast::{self, Text},
    context::Context,
    error::{PassResult, RichError},
    lexer::T,
    located::Located,
    parser::{metadata::metadata_entry, section::section, step::step},
    span::Span,
    Extensions,
};

pub(crate) use block_parser::BlockParser;
use token_stream::{Token, TokenStream};

/// Events generated by [`PullParser`]
///
/// When [`Event::StartStep`] is emitted, a later [`Event::EndStep`] is guaranteed.
/// The events inside these are items inside the step. No other [`Event::StartStep`]
/// will be emitted before the matching end event, as steps can't be nested
/// in cooklang.
#[derive(Debug, Clone, PartialEq)]
pub enum Event<'i> {
    Metadata { key: Text<'i>, value: Text<'i> },
    Section { name: Option<Text<'i>> },
    StartStep { is_text: bool },
    EndStep { is_text: bool },
    Text(Text<'i>),
    Ingredient(Located<ast::Ingredient<'i>>),
    Cookware(Located<ast::Cookware<'i>>),
    Timer(Located<ast::Timer<'i>>),

    Error(ParserError),
    Warning(ParserWarning),
}

/// Cooklang pull parser
///
/// This parser is an iterator of [`Event`]. No analysis pass is performed yet,
/// so no references, units or nothing is checked, just the structure of the
/// input.
#[derive(Debug)]
pub struct PullParser<'i, T>
where
    T: Iterator<Item = Token>,
{
    input: &'i str,
    tokens: std::iter::Peekable<T>,
    block: Vec<Token>,
    queue: VecDeque<Event<'i>>,
    extensions: Extensions,
}

impl<'i> PullParser<'i, TokenStream<'i>> {
    /// Creates a new parser
    pub fn new(input: &'i str, extensions: Extensions) -> Self {
        Self::new_from_token_iter(input, extensions, TokenStream::new(input))
    }
}

impl<'i, T> PullParser<'i, T>
where
    T: Iterator<Item = Token>,
{
    pub(crate) fn new_from_token_iter(input: &'i str, extensions: Extensions, tokens: T) -> Self {
        Self {
            input,
            tokens: tokens.peekable(),
            block: Vec::new(),
            extensions,
            queue: VecDeque::new(),
        }
    }

    /// Transforms the parser into another [`Event`] iterator that only
    /// generates [`Event::Metadata`] blocks.
    ///
    /// Warnings and errors may be generated too.
    ///
    /// This is not just filtering, the parsing process is different and
    /// optimized to ignore everything else.
    pub fn into_meta_iter(mut self) -> impl Iterator<Item = Event<'i>> {
        std::iter::from_fn(move || self.next_metadata())
    }
}

fn is_empty_token(tok: &Token) -> bool {
    matches!(
        tok.kind,
        T![ws] | T![block comment] | T![line comment] | T![newline]
    )
}

fn is_single_line_marker(first: Option<&Token>) -> bool {
    matches!(first, Some(mt![meta | =]))
}

struct LineInfo {
    is_empty: bool,
    is_single_line: bool,
}

impl<'i, T> PullParser<'i, T>
where
    T: Iterator<Item = Token>,
{
    fn pull_line(&mut self) -> Option<LineInfo> {
        let mut is_empty = true;
        let mut no_tokens = true;
        let is_single_line = is_single_line_marker(self.tokens.peek());
        for tok in self.tokens.by_ref() {
            self.block.push(tok);
            no_tokens = false;

            if !is_empty_token(&tok) {
                is_empty = false;
            }

            if tok.kind == T![newline] {
                break;
            }
        }
        if no_tokens {
            None
        } else {
            Some(LineInfo {
                is_empty,
                is_single_line,
            })
        }
    }

    /// Advances a block. Store the tokens, newline/eof excluded.
    pub(crate) fn next_block(&mut self) -> Option<()> {
        self.block.clear();
        let multiline_ext = self.extensions.contains(Extensions::MULTILINE_STEPS);

        // start and end are used to track the "non empty" part of the block
        let mut start = 0;
        let mut end;

        let mut current_line = self.pull_line()?;

        // Eat empty lines
        while current_line.is_empty {
            start = self.block.len();
            current_line = self.pull_line()?;
        }

        // Check if more lines have to be consumed
        let multiline = multiline_ext && !current_line.is_single_line;
        end = self.block.len();
        if multiline {
            loop {
                if is_single_line_marker(self.tokens.peek()) {
                    break;
                }
                match self.pull_line() {
                    None => break,
                    Some(line) if line.is_empty => break,
                    _ => {}
                }
                end = self.block.len();
            }
        }

        // trim trailing newline
        while let mt![newline] = self.block[end - 1] {
            if end <= start {
                break;
            }
            end -= 1;
        }
        // trim empty lines
        let trimmed_block = &self.block[start..end];
        if trimmed_block.is_empty() {
            return None;
        }

        let mut bp = BlockParser::new(trimmed_block, self.input, &mut self.queue, self.extensions);
        parse_block(&mut bp);
        bp.finish();

        Some(())
    }

    fn next_metadata_block(&mut self) -> Option<()> {
        self.block.clear();

        // eat until meta is found
        let mut last = T![newline];
        loop {
            let curr = self.tokens.peek()?.kind;
            if last == T![newline] && curr == T![meta] {
                break;
            }
            self.tokens.next();
            last = curr;
        }

        // eat until newline or end
        for tok in self.tokens.by_ref() {
            if tok.kind == T![newline] {
                break;
            }
            self.block.push(tok);
        }

        let mut bp = BlockParser::new(&self.block, self.input, &mut self.queue, self.extensions);
        if let Some(ev) = metadata_entry(&mut bp) {
            bp.event(ev);
        }
        bp.finish();

        Some(())
    }

    pub(crate) fn next_metadata(&mut self) -> Option<Event<'i>> {
        self.queue.pop_front().or_else(|| {
            self.next_metadata_block()?;
            self.next_metadata()
        })
    }
}

impl<'i, T> Iterator for PullParser<'i, T>
where
    T: Iterator<Item = Token>,
{
    type Item = Event<'i>;

    fn next(&mut self) -> Option<Self::Item> {
        self.queue.pop_front().or_else(|| {
            self.next_block()?;
            self.next()
        })
    }
}

fn parse_block(line: &mut BlockParser) {
    let meta_or_section = match line.peek() {
        T![meta] => line.with_recover(metadata_entry),
        T![=] => line.with_recover(section),
        _ => None,
    };

    if let Some(ev) = meta_or_section {
        line.event(ev);
        return;
    }
    step(line);
}

/// Builds an [`Ast`](ast::Ast) given an [`Event`] iterator
///
/// Probably the iterator you want is an instance of [`PullParser`].
#[tracing::instrument(level = "debug", skip_all)]
pub fn build_ast<'input>(
    events: impl Iterator<Item = Event<'input>>,
) -> PassResult<ast::Ast<'input>, ParserError, ParserWarning> {
    let mut blocks = Vec::new();
    let mut items = Vec::new();
    let mut ctx = Context::default();
    for event in events {
        match event {
            Event::Metadata { key, value } => blocks.push(ast::Block::Metadata { key, value }),
            Event::Section { name } => blocks.push(ast::Block::Section { name }),
            Event::StartStep { .. } => items.clear(),
            Event::EndStep { is_text } => {
                if !items.is_empty() {
                    blocks.push(ast::Block::Step {
                        is_text,
                        items: std::mem::take(&mut items),
                    })
                }
            }
            Event::Text(t) => items.push(ast::Item::Text(t)),
            Event::Ingredient(c) => items.push(ast::Item::Ingredient(c)),
            Event::Cookware(c) => items.push(ast::Item::Cookware(c)),
            Event::Timer(c) => items.push(ast::Item::Timer(c)),
            Event::Error(e) => ctx.error(e),
            Event::Warning(w) => ctx.warn(w),
        }
    }
    let ast = ast::Ast { blocks };
    ctx.finish(Some(ast))
}

/// get the span for a slice of tokens. panics if the slice is empty
pub(crate) fn tokens_span(tokens: &[Token]) -> Span {
    debug_assert!(!tokens.is_empty(), "tokens_span tokens empty");
    let start = tokens.first().unwrap().span.start();
    let end = tokens.last().unwrap().span.end();
    Span::new(start, end)
}

// match token type
macro_rules! mt {
    ($($reprs:tt)|*) => {
        $(Token {
            kind: T![$reprs],
            ..
        })|+
    }
}
pub(crate) use mt;

/// Errors generated by the [`PullParser`]
#[derive(Debug, Error, Clone, PartialEq)]
pub enum ParserError {
    #[error("A {container} is missing: {what}")]
    ComponentPartMissing {
        container: &'static str,
        what: &'static str,
        expected_pos: Span,
    },

    #[error("A {container} cannot have: {what}")]
    ComponentPartNotAllowed {
        container: &'static str,
        what: &'static str,
        to_remove: Span,
        help: Option<&'static str>,
    },

    #[error("Invalid {container} {what}: {reason}")]
    ComponentPartInvalid {
        container: &'static str,
        what: &'static str,
        reason: &'static str,
        labels: Vec<(Span, Option<Cow<'static, str>>)>,
        help: Option<&'static str>,
    },

    #[error("Duplicate ingredient modifier: {dup}")]
    DuplicateModifiers { modifiers_span: Span, dup: String },

    #[error("Error parsing integer number")]
    ParseInt {
        bad_bit: Span,
        source: std::num::ParseIntError,
    },

    #[error("Error parsing decimal number")]
    ParseFloat {
        bad_bit: Span,
        source: std::num::ParseFloatError,
    },

    #[error("Division by zero")]
    DivisionByZero { bad_bit: Span },

    #[error("Quantity scaling conflict")]
    QuantityScalingConflict { bad_bit: Span },
}

/// Warnings generated by the [`PullParser`]
#[derive(Debug, Error, Clone, PartialEq)]
pub enum ParserWarning {
    #[error("Empty metadata value for key: {key}")]
    EmptyMetadataValue { key: Located<String> },
    #[error("A {container} cannot have {what}, it will be ignored")]
    ComponentPartIgnored {
        container: &'static str,
        what: &'static str,
        ignored: Span,
        help: Option<&'static str>,
    },
}

impl RichError for ParserError {
    fn labels(&self) -> Vec<(Span, Option<Cow<'static, str>>)> {
        use crate::error::label;
        match self {
            ParserError::ComponentPartMissing {
                expected_pos: component_span,
                what,
                ..
            } => {
                vec![label!(component_span, format!("expected {what}"))]
            }
            ParserError::ComponentPartNotAllowed { to_remove, .. } => {
                vec![label!(to_remove, "remove this")]
            }
            ParserError::ComponentPartInvalid { labels, .. } => labels.clone(),
            ParserError::DuplicateModifiers { modifiers_span, .. } => vec![label!(modifiers_span)],
            ParserError::ParseInt { bad_bit, .. } => vec![label!(bad_bit)],
            ParserError::ParseFloat { bad_bit, .. } => vec![label!(bad_bit)],
            ParserError::DivisionByZero { bad_bit } => vec![label!(bad_bit)],
            ParserError::QuantityScalingConflict { bad_bit } => vec![label!(bad_bit)],
        }
    }

    fn help(&self) -> Option<Cow<'static, str>> {
        use crate::error::help;
        match self {
            ParserError::ComponentPartNotAllowed { help, .. } => help!(opt help),
            ParserError::ComponentPartInvalid { help, .. } => help!(opt help),
            ParserError::DuplicateModifiers { .. } => help!("Remove duplicate modifiers"),
            ParserError::DivisionByZero { .. } => {
                help!("Change this please, we don't want an infinite amount of anything")
            }
            ParserError::QuantityScalingConflict { .. } => help!("A quantity cannot have the auto scaling marker (*) and have fixed values at the same time"),
            _ => None,
        }
    }

    fn code(&self) -> Option<&'static str> {
        Some("parser")
    }
}

impl RichError for ParserWarning {
    fn labels(&self) -> Vec<(Span, Option<Cow<'static, str>>)> {
        use crate::error::label;
        match self {
            ParserWarning::EmptyMetadataValue { key } => {
                vec![label!(key)]
            }
            ParserWarning::ComponentPartIgnored { ignored, .. } => {
                vec![label!(ignored, "this is ignored")]
            }
        }
    }

    fn help(&self) -> Option<Cow<'static, str>> {
        use crate::error::help;
        match self {
            ParserWarning::EmptyMetadataValue { .. } => None,
            ParserWarning::ComponentPartIgnored { help, .. } => help!(opt help),
        }
    }

    fn code(&self) -> Option<&'static str> {
        Some("parser")
    }

    fn kind(&self) -> ariadne::ReportKind {
        ariadne::ReportKind::Warning
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::ast::*;

    #[test]
    fn just_metadata() {
        let parser = PullParser::new(
            indoc! {r#">> entry: true
        a test @step @salt{1%mg} more text
        a test @step @salt{1%mg} more text
        a test @step @salt{1%mg} more text
        >> entry2: uwu
        a test @step @salt{1%mg} more text
        "#},
            Extensions::empty(),
        );
        let events = parser.into_meta_iter().collect::<Vec<_>>();
        assert_eq!(
            events,
            vec![
                Event::Metadata {
                    key: Text::from_str(" entry", 2),
                    value: Text::from_str(" true", 10)
                },
                Event::Metadata {
                    key: Text::from_str(" entry2", 126),
                    value: Text::from_str(" uwu", 134)
                },
            ]
        );
    }

    #[test]
    fn multiline_spaces() {
        let parser = PullParser::new(
            "  This is a step           -- comment\n and this line continues  -- another comment",
            Extensions::MULTILINE_STEPS,
        );
        let (ast, warn, err) = build_ast(parser).into_tuple();

        // Only whitespace between line should be trimmed
        assert!(warn.is_empty());
        assert!(err.is_empty());
        assert_eq!(
            ast.unwrap().blocks,
            vec![Block::Step {
                is_text: false,
                items: vec![Item::Text({
                    let mut t = Text::empty(0);
                    t.append_str("  This is a step           ", 0);
                    t.append_fragment(TextFragment::soft_break("\n", 37));
                    t.append_str(" and this line continues  ", 39);
                    t
                })]
            }]
        );
    }
}
