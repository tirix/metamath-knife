use diag::Diagnostic;
use parser::Comparer;
use parser::NO_STATEMENT;
use parser::SegmentId;
use parser::SegmentOrder;
use parser::StatementAddress;
use parser::StatementRef;
use parser::StatementType;
use parser::TokenPtr;
use scopeck::ExprFragment;
use scopeck::Frame;
use scopeck::ScopeReader;
use scopeck::ScopeResult;
use segment_set::SegmentSet;
use std::cmp::Ordering;
use std::ops::BitOrAssign;
use std::ops::Range;
use std::slice;
use std::sync::Arc;
use std::u32;
use std::usize;
use util::fast_clear;
use util::fast_extend;
use util::fast_truncate;
use util::HashMap;
use util::new_map;

#[derive(Clone)]
struct Bitset {
    head: usize,
    tail: Option<Box<Vec<usize>>>,
}

fn bits_per_word() -> usize {
    usize::max_value().count_ones() as usize
}

impl Bitset {
    fn new() -> Bitset {
        Bitset {
            head: 0,
            tail: None,
        }
    }

    fn tail(&self) -> &[usize] {
        match self.tail {
            None => Default::default(),
            Some(ref bx) => &bx,
        }
    }

    fn tail_mut(&mut self) -> &mut Vec<usize> {
        if self.tail.is_none() {
            self.tail = Some(Box::new(Vec::new()));
        }
        self.tail.as_mut().unwrap()
    }

    fn set_bit(&mut self, bit: usize) {
        if bit < bits_per_word() {
            self.head |= 1 << bit;
        } else {
            let word = bit / bits_per_word() - 1;
            let tail = self.tail_mut();
            if word >= tail.len() {
                tail.resize(word + 1, 0);
            }
            tail[word] |= 1 << (bit & (bits_per_word() - 1));
        }
    }

    fn has_bit(&self, bit: usize) -> bool {
        if bit < bits_per_word() {
            (self.head & (1 << bit)) != 0
        } else {
            let word = bit / bits_per_word() - 1;
            let tail = self.tail();
            if word >= tail.len() {
                false
            } else {
                (tail[word] & (1 << (bit & (bits_per_word() - 1)))) != 0
            }
        }
    }
}

impl<'a> BitOrAssign<&'a Bitset> for Bitset {
    fn bitor_assign(&mut self, rhs: &'a Bitset) {
        self.head |= rhs.head;
        if !rhs.tail().is_empty() {
            let rtail = rhs.tail();
            let stail = self.tail_mut();
            if rtail.len() > stail.len() {
                stail.resize(rtail.len(), 0);
            }
            for i in 0..rtail.len() {
                stail[i] |= rtail[i];
            }
        }
    }
}

impl<'a> IntoIterator for &'a Bitset {
    type Item = usize;
    type IntoIter = BitsetIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        BitsetIter {
            bits: self.head,
            offset: 0,
            buffer: self.tail().iter(),
        }
    }
}

struct BitsetIter<'a> {
    bits: usize,
    offset: usize,
    buffer: slice::Iter<'a, usize>,
}

impl<'a> Iterator for BitsetIter<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.bits == 0 {
            self.offset += bits_per_word();
            match self.buffer.next() {
                Some(bits) => self.bits = *bits,
                None => return None,
            }
        }
        let tz = self.bits.trailing_zeros() as usize;
        self.bits &= self.bits - 1;
        Some(tz + self.offset)
    }
}

enum PreparedStep<'a> {
    Hyp(Bitset, TokenPtr<'a>, Range<usize>),
    Assert(&'a Frame),
}

struct StackSlot<'a> {
    vars: Bitset,
    code: TokenPtr<'a>,
    expr: Range<usize>,
}

struct VerifyState<'a> {
    order: &'a SegmentOrder,
    scoper: ScopeReader<'a>,
    cur_frame: &'a Frame,
    prepared: Vec<PreparedStep<'a>>,
    prep_buffer: Vec<u8>,
    stack: Vec<StackSlot<'a>>,
    stack_buffer: Vec<u8>,
    temp_buffer: Vec<u8>,
    subst_vars: Vec<Bitset>,
    subst_exprs: Vec<Range<usize>>,
    var2bit: HashMap<TokenPtr<'a>, usize>,
    dv_map: Vec<Bitset>,
}

