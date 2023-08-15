use smallvec::SmallVec;

use crate::{
    ast, context::Recover, error::label, lexer::T, located::Located, quantity::Value, span::Span,
    Extensions,
};

use super::{mt, token_stream::Token, tokens_span, BlockParser, ParserError};

pub struct ParsedQuantity<'a> {
    pub quantity: Located<ast::Quantity<'a>>,
    pub unit_separator: Option<Span>,
}

/// "parent" block parser. This is just to emit error/warnings and get the text. No tokens will be consumed
/// `tokens` inside '{' '}'. must not be empty
pub(crate) fn parse_quantity<'input>(
    bp: &mut BlockParser<'_, 'input>,
    tokens: &[Token],
) -> ParsedQuantity<'input> {
    assert!(!tokens.is_empty(), "empty quantity tokens. this is a bug.");

    // create an insolated sub-block for the quantity tokens
    let mut bp2 = BlockParser::new(
        tokens.first().unwrap().span.start(),
        tokens,
        bp.input,
        bp.extensions,
    );

    let advanced = bp2
        .extension(Extensions::ADVANCED_UNITS)
        .then(|| bp2.with_recover(parse_advanced_quantity))
        .flatten();
    let quantity = advanced.unwrap_or_else(|| parse_regular_quantity(&mut bp2));

    bp.events.append(&mut bp2.events);

    quantity
}

fn parse_regular_quantity<'i>(bp: &mut BlockParser<'_, 'i>) -> ParsedQuantity<'i> {
    let mut value = many_values(bp);
    let mut unit_separator = None;
    let unit = match bp.peek() {
        T![%] => {
            let sep = bp.bump_any();
            unit_separator = Some(sep.span);
            let unit = bp.consume_rest();
            if unit
                .iter()
                .all(|t| matches!(t.kind, T![ws] | T![block comment]))
            {
                let span = if unit.is_empty() {
                    Span::pos(sep.span.end())
                } else {
                    Span::new(sep.span.start(), unit.last().unwrap().span.end())
                };
                bp.error(ParserError::ComponentPartInvalid {
                    container: "quantity",
                    what: "unit",
                    reason: "is empty",
                    labels: vec![
                        label!(sep.span, "remove this"),
                        label!(span, "or add unit here"),
                    ],
                    help: None,
                });
                None
            } else {
                Some(bp.text(sep.span.end(), unit))
            }
        }
        T![eof] => None,
        _ => {
            bp.consume_rest();
            let text = bp.text(bp.tokens().first().unwrap().span.start(), bp.tokens());
            let text_val = Value::Text {
                value: text.text_trimmed().into_owned(),
            };
            value = ast::QuantityValue::Single {
                value: Located::new(text_val, text.span()),
                auto_scale: None,
            };
            None
        }
    };

    ParsedQuantity {
        quantity: Located::new(ast::Quantity { value, unit }, tokens_span(bp.tokens())),
        unit_separator,
    }
}

fn parse_advanced_quantity<'i>(bp: &mut BlockParser<'_, 'i>) -> Option<ParsedQuantity<'i>> {
    if bp
        .tokens()
        .iter()
        .any(|t| matches!(t.kind, T![|] | T![*] | T![%]))
    {
        return None;
    }

    bp.ws_comments();
    let value_tokens = bp.consume_while(|t| !matches!(t, T![word]));

    if value_tokens.is_empty() || value_tokens.last().unwrap().kind != T![ws] {
        return None;
    }

    let value_tokens = {
        // beginning already trimmed
        let end_pos = value_tokens
            .iter()
            .rposition(|t| !matches!(t.kind, T![ws] | T![block comment]))
            .unwrap(); // ws_comments were already cosumed and then checked non empty
        &value_tokens[..=end_pos]
    };

    let value_span = {
        let start = value_tokens.first().unwrap().span.start();
        let end = value_tokens.last().unwrap().span.end();
        Span::new(start, end)
    };

    let result = numeric_value(value_tokens, bp)?;
    let value = match result {
        Ok(value) => value,
        Err(err) => {
            bp.error(err);
            Value::recover()
        }
    };
    let value = Located::new(value, value_span);

    let unit = bp.consume_rest();
    if unit.is_empty() {
        return None;
    }
    let unit = bp.text(unit.first().unwrap().span.start(), unit);
    Some(ParsedQuantity {
        quantity: Located::new(
            ast::Quantity {
                value: ast::QuantityValue::Single {
                    value,
                    auto_scale: None,
                },
                unit: Some(unit),
            },
            tokens_span(bp.tokens()),
        ),
        unit_separator: None,
    })
}

