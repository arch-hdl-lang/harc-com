#!/usr/bin/env python3
"""Independent expression interpreter and exhaustive finite solver contracts.

No compiler AST, evaluator or emitted solver is used to compute expectations.
The same generated source is checked independently in each supported backend.
"""
import argparse
import dataclasses
import itertools
import json
import os
from pathlib import Path
import random
import re
from arithmetic_solver import SmtRunner, check_observations, mask, observe, quotient, run, test

SEED = 0x45585052
WORD = 64


@dataclasses.dataclass(frozen=True)
class Ty:
    signed: bool = True
    width: int = 64
    boolean: bool = False


I, U, B = Ty(), Ty(False), Ty(True, 1, True)


@dataclasses.dataclass(frozen=True)
class Value:
    bits: int
    ty: Ty

    def number(self, word=WORD):
        n = self.bits & mask(word)
        return n - (1 << word) if self.ty.signed and n >> (word - 1) else n


@dataclasses.dataclass(frozen=True)
class Node:
    op: str
    args: tuple = ()
    data: object = None


def lit(n):
    return Node('lit', data=n)


def var(name, ty=I):
    return Node('var', data=(name, ty))


def cast(a, ty):
    return Node('cast', (a,), ty)


def shape(e):
    if e.op == 'var':
        return e.data[1]
    if e.op == 'lit':
        return I
    if e.op == 'bool':
        return B
    if e.op == 'cast':
        return e.data
    if e.op in ('==', '!=', '<', '<=', '>', '>=', '&&', '||', '!'):
        return B
    if e.op in ('neg', '~', '<<', '>>'):
        return shape(e.args[0])
    if e.op == '?:':
        a, b = map(shape, e.args[1:])
    else:
        a, b = map(shape, e.args)
    return Ty(False if e.op in ('+%', '-%', '*%') else a.signed and b.signed,
              max(a.width, b.width), a.boolean and b.boolean)


def normalize(bits, ty):
    """The `ty.width`-bit value of a bit pattern, in a 64-bit carrier.

    `as uint<W>` keeps the low W bits; `as sint<W>` also sign-extends from
    bit W - 1 (spec section 2, runtime casts; the constant folder agrees).
    """
    low = bits & mask(ty.width)
    if ty.signed and ty.width and low >> (ty.width - 1):
        low -= 1 << ty.width
    return low & mask(WORD)


def interpret(e, env, word=WORD, legacy_casts=False):
    """64-bit carrier semantics; `word=w` supplies typed-SMT BV semantics.

    `legacy_casts` models the deprecated `--codegen v1` backend, whose casts
    are carrier relabels that leave the 64-bit value unchanged. TB-IR (the
    language) gives a cast the target-width value.
    """
    ty = shape(e)
    if e.op == 'lit':
        return Value(e.data & mask(word), ty)
    if e.op == 'bool':
        return Value(int(e.data), B)
    if e.op == 'var':
        return Value(env[e.data[0]] & mask(word), ty)
    if e.op == 'cast':
        inner = interpret(e.args[0], env, word, legacy_casts).bits
        if legacy_casts or word != WORD:
            return Value(inner, ty)
        return Value(normalize(inner, ty), ty)
    a = interpret(e.args[0], env, word, legacy_casts)
    # Laziness is part of the interpreter, including branches with no value.
    if e.op == '&&' and a.bits == 0:
        return Value(0, B)
    if e.op == '||' and a.bits != 0:
        return Value(1, B)
    if e.op == '?:':
        chosen = e.args[1] if a.bits else e.args[2]
        return Value(interpret(chosen, env, word, legacy_casts).bits, ty)
    if e.op in ('neg', '~', '!'):
        value = {'neg': lambda: -a.number(word), '~': lambda: ~a.bits,
                 '!': lambda: int(a.bits == 0)}[e.op]()
    else:
        b = interpret(e.args[1], env, word, legacy_casts)
        signed = a.ty.signed and b.ty.signed
        x, y = (a.number(word), b.number(word)) if signed else (a.bits, b.bits)
        if e.op in ('/', '%'):
            if y == 0:
                raise ZeroDivisionError('evaluated zero divisor')
            if signed and x == -(1 << (word - 1)) and y == -1:
                raise OverflowError('signed division overflow')
        value = {
            '+': lambda: x + y, '-': lambda: x - y, '*': lambda: x * y,
            '+%': lambda: (a.bits + b.bits) & mask(ty.width),
            '-%': lambda: (a.bits - b.bits) & mask(ty.width),
            '*%': lambda: (a.bits * b.bits) & mask(ty.width),
            '/': lambda: quotient(x, y), '%': lambda: x - quotient(x, y) * y,
            '&': lambda: a.bits & b.bits, '|': lambda: a.bits | b.bits,
            '^': lambda: a.bits ^ b.bits,
            '==': lambda: int(a.bits == b.bits), '!=': lambda: int(a.bits != b.bits),
            '<': lambda: int(x < y), '<=': lambda: int(x <= y),
            '>': lambda: int(x > y), '>=': lambda: int(x >= y),
            '<<': lambda: a.bits << b.bits,
            '>>': lambda: a.number(word) >> b.bits,
            '&&': lambda: int(a.bits != 0 and b.bits != 0),
            '||': lambda: int(a.bits != 0 or b.bits != 0),
        }[e.op]()
    # Signed C++ arithmetic is exercised only where its result is defined.
    if word == 64 and ty.signed and e.op in ('+', '-', '*', 'neg'):
        if not -(1 << 63) <= value < (1 << 63):
            raise OverflowError('undefined signed host overflow in generated domain')
    return Value(value & mask(word), ty)


