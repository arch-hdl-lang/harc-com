use harc::ast::ExprKind;
use harc::codegen::cpp_tb;
use harc::constraints::{
    elaborate_constraints, FieldAttrArgSchema, FieldTypeClass, RelationBodySchema, Signedness,
};
use harc::parser::parse_source;

#[test]
fn elaborates_transaction_field_types_and_non_random_marker() {
    let parsed = parse_source(
        r#"enum Color { RED, GREEN, BLUE }

transaction Packet
    addr : uint<32>
    delta : sint<13>
    raw : bits<7>
    flag : bool
    bitflag : Bit
    color : Color
    !mode : uint<2> default 0
end transaction Packet"#,
    )
    .unwrap();

    let elaborated = elaborate_constraints(&parsed);
    assert!(elaborated.errors.is_empty(), "{:?}", elaborated.errors);
    let txn = elaborated.transaction("Packet").expect("Packet schema");
    assert_eq!(txn.fields.len(), 7);

    let addr = &txn.fields[0];
    assert_eq!(addr.ty.class, FieldTypeClass::UInt);
    assert_eq!(addr.ty.width, Some(32));
    assert_eq!(addr.ty.signedness, Signedness::Unsigned);

    let delta = &txn.fields[1];
    assert_eq!(delta.ty.class, FieldTypeClass::SInt);
    assert_eq!(delta.ty.width, Some(13));
    assert_eq!(delta.ty.signedness, Signedness::Signed);

    assert_eq!(txn.fields[2].ty.class, FieldTypeClass::Bits);
    assert_eq!(txn.fields[2].ty.width, Some(7));
    assert_eq!(txn.fields[3].ty.class, FieldTypeClass::Bool);
    assert_eq!(txn.fields[3].ty.width, Some(1));
    assert_eq!(txn.fields[4].ty.class, FieldTypeClass::Bit);
    assert_eq!(txn.fields[4].ty.width, Some(1));

    let color = &txn.fields[5];
    assert_eq!(color.ty.class, FieldTypeClass::Enum);
    assert_eq!(color.ty.width, Some(2));
    let domain = color.ty.enum_domain.as_ref().expect("enum domain");
    assert_eq!(domain.name, "Color");
    assert_eq!(domain.variants, ["RED", "GREEN", "BLUE"]);

    let mode = &txn.fields[6];
    assert!(mode.non_random);
    assert!(mode.has_default);
}

#[test]
fn elaborates_keeps_attributes_and_when_subtypes_without_codegen_changes() {
    let parsed = parse_source(
        r#"enum Op { READ, WRITE }

transaction AxiTxn
    op : Op
    len : uint<8> with [range(1, 16)] [dist {[1..4] :/ 70, [5..16] :/ 30}]
    keep len in [1..16]

    when op == WRITE
        data : bits<32>
        keep data != 0
    end when
end transaction AxiTxn"#,
    )
    .unwrap();

    let elaborated = elaborate_constraints(&parsed);
    assert!(elaborated.errors.is_empty(), "{:?}", elaborated.errors);
    let txn = elaborated.transaction("AxiTxn").expect("AxiTxn schema");

    assert_eq!(txn.keeps.len(), 1);
    assert!(matches!(
        &*txn.keeps[0].expr.kind,
        ExprKind::Membership { .. }
    ));

    let len = txn.fields.iter().find(|f| f.name == "len").expect("len");
    assert_eq!(len.attrs.len(), 2);
    assert_eq!(len.attrs[0].name, "range");
    assert_eq!(len.attrs[0].args.len(), 2);
    assert!(matches!(len.attrs[0].args[0], FieldAttrArgSchema::Expr(_)));
    assert_eq!(len.attrs[1].name, "dist");
    assert!(matches!(len.attrs[1].args[0], FieldAttrArgSchema::Dist(_)));

    assert_eq!(txn.when_subtypes.len(), 1);
    let write = &txn.when_subtypes[0];
    assert_eq!(write.fields.len(), 1);
    assert_eq!(write.fields[0].name, "data");
    assert_eq!(write.keeps.len(), 1);
}

#[test]
fn elaborates_relation_metadata() {
    let parsed = parse_source(
        r#"transaction T
    addr : uint<32>
end transaction T

relation Aligned(x: T) = x.addr % 4 == 0

relation Bounded(x: T)
    x.addr >= 16
    x.addr <= 1024
end relation Bounded"#,
    )
    .unwrap();

    let elaborated = elaborate_constraints(&parsed);
    assert!(elaborated.errors.is_empty(), "{:?}", elaborated.errors);

    let aligned = elaborated.relation("Aligned").expect("Aligned relation");
    assert_eq!(aligned.params.len(), 1);
    assert_eq!(aligned.params[0].name, "x");
    assert!(matches!(aligned.body, RelationBodySchema::Alias(_)));

    let bounded = elaborated.relation("Bounded").expect("Bounded relation");
    assert_eq!(bounded.params.len(), 1);
    match &bounded.body {
        RelationBodySchema::Block(clauses) => assert_eq!(clauses.len(), 2),
        RelationBodySchema::Alias(_) => panic!("Bounded should be block-form"),
    }
}

#[test]
fn foundation_scaffold_preserves_existing_codegen_solver_path() {
    let parsed = parse_source(
        r#"transaction T
    addr : uint<32>
    len : uint<8>
    keep len in [1..16]
end transaction T

relation Aligned(x: T) = x.addr % 4 == 0

test SolverPathTest
    let dut : DummyDut
    run
        let a : T
        randomize(a)

        let b : T
        randomize(b) with
            b.addr > 100
        end randomize

        let c : T
        randomize(c) with Aligned(c) end randomize
    end run
end test SolverPathTest"#,
    )
    .unwrap();

    let _schemas = elaborate_constraints(&parsed);
    let cpp = cpp_tb::emit(&parsed).expect("existing codegen should still emit");

    assert!(
        cpp.contains("z3::context _ctx;") && cpp.contains("z3::solver _s(_ctx);"),
        "keep-backed and with-backed randomize calls should still use the existing Z3 path:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::uge(_z_len") && cpp.contains("z3::ule(_z_len"),
        "transaction keeps should still reach codegen solver constraints:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::ugt(_z_addr, _ctx.bv_val((uint64_t)100"),
        "randomize-with body should still reach codegen solver constraints:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::urem(_z_addr"),
        "relation body should still inline into codegen solver constraints:\n{cpp}"
    );
}