fn many_values(bp: &mut BlockParser) -> ast::QuantityValue {
    let mut values: Vec<Located<Value>> = vec![];
    let mut auto_scale = None;

    loop {
        let value_tokens = bp.consume_while(|t| !matches!(t, T![|] | T![*] | T![%]));
        values.push(parse_value(value_tokens, bp));

        match bp.peek() {
            T![|] => {
                bp.bump_any();
            }
            T![*] => {
                let tok = bp.bump_any();
                if values.len() == 1 {
                    auto_scale = Some(tok.span);
                } else {
                    bp.error(ParserError::QuantityScalingConflict {
                        bad_bit: Span::new(values[0].span().end(), tok.span.end()),
                    });
                }
                break;
            }
            _ => break,
        }
    }

    match values.len() {
        1 => ast::QuantityValue::Single {
            value: values.pop().unwrap(),
            auto_scale,
        },
        2.. => {
            if let Some(span) = auto_scale {
                bp.error(ParserError::ComponentPartInvalid {
                    container: "quantity",
                    what: "value",
                    reason: "auto scale is not compatible with multiple values",
                    labels: vec![label!(span, "remove this")],
                    help: None,
                });
            }
            ast::QuantityValue::Many(values)
        }
        _ => unreachable!(), // first iter is guaranteed
    }
}

fn parse_value(tokens: &[Token], bp: &mut BlockParser) -> Located<Value> {
    let start = tokens
        .first()
        .map(|t| t.span.start())
        .unwrap_or(bp.current_offset()); // if empty, use the current offset
    let end = bp.current_offset();
    let span = Span::new(start, end);

    let result = numeric_value(tokens, bp).unwrap_or_else(|| Ok(text_value(tokens, start, bp)));

    let val = match result {
        Ok(value) => value,
        Err(err) => {
            bp.error(err);
            Value::recover()
        }
    };

    Located::new(val, span)
}

fn text_value(tokens: &[Token], offset: usize, bp: &mut BlockParser) -> Value {
    let text = bp.text(offset, tokens);
    if text.is_text_empty() {
        bp.error(ParserError::ComponentPartInvalid {
            container: "quantity",
            what: "value",
            reason: "is empty",
            labels: vec![label!(text.span(), "empty value here")],
            help: None,
        });
    }
    Value::Text {
        value: text.text_trimmed().into_owned(),
    }
}

fn numeric_value(tokens: &[Token], bp: &BlockParser) -> Option<Result<Value, ParserError>> {
    // All the numeric values will be at most 4 tokens
    let filtered_tokens: SmallVec<[Token; 4]> = tokens
        .iter()
        .filter(|t| !matches!(t.kind, T![ws] | T![line comment] | T![block comment]))
        .copied()
        .collect();

    let r = match *filtered_tokens.as_slice() {
        // int
        [t @ mt![int]] => int(t, bp).map(|v| Value::Number { value: v }),
        // float
        [t @ mt![float]] => float(t, bp).map(|v| Value::Number { value: v }),
        // mixed number
        [i @ mt![int], a @ mt![int], mt![/], b @ mt![int]] => {
            mixed_num(i, a, b, bp).map(|v| Value::Number { value: v })
        }
        // frac
        [a @ mt![int], mt![/], b @ mt![int]] => frac(a, b, bp).map(|v| Value::Number { value: v }),
        // range
        [s @ mt![int | float], mt![-], e @ mt![int | float]]
            if bp.extension(Extensions::RANGE_VALUES) =>
        {
            range(s, e, bp).map(|v| Value::Range { value: v })
        }
        // other => text
        _ => return None,
    };
    Some(r)
}

fn mixed_num(i: Token, a: Token, b: Token, bp: &BlockParser) -> Result<f64, ParserError> {
    let i = int(i, bp)?;
    let f = frac(a, b, bp)?;
    Ok(i + f)
}

fn frac(a: Token, b: Token, line: &BlockParser) -> Result<f64, ParserError> {
    let span = Span::new(a.span.start(), b.span.end());
    let a = int(a, line)?;
    let b = int(b, line)?;

    if b == 0.0 {
        Err(ParserError::DivisionByZero { bad_bit: span })
    } else {
        Ok(a / b)
    }
}

fn range(
    s: Token,
    e: Token,
    bp: &BlockParser,
) -> Result<std::ops::RangeInclusive<f64>, ParserError> {
    let start = num(s, bp)?;
    let end = num(e, bp)?;
    Ok(start..=end)
}

fn num(t: Token, block: &BlockParser) -> Result<f64, ParserError> {
    match t.kind {
        T![int] => int(t, block),
        T![float] => float(t, block),
        _ => panic!("Unexpected num token: {t:?}"),
    }
}

fn int(tok: Token, block: &BlockParser) -> Result<f64, ParserError> {
    assert_eq!(tok.kind, T![int]);
    block
        .as_str(tok)
        .parse::<u32>()
        .map(|i| i as f64)
        .map_err(|e| ParserError::ParseInt {
            bad_bit: tok.span,
            source: e,
        })
}