def render(e, names=None, extra=False):
    names = names or {}
    if e.op == 'var':
        text = names.get(e.data[0], e.data[0])
    elif e.op == 'lit':
        text = f'({e.data} as sint<64>)'
    elif e.op == 'bool':
        text = str(e.data).lower()
    elif e.op == 'cast':
        t = e.data
        text = f'({render(e.args[0], names, extra)} as {"sint" if t.signed else "uint"}<{t.width}>)'
    elif e.op == '?:':
        a, b, c = (render(x, names, extra) for x in e.args)
        text = f'({a} ? {b} : {c})'
    elif e.op in ('neg', '~', '!'):
        text = f'({"-" if e.op == "neg" else e.op} {render(e.args[0], names, extra)})'
    else:
        a, b = (render(x, names, extra) for x in e.args)
        text = f'({a} {e.op} {b})'
    return f'(({text}))' if extra else text


def trees(seed, random_count=80):
    rng = random.Random(seed)
    leaves = [var('x'), var('y'), var('u', U), lit(0), lit(1), lit(3), lit(7)]
    def number(depth):
        if depth == 0:
            return rng.choice(leaves)
        op = rng.choice(['+', '-', '*', '/', '%', '&', '|', '^', 'neg', '~',
                         '<<', '>>', 'wrap', 'cast', '?:'])
        if op == 'cast':
            return cast(number(depth-1), Ty(rng.choice([False, True]), rng.choice([8, 32, 64])))
        if op == 'neg':
            return Node(op, (cast(Node('%', (number(depth-1), lit(127))), I),))
        if op == '~':
            # File constants complement all 64 carrier bits. TBIR runtime
            # complements at the operand's declared width; explicit uint64
            # keeps this cross-context property in their shared domain.
            return Node(op, (cast(number(depth-1), U),))
        if op in ('/', '%'):
            return Node(op, (number(depth-1), lit(rng.choice([1, 2, 3]))))
        if op in ('<<', '>>'):
            a = number(depth-1)
            if op == '<<':
                a = cast(a, U)
            return Node(op, (a, lit(rng.randrange(4))))
        if op == 'wrap':
            t = Ty(False, rng.choice([8, 16, 32, 64]))
            return Node(rng.choice(['+%', '-%', '*%']),
                        (cast(number(depth-1), t), cast(number(depth-1), t)))
        if op == '?:':
            return Node(op, (boolean(depth-1), number(depth-1), number(depth-1)))
        a, b = number(depth-1), number(depth-1)
        if op in ('+', '-', '*'):
            a, b = Node('%', (a, lit(127))), Node('%', (b, lit(127)))
        return Node(op, (a, b))
    def boolean(depth):
        if depth and rng.randrange(3) == 0:
            return Node(rng.choice(['&&', '||']), (boolean(depth-1), boolean(depth-1)))
        return Node(rng.choice(['==', '!=', '<', '<=', '>', '>=']),
                    (number(max(0, depth-1)), number(max(0, depth-1))))
    numeric_count = random_count * 3 // 5
    result = [number(4) for _ in range(numeric_count)] + [boolean(4) for _ in range(random_count - numeric_count)]
    bad = Node('/', (lit(1), lit(0)))
    bad_bool = Node('==', (bad, lit(0)))
    result += [Node('&&', (Node('bool', data=False), bad_bool)),
               Node('||', (Node('bool', data=True), bad_bool)),
               Node('?:', (Node('bool', data=True), var('x'), bad)),
               Node('?:', (Node('bool', data=False), bad, var('y')))]
    # Explicit nesting, signed relabel and wrap/conditional interactions ensure
    # these boundaries remain covered independently of the seeded trees.
    for op in ['+%', '-%', '*%']:
        wrapped = Node(op, (cast(var('x'), Ty(False, 8)), cast(var('y'), Ty(False, 8))))
        selected = Node('?:', (Node('<', (var('x'), var('y'))), wrapped, var('u', U)))
        result += [Node('>>', (cast(selected, I), lit(1))), Node('<', (selected, lit(0)))]
    return result


