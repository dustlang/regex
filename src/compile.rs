// Copyright 2014-2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::HashMap;
use std::iter;
use std::result;
use std::sync::Arc;

use syntax::{
    Expr, Repeater, CharClass, ClassRange, ByteClass, ByteRange,
    is_word_byte,
};
use utf8_ranges::{Utf8Range, Utf8Sequence, Utf8Sequences};

use prog::{
    Program, Inst, InstPtr, EmptyLook,
    InstSave, InstSplit, InstEmptyLook, InstChar, InstRanges, InstBytes,
};

use Error;

type Result = result::Result<Patch, Error>;

#[derive(Debug)]
struct Patch {
    hole: Hole,
    entry: InstPtr,
}

/// A compiler translates a regular expression AST to a sequence of
/// instructions. The sequence of instructions represents an NFA.
pub struct Compiler {
    insts: Vec<MaybeInst>,
    compiled: Program,
    capture_name_idx: HashMap<String, usize>,
    num_exprs: usize,
    size_limit: usize,
    suffix_cache: SuffixCache,
    utf8_seqs: Option<Utf8Sequences>,
    byte_classes: ByteClassSet,
}

impl Compiler {
    /// Create a new regular expression compiler.
    ///
    /// Various options can be set before calling `compile` on an expression.
    pub fn new() -> Self {
        Compiler {
            insts: vec![],
            compiled: Program::new(),
            capture_name_idx: HashMap::new(),
            num_exprs: 0,
            size_limit: 10 * (1 << 20),
            suffix_cache: SuffixCache::new(1000),
            utf8_seqs: Some(Utf8Sequences::new('\x00', '\x00')),
            byte_classes: ByteClassSet::new(),
        }
    }

    /// The size of the resulting program is limited by size_limit. If
    /// the program approximately exceeds the given size (in bytes), then
    /// compilation will stop and return an error.
    pub fn size_limit(mut self, size_limit: usize) -> Self {
        self.size_limit = size_limit;
        self
    }

    /// If bytes is true, then the program is compiled as a byte based
    /// automaton, which incorporates UTF-8 decoding into the machine. If it's
    /// false, then the automaton is Unicode scalar value based, e.g., an
    /// engine utilizing such an automaton is resposible for UTF-8 decoding.
    ///
    /// The specific invariant is that when returning a byte based machine,
    /// the neither the `Char` nor `Ranges` instructions are produced.
    /// Conversely, when producing a Unicode scalar value machine, the `Bytes`
    /// instruction is never produced.
    ///
    /// Note that `dfa(true)` implies `bytes(true)`.
    pub fn bytes(mut self, yes: bool) -> Self {
        self.compiled.is_bytes = yes;
        self
    }

    /// When disabled, the program compiled may match arbitrary bytes.
    ///
    /// When enabled (the default), all compiled programs exclusively match
    /// valid UTF-8 bytes.
    pub fn only_utf8(mut self, yes: bool) -> Self {
        self.compiled.only_utf8 = yes;
        self
    }

    /// When set, the machine returned is suitable for use in the DFA matching
    /// engine.
    ///
    /// In particular, this ensures that if the regex is not anchored in the
    /// beginning, then a preceding `.*?` is included in the program. (The NFA
    /// based engines handle the preceding `.*?` explicitly, which is difficult
    /// or impossible in the DFA engine.)
    pub fn dfa(mut self, yes: bool) -> Self {
        self.compiled.is_dfa = yes;
        self
    }

    /// When set, the machine returned is suitable for matching text in
    /// reverse. In particular, all concatenations are flipped.
    pub fn reverse(mut self, yes: bool) -> Self {
        self.compiled.is_reverse = yes;
        self
    }

    /// Compile a regular expression given its AST.
    ///
    /// The compiler is guaranteed to succeed unless the program exceeds the
    /// specified size limit. If the size limit is exceeded, then compilation
    /// stops and returns an error.
    pub fn compile(
        mut self,
        exprs: &[Expr],
    ) -> result::Result<Program, Error> {
        debug_assert!(exprs.len() >= 1);
        self.num_exprs = exprs.len();
        if exprs.len() == 1 {
            self.compile_one(&exprs[0])
        } else {
            self.compile_many(exprs)
        }
    }

