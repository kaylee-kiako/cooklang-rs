use super::{token_stream::Token, Event, ParserError, ParserWarning};
use crate::{
    ast::{self, TextFragment},
    lexer::{TokenKind, T},
    Extensions,
};

pub(crate) struct BlockParser<'t, 'i> {
    base_offset: usize,
    tokens: &'t [Token],
    pub(crate) current: usize,
    pub(crate) input: &'i str,
    pub(crate) extensions: Extensions,
    pub(crate) events: Vec<Event<'i>>,
}

impl<'t, 'i> BlockParser<'t, 'i> {
    /// Create it from separate parts.
    /// - tokens must be adjacent (checked in debug)
    /// - slices's tokens's span must refer to the input (checked in debug)
    /// - input is the whole input str given to the lexer
    pub(crate) fn new(
        base_offset: usize,
        tokens: &'t [Token],
        input: &'i str,
        extensions: Extensions,
    ) -> Self {
        debug_assert!(
            tokens.is_empty()
                || (tokens.first().unwrap().span.start() < input.len()
                    && tokens.last().unwrap().span.end() <= input.len()),
            "tokens out of input bounds"
        );
        debug_assert!(
            tokens
                .windows(2)
                .all(|w| w[0].span.end() == w[1].span.start()),
            "tokens are not adjacent"
        );
        Self {
            base_offset,
            tokens,
            current: 0,
            input,
            extensions,
            events: Vec::default(),
        }
    }

    pub(crate) fn event(&mut self, ev: Event<'i>) {
        self.events.push(ev);
    }

    /// Finish parsing the line, this will return the events generated
    ///
    /// Panics if any token is left.
    pub(crate) fn finish(self) -> Vec<Event<'i>> {
        assert_eq!(
            self.current,
            self.tokens.len(),
            "Block tokens not parsed. this is a bug"
        );
        self.events
    }

    pub(crate) fn extension(&self, ext: Extensions) -> bool {
        self.extensions.contains(ext)
    }

    /// Runs a function that can fail to parse the input.
    ///
    /// If the function succeeds, is just as it was called withtout recover.
    /// If the function fails, any token eaten by it will be restored.
    ///
    /// Note that any other state modification such as adding errors to the
    /// context will not be rolled back.
    pub(crate) fn with_recover<F, O>(&mut self, f: F) -> Option<O>
    where
        F: FnOnce(&mut Self) -> Option<O>,
    {
        let old_current = self.current;
        let r = f(self);
        if r.is_none() {
            self.current = old_current;
        }
        r
    }

    /// Gets a token's matching str from the input
    pub(crate) fn as_str(&self, token: Token) -> &'i str {
        &self.input[token.span.range()]
    }

    pub(crate) fn text(&self, offset: usize, tokens: &[Token]) -> ast::Text<'i> {
        debug_assert!(
            tokens
                .windows(2)
                .all(|w| w[0].span.end() == w[1].span.start()),
            "tokens are not adjacent"
        );

        let mut t = ast::Text::empty(offset);
        if tokens.is_empty() {
            return t;
        }
        let mut start = tokens[0].span.start();
        let mut end = start;
        assert_eq!(offset, start, "Offset of {:?} must be {offset}", tokens[0]);

        for token in tokens {
            match token.kind {
                T![newline] => {
                    t.append_str(&self.input[start..end], start);
                    t.append_fragment(TextFragment::soft_break(
                        &self.input[token.span.range()],
                        token.span.start(),
                    ));
                    start = token.span.end();
                    end = start;
                }
                T![line comment] | T![block comment] => {
                    t.append_str(&self.input[start..end], start);
                    start = token.span.end();
                    end = start;
                }
                T![escaped] => {
                    t.append_str(&self.input[start..end], start);
                    debug_assert_eq!(token.len(), 2, "unexpected escaped token length");
                    start = token.span.start() + 1; // skip "\"
                    end = token.span.end()
                }
                _ => end = token.span.end(),
            }
        }
        t.append_str(&self.input[start..end], start);
        t
    }

    /// Returns the current offset from the start of input
    pub(crate) fn current_offset(&self) -> usize {
        self.parsed()
            .last()
            .map(|t| t.span.end())
            .unwrap_or(self.base_offset)
    }

    pub(crate) fn tokens_consumed(&self) -> usize {
        self.current
    }

    pub(crate) fn tokens(&self) -> &'t [Token] {
        self.tokens
    }

    pub(crate) fn parsed(&self) -> &'t [Token] {
        self.tokens.split_at(self.current).0
    }

    /// Returns the not parsed tokens
    pub(crate) fn rest(&self) -> &'t [Token] {
        self.tokens.split_at(self.current).1
    }

    pub(crate) fn consume_rest(&mut self) -> &'t [Token] {
        let r = self.rest();
        self.current += r.len();
        r
    }

    /// Peeks the next token without consuming it.
    pub(crate) fn peek(&self) -> TokenKind {
        self.tokens
            .get(self.current)
            .map(|token| token.kind)
            .unwrap_or(TokenKind::Eof)
    }

    /// Checks the next token without consuming it.
    pub(crate) fn at(&self, kind: TokenKind) -> bool {
        self.peek() == kind
    }

    /// Advance to the next token.
    #[must_use]
    pub(crate) fn next_token(&mut self) -> Option<Token> {
        if let Some(token) = self.tokens.get(self.current) {
            self.current += 1;
            Some(*token)
        } else {
            None
        }
    }

    /// Same as [Self::next_token] but panics if there are no more tokens.
    pub(crate) fn bump_any(&mut self) -> Token {
        self.next_token()
            .expect("Expected token, but there was none")
    }

    /// Call [Self::next_token] but panics if the next token is not `expected`.
    pub(crate) fn bump(&mut self, expected: TokenKind) -> Token {
        let token = self.bump_any();
        assert_eq!(
            token.kind, expected,
            "Expected '{expected:?}', but got '{:?}'",
            token.kind
        );
        token
    }

    /// Takes until condition reached, if never reached, return none
    pub(crate) fn until(&mut self, f: impl Fn(TokenKind) -> bool) -> Option<&'t [Token]> {
        let rest = self.rest();
        let pos = rest.iter().position(|t| f(t.kind))?;
        let s = &rest[..pos];
        self.current += pos;
        Some(s)
    }

    /// Consumes while the closure returns true or the block ends
    pub(crate) fn consume_while(&mut self, f: impl Fn(TokenKind) -> bool) -> &'t [Token] {
        let rest = self.rest();
        let pos = rest.iter().position(|t| !f(t.kind)).unwrap_or(rest.len());
        let s = &rest[..pos];
        self.current += pos;
        s
    }

    pub(crate) fn ws_comments(&mut self) -> &'t [Token] {
        self.consume_while(|t| matches!(t, T![ws] | T![line comment] | T![block comment]))
    }

    /// Call [Self::next_token] if the next token is `expected`.
    #[must_use]
    pub(crate) fn consume(&mut self, expected: TokenKind) -> Option<Token> {
        if self.at(expected) {
            Some(self.bump_any())
        } else {
            None
        }
    }

    pub(crate) fn error(&mut self, error: ParserError) {
        self.event(Event::Error(error))
    }
    pub(crate) fn warn(&mut self, warn: ParserWarning) {
        self.event(Event::Warning(warn))
    }
}