def generate_expressions(root, seed, expressions=None):
    expressions = trees(seed) if expressions is None else expressions
    environments = [dict(x=-7, y=3, u=5), dict(x=0, y=-2, u=0),
                    dict(x=7, y=1, u=15), dict(x=-1, y=0, u=1)]
    constants, body, expected, expected_v1, cases = [], '', {}, {}, []
    for ei, e in enumerate(expressions):
        for vi, env in enumerate(environments):
            want = interpret(e, env)
            # The deprecated v1 backend keeps relabel casts; it is checked
            # against that model, the language (TB-IR) against `want`.
            want_v1 = interpret(e, env, legacy_casts=True)
            for variant in range(2):
                key = f'e{ei:03d}v{vi}p{variant}'
                const_names, local_names = {}, {}
                for name, value in env.items():
                    t = U if name == 'u' else I
                    typ = f'{"sint" if t.signed else "uint"}<64>'
                    # Prefix changes plus redundant parentheses exercise source
                    # transformations without changing oracle interpretation.
                    cn, ln = f'K{key}{name}', f'r{key}{name}'
                    const_names[name], local_names[name] = cn, ln
                    constants.append(f'const {cn} : {typ} = {value}\n')
                    body += f'        let {ln} : {typ} = {value}\n'
                constant, runtime = render(e, const_names, bool(variant)), render(e, local_names, bool(variant))
                constants.append(f'const C{key} = {constant}\n')
                body += f'        let R{key} = {runtime}\n'
                width = 1 if want.ty.boolean else 64
                for context, value in [('const', f'C{key}'), ('runtime', f'R{key}')]:
                    identity = key + context
                    expr = value if width == 1 else f'({value} as uint<64>)'
                    expected[identity] = [want.bits, width]
                    expected_v1[identity] = [want_v1.bits, width]
                    body += observe(identity, expr, width)
                cases.append({'id': key, 'tree': dataclasses.asdict(e), 'environment': env,
                              'source': runtime, 'expected': want.bits, 'width': width})
    assert expressions and len(expected) == len(expressions) * len(environments) * 4
    (root/'expressions.harc').write_text(''.join(constants) + test('Expressions', body))
    (root/'expression_expected.json').write_text(json.dumps(expected, indent=2))
    (root/'expression_expected_v1.json').write_text(json.dumps(expected_v1, indent=2))
    (root/'expression_cases.json').write_text(json.dumps(cases, indent=2))