    fn compile_one(mut self, expr: &Expr) -> result::Result<Program, Error> {
        // If we're compiling a forward DFA and we aren't anchored, then
        // add a `.*?` before the first capture group.
        // Other matching engines handle this by baking the logic into the
        // matching engine itself.
        let mut dotstar_patch = Patch { hole: Hole::None, entry: 0 };
        self.compiled.is_anchored_start = expr.is_anchored_start();
        self.compiled.is_anchored_end = expr.is_anchored_end();
        if self.compiled.needs_dotstar() {
            dotstar_patch = try!(self.c_dotstar());
            self.compiled.start = dotstar_patch.entry;
        }
        self.compiled.captures = vec![None];
        let patch = try!(self.c_capture(0, expr));
        if self.compiled.needs_dotstar() {
            self.fill(dotstar_patch.hole, patch.entry);
        } else {
            self.compiled.start = patch.entry;
        }
        self.fill_to_next(patch.hole);
        self.compiled.matches = vec![self.insts.len()];
        self.push_compiled(Inst::Match(0));
        self.compile_finish()
    }

    fn compile_many(
        mut self,
        exprs: &[Expr],
    ) -> result::Result<Program, Error> {
        debug_assert!(exprs.len() > 1);

        self.compiled.is_anchored_start =
            exprs.iter().all(|e| e.is_anchored_start());
        self.compiled.is_anchored_end =
            exprs.iter().all(|e| e.is_anchored_end());
        let mut dotstar_patch = Patch { hole: Hole::None, entry: 0 };
        if self.compiled.needs_dotstar() {
            dotstar_patch = try!(self.c_dotstar());
            self.compiled.start = dotstar_patch.entry;
        } else {
            self.compiled.start = 0; // first instruction is always split
        }
        self.fill_to_next(dotstar_patch.hole);

        let mut prev_hole = Hole::None;
        for (i, expr) in exprs[0..exprs.len() - 1].iter().enumerate() {
            self.fill_to_next(prev_hole);
            let split = self.push_split_hole();
            let Patch { hole, entry } = try!(self.c_capture(0, expr));
            self.fill_to_next(hole);
            self.compiled.matches.push(self.insts.len());
            self.push_compiled(Inst::Match(i));
            prev_hole = self.fill_split(split, Some(entry), None);
        }
        let i = exprs.len() - 1;
        let Patch { hole, entry } = try!(self.c_capture(0, &exprs[i]));
        self.fill(prev_hole, entry);
        self.fill_to_next(hole);
        self.compiled.matches.push(self.insts.len());
        self.push_compiled(Inst::Match(i));
        self.compile_finish()
    }

    fn compile_finish(mut self) -> result::Result<Program, Error> {
        self.compiled.insts =
            self.insts.into_iter().map(|inst| inst.unwrap()).collect();
        self.compiled.byte_classes = self.byte_classes.byte_classes();
        self.compiled.capture_name_idx = Arc::new(self.capture_name_idx);
        Ok(self.compiled)
    }

    fn c(&mut self, expr: &Expr) -> Result {
        use prog;
        use syntax::Expr::*;

        try!(self.check_size());
        match *expr {
            Empty => Ok(Patch { hole: Hole::None, entry: self.insts.len() }),
            Literal { ref chars, casei } => self.c_literal(chars, casei),
            LiteralBytes { ref bytes, casei } => self.c_bytes(bytes, casei),
            AnyChar => self.c_class(&[ClassRange {
                start: '\x00',
                end: '\u{10ffff}',
            }]),
            AnyCharNoNL => {
                self.c_class(&[
                    ClassRange { start: '\x00', end: '\x09' },
                    ClassRange { start: '\x0b', end: '\u{10ffff}' },
                ])
            }
            AnyByte => {
                self.c_class_bytes(&[ByteRange { start: 0, end: 0xFF }])
            }
            AnyByteNoNL => {
                self.c_class_bytes(&[
                    ByteRange { start: 0, end: 0x9 },
                    ByteRange { start: 0xB, end: 0xFF },
                ])
            }
            Class(ref cls) => {
                self.c_class(cls)
            }
            ClassBytes(ref cls) => {
                self.c_class_bytes(cls)
            }
            StartLine if self.compiled.is_reverse => {
                self.byte_classes.set_range(b'\n', b'\n');
                self.c_empty_look(prog::EmptyLook::EndLine)
            }
            StartLine => {
                self.byte_classes.set_range(b'\n', b'\n');
                self.c_empty_look(prog::EmptyLook::StartLine)
            }
            EndLine if self.compiled.is_reverse => {
                self.byte_classes.set_range(b'\n', b'\n');
                self.c_empty_look(prog::EmptyLook::StartLine)
            }
            EndLine => {
                self.byte_classes.set_range(b'\n', b'\n');
                self.c_empty_look(prog::EmptyLook::EndLine)
            }
            StartText if self.compiled.is_reverse => {
                self.c_empty_look(prog::EmptyLook::EndText)
            }
            StartText => {
                self.c_empty_look(prog::EmptyLook::StartText)
            }
            EndText if self.compiled.is_reverse => {
                self.c_empty_look(prog::EmptyLook::StartText)
            }
            EndText => {
                self.c_empty_look(prog::EmptyLook::EndText)
            }
            WordBoundary => {
                self.compiled.has_unicode_word_boundary = true;
                self.byte_classes.set_word_boundary();
                self.c_empty_look(prog::EmptyLook::WordBoundary)
            }
            NotWordBoundary => {
                self.compiled.has_unicode_word_boundary = true;
                self.byte_classes.set_word_boundary();
                self.c_empty_look(prog::EmptyLook::NotWordBoundary)
            }
            WordBoundaryAscii => {
                self.byte_classes.set_word_boundary();
                self.c_empty_look(prog::EmptyLook::WordBoundaryAscii)
            }
            NotWordBoundaryAscii => {
                self.byte_classes.set_word_boundary();
                self.c_empty_look(prog::EmptyLook::NotWordBoundaryAscii)
            }
            Group { ref e, i: None, name: None } => self.c(e),
            Group { ref e, i, ref name } => {
                // it's impossible to have a named capture without an index
                let i = i.expect("capture index");
                if i >= self.compiled.captures.len() {
                    self.compiled.captures.push(name.clone());
                    if let Some(ref name) = *name {
                        self.capture_name_idx.insert(name.to_owned(), i);
                    }
                }
                self.c_capture(2 * i, e)
            }
            Concat(ref es) => {
                if self.compiled.is_reverse {
                    self.c_concat(es.iter().rev())
                } else {
                    self.c_concat(es)
                }
            }
            Alternate(ref es) => self.c_alternate(&**es),
            Repeat { ref e, r, greedy } => self.c_repeat(e, r, greedy),
        }
    }