fn map_var<'a>(state: &mut VerifyState<'a>, token: TokenPtr<'a>) -> usize {
    let nbit = state.var2bit.len();
    let dvmapr = &mut state.dv_map;
    *state.var2bit.entry(token).or_insert_with(|| {
        dvmapr.push(Bitset::new());
        nbit
    })
}

fn prepare_step(state: &mut VerifyState, label: TokenPtr) -> Option<Diagnostic> {
    let frame = match state.scoper.get(label) {
        Some(fp) => fp,
        None => return Some(Diagnostic::StepMissing(label.to_owned())),
    };

    let valid = frame.valid;
    let pos = state.cur_frame.valid.start;
    if state.order.cmp(&pos, &valid.start) != Ordering::Greater {
        return Some(Diagnostic::StepUsedBeforeDefinition(label.to_owned()));
    }

    if valid.end != NO_STATEMENT {
        if pos.segment_id != valid.start.segment_id || pos.index >= valid.end {
            return Some(Diagnostic::StepUsedAfterScope(label.to_owned()));
        }
    }

    if frame.stype == StatementType::Axiom || frame.stype == StatementType::Provable {
        state.prepared.push(PreparedStep::Assert(frame));
    } else {
        let mut vars = Bitset::new();

        for var in &frame.mandatory_vars {
            vars.set_bit(map_var(state, var));
        }

        let tos = state.prep_buffer.len();
        fast_extend(&mut state.prep_buffer, &frame.stub_expr);
        let ntos = state.prep_buffer.len();
        state.prepared
            .push(PreparedStep::Hyp(vars, &frame.target.typecode, tos..ntos));
    }

    return None;
}

fn do_substitute(target: &mut Vec<u8>,
                 expr: &[ExprFragment],
                 vars: &[Range<usize>],
                 var_buffer: &[u8]) {
    for part in expr {
        match *part {
            ExprFragment::Var(ix) => {
                fast_extend(target, &var_buffer[vars[ix].clone()]);
            }
            ExprFragment::Constant(ref string) => {
                fast_extend(target, &string);
            }
        }
    }
}

fn do_substitute_raw(target: &mut Vec<u8>, expr: &[ExprFragment], vars: &[Vec<u8>]) {
    for part in expr {
        match *part {
            ExprFragment::Var(ix) => {
                fast_extend(target, &vars[ix]);
                target.push(b' ');
            }
            ExprFragment::Constant(ref string) => {
                fast_extend(target, &string);
            }
        }
    }
}

fn do_substitute_vars(expr: &[ExprFragment], vars: &[Bitset]) -> Bitset {
    let mut out = Bitset::new();
    for part in expr {
        match *part {
            ExprFragment::Var(ix) => out |= &vars[ix],
            ExprFragment::Constant(_) => {}
        }
    }
    out
}