@dataclasses.dataclass(frozen=True)
class Problem:
    id: str
    width: int
    signed_x: bool
    signed_y: bool
    predicate: Node
    guards: tuple = ()

    def accepts(self, x, y, word=None):
        env = dict(x=x, y=y)
        for g in self.guards:
            if not interpret(g, env, word or self.width).bits:
                return False
        return bool(interpret(self.predicate, env, word or self.width).bits)

    def domain(self):
        def values(signed):
            return range(-(1 << (self.width-1)), 1 << (self.width-1)) if signed else range(1 << self.width)
        return itertools.product(values(self.signed_x), values(self.signed_y))

    def source(self):
        tx = f'{"sint" if self.signed_x else "uint"}<{self.width}>'
        ty = f'{"sint" if self.signed_y else "uint"}<{self.width}>'
        # Literal widths are inferred by the typed solver in constraints;
        # casts are intentionally not part of its supported surface.
        def constraint(e):
            if e.op == 'lit':
                return f'C{self.id}min' if e.data < -1 else (f'C{self.id}negone' if e.data == -1 else str(e.data))
            if e.op == 'var':
                return e.data[0]
            if e.op == '!':
                return f'!({constraint(e.args[0])})'
            a, b = map(constraint, e.args)
            return f'({a} {e.op} {b})'
        constants = ''
        if self.signed_x and self.guards:
            constants = (f'const C{self.id}min : {tx} = {-(1 << (self.width-1))}\n'
                         f'const C{self.id}negone : {ty} = -1\n')
        clauses = ''.join(f'    keep {constraint(g)}\n' for g in self.guards + (self.predicate,))
        return constants + f'transaction T{self.id}\n    x : {tx}\n    y : {ty}\n{clauses}end transaction T{self.id}\n'


def problems():
    result = []
    for width in [2, 3, 4]:
        for signed in [False, True]:
            t = Ty(signed, width)
            x, y = var('x', t), var('y', t)
            zero = lit(0)
            eq, lt = Node('==', (x, y)), Node('<', (x, y))
            families = [('equal', eq, ()), ('ordered', lt, ()),
                        ('partition', Node('||', (lt, eq)), ()),
                        ('subset', Node('==', (Node('&', (x, y)), x)), ()),
                        ('disjoint', Node('==', (Node('&', (x, y)), zero)), ())]
            guards = [Node('!=', (y, zero))]
            if signed:
                overflow = Node('&&', (Node('==', (x, lit(-(1 << (width-1))))), Node('==', (y, lit(-1)))))
                guards.append(Node('!', (overflow,)))
            families += [('quotient_zero', Node('==', (Node('/', (x, y)), zero)), tuple(guards)),
                         ('divisible', Node('==', (Node('%', (x, y)), zero)), tuple(guards))]
            for name, predicate, guard in families:
                if width == 4 and name not in ('ordered', 'subset'):
                    continue
                result.append(Problem(f'w{width}{"s" if signed else "u"}{name}', width, signed, signed, predicate, guard))
            # Unsigned count even for signed LHS; shifts preserve all count bits.
            uy = var('y', Ty(False, width))
            shift = Node('==', (Node('>>', (x, uy)), zero))
            result.append(Problem(f'w{width}{"s" if signed else "u"}shift', width, signed, False, shift))
    return result


def mathematical_accepts(p, x, y):
    """A second, non-AST mathematical reference for each predicate family."""
    if not (-(1 << (p.width-1)) if p.signed_x else 0) <= x <= ((1 << (p.width-1))-1 if p.signed_x else mask(p.width)):
        return False
    if not (-(1 << (p.width-1)) if p.signed_y else 0) <= y <= ((1 << (p.width-1))-1 if p.signed_y else mask(p.width)):
        return False
    family = p.id[3:]
    if family == 'equal':
        return x == y
    if family == 'ordered':
        return x < y
    if family == 'partition':
        return x <= y
    if family == 'subset':
        return (x & y & mask(p.width)) == (x & mask(p.width))
    if family == 'disjoint':
        return (x & y & mask(p.width)) == 0
    if family == 'shift':
        return x >= 0 and x < (1 << y)
    if not y or (p.signed_x and x == -(1 << (p.width-1)) and y == -1):
        return False
    if family == 'quotient_zero':
        return abs(x) < abs(y)
    if family == 'divisible':
        return x % y == 0
    raise AssertionError(f'unknown model contract {family}')