    fn c_capture(&mut self, first_slot: usize, expr: &Expr) -> Result {
        if self.num_exprs > 1 || self.compiled.is_dfa {
            // Don't ever compile Save instructions for regex sets because
            // they are never used. They are also never used in DFA programs
            // because DFAs can't handle captures.
            self.c(expr)
        } else {
            let entry = self.insts.len();
            let hole = self.push_hole(InstHole::Save { slot: first_slot });
            let patch = try!(self.c(expr));
            self.fill(hole, patch.entry);
            self.fill_to_next(patch.hole);
            let hole = self.push_hole(InstHole::Save { slot: first_slot + 1 });
            Ok(Patch { hole: hole, entry: entry })
        }
    }

    fn c_dotstar(&mut self) -> Result {
        Ok(if !self.compiled.only_utf8() {
            try!(self.c(&Expr::Repeat {
                e: Box::new(Expr::AnyByte),
                r: Repeater::ZeroOrMore,
                greedy: false,
            }))
        } else {
            try!(self.c(&Expr::Repeat {
                e: Box::new(Expr::AnyChar),
                r: Repeater::ZeroOrMore,
                greedy: false,
            }))
        })
    }

    fn c_literal(&mut self, chars: &[char], casei: bool) -> Result {
        debug_assert!(!chars.is_empty());
        let mut chars: Box<Iterator<Item=&char>> =
            if self.compiled.is_reverse {
                Box::new(chars.iter().rev())
            } else {
                Box::new(chars.iter())
            };
        let first = *chars.next().expect("non-empty literal");
        let Patch { mut hole, entry } = try!(self.c_char(first, casei));
        for &c in chars {
            let p = try!(self.c_char(c, casei));
            self.fill(hole, p.entry);
            hole = p.hole;
        }
        Ok(Patch { hole: hole, entry: entry })
    }

    fn c_char(&mut self, c: char, casei: bool) -> Result {
        if casei {
            self.c_class(&CharClass::new(vec![
                ClassRange { start: c, end: c },
            ]).case_fold())
        } else {
            self.c_class(&[ClassRange { start: c, end: c }])
        }
    }

    fn c_class(&mut self, ranges: &[ClassRange]) -> Result {
        assert!(!ranges.is_empty());
        if self.compiled.uses_bytes() {
            CompileClass {
                c: self,
                ranges: ranges,
            }.compile()
        } else {
            let ranges: Vec<(char, char)> =
                ranges.iter().map(|r| (r.start, r.end)).collect();
            let hole = if ranges.len() == 1 && ranges[0].0 == ranges[0].1 {
                self.push_hole(InstHole::Char { c: ranges[0].0 })
            } else {
                self.push_hole(InstHole::Ranges { ranges: ranges })
            };
            Ok(Patch { hole: hole, entry: self.insts.len() - 1 })
        }
    }