fn float(tok: Token, bp: &BlockParser) -> Result<f64, ParserError> {
    assert_eq!(tok.kind, T![float]);
    bp.as_str(tok)
        .parse::<f64>()
        .map_err(|e| ParserError::ParseFloat {
            bad_bit: tok.span,
            source: e,
        })
}

#[cfg(test)]
mod tests {
    use crate::{
        ast::{QuantityValue, Text},
        parser::token_stream::TokenStream,
    };

    macro_rules! t {
        ($input:literal) => {
            t!($input, $crate::Extensions::all())
        };
        ($input:literal, $extensions:expr) => {{
            let input = $input;
            let tokens = TokenStream::new(input).collect::<Vec<_>>();
            let mut bp = BlockParser::new(0, &[], input, $extensions);
            let q = parse_quantity(&mut bp, &tokens);
            let mut ctx = crate::context::Context::<
                crate::parser::ParserError,
                crate::parser::ParserWarning,
            >::default();
            bp.finish().into_iter().for_each(|ev| match ev {
                crate::parser::Event::Error(e) => ctx.error(e),
                crate::parser::Event::Warning(w) => ctx.warn(w),
                _ => {}
            });
            (q.quantity.into_inner(), q.unit_separator, ctx)
        }};
    }

    use super::*;
    #[test]
    fn basic_quantity() {
        let (q, s, _) = t!("100%ml");
        assert_eq!(
            q.value,
            QuantityValue::Single {
                value: Located::new(Value::Number { value: 100.0 }, 0..3),
                auto_scale: None,
            }
        );
        assert_eq!(s, Some(Span::new(3, 4)));
        assert_eq!(q.unit.unwrap().text(), "ml");
    }

    #[test]
    fn no_separator_ext() {
        let (q, s, ctx) = t!("100 ml");
        assert_eq!(
            q.value,
            QuantityValue::Single {
                value: Located::new(Value::Number { value: 100.0 }, 0..3),
                auto_scale: None
            }
        );
        assert_eq!(s, None);
        assert_eq!(q.unit.unwrap().text(), "ml");
        assert!(ctx.is_empty());

        let (q, s, ctx) = t!("100 ml", Extensions::all() ^ Extensions::ADVANCED_UNITS);
        assert_eq!(
            q.value,
            QuantityValue::Single {
                value: Located::new(
                    Value::Text {
                        value: "100 ml".into()
                    },
                    0..6
                ),
                auto_scale: None
            }
        );
        assert_eq!(s, None);
        assert_eq!(q.unit, None);
        assert!(ctx.is_empty());
    }

    #[test]
    fn many_values() {
        let (q, s, ctx) = t!("100|200|300%ml");
        assert_eq!(
            q.value,
            QuantityValue::Many(vec![
                Located::new(Value::Number { value: 100.0 }, 0..3),
                Located::new(Value::Number { value: 200.0 }, 4..7),
                Located::new(Value::Number { value: 300.0 }, 8..11),
            ])
        );
        assert_eq!(s, Some((11..12).into()));
        assert_eq!(q.unit.unwrap(), Text::from_str("ml", 12));
        assert!(ctx.is_empty());

        let (q, s, ctx) = t!("100|2-3|str*%ml");
        assert_eq!(
            q.value,
            QuantityValue::Many(vec![
                Located::new(Value::Number { value: 100.0 }, 0..3),
                Located::new(Value::Range { value: 2.0..=3.0 }, 4..7),
                Located::new(
                    Value::Text {
                        value: "str".into()
                    },
                    8..11
                ),
            ])
        );
        assert_eq!(s, Some((12..13).into()));
        assert_eq!(q.unit.unwrap(), Text::from_str("ml", 13));
        assert_eq!(ctx.errors.len(), 1);
        assert!(ctx.warnings.is_empty());

        let (q, _, ctx) = t!("100|");
        assert_eq!(
            q.value,
            QuantityValue::Many(vec![
                Located::new(Value::Number { value: 100.0 }, 0..3),
                Located::new(Value::Text { value: "".into() }, 4..4)
            ])
        );
        assert_eq!(ctx.errors.len(), 1);
        assert!(ctx.warnings.is_empty());
    }

    #[test]
    fn range_value() {
        let (q, _, _) = t!("2-3");
        assert_eq!(
            q.value,
            QuantityValue::Single {
                value: Located::new(Value::Range { value: 2.0..=3.0 }, 0..3),
                auto_scale: None
            }
        );
        assert_eq!(q.unit, None);
    }

    #[test]
    fn range_value_no_extension() {
        let (q, _, _) = t!("2-3", Extensions::empty());
        assert_eq!(
            q.value,
            QuantityValue::Single {
                value: Located::new(
                    Value::Text {
                        value: "2-3".into()
                    },
                    0..3
                ),
                auto_scale: None
            }
        );
        assert_eq!(q.unit, None);
    }
}