def generate_models(root):
    typed = root/'models'
    typed.mkdir(exist_ok=True)
    all_problems = problems()
    native, body, manifest = [], '', []
    for p in all_problems:
        source = p.source()
        native.append(source)
        (typed/(p.id+'.harc')).write_text(source)
        expected = []
        for x, y in p.domain():
            # The selected formulas have equal declared-width and host-carrier
            # truth values. This assertion is a domain contract, not filtering.
            accepted = mathematical_accepts(p, x, y)
            assert accepted == p.accepts(x, y) == p.accepts(x, y, 64), (p.id, x, y)
            if accepted:
                expected.append([x, y])
        assert expected and len(expected) < (1 << (2*p.width)), p.id
        manifest.append({'id': p.id, 'width': p.width, 'signed_x': p.signed_x,
                         'signed_y': p.signed_y, 'expected': expected})
        n = 1 << p.width
        biasx = 1 << (p.width-1) if p.signed_x else 0
        biasy = 1 << (p.width-1) if p.signed_y else 0
        # HARC ranges include the upper bound: n+1 gives n+2 probes.
        body += f'''        let p{p.id} : T{p.id}
        for ax in 0..{n+1}
            for by in 0..{n+1}
                let px : sint<64> = (ax as sint<64>) - {biasx+1}
                let py : sint<64> = (by as sint<64>) - {biasy+1}
                log(info, "MODEL_BEGIN {p.id} ${{ax}} ${{by}}")
                blocking randomize(p{p.id}) with
                    p{p.id}.x == px
                    p{p.id}.y == py
                end randomize
                log(info, "MODEL_END {p.id} ${{ax}} ${{by}} ${{(p{p.id}.x as uint<64>):016x}} ${{(p{p.id}.y as uint<64>):016x}}")
            end for
        end for
'''
    assert len(all_problems) == 38
    assert sum(1 << (2*p.width) for p in all_problems) == 2816
    (root/'model_expected.json').write_text(json.dumps(manifest, indent=2))
    (root/'models.harc').write_text(''.join(native) + test('Models', body))


def typed_models(root):
    engine = SmtRunner()
    for p in problems():
        smt = (root/'models'/(p.id+'.smt2')).read_text()
        assert smt.count('(check-sat)') == 1
        prefix = smt.replace('(check-sat)', '')
        actual, expected, queries = set(), set(), []
        for x, y in p.domain():
            pins = f'(assert (= f_x (_ bv{x & mask(p.width)} {p.width})))\n(assert (= f_y (_ bv{y & mask(p.width)} {p.width})))\n'
            status = engine.check(prefix + pins + '(check-sat)\n').strip()
            if status not in ('sat', 'unsat'):
                raise AssertionError(f'{p.id} ({x},{y}) solver incomplete: {status}')
            if status == 'sat':
                actual.add((x, y))
            if mathematical_accepts(p, x, y):
                expected.add((x, y))
            queries.append([x, y, status])
        (root/'models'/(p.id+'.json')).write_text(json.dumps(queries))
        compare_models(p.id, actual, expected)
    # Mutate real compiler-emitted SMT. One defect admits invalid diagonal
    # assignments; the other drops valid models while retaining some models.
    witness = next(p for p in problems() if p.id == 'w2uordered')
    original = (root/'models'/(witness.id+'.smt2')).read_text()
    assert original.count('bvult') == 1
    prefix = original.replace('(check-sat)', '')
    reference = {(x, y) for x, y in witness.domain() if mathematical_accepts(witness, x, y)}
    mutations = [
        ('admits-invalid', prefix.replace('bvult', 'bvule'), 'extra'),
        ('drops-valid', prefix + '(assert (= f_x (_ bv0 2)))\n', 'missing'),
    ]
    evidence = []
    for name, mutant, direction in mutations:
        (root/'models'/(name+'.smt2')).write_text(mutant + '(check-sat)\n')
        actual = set()
        for x, y in witness.domain():
            pins = f'(assert (= f_x (_ bv{x} 2)))\n(assert (= f_y (_ bv{y} 2)))\n'
            answer = engine.check(mutant + pins + '(check-sat)\n').strip()
            assert answer in ('sat', 'unsat'), (name, answer)
            if answer == 'sat':
                actual.add((x, y))
        missing, extra = reference - actual, actual - reference
        assert actual, 'mutation must not be a vacuous all-UNSAT example'
        assert (extra and not missing) if direction == 'extra' else (missing and not extra)
        try:
            compare_models(name, actual, reference)
        except AssertionError:
            evidence.append({'mutation': name, 'missing': sorted(missing), 'extra': sorted(extra)})
        else:
            raise AssertionError(f'model contract missed mutation {name}')
    (root/'model_mutations.json').write_text(json.dumps(evidence, indent=2))
    print('typed models: 38 exact model sets, 2816 assignments; 2 SMT mutations rejected', flush=True)


def compare_models(label, actual, expected):
    missing, extra = expected - actual, actual - expected
    if missing or extra:
        raise AssertionError(f'{label}: missing valid models {sorted(missing)[:8]}; accepted invalid models {sorted(extra)[:8]}')


