use std::str::FromStr;

use nom::branch::alt;
use nom::bytes::complete::{is_not, tag, tag_no_case, take};
use nom::character::complete::{alpha1, alphanumeric1, char, digit1, one_of};
use nom::combinator::{cut, map, opt, recognize, value};
use nom::error::context;
use nom::multi::{fold_many0, many0, separated_list0, separated_list1};
use nom::sequence::{delimited, pair, preceded, separated_pair, tuple};
use nom::IResult;

use crate::expr::{BinaryOperator, Expr, Literal, UnaryOperator};
use crate::planner::window::Period;
use crate::sql::ast::{GroupBy, Select, Source, SourceFrom};
use crate::Window;

pub fn sp(input: &str) -> IResult<&str, ()> {
    fold_many0(value((), one_of(" \t\n\r")), (), |_, _| ())(input)
}

pub fn ident(input: &str) -> IResult<&str, &str> {
    context(
        "ident",
        recognize(pair(
            alt((alpha1, tag("_"), tag("@"))),
            many0(alt((alphanumeric1, tag("_")))),
        )),
    )(input)
}

pub fn boolean(input: &str) -> IResult<&str, bool> {
    context(
        "boolean",
        alt((
            map(tag_no_case("true"), |_| true),
            map(tag_no_case("false"), |_| false),
        )),
    )(input)
}

pub fn integer(input: &str) -> IResult<&str, i64> {
    context(
        "integer",
        map(recognize(tuple((opt(char('-')), digit1))), |s| {
            i64::from_str(s).unwrap()
        }),
    )(input)
}

pub fn float(input: &str) -> IResult<&str, f64> {
    context(
        "float",
        map(
            recognize(tuple((
                opt(char('-')),
                alt((
                    map(tuple((digit1, pair(char('.'), opt(digit1)))), |_| ()),
                    map(tuple((char('.'), digit1)), |_| ()),
                )),
                opt(tuple((
                    alt((char('e'), char('E'))),
                    opt(alt((char('+'), char('-')))),
                    cut(digit1),
                ))),
            ))),
            |s| f64::from_str(s).unwrap(),
        ),
    )(input)
}

fn raw_string_quoted(input: &str, is_single_quote: bool) -> IResult<&str, String> {
    let quote_str = if is_single_quote { "\'" } else { "\"" };
    let double_quote_str = if is_single_quote { "\'\'" } else { "\"\"" };
    let backslash_quote = if is_single_quote { "\\\'" } else { "\\\"" };
    delimited(
        tag(quote_str),
        fold_many0(
            alt((
                is_not(backslash_quote),
                map(tag(double_quote_str), |_| -> &str {
                    if is_single_quote {
                        "\'"
                    } else {
                        "\""
                    }
                }),
                map(tag("\\\\"), |_| "\\"),
                map(tag("\\b"), |_| "\x7f"),
                map(tag("\\r"), |_| "\r"),
                map(tag("\\n"), |_| "\n"),
                map(tag("\\t"), |_| "\t"),
                map(tag("\\0"), |_| "\0"),
                map(tag("\\Z"), |_| "\x1A"),
                preceded(tag("\\"), take(1usize)),
            )),
            String::new(),
            |mut acc: String, s: &str| {
                acc.push_str(s);
                acc
            },
        ),
        tag(quote_str),
    )(input)
}

fn raw_string_single_quoted(input: &str) -> IResult<&str, String> {
    raw_string_quoted(input, true)
}

fn raw_string_double_quoted(input: &str) -> IResult<&str, String> {
    raw_string_quoted(input, false)
}

pub fn string(input: &str) -> IResult<&str, String> {
    context(
        "string",
        alt((raw_string_single_quoted, raw_string_double_quoted)),
    )(input)
}

pub fn literal(input: &str) -> IResult<&str, Literal> {
    context(
        "literal",
        alt((
            map(boolean, Literal::Boolean),
            map(float, Literal::Float),
            map(integer, Literal::Int),
            map(string, Literal::String),
        )),
    )(input)
}

pub fn name(input: &str) -> IResult<&str, String> {
    context("name", alt((string, map(ident, ToString::to_string))))(input)
}

