use ferrow_fixture::Rng;
use ferrow_query::{Expr, Pattern, Query};

#[test]
fn precedence_modifiers_and_error_spans() {
    let q = Query::parse("a|b !<c|d>").unwrap();
    assert!(matches!(q.expression, Some(Expr::Or(_))));
    let q = Query::parse("case:path:wholeword:file:*.log").unwrap();
    let t = &q.terms[0];
    assert!(t.case_sensitive && t.path && t.wholeword);
    assert_eq!(t.directory, Some(false));
    assert_eq!(t.pattern, Pattern::Wildcard);
    assert_eq!(t.span, 0..30);
    for input in [
        "<a|b",
        "a|",
        "|a",
        "<>",
        "a>b",
        "folder:file:a",
        "ext:a;",
        "depth:-1",
        "size:>10mb",
        "regex:[",
    ] {
        let error = Query::parse(input).unwrap_err();
        let span = error.span();
        assert!(
            span.start <= span.end && span.end <= input.len(),
            "{input}: {span:?}"
        );
    }
    for input in [
        "<".repeat(100) + "a",
        "!".repeat(100) + "a",
        "a ".repeat(300),
        "a".repeat(5000),
    ] {
        assert!(Query::parse(&input).is_err());
    }
}

#[test]
fn random_unicode_input_never_panics_and_errors_stay_in_input() {
    let mut rng = Rng::new(123);
    let alphabet: Vec<char> = "aB:*!?|<>\";\\()[]{} \t\nÄΣ文🙂\0".chars().collect();
    for _ in 0..3000 {
        let input: String = (0..rng.below(96)).map(|_| *rng.pick(&alphabet)).collect();
        if let Err(error) = Query::parse(&input) {
            let span = error.span();
            assert!(
                span.start <= span.end && span.end <= input.len(),
                "{input:?}: {span:?}"
            );
        }
    }
}
