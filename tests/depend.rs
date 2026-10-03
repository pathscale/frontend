use frontend::frontend_facts::depend::{
    AssignmentFact, Dependency, Exit, ExitKind, FunctionDependence, LiteralKind, TextRange,
    dependence,
};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn analyze(source: &str, function: &str) -> FunctionDependence {
    frontend::unwind_janky::install_catcher(catcher);
    let before = frontend::rustc_const_eval::interp_cx_new_count();
    let facts = dependence(source, function).expect("function parses and is found");
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), before);
    facts
}

fn has_variable(dependencies: &[Dependency], name: &str) -> bool {
    dependencies.iter().any(|dependency| {
        matches!(dependency, Dependency::Variable { variable, .. } if variable.name == name)
    })
}

fn exit(facts: &FunctionDependence, kind: ExitKind) -> &Exit {
    facts.exits.iter().find(|exit| exit.kind == kind).expect("exit exists")
}

#[test]
fn branch_scan_keeps_control_dependencies_separate_from_length() {
    let source = r#"
        fn scan(bytes: &[u8], close: u8) -> usize {
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == close {
                    return i + 1;
                }
                i += 1;
            }
            bytes.len()
        }

        fn length(src: &[u8], close: u8) -> usize {
            src.len()
        }
    "#;

    let scan = analyze(source, "scan");
    let returned = exit(&scan, ExitKind::Return);
    assert!(has_variable(&returned.data_dependencies, "i"));
    assert!(!has_variable(&returned.data_dependencies, "close"));
    let branch = returned
        .control_dependencies
        .iter()
        .find(|condition| condition.condition == "bytes[i] == close")
        .expect("the returned index is controlled by the closer comparison");
    assert!(has_variable(&branch.operands, "bytes"));
    assert!(has_variable(&branch.operands, "i"));
    assert!(has_variable(&branch.operands, "close"));

    let length = analyze(source, "length");
    let tail = exit(&length, ExitKind::Tail);
    assert!(has_variable(&tail.data_dependencies, "src"));
    assert!(!has_variable(&tail.data_dependencies, "close"));
    assert!(tail.control_dependencies.is_empty());
}

#[test]
fn mutable_binding_can_be_reported_never_assigned() {
    let facts = analyze("fn unchanged(mut i: usize) -> usize { i }", "unchanged");
    let assignment = facts
        .mutable_assignments
        .iter()
        .find(|assignment| assignment.variable.name == "i")
        .expect("mutable parameter is reported");
    assert_eq!(assignment.assigned, AssignmentFact::Never);
    assert!(assignment.assignment_sites.is_empty());
}

#[test]
fn dependency_occurrence_tracks_a_read_through_a_local_binding() {
    let source = "fn alias(item: usize) -> usize { let value = item; value }";
    let facts = analyze(source, "alias");
    let returned = exit(&facts, ExitKind::Tail);
    let value_start = source.rfind("value").expect("tail value is present") as u32;
    let value_occurrence = TextRange { start: value_start, end: value_start + 5 };

    assert!(returned.data_dependencies.iter().any(|dependency| {
        matches!(
            dependency,
            Dependency::Variable { variable, occurrence }
                if variable.name == "item" && *occurrence == value_occurrence
        )
    }));
}

#[test]
fn mutable_assignment_reports_each_assignment_expression_span() {
    let source = "fn update(mut state: usize) -> usize { state += 1; state }";
    let facts = analyze(source, "update");
    let assignment = facts
        .mutable_assignments
        .iter()
        .find(|assignment| assignment.variable.name == "state")
        .expect("mutable parameter is reported");
    let start = source.find("state += 1").expect("assignment is present") as u32;

    assert_eq!(
        assignment.assignment_sites.as_slice(),
        &[TextRange { start, end: start + "state += 1".len() as u32 }]
    );
}

#[test]
fn early_return_inside_loop_reaches_a_loop_fixpoint() {
    let source = r#"
        fn find(mut i: usize, close: usize) -> usize {
            loop {
                if i == close {
                    return i;
                }
                i += 1;
            }
        }
    "#;
    let facts = analyze(source, "find");
    let returned = exit(&facts, ExitKind::Return);
    assert!(has_variable(&returned.data_dependencies, "i"));
    let branch = returned
        .control_dependencies
        .iter()
        .find(|condition| condition.condition == "i == close")
        .expect("the early return is controlled by the loop comparison");
    assert!(has_variable(&branch.operands, "i"));
    assert!(has_variable(&branch.operands, "close"));

    let assignment = facts
        .mutable_assignments
        .iter()
        .find(|assignment| assignment.variable.name == "i")
        .expect("mutable parameter is reported");
    assert_eq!(assignment.assigned, AssignmentFact::Ever);
}