pub fn column(input: &str) -> IResult<&str, Expr> {
    context(
        "input",
        alt((
            map(
                separated_pair(name, char('.'), name),
                |(qualifier, name)| Expr::Column {
                    qualifier: Some(qualifier),
                    name,
                },
            ),
            map(name, |name| Expr::Column {
                qualifier: None,
                name,
            }),
            map(
                separated_pair(name, char('.'), char('*')),
                |(qualifier, _)| Expr::Wildcard {
                    qualifier: Some(qualifier),
                },
            ),
            map(char('*'), |_| Expr::Wildcard { qualifier: None }),
        )),
    )(input)
}

pub fn expr(input: &str) -> IResult<&str, Expr> {
    context("expr", expr_a)(input)
}

fn expr_call(input: &str) -> IResult<&str, Expr> {
    let func_name = alt((
        map(tuple((ident, char('.'), ident)), |(namespace, _, name)| {
            (Some(namespace), name)
        }),
        map(ident, |name| (None, name)),
    ));
    let arguments = separated_list0(char(','), delimited(sp, expr, sp));
    context(
        "expr_call",
        map(
            tuple((func_name, sp, char('('), sp, arguments, sp, char(')'))),
            |((namespace, name), _, _, _, args, _, _)| Expr::Call {
                namespace: namespace.map(ToString::to_string),
                name: name.to_string(),
                args,
            },
        ),
    )(input)
}

fn expr_primitive(input: &str) -> IResult<&str, Expr> {
    let parens = map(
        tuple((char('('), sp, expr, sp, char(')'))),
        |(_, _, expr, _, _)| expr,
    );
    let p = alt((
        parens,
        expr_unary,
        expr_call,
        map(literal, Expr::Literal),
        column,
    ));
    context("expr_primitive", delimited(sp, p, sp))(input)
}

fn expr_unary(input: &str) -> IResult<&str, Expr> {
    let op = alt((
        value(UnaryOperator::Not, tag_no_case("not")),
        value(UnaryOperator::Neg, char('-')),
    ));
    map(separated_pair(op, sp, expr), |(op, expr)| Expr::Unary {
        op,
        expr: Box::new(expr),
    })(input)
}

fn expr_a(input: &str) -> IResult<&str, Expr> {
    let (input, lhs) = expr_b(input)?;
    let (input, exprs) = many0(tuple((
        value(BinaryOperator::Or, tag_no_case("or")),
        expr_b,
    )))(input)?;
    Ok((input, parse_expr(lhs, exprs)))
}

fn expr_b(input: &str) -> IResult<&str, Expr> {
    let (input, lhs) = expr_c(input)?;
    let (input, exprs) = many0(tuple((
        value(BinaryOperator::Or, tag_no_case("and")),
        expr_c,
    )))(input)?;
    Ok((input, parse_expr(lhs, exprs)))
}

fn expr_c(input: &str) -> IResult<&str, Expr> {
    let (input, lhs) = expr_d(input)?;
    let (input, exprs) = many0(tuple((
        alt((
            value(BinaryOperator::Eq, tag("=")),
            value(BinaryOperator::NotEq, tag("!=")),
            value(BinaryOperator::NotEq, tag("<>")),
            value(BinaryOperator::Lt, tag("<")),
            value(BinaryOperator::LtEq, tag("<")),
            value(BinaryOperator::Gt, tag(">")),
            value(BinaryOperator::GtEq, tag(">=")),
        )),
        expr_d,
    )))(input)?;
    Ok((input, parse_expr(lhs, exprs)))
}

fn expr_d(input: &str) -> IResult<&str, Expr> {
    let (input, lhs) = expr_e(input)?;
    let (input, exprs) = many0(tuple((
        alt((
            value(BinaryOperator::Plus, char('+')),
            value(BinaryOperator::Minus, char('-')),
        )),
        expr_e,
    )))(input)?;
    Ok((input, parse_expr(lhs, exprs)))
}

fn expr_e(input: &str) -> IResult<&str, Expr> {
    let (input, lhs) = expr_primitive(input)?;
    let (input, exprs) = many0(tuple((
        alt((
            value(BinaryOperator::Multiply, char('*')),
            value(BinaryOperator::Divide, char('/')),
        )),
        expr_primitive,
    )))(input)?;
    Ok((input, parse_expr(lhs, exprs)))
}

fn parse_expr(expr: Expr, rem: Vec<(BinaryOperator, Expr)>) -> Expr {
    rem.into_iter().fold(expr, |lhs, (op, rhs)| Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    })
}