def native_expressions(root, harc, backends=('v1', 'tbir')):
    for backend in backends:
        name = 'expression_expected_v1.json' if backend == 'v1' else 'expression_expected.json'
        expected = json.loads((root/name).read_text())
        code, output = run([harc, 'sim', root/'expressions.harc', '--sv', root/'dut.sv', '--top', 'Top',
                            '--codegen', backend, '--seed', '1', '--jobs', '2', '--outdir', root/f'expressions_{backend}'], root, 240, env={**os.environ, 'HARC_NO_LEARN': '1'})
        (root/f'expressions_{backend}.log').write_text(output)
        if code != 0 or 'ALL TESTS PASSED' not in output:
            raise AssertionError(f'expressions/{backend}: {output[-4000:]}; {root}')
        check_observations(output, expected)
        print(f'expressions/{backend}: {len(expected)} oracle comparisons', flush=True)


def native_models(root, harc):
    expected = json.loads((root/'model_expected.json').read_text())
    for backend in ['v1', 'tbir']:
        code, output = run([harc, 'sim', root/'models.harc', '--sv', root/'dut.sv', '--top', 'Top',
                            '--codegen', backend, '--seed', '1', '--jobs', '2', '--outdir', root/f'models_{backend}'], root, 300, env={**os.environ, 'HARC_NO_LEARN': '1'})
        (root/f'models_{backend}.log').write_text(output)
        if code <= 0 or 'UNSAT' not in output:
            raise AssertionError(f'models/{backend} must complete and report excluded assignments: {output[-4000:]}')
        entries = re.findall(r'MODEL_BEGIN (\w+) (\d+) (\d+)\n(.*?)MODEL_END \1 \2 \3 ([0-9a-fA-F]+) ([0-9a-fA-F]+)', output, re.S)
        seen, actual = set(), {p['id']: set() for p in expected}
        byid = {p['id']: p for p in expected}
        for identity, ix, iy, segment, vx, vy in entries:
            if identity not in byid:
                raise AssertionError(f'unexpected model problem {identity}')
            p = byid[identity]
            key = (identity, int(ix), int(iy))
            if key in seen or int(ix) >= (1 << p['width']) + 2 or int(iy) >= (1 << p['width']) + 2:
                raise AssertionError(f'duplicate/out-of-domain query {key}')
            seen.add(key)
            x = int(ix) - ((1 << (p['width']-1)) if p['signed_x'] else 0) - 1
            y = int(iy) - ((1 << (p['width']-1)) if p['signed_y'] else 0) - 1
            if re.search(r'\bunknown\b', segment, re.I):
                raise AssertionError(f'{key}: native solver did not decide the query')
            if 'UNSAT' not in segment:
                if int(vx, 16) != x & mask(64) or int(vy, 16) != y & mask(64):
                    raise AssertionError(f'{key}: solver ignored pins')
                actual[identity].add((x, y))
        assert len(seen) == 4120, f'{backend}: incomplete model traversal ({len(seen)})'
        failures = 4120 - sum(len(p['expected']) for p in expected)
        assert re.findall(r'(?m)^(\d+) TESTS FAILED$', output) == [str(failures)], \
            f'{backend}: native run did not finish with exactly the excluded assignments: {output[-2000:]}'
        for p in expected:
            compare_models(f'{backend}/{p["id"]}', actual[p['id']], set(map(tuple, p['expected'])))
        print(f'models/{backend}: 38 exact model sets, 4120 assignments including boundaries', flush=True)