    fn c_bytes(&mut self, bytes: &[u8], casei: bool) -> Result {
        debug_assert!(!bytes.is_empty());
        let mut bytes: Box<Iterator<Item=&u8>> =
            if self.compiled.is_reverse {
                Box::new(bytes.iter().rev())
            } else {
                Box::new(bytes.iter())
            };
        let first = *bytes.next().expect("non-empty literal");
        let Patch { mut hole, entry } = try!(self.c_byte(first, casei));
        for &b in bytes {
            let p = try!(self.c_byte(b, casei));
            self.fill(hole, p.entry);
            hole = p.hole;
        }
        Ok(Patch { hole: hole, entry: entry })
    }

    fn c_byte(&mut self, b: u8, casei: bool) -> Result {
        if casei {
            self.c_class_bytes(&ByteClass::new(vec![
                ByteRange { start: b, end: b },
            ]).case_fold())
        } else {
            self.c_class_bytes(&[ByteRange { start: b, end: b }])
        }
    }

    fn c_class_bytes(&mut self, ranges: &[ByteRange]) -> Result {
        debug_assert!(!ranges.is_empty());

        let first_split_entry = self.insts.len();
        let mut holes = vec![];
        let mut prev_hole = Hole::None;
        for r in &ranges[0..ranges.len() - 1] {
            self.fill_to_next(prev_hole);
            let split = self.push_split_hole();
            let next = self.insts.len();
            self.byte_classes.set_range(r.start, r.end);
            holes.push(self.push_hole(InstHole::Bytes {
                start: r.start, end: r.end,
            }));
            prev_hole = self.fill_split(split, Some(next), None);
        }
        let next = self.insts.len();
        let r = &ranges[ranges.len() - 1];
        self.byte_classes.set_range(r.start, r.end);
        holes.push(self.push_hole(InstHole::Bytes {
            start: r.start, end: r.end,
        }));
        self.fill(prev_hole, next);
        Ok(Patch { hole: Hole::Many(holes), entry: first_split_entry })
    }

    fn c_empty_look(&mut self, look: EmptyLook) -> Result {
        let hole = self.push_hole(InstHole::EmptyLook { look: look });
        Ok(Patch { hole: hole, entry: self.insts.len() - 1 })
    }