fn projection_field(input: &str) -> IResult<&str, Expr> {
    context(
        "projection_field",
        alt((
            map(
                tuple((expr, sp, tag_no_case("as"), sp, name)),
                |(expr, _, _, _, alias)| expr.alias(alias),
            ),
            expr,
        )),
    )(input)
}

fn source_from(input: &str) -> IResult<&str, SourceFrom> {
    context(
        "source_from",
        alt((
            map(
                tuple((char('('), sp, select, sp, char(')'))),
                |(_, _, sub_query, _, _)| SourceFrom::SubQuery(Box::new(sub_query)),
            ),
            map(name, SourceFrom::Named),
        )),
    )(input)
}

fn source(input: &str) -> IResult<&str, Source> {
    context(
        "source",
        alt((
            map(
                tuple((source_from, sp, tag_no_case("as"), sp, name)),
                |(from, _, _, _, alias)| Source {
                    from,
                    alias: Some(alias),
                },
            ),
            map(source_from, |from| Source { from, alias: None }),
        )),
    )(input)
}

fn group_by(input: &str) -> IResult<&str, GroupBy> {
    context(
        "group_by",
        map(
            tuple((
                tag_no_case("group"),
                sp,
                tag_no_case("by"),
                sp,
                separated_list1(char(','), delimited(sp, expr, sp)),
            )),
            |(_, _, _, _, exprs)| GroupBy { exprs },
        ),
    )(input)
}

fn duration(input: &str) -> IResult<&str, i64> {
    let seconds = map(pair(integer, tag_no_case("s")), |(n, _)| n * 1000);
    let milliseconds = map(pair(integer, tag_no_case("ms")), |(n, _)| n);
    let minutes = map(pair(integer, tag_no_case("m")), |(n, _)| n * 1000 * 60);
    context("duration", alt((seconds, milliseconds, minutes)))(input)
}

fn window(input: &str) -> IResult<&str, Window> {
    let fixed_window = map(
        tuple((
            tag_no_case("fixed"),
            sp,
            char('('),
            sp,
            duration,
            sp,
            char(')'),
        )),
        |(_, _, _, _, length, _, _)| Window::Fixed { length },
    );
    let sliding_window = map(
        tuple((
            tag_no_case("sliding"),
            sp,
            char('('),
            sp,
            duration,
            sp,
            char(','),
            sp,
            duration,
            sp,
            char(')'),
        )),
        |(_, _, _, _, length, _, _, _, interval, _, _)| Window::Sliding { length, interval },
    );
    let period_window = map(
        alt((
            value(Period::Day, tag_no_case("day")),
            value(Period::Week, tag_no_case("week")),
            value(Period::Month, tag_no_case("month")),
            value(Period::Year, tag_no_case("year")),
        )),
        |period| Window::Period { period },
    );

    context(
        "window",
        map(
            tuple((
                tag_no_case("window"),
                sp,
                alt((fixed_window, sliding_window, period_window)),
            )),
            |(_, _, window)| window,
        ),
    )(input)
}