fn execute_step(state: &mut VerifyState, index: usize) -> Option<Diagnostic> {
    if index >= state.prepared.len() {
        return Some(Diagnostic::StepOutOfRange);
    }

    let fref = match state.prepared[index] {
        PreparedStep::Hyp(ref vars, code, ref expr) => {
            let tos = state.stack_buffer.len();
            fast_extend(&mut state.stack_buffer, &state.prep_buffer[expr.clone()]);
            let ntos = state.stack_buffer.len();
            state.stack.push(StackSlot {
                vars: vars.clone(),
                code: code,
                expr: tos..ntos,
            });
            return None;
        }
        PreparedStep::Assert(fref) => fref,
    };

    if state.stack.len() < fref.hypotheses.len() {
        return Some(Diagnostic::ProofUnderflow);
    }
    let sbase = state.stack.len() - fref.hypotheses.len();

    state.subst_exprs.clear();
    state.subst_vars.clear();
    state.subst_exprs.resize(fref.mandatory_vars.len(), 0..0);
    state.subst_vars.resize(fref.mandatory_vars.len(), Bitset::new());

    // check $f, build substitution
    for (ix, hyp) in fref.hypotheses.iter().enumerate() {
        if hyp.is_float {
            let slot = &state.stack[sbase + ix];
            if slot.code != &hyp.expr.typecode[..] {
                return Some(Diagnostic::StepFloatWrongType);
            }
            state.subst_vars[hyp.variable_index] = slot.vars.clone();
            state.subst_exprs[hyp.variable_index] = slot.expr.clone();
        }
    }

    // check $e
    for (ix, hyp) in fref.hypotheses.iter().enumerate() {
        if !hyp.is_float {
            let slot = &state.stack[sbase + ix];
            if slot.code != &hyp.expr.typecode[..] {
                return Some(Diagnostic::StepEssenWrongType);
            }
            fast_clear(&mut state.temp_buffer);
            do_substitute(&mut state.temp_buffer,
                          &hyp.expr.tail,
                          &state.subst_exprs,
                          &state.stack_buffer);
            if state.stack_buffer[slot.expr.clone()] != state.temp_buffer[..] {
                return Some(Diagnostic::StepEssenWrong);
            }
        }
    }

    fast_clear(&mut state.temp_buffer);
    do_substitute(&mut state.temp_buffer,
                  &fref.target.tail,
                  &state.subst_exprs,
                  &state.stack_buffer);

    state.stack.truncate(sbase);
    fast_truncate(&mut state.stack_buffer,
                  if sbase == 0 {
                      0
                  } else {
                      state.stack[sbase - 1].expr.end
                  });
    let tos = state.stack_buffer.len();
    fast_extend(&mut state.stack_buffer, &state.temp_buffer);
    let ntos = state.stack_buffer.len();

    state.stack.push(StackSlot {
        code: &fref.target.typecode,
        vars: do_substitute_vars(&fref.target.tail, &state.subst_vars),
        expr: tos..ntos,
    });

    // check $d
    for &(ix1, ix2) in &fref.mandatory_dv {
        for var1 in &state.subst_vars[ix1] {
            for var2 in &state.subst_vars[ix2] {
                if !state.dv_map[var1].has_bit(var2) {
                    return Some(Diagnostic::ProofDvViolation);
                }
            }
        }
    }

    return None;
}

fn finalize_step(state: &mut VerifyState) -> Option<Diagnostic> {
    if state.stack.len() == 0 {
        return Some(Diagnostic::ProofNoSteps);
    }
    if state.stack.len() > 1 {
        return Some(Diagnostic::ProofExcessEnd);
    }
    let tos = state.stack.last().unwrap();

    if tos.code != &state.cur_frame.target.typecode[..] {
        return Some(Diagnostic::ProofWrongTypeEnd);
    }

    fast_clear(&mut state.temp_buffer);
    do_substitute_raw(&mut state.temp_buffer,
                      &state.cur_frame.target.tail,
                      &state.cur_frame.mandatory_vars);

    if state.stack_buffer[tos.expr.clone()] != state.temp_buffer[..] {
        return Some(Diagnostic::ProofWrongExprEnd);
    }

    None
}

fn save_step(state: &mut VerifyState) {
    let top = state.stack.last().expect("can_save should prevent getting here");
    let tos = state.prep_buffer.len();
    fast_extend(&mut state.prep_buffer,
                &state.stack_buffer[top.expr.clone()]);
    let ntos = state.prep_buffer.len();
    state.prepared.push(PreparedStep::Hyp(top.vars.clone(), top.code, tos..ntos));
}