#[test]
fn probe_tmp_quoted_end_depend() {
    let source = "fn quoted_end(src: &str, mut i: usize) -> usize {\n    let mut just = false ; for (byte , the) in src [i ..] . char_indices () { let byte = i + byte ; if just { just = false ; } else if the == '\\\\' { just = true ; } else if the == '\"' { return byte + the . len_utf8 () ; } } src . len ()\n}";
    let facts = analyze(source, "quoted_end");
    eprintln!("PROBE3 unknowns={:?}", facts.unknowns);
}

#[test]
fn probe_tmp_quoted_end_depend_with_doc() {
    let source = include_str!("probe_src.rs.txt");
    let facts = analyze(source, "quoted_end");
    eprintln!("PROBE4 unknowns={:?}", facts.unknowns);
}

#[test]
fn comparison_literal_reaches_a_controlled_exit() {
    let source = r#"fn quoted(the: char) -> usize {
        if the == '"' { 1 } else { 0 }
    }"#;
    let facts = analyze(source, "quoted");
    let literal = facts
        .literal_operands
        .iter()
        .find(|literal| {
            literal.kind == LiteralKind::Char
                && literal.literal == "\""
                && literal.compared_with.as_deref() == Some("the")
        })
        .expect("the compared character reaches the outgoing value");

    assert_eq!(literal.text, "'\"'");
    assert_eq!(literal.condition.as_deref(), Some("the == '\"'"));
    assert_eq!(
        source.get(literal.span.start as usize..literal.span.end as usize),
        Some("'\"'")
    );
    assert!(facts.exits.iter().any(|exit| exit.span == literal.exit));

    let reversed_source = r#"fn quoted(the: char) -> usize {
        if '"' == the { 1 } else { 0 }
    }"#;
    let reversed = analyze(reversed_source, "quoted");
    assert!(reversed.literal_operands.iter().any(|literal| {
        literal.kind == LiteralKind::Char
            && literal.literal == "\""
            && literal.compared_with.as_deref() == Some("the")
            && literal.condition.as_deref() == Some("'\"' == the")
    }));
}

#[test]
fn match_pattern_literal_reaches_its_arm_value() {
    let source = r#"fn quoted(the: char) -> usize {
        match the { '"' => 1, 'x' => 1, _ => 0 }
    }"#;
    let facts = analyze(source, "quoted");
    let literal = facts
        .literal_operands
        .iter()
        .find(|literal| {
            literal.kind == LiteralKind::Char
                && literal.literal == "\""
                && literal.compared_with.as_deref() == Some("the")
                && literal.condition.as_deref() == Some("the matches '\"'")
        })
        .expect("the match character reaches the matching arm value");
    let alternative = facts
        .literal_operands
        .iter()
        .find(|literal| {
            literal.kind == LiteralKind::Char
                && literal.literal == "x"
                && literal.compared_with.as_deref() == Some("the")
        })
        .expect("the alternative pattern is reported with its scrutinee");

    assert_eq!(literal.text, "'\"'");
    assert_eq!(alternative.text, "'x'");
    assert!(facts.exits.iter().any(|exit| exit.span == literal.exit));
}

#[test]
fn method_argument_literal_reaches_the_returned_receiver() {
    let source = r#"fn quoted() -> String {
        let mut output = String::new();
        output.push('"');
        output.push('x');
        output
    }"#;
    let facts = analyze(source, "quoted");
    let literal = facts
        .literal_operands
        .iter()
        .find(|literal| {
            literal.kind == LiteralKind::Char
                && literal.literal == "\""
                && literal.compared_with.as_deref() == Some("output")
        })
        .expect("the method argument reaches the returned receiver");
    let second = facts
        .literal_operands
        .iter()
        .find(|literal| {
            literal.kind == LiteralKind::Char
                && literal.literal == "x"
                && literal.compared_with.as_deref() == Some("output")
        })
        .expect("a second method argument reaches the same receiver");

    assert_eq!(literal.text, "'\"'");
    assert_eq!(second.text, "'x'");
    assert!(facts.exits.iter().any(|exit| exit.span == literal.exit));
}

#[test]
fn outgoing_values_report_each_supported_literal_kind() {
    let facts = analyze("fn literals() { ('x', b'y', \"z\", 3) }", "literals");

    assert!(facts.literal_operands.iter().any(|literal| {
        literal.kind == LiteralKind::Char && literal.literal == "x" && literal.text == "'x'"
    }));
    assert!(facts.literal_operands.iter().any(|literal| {
        literal.kind == LiteralKind::Byte && literal.literal == "121" && literal.text == "b'y'"
    }));
    assert!(facts.literal_operands.iter().any(|literal| {
        literal.kind == LiteralKind::Str && literal.literal == "z" && literal.text == "\"z\""
    }));
    assert!(facts.literal_operands.iter().any(|literal| {
        literal.kind == LiteralKind::Int && literal.literal == "3" && literal.text == "3"
    }));
}