pub fn select(input: &str) -> IResult<&str, Select> {
    let projection = separated_list1(char(','), delimited(sp, projection_field, sp));
    let where_clause = map(tuple((tag_no_case("where"), sp, expr)), |(_, _, expr)| expr);
    let having_clause = map(tuple((tag_no_case("having"), sp, expr)), |(_, _, expr)| {
        expr
    });

    context(
        "select",
        map(
            tuple((
                tag_no_case("select"),
                delimited(sp, projection, sp),
                tag_no_case("from"),
                delimited(sp, source, sp),
                opt(delimited(sp, where_clause, sp)),
                opt(delimited(sp, group_by, sp)),
                opt(delimited(sp, having_clause, sp)),
                opt(delimited(sp, window, sp)),
            )),
            |(_, projection, _, source, where_clause, group_by, having_clause, window)| Select {
                projection,
                source,
                where_clause,
                having_clause,
                group_clause: group_by,
                window,
            },
        ),
    )(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sp() {
        assert_eq!(sp(" \t\r\n"), Ok(("", ())));
    }

    #[test]
    fn test_ident() {
        assert_eq!(ident("a"), Ok(("", "a")));
        assert_eq!(ident("abc"), Ok(("", "abc")));
        assert_eq!(ident("ABC"), Ok(("", "ABC")));
        assert_eq!(ident("a1"), Ok(("", "a1")));
        assert_eq!(ident("A1"), Ok(("", "A1")));
        assert_eq!(ident("a_b"), Ok(("", "a_b")));
        assert_eq!(ident("_ab"), Ok(("", "_ab")));
    }

    #[test]
    fn test_bool() {
        assert_eq!(boolean("true"), Ok(("", true)));
        assert_eq!(boolean("false"), Ok(("", false)));
        assert_eq!(boolean("True"), Ok(("", true)));
        assert_eq!(boolean("False"), Ok(("", false)));
        assert_eq!(boolean("TRUE"), Ok(("", true)));
        assert_eq!(boolean("FALSE"), Ok(("", false)));
    }

    #[test]
    fn test_integer() {
        assert_eq!(integer("123"), Ok(("", 123)));
        assert_eq!(integer("0123"), Ok(("", 123)));
        assert_eq!(integer("230"), Ok(("", 230)));
    }

    #[test]
    fn test_float() {
        assert_eq!(float("123.12"), Ok(("", 123.12)));
        assert_eq!(float("0123.45"), Ok(("", 123.45)));
        assert_eq!(float("12.0e+2"), Ok(("", 1200.0)));
        assert_eq!(float("12.0e-2"), Ok(("", 0.12)));
    }

    #[test]
    fn test_string() {
        assert_eq!(string(r#""abc""#), Ok(("", "abc".to_string())));
        assert_eq!(string(r#"'abc'"#), Ok(("", "abc".to_string())));
        assert_eq!(string(r#"'\nab\rc'"#), Ok(("", "\nab\rc".to_string())));
    }

    #[test]
    fn test_literal() {
        assert_eq!(literal(r#"true"#), Ok(("", Literal::Boolean(true))));
        assert_eq!(literal(r#"0"#), Ok(("", Literal::Int(0))));
        assert_eq!(literal(r#"127"#), Ok(("", Literal::Int(127))));
        assert_eq!(literal(r#"-128"#), Ok(("", Literal::Int(-128))));
        assert_eq!(
            literal(r#""abc""#),
            Ok(("", Literal::String("abc".to_string())))
        );
    }

    #[test]
    fn test_name() {
        assert_eq!(name(r#""abc""#), Ok(("", "abc".to_string())));
        assert_eq!(name(r#"abc"#), Ok(("", "abc".to_string())));
    }

    #[test]
    fn test_column() {
        assert_eq!(
            column(r#""abc".a"#),
            Ok((
                "",
                Expr::Column {
                    qualifier: Some("abc".to_string()),
                    name: "a".to_string()
                }
            ))
        );

        assert_eq!(
            column(r#"abc.'123'"#),
            Ok((
                "",
                Expr::Column {
                    qualifier: Some("abc".to_string()),
                    name: "123".to_string()
                }
            ))
        );

        assert_eq!(
            column(r#"abc"#),
            Ok((
                "",
                Expr::Column {
                    qualifier: None,
                    name: "abc".to_string()
                }
            ))
        );

        assert_eq!(
            column(r#"'123'"#),
            Ok((
                "",
                Expr::Column {
                    qualifier: None,
                    name: "123".to_string()
                }
            ))
        );
    }

    #[test]
    fn test_expr() {
        assert_eq!(
            expr(r#"2000+4/2"#),
            Ok((
                "",
                Expr::Literal(Literal::Int(2000))
                    + Expr::Literal(Literal::Int(4)) / Expr::Literal(Literal::Int(2))
            ))
        );

        assert_eq!(
            expr(r#"(2000+4)/2"#),
            Ok((
                "",
                (Expr::Literal(Literal::Int(2000)) + Expr::Literal(Literal::Int(4)))
                    / Expr::Literal(Literal::Int(2))
            ))
        );
    }

    #[test]
    fn test_expr_call() {
        assert_eq!(
            expr_call(r#"sum(a)"#),
            Ok((
                "",
                Expr::Call {
                    namespace: None,
                    name: "sum".to_string(),
                    args: vec![Expr::Column {
                        qualifier: None,
                        name: "a".to_string()
                    }]
                }
            ))
        );

        assert_eq!(
            expr_call(r#"c(a, 1, b, 2)"#),
            Ok((
                "",
                Expr::Call {
                    namespace: None,
                    name: "c".to_string(),
                    args: vec![
                        Expr::Column {
                            qualifier: None,
                            name: "a".to_string()
                        },
                        Expr::Literal(Literal::Int(1)),
                        Expr::Column {
                            qualifier: None,
                            name: "b".to_string()
                        },
                        Expr::Literal(Literal::Int(2)),
                    ]
                }
            ))
        );

        assert_eq!(
            expr_call(r#"abc.sum(a)"#),
            Ok((
                "",
                Expr::Call {
                    namespace: Some("abc".to_string()),
                    name: "sum".to_string(),
                    args: vec![Expr::Column {
                        qualifier: None,
                        name: "a".to_string()
                    }]
                }
            ))
        );
    }

    #[test]
    fn test_source() {
        assert_eq!(
            source(r#"abc"#),
            Ok((
                "",
                Source {
                    from: SourceFrom::Named("abc".to_string()),
                    alias: None
                }
            ))
        );
    }

    #[test]
    fn test_window() {
        assert_eq!(
            window(r#"window fixed(5m)"#),
            Ok((
                "",
                Window::Fixed {
                    length: 1000 * 5 * 60
                },
            ))
        );

        assert_eq!(
            window(r#"window sliding(5m, 1m)"#),
            Ok((
                "",
                Window::Sliding {
                    length: 1000 * 5 * 60,
                    interval: 1000 * 60,
                },
            ))
        );

        assert_eq!(
            window(r#"window day"#),
            Ok((
                "",
                Window::Period {
                    period: Period::Day
                },
            ))
        );

        assert_eq!(
            window(r#"window week"#),
            Ok((
                "",
                Window::Period {
                    period: Period::Week
                },
            ))
        );

        assert_eq!(
            window(r#"window month"#),
            Ok((
                "",
                Window::Period {
                    period: Period::Month
                },
            ))
        );

        assert_eq!(
            window(r#"window year"#),
            Ok((
                "",
                Window::Period {
                    period: Period::Year
                },
            ))
        );
    }

    #[test]
    fn test_select() {
        assert_eq!(
            select(r#"select a, b, a+b, sum(a) from t"#),
            Ok((
                "",
                Select {
                    projection: vec![
                        Expr::Column {
                            qualifier: None,
                            name: "a".to_string()
                        },
                        Expr::Column {
                            qualifier: None,
                            name: "b".to_string()
                        },
                        Expr::Column {
                            qualifier: None,
                            name: "a".to_string()
                        } + Expr::Column {
                            qualifier: None,
                            name: "b".to_string()
                        },
                        Expr::Call {
                            namespace: None,
                            name: "sum".to_string(),
                            args: vec![Expr::Column {
                                qualifier: None,
                                name: "a".to_string()
                            }]
                        }
                    ],
                    source: Source {
                        from: SourceFrom::Named("t".to_string()),
                        alias: None
                    },
                    where_clause: None,
                    having_clause: None,
                    group_clause: None,
                    window: None,
                },
            )),
        );

        assert_eq!(
            select(r#"select a, b from t where a>10"#),
            Ok((
                "",
                Select {
                    projection: vec![
                        Expr::Column {
                            qualifier: None,
                            name: "a".to_string()
                        },
                        Expr::Column {
                            qualifier: None,
                            name: "b".to_string()
                        },
                    ],
                    source: Source {
                        from: SourceFrom::Named("t".to_string()),
                        alias: None
                    },
                    where_clause: Some(
                        Expr::Column {
                            qualifier: None,
                            name: "a".to_string()
                        }
                        .gt(Expr::Literal(Literal::Int(10)))
                    ),
                    having_clause: None,
                    group_clause: None,
                    window: None
                },
            )),
        );

        assert_eq!(
            select(r#"select a, b from t where a>10 group by b window fixed(5m)"#),
            Ok((
                "",
                Select {
                    projection: vec![
                        Expr::Column {
                            qualifier: None,
                            name: "a".to_string()
                        },
                        Expr::Column {
                            qualifier: None,
                            name: "b".to_string()
                        },
                    ],
                    source: Source {
                        from: SourceFrom::Named("t".to_string()),
                        alias: None
                    },
                    where_clause: Some(
                        Expr::Column {
                            qualifier: None,
                            name: "a".to_string()
                        }
                        .gt(Expr::Literal(Literal::Int(10)))
                    ),
                    having_clause: None,
                    group_clause: Some(GroupBy {
                        exprs: vec![Expr::Column {
                            qualifier: None,
                            name: "b".to_string()
                        }]
                    }),
                    window: Some(Window::Fixed {
                        length: 5 * 1000 * 60
                    })
                },
            )),
        );
    }
}