// proofs are not self-synchronizing, so it's not likely to get >1 usable error
fn verify_proof(sset: &SegmentSet, scopes: ScopeReader, stmt: StatementRef) -> Option<Diagnostic> {
    // only intend to check $p statements
    if stmt.statement.stype != StatementType::Provable {
        return None;
    }

    // no valid frame -> no use checking
    // may wish to record a secondary error?
    let cur_frame = match scopes.get(stmt.label()) {
        None => return None,
        Some(x) => x,
    };
    let mut state = VerifyState {
        scoper: scopes,
        order: &sset.order,
        cur_frame: cur_frame,
        stack: Vec::new(),
        stack_buffer: Vec::new(),
        prepared: Vec::new(),
        prep_buffer: Vec::new(),
        temp_buffer: Vec::new(),
        subst_vars: Vec::new(),
        subst_exprs: Vec::new(),
        var2bit: new_map(),
        dv_map: Vec::new(),
    };

    for &(ref var1, ref var2) in &cur_frame.optional_dv {
        let ix1 = map_var(&mut state, var1);
        let ix2 = map_var(&mut state, var2);
        state.dv_map[ix1].set_bit(ix2);
        state.dv_map[ix2].set_bit(ix1);
    }

    if stmt.proof_slice_at(0) == b"(" {
        let mut i = 1;

        for h in &cur_frame.hypotheses {
            if let Some(err) = prepare_step(&mut state, &h.label) {
                return Some(err);
            }
        }

        loop {
            if i >= stmt.proof_len() {
                return Some(Diagnostic::ProofUnterminatedRoster);
            }
            let chunk = stmt.proof_slice_at(i);
            i += 1;

            if chunk == b")" {
                break;
            }

            if let Some(err) = prepare_step(&mut state, chunk) {
                return Some(err);
            }
        }

        let mut k = 0usize;
        let mut can_save = false;
        while i < stmt.proof_len() {
            let chunk = stmt.proof_slice_at(i);
            for &ch in chunk {
                if ch >= b'A' && ch <= b'T' {
                    k = k * 20 + (ch - b'A') as usize;
                    if let Some(err) = execute_step(&mut state, k) {
                        return Some(err);
                    }
                    k = 0;
                    can_save = true;
                } else if ch >= b'U' && ch <= b'Y' {
                    k = k * 5 + 1 + (ch - b'U') as usize;
                    if k >= (u32::max_value() as usize / 20) - 1 {
                        return Some(Diagnostic::ProofMalformedVarint);
                    }
                    can_save = false;
                } else if ch == b'Z' {
                    if !can_save {
                        return Some(Diagnostic::ProofInvalidSave);
                    }
                    save_step(&mut state);
                    can_save = false;
                } else if ch == b'?' {
                    if k > 0 {
                        return Some(Diagnostic::ProofMalformedVarint);
                    }
                    return Some(Diagnostic::ProofIncomplete);
                }
            }
            i += 1;
        }

        if k > 0 {
            return Some(Diagnostic::ProofMalformedVarint);
        }
    } else {
        let mut count = 0;
        for i in 0..stmt.proof_len() {
            let chunk = stmt.proof_slice_at(i);
            if chunk == b"?" {
                return Some(Diagnostic::ProofIncomplete);
            } else {
                if let Some(err) = prepare_step(&mut state, chunk) {
                    return Some(err);
                }
                if let Some(err) = execute_step(&mut state, count) {
                    return Some(err);
                }
                count += 1;
            }
        }
    }

    if let Some(err) = finalize_step(&mut state) {
        return Some(err);
    }

    return None;
}

struct VerifySegment {
    diagnostics: HashMap<StatementAddress, Diagnostic>,
}

pub struct VerifyResult {
    segments: HashMap<SegmentId, Arc<VerifySegment>>,
}

impl VerifyResult {
    pub fn diagnostics(&self) -> Vec<(StatementAddress, Diagnostic)> {
        let mut out = Vec::new();
        for vsr in self.segments.values() {
            for (&sa, &ref diag) in &vsr.diagnostics {
                out.push((sa, diag.clone()));
            }
        }
        out
    }
}

fn verify_segment(sset: &SegmentSet, scopes: &ScopeResult, sid: SegmentId) -> VerifySegment {
    let reader = ScopeReader::new(scopes);
    let mut out = VerifySegment { diagnostics: new_map() };
    for stmt in sset.segment(sid).statement_iter() {
        if let Some(diag) = verify_proof(sset, reader, stmt) {
            out.diagnostics.insert(stmt.address(), diag);
        }
    }
    out
}

pub fn verify(segments: &SegmentSet, scope: &ScopeResult) -> VerifyResult {
    let mut out = VerifyResult { segments: new_map() };
    for sref in segments.segments() {
        out.segments.insert(sref.id, Arc::new(verify_segment(segments, scope, sref.id)));
    }
    out
}