def native_capture_ranks(root, harc):
    source = """transaction U32
    x : uint<32>
end transaction U32
transaction S32
    x : sint<32>
end transaction S32
transaction S8
    x : sint<8>
end transaction S8
""" + test('CaptureRanks', """        let flag : bool = true
        let lim64 : sint<64> = -1
        let lim8 : uint<8> = 255
        let lim32 : uint<32> = 0
        let a : U32
        let b : S32
        let c : S8
        log(info, "RANK_BEGIN A")
        blocking randomize(a) with
            a.x == 0
            a.x < lim64
        end randomize
        log(info, "RANK_END A")
        log(info, "RANK_BEGIN B")
        blocking randomize(b) with
            b.x == -1
            b.x < lim8
            !((b.x < 0) != flag)
            flag
        end randomize
        log(info, "RANK_END B ${(b.x as uint<64>):016x}")
        log(info, "RANK_BEGIN C")
        blocking randomize(c) with
            c.x == -1
            c.x < lim32
        end randomize
        log(info, "RANK_END C")
""")
    (root/'capture_ranks.harc').write_text(source)
    # This newly supported TBIR capture boundary has mixed-rank contracts
    # separate from the common-domain exhaustive model family above.
    code, output = run([harc, 'sim', root/'capture_ranks.harc', '--sv', root/'dut.sv', '--top', 'Top',
                        '--codegen', 'tbir', '--seed', '1', '--jobs', '2',
                        '--outdir', root/'capture_ranks'], root, 180,
                       env={**os.environ, 'HARC_NO_LEARN': '1'})
    (root/'capture_ranks.log').write_text(output)
    assert code > 0, output
    assert re.findall(r'(?m)^(\d+) TESTS FAILED$', output) == ['2'], output
    for identity, unsat in [('A', True), ('B', False), ('C', True)]:
        segments = re.findall(rf'RANK_BEGIN {identity}\n(.*?)RANK_END {identity}', output, re.S)
        assert len(segments) == 1 and ('UNSAT' in segments[0]) == unsat, output
        assert 'unknown' not in segments[0].lower(), output
    assert 'RANK_END B ffffffffffffffff' in output, output
    print('capture ranks/tbir: 3 signed/unsigned runtime binding contracts', flush=True)


def self_test():
    assert interpret(Node('/', (lit(-5), lit(3))), {}).number() == -1
    assert interpret(Node('%', (lit(-5), lit(3))), {}).number() == -2
    # A cast is the target-width value: `-1 as uint<8>` is 255, `0xFC as
    # sint<8>` is -4, `-1 as uint<64>` is all ones; only v1 relabels.
    assert interpret(cast(lit(-1), Ty(False, 8)), {}).bits == 0xFF, 'uint cast must mask'
    assert interpret(cast(lit(0xFC), Ty(True, 8)), {}).number() == -4, 'sint cast must sign-extend'
    assert interpret(cast(lit(0x1FC), Ty(True, 8)), {}).number() == -4, 'narrowing sint cast'
    assert interpret(cast(lit(-1), Ty(False, 64)), {}).bits == mask(64)
    assert interpret(cast(lit(-1), Ty(False, 8)), {}, legacy_casts=True).bits == mask(64), \
        'v1 cast relabel must not truncate'
    assert interpret(Node('+%', (cast(lit(255), Ty(False, 8)), cast(lit(1), Ty(False, 8)))), {}).bits == 0
    bad = Node('/', (lit(1), lit(0)))
    assert interpret(Node('?:', (Node('bool', data=True), lit(7), bad)), {}).bits == 7
    try:
        interpret(bad, {})
    except ZeroDivisionError:
        pass
    else:
        raise AssertionError('interpreter swallowed live error')
    for actual in [set(), {(0, 0), (1, 1)}, {(1, 1)}]:
        try:
            compare_models('injected defect', actual, {(0, 0)})
        except AssertionError:
            pass
        else:
            raise AssertionError('model comparator accepted missing/extra models')
    compare_models('identity', {(0, 0)}, {(0, 0)})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--phase', choices=['generate', 'typed', 'expressions', 'models'], required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--harc', type=Path)
    parser.add_argument('--seed', type=lambda n: int(n, 0), default=SEED)
    args = parser.parse_args()
    if not __debug__:
        parser.error('Python optimization disables property guards')
    self_test()
    root = args.out.resolve()
    if args.phase == 'generate':
        root.mkdir(parents=True, exist_ok=True)
        (root/'dut.sv').write_text('module Top(input logic clk); endmodule\n')
        generate_expressions(root, args.seed)
        generate_models(root)
        (root/'seed.json').write_text(json.dumps({'seed': args.seed}))
    elif args.phase == 'typed':
        typed_models(root)
    elif args.phase in ('expressions', 'models'):
        if not args.harc:
            parser.error('--harc required')
        (native_expressions if args.phase == 'expressions' else native_models)(root, args.harc.resolve())
        if args.phase == 'models':
            native_capture_ranks(root, args.harc.resolve())


if __name__ == '__main__':
    main()