    fn c_concat<'a, I>(&mut self, exprs: I) -> Result
            where I: IntoIterator<Item=&'a Expr> {
        let mut exprs = exprs.into_iter();
        let first = match exprs.next() {
            Some(expr) => expr,
            None => {
                return Ok(Patch { hole: Hole::None, entry: self.insts.len() })
            }
        };
        let Patch { mut hole, entry } = try!(self.c(first));
        for e in exprs {
            let p = try!(self.c(e));
            self.fill(hole, p.entry);
            hole = p.hole;
        }
        Ok(Patch { hole: hole, entry: entry })
    }

    fn c_alternate(&mut self, exprs: &[Expr]) -> Result {
        debug_assert!(
            exprs.len() >= 2, "alternates must have at least 2 exprs");

        // Initial entry point is always the first split.
        let first_split_entry = self.insts.len();

        // Save up all of the holes from each alternate. They will all get
        // patched to point to the same location.
        let mut holes = vec![];

        let mut prev_hole = Hole::None;
        for e in &exprs[0..exprs.len() - 1] {
            self.fill_to_next(prev_hole);
            let split = self.push_split_hole();
            let Patch { hole, entry } = try!(self.c(e));
            holes.push(hole);
            prev_hole = self.fill_split(split, Some(entry), None);
        }
        let Patch { hole, entry } = try!(self.c(&exprs[exprs.len() - 1]));
        holes.push(hole);
        self.fill(prev_hole, entry);
        Ok(Patch { hole: Hole::Many(holes), entry: first_split_entry })
    }

    fn c_repeat(
        &mut self,
        expr: &Expr,
        kind: Repeater,
        greedy: bool,
    ) -> Result {
        match kind {
            Repeater::ZeroOrOne => self.c_repeat_zero_or_one(expr, greedy),
            Repeater::ZeroOrMore => self.c_repeat_zero_or_more(expr, greedy),
            Repeater::OneOrMore => self.c_repeat_one_or_more(expr, greedy),
            Repeater::Range { min, max: None } => {
                self.c_repeat_range_min_or_more(expr, greedy, min)
            }
            Repeater::Range { min, max: Some(max) } => {
                self.c_repeat_range(expr, greedy, min, max)
            }
        }
    }

    fn c_repeat_zero_or_one(
        &mut self,
        expr: &Expr,
        greedy: bool,
    ) -> Result {
        let split_entry = self.insts.len();
        let split = self.push_split_hole();
        let Patch { hole: hole_rep, entry: entry_rep } = try!(self.c(expr));

        let split_hole = if greedy {
            self.fill_split(split, Some(entry_rep), None)
        } else {
            self.fill_split(split, None, Some(entry_rep))
        };
        let holes = vec![hole_rep, split_hole];
        Ok(Patch { hole: Hole::Many(holes), entry: split_entry })
    }

    fn c_repeat_zero_or_more(
        &mut self,
        expr: &Expr,
        greedy: bool,
    ) -> Result {
        let split_entry = self.insts.len();
        let split = self.push_split_hole();
        let Patch { hole: hole_rep, entry: entry_rep } = try!(self.c(expr));

        self.fill(hole_rep, split_entry);
        let split_hole = if greedy {
            self.fill_split(split, Some(entry_rep), None)
        } else {
            self.fill_split(split, None, Some(entry_rep))
        };
        Ok(Patch { hole: split_hole, entry: split_entry })
    }

    fn c_repeat_one_or_more(
        &mut self,
        expr: &Expr,
        greedy: bool,
    ) -> Result {
        let Patch { hole: hole_rep, entry: entry_rep } = try!(self.c(expr));
        self.fill_to_next(hole_rep);
        let split = self.push_split_hole();

        let split_hole = if greedy {
            self.fill_split(split, Some(entry_rep), None)
        } else {
            self.fill_split(split, None, Some(entry_rep))
        };
        Ok(Patch { hole: split_hole, entry: entry_rep })
    }

    fn c_repeat_range_min_or_more(
        &mut self,
        expr: &Expr,
        greedy: bool,
        min: u32,
    ) -> Result {
        let min = u32_to_usize(min);
        let patch_concat = try!(self.c_concat(iter::repeat(expr).take(min)));
        let patch_rep = try!(self.c_repeat_zero_or_more(expr, greedy));
        self.fill(patch_concat.hole, patch_rep.entry);
        Ok(Patch { hole: patch_rep.hole, entry: patch_concat.entry })
    }

    fn c_repeat_range(
        &mut self,
        expr: &Expr,
        greedy: bool,
        min: u32,
        max: u32,
    ) -> Result {
        let (min, max) = (u32_to_usize(min), u32_to_usize(max));
        let patch_concat = try!(self.c_concat(iter::repeat(expr).take(min)));
        let initial_entry = patch_concat.entry;
        if min == max {
            return Ok(patch_concat);
        }
        // It is much simpler to compile, e.g., `a{2,5}` as:
        //
        //     aaa?a?a?
        //
        // But you end up with a sequence of instructions like this:
        //
        //     0: 'a'
        //     1: 'a',
        //     2: split(3, 4)
        //     3: 'a'
        //     4: split(5, 6)
        //     5: 'a'
        //     6: split(7, 8)
        //     7: 'a'
        //     8: MATCH
        //
        // This is *incredibly* inefficient because the splits end
        // up forming a chain, which has to be resolved everything a
        // transition is followed.
        let mut holes = vec![];
        let mut prev_hole = patch_concat.hole;
        for _ in min..max {
            self.fill_to_next(prev_hole);
            let split = self.push_split_hole();
            let Patch { hole, entry } = try!(self.c(expr));
            prev_hole = hole;
            if greedy {
                holes.push(self.fill_split(split, Some(entry), None));
            } else {
                holes.push(self.fill_split(split, None, Some(entry)));
            }
        }
        holes.push(prev_hole);
        Ok(Patch { hole: Hole::Many(holes), entry: initial_entry })
    }

    fn fill(&mut self, hole: Hole, goto: InstPtr) {
        match hole {
            Hole::None => {}
            Hole::One(pc) => {
                self.insts[pc].fill(goto);
            }
            Hole::Many(holes) => {
                for hole in holes {
                    self.fill(hole, goto);
                }
            }
        }
    }

    fn fill_to_next(&mut self, hole: Hole) {
        let next = self.insts.len();
        self.fill(hole, next);
    }

    fn fill_split(
        &mut self,
        hole: Hole,
        goto1: Option<InstPtr>,
        goto2: Option<InstPtr>,
    ) -> Hole {
        match hole {
            Hole::None => Hole::None,
            Hole::One(pc) => {
                match (goto1, goto2) {
                    (Some(goto1), Some(goto2)) => {
                        self.insts[pc].fill_split(goto1, goto2);
                        Hole::None
                    }
                    (Some(goto1), None) => {
                        self.insts[pc].half_fill_split_goto1(goto1);
                        Hole::One(pc)
                    }
                    (None, Some(goto2)) => {
                        self.insts[pc].half_fill_split_goto2(goto2);
                        Hole::One(pc)
                    }
                    (None, None) => unreachable!("at least one of the split \
                                                  holes must be filled"),
                }
            }
            Hole::Many(holes) => {
                let mut new_holes = vec![];
                for hole in holes {
                    new_holes.push(self.fill_split(hole, goto1, goto2));
                }
                if new_holes.is_empty() {
                    Hole::None
                } else if new_holes.len() == 1 {
                    new_holes.pop().unwrap()
                } else {
                    Hole::Many(new_holes)
                }
            }
        }
    }

    fn push_compiled(&mut self, inst: Inst) {
        self.insts.push(MaybeInst::Compiled(inst));
    }

    fn push_hole(&mut self, inst: InstHole) -> Hole {
        let hole = self.insts.len();
        self.insts.push(MaybeInst::Uncompiled(inst));
        Hole::One(hole)
    }

    fn push_split_hole(&mut self) -> Hole {
        let hole = self.insts.len();
        self.insts.push(MaybeInst::Split);
        Hole::One(hole)
    }

    fn check_size(&self) -> result::Result<(), Error> {
        use std::mem::size_of;

        if self.insts.len() * size_of::<Inst>() > self.size_limit {
            Err(Error::CompiledTooBig(self.size_limit))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
enum Hole {
    None,
    One(InstPtr),
    Many(Vec<Hole>),
}

#[derive(Clone, Debug)]
enum MaybeInst {
    Compiled(Inst),
    Uncompiled(InstHole),
    Split,
    Split1(InstPtr),
    Split2(InstPtr),
}

impl MaybeInst {
    fn fill(&mut self, goto: InstPtr) {
        let filled = match *self {
            MaybeInst::Uncompiled(ref inst) => inst.fill(goto),
            MaybeInst::Split1(goto1) => {
                Inst::Split(InstSplit { goto1: goto1, goto2: goto })
            }
            MaybeInst::Split2(goto2) => {
                Inst::Split(InstSplit { goto1: goto, goto2: goto2 })
            }
            _ => unreachable!("not all instructions were compiled! \
                               found uncompiled instruction: {:?}", self),
        };
        *self = MaybeInst::Compiled(filled);
    }

    fn fill_split(&mut self, goto1: InstPtr, goto2: InstPtr) {
        let filled = match *self {
            MaybeInst::Split => {
                Inst::Split(InstSplit { goto1: goto1, goto2: goto2 })
            }
            _ => unreachable!("must be called on Split instruction, \
                               instead it was called on: {:?}", self),
        };
        *self = MaybeInst::Compiled(filled);
    }

    fn half_fill_split_goto1(&mut self, goto1: InstPtr) {
        let half_filled = match *self {
            MaybeInst::Split => goto1,
            _ => unreachable!("must be called on Split instruction, \
                               instead it was called on: {:?}", self),
        };
        *self = MaybeInst::Split1(half_filled);
    }

    fn half_fill_split_goto2(&mut self, goto2: InstPtr) {
        let half_filled = match *self {
            MaybeInst::Split => goto2,
            _ => unreachable!("must be called on Split instruction, \
                               instead it was called on: {:?}", self),
        };
        *self = MaybeInst::Split2(half_filled);
    }

    fn unwrap(self) -> Inst {
        match self {
            MaybeInst::Compiled(inst) => inst,
            _ => unreachable!("must be called on a compiled instruction, \
                               instead it was called on: {:?}", self),
        }
    }
}

#[derive(Clone, Debug)]
enum InstHole {
    Save { slot: usize },
    EmptyLook { look: EmptyLook },
    Char { c: char },
    Ranges { ranges: Vec<(char, char)> },
    Bytes { start: u8, end: u8 },
}

impl InstHole {
    fn fill(&self, goto: InstPtr) -> Inst {
        match *self {
            InstHole::Save { slot } => Inst::Save(InstSave {
                goto: goto,
                slot: slot,
            }),
            InstHole::EmptyLook { look } => Inst::EmptyLook(InstEmptyLook {
                goto: goto,
                look: look,
            }),
            InstHole::Char { c } => Inst::Char(InstChar {
                goto: goto,
                c: c,
            }),
            InstHole::Ranges { ref ranges } => Inst::Ranges(InstRanges {
                goto: goto,
                ranges: ranges.clone(),
            }),
            InstHole::Bytes { start, end } => Inst::Bytes(InstBytes {
                goto: goto,
                start: start,
                end: end,
            }),
        }
    }
}

struct CompileClass<'a, 'b> {
    c: &'a mut Compiler,
    ranges: &'b [ClassRange],
}

impl<'a, 'b> CompileClass<'a, 'b> {
    fn compile(mut self) -> Result {
        let mut holes = vec![];
        let mut initial_entry = None;
        let mut last_split = Hole::None;
        let mut utf8_seqs = self.c.utf8_seqs.take().unwrap();
        self.c.suffix_cache.clear();

        for (i, ref range) in self.ranges.iter().enumerate() {
            let is_last_range = i + 1 == self.ranges.len();
            utf8_seqs.reset(range.start, range.end);
            let mut it = (&mut utf8_seqs).peekable();
            loop {
                let utf8_seq = match it.next() {
                    None => break,
                    Some(utf8_seq) => utf8_seq,
                };
                if is_last_range && it.peek().is_none() {
                    let Patch { hole, entry } = try!(self.c_utf8_seq(&utf8_seq));
                    holes.push(hole);
                    self.c.fill(last_split, entry);
                    last_split = Hole::None;
                    if initial_entry.is_none() {
                        initial_entry = Some(entry);
                    }
                } else {
                    if initial_entry.is_none() {
                        initial_entry = Some(self.c.insts.len());
                    }
                    self.c.fill_to_next(last_split);
                    last_split = self.c.push_split_hole();
                    let Patch { hole, entry } = try!(self.c_utf8_seq(&utf8_seq));
                    holes.push(hole);
                    last_split = self.c.fill_split(last_split, Some(entry), None);
                }
            }
        }
        self.c.utf8_seqs = Some(utf8_seqs);
        Ok(Patch {
            hole: Hole::Many(holes),
            entry: initial_entry.unwrap(),
        })
    }

    fn c_utf8_seq(&mut self, seq: &Utf8Sequence) -> Result {
        if self.c.compiled.is_reverse {
            self.c_utf8_seq_(seq)
        } else {
            self.c_utf8_seq_(seq.into_iter().rev())
        }
    }

    fn c_utf8_seq_<'r, I>(&mut self, seq: I) -> Result
            where I: IntoIterator<Item=&'r Utf8Range> {
        // The initial instruction for each UTF-8 sequence should be the same.
        let mut from_inst = ::std::usize::MAX;
        let mut last_hole = Hole::None;
        for byte_range in seq {
            let key = SuffixCacheKey {
                from_inst: from_inst,
                start: byte_range.start,
                end: byte_range.end,
            };
            {
                let pc = self.c.insts.len();
                if let Some(cached_pc) = self.c.suffix_cache.get(key, pc) {
                    from_inst = cached_pc;
                    continue;
                }
            }
            self.c.byte_classes.set_range(byte_range.start, byte_range.end);
            if from_inst == ::std::usize::MAX {
                last_hole = self.c.push_hole(InstHole::Bytes {
                    start: byte_range.start,
                    end: byte_range.end,
                });
            } else {
                self.c.push_compiled(Inst::Bytes(InstBytes {
                    goto: from_inst,
                    start: byte_range.start,
                    end: byte_range.end,
                }));
            }
            from_inst = self.c.insts.len().checked_sub(1).unwrap();
            debug_assert!(from_inst < ::std::usize::MAX);
        }
        debug_assert!(from_inst < ::std::usize::MAX);
        Ok(Patch { hole: last_hole, entry: from_inst })
    }
}

/// SuffixCache is a simple bounded hash map for caching suffix entries in
/// UTF-8 automata. For example, consider the Unicode range \u{0}-\u{FFFF}.
/// The set of byte ranges looks like this:
///
/// [0-7F]
/// [C2-DF][80-BF]
/// [E0][A0-BF][80-BF]
/// [E1-EC][80-BF][80-BF]
/// [ED][80-9F][80-BF]
/// [EE-EF][80-BF][80-BF]
///
/// Each line above translates to one alternate in the compiled regex program.
/// However, all but one of the alternates end in the same suffix, which is
/// a waste of an instruction. The suffix cache facilitates reusing them across
/// alternates.
///
/// Note that a HashMap could be trivially used for this, but we don't need its
/// overhead. Some small bounded space (LRU style) is more than enough.
struct SuffixCache {
    table: Vec<SuffixCacheEntry>,
    // Every time the cache is cleared, we increment the version number instead
    // of actually zeroing memory. Since we store a copy of the current version
    // in every element, all we need to do is make sure to invalidate any stale
    // entries upon access. This saves quite a bit of time!
    version: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
struct SuffixCacheEntry {
    key: SuffixCacheKey,
    pc: InstPtr,
    version: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
struct SuffixCacheKey {
    from_inst: InstPtr,
    start: u8,
    end: u8,
}

impl SuffixCache {
    fn new(size: usize) -> Self {
        SuffixCache {
            table: vec![SuffixCacheEntry::default(); size],
            version: 0,
        }
    }

    fn get(&mut self, key: SuffixCacheKey, pc: InstPtr) -> Option<InstPtr> {
        let h = self.hash(&key);
        let e = self.table[h];
        if e.key == key && e.version == self.version {
            Some(e.pc)
        } else {
            self.table[h] = SuffixCacheEntry {
                key: key,
                pc: pc,
                version: self.version,
            };
            None
        }
    }

    fn clear(&mut self) {
        self.version += 1;
    }

    fn hash(&self, suffix: &SuffixCacheKey) -> usize {
        // Basic FNV-1a hash as described:
        // https://en.wikipedia.org/wiki/Fowler%E2%80%93Noll%E2%80%93Vo_hash_function
        const FNV_PRIME: u64 = 1099511628211;
        let mut h = 14695981039346656037;
        h = (h ^ (suffix.from_inst as u64)).wrapping_mul(FNV_PRIME);
        h = (h ^ (suffix.start as u64)).wrapping_mul(FNV_PRIME);
        h = (h ^ (suffix.end as u64)).wrapping_mul(FNV_PRIME);
        (h as usize) % self.table.len()
    }
}

struct ByteClassSet([bool; 256]);

impl ByteClassSet {
    fn new() -> Self {
        ByteClassSet([false; 256])
    }

    fn set_range(&mut self, start: u8, end: u8) {
        debug_assert!(start <= end);
        if start > 0 {
            self.0[start as usize - 1] = true;
        }
        self.0[end as usize] = true;
    }

    fn set_word_boundary(&mut self) {
        // We need to mark all ranges of bytes whose pairs result in
        // evaluating \b differently.
        let iswb = is_word_byte;
        let mut b1: u16 = 0;
        let mut b2: u16;
        while b1 <= 255 {
            b2 = b1 + 1;
            while b2 <= 255 && iswb(b1 as u8) == iswb(b2 as u8) {
                b2 += 1;
            }
            self.set_range(b1 as u8, (b2 - 1) as u8);
            b1 = b2;
        }
    }

    fn byte_classes(&self) -> Vec<u8> {
        // N.B. If you're debugging the DFA, it's useful to simply return
        // `(0..256).collect()`, which effectively removes the byte classes
        // and makes the transitions easier to read.
        // (0usize..256).map(|x| x as u8).collect()
        let mut byte_classes = vec![0; 256];
        let mut class = 0u8;
        for i in 0..256 {
            byte_classes[i] = class;
            if self.0[i] {
                class = class.checked_add(1).unwrap();
            }
        }
        byte_classes
    }
}

fn u32_to_usize(n: u32) -> usize {
    if (n as u64) > (::std::usize::MAX as u64) {
        panic!("BUG: {} is too big to be pointer sized", n)
    }
    n as usize
}

#[cfg(test)]
mod tests {
    use super::ByteClassSet;

    #[test]
    fn byte_classes() {
        let mut set = ByteClassSet::new();
        set.set_range(b'a', b'z');
        let classes = set.byte_classes();
        assert_eq!(classes[0], 0);
        assert_eq!(classes[1], 0);
        assert_eq!(classes[2], 0);
        assert_eq!(classes[b'a' as usize - 1], 0);
        assert_eq!(classes[b'a' as usize], 1);
        assert_eq!(classes[b'm' as usize], 1);
        assert_eq!(classes[b'z' as usize], 1);
        assert_eq!(classes[b'z' as usize + 1], 2);
        assert_eq!(classes[254], 2);
        assert_eq!(classes[255], 2);

        let mut set = ByteClassSet::new();
        set.set_range(0, 2);
        set.set_range(4, 6);
        let classes = set.byte_classes();
        assert_eq!(classes[0], 0);
        assert_eq!(classes[1], 0);
        assert_eq!(classes[2], 0);
        assert_eq!(classes[3], 1);
        assert_eq!(classes[4], 2);
        assert_eq!(classes[5], 2);
        assert_eq!(classes[6], 2);
        assert_eq!(classes[7], 3);
        assert_eq!(classes[255], 3);
    }
}
