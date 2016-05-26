// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A pass that promotes borrows of constant rvalues.
//!
//! The rvalues considered constant are trees of temps,
//! each with exactly one initialization, and holding
//! a constant value with no interior mutability.
//! They are placed into a new MIR constant body in
//! `promoted` and the borrow rvalue is replaced with
//! a `Literal::Promoted` using the index into `promoted`
//! of that constant MIR.
//!
//! This pass assumes that every use is dominated by an
//! initialization and can otherwise silence errors, if
//! move analysis runs after promotion on broken MIR.

use rustc::mir::repr::*;
use rustc::mir::visit::{LvalueContext, MutVisitor, Visitor};
use rustc::ty::{self, TyCtxt};
use syntax::codemap::Span;

use build::Location;
use traversal::ReversePostorder;

use std::mem;

/// State of a temporary during collection and promotion.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TempState {
    /// No references to this temp.
    Undefined,
    /// One direct assignment and any number of direct uses.
    /// A borrow of this temp is promotable if the assigned
    /// value is qualified as constant.
    Defined {
        location: Location,
        uses: usize
    },
    /// Any other combination of assignments/uses.
    Unpromotable,
    /// This temp was part of an rvalue which got extracted
    /// during promotion and needs cleanup.
    PromotedOut
}

impl TempState {
    pub fn is_promotable(&self) -> bool {
        if let TempState::Defined { uses, .. } = *self {
            uses > 0
        } else {
            false
        }
    }
}

/// A "root candidate" for promotion, which will become the
/// returned value in a promoted MIR, unless it's a subset
/// of a larger candidate.
pub enum Candidate {
    /// Borrow of a constant temporary.
    Ref(Location),

    /// Array of indices found in the third argument of
    /// a call to one of the simd_shuffleN intrinsics.
    ShuffleIndices(BasicBlock)
}

struct TempCollector {
    temps: Vec<TempState>,
    location: Location,
    span: Span
}

impl<'tcx> Visitor<'tcx> for TempCollector {
    fn visit_lvalue(&mut self, lvalue: &Lvalue<'tcx>, context: LvalueContext) {
        self.super_lvalue(lvalue, context);
        if let Lvalue::Temp(index) = *lvalue {
            // Ignore drops, if the temp gets promoted,
            // then it's constant and thus drop is noop.
            if let LvalueContext::Drop = context {
                return;
            }

            let temp = &mut self.temps[index as usize];
            if *temp == TempState::Undefined {
                match context {
                    LvalueContext::Store |
                    LvalueContext::Call => {
                        *temp = TempState::Defined {
                            location: self.location,
                            uses: 0
                        };
                        return;
                    }
                    _ => { /* mark as unpromotable below */ }
                }
            } else if let TempState::Defined { ref mut uses, .. } = *temp {
                match context {
                    LvalueContext::Borrow {..} |
                    LvalueContext::Consume |
                    LvalueContext::Inspect => {
                        *uses += 1;
                        return;
                    }
                    _ => { /* mark as unpromotable below */ }
                }
            }
            *temp = TempState::Unpromotable;
        }
    }

    fn visit_statement(&mut self, bb: BasicBlock, statement: &Statement<'tcx>) {
        assert_eq!(self.location.block, bb);
        self.span = statement.span;
        self.super_statement(bb, statement);
        self.location.statement_index += 1;
    }

    fn visit_terminator(&mut self, bb: BasicBlock, terminator: &Terminator<'tcx>) {
        self.span = terminator.span;
        self.super_terminator(bb, terminator);
    }

    fn visit_basic_block_data(&mut self, bb: BasicBlock, data: &BasicBlockData<'tcx>) {
        self.location.statement_index = 0;
        self.location.block = bb;
        self.super_basic_block_data(bb, data);
    }
}

pub fn collect_temps(mir: &Mir, rpo: &mut ReversePostorder) -> Vec<TempState> {
    let mut collector = TempCollector {
        temps: vec![TempState::Undefined; mir.temp_decls.len()],
        location: Location {
            block: START_BLOCK,
            statement_index: 0
        },
        span: mir.span
    };
    for (bb, data) in rpo {
        collector.visit_basic_block_data(bb, data);
    }
    collector.temps
}

struct Promoter<'a, 'tcx: 'a> {
    source: &'a mut Mir<'tcx>,
    promoted: Mir<'tcx>,
    temps: &'a mut Vec<TempState>,

    /// If true, all nested temps are also kept in the
    /// source MIR, not moved to the promoted MIR.
    keep_original: bool
}

impl<'a, 'tcx> Promoter<'a, 'tcx> {
    fn new_block(&mut self) -> BasicBlock {
        let index = self.promoted.basic_blocks.len();
        self.promoted.basic_blocks.push(BasicBlockData {
            statements: vec![],
            terminator: Some(Terminator {
                span: self.promoted.span,
                scope: ScopeId::new(0),
                kind: TerminatorKind::Return
            }),
            is_cleanup: false
        });
        BasicBlock::new(index)
    }

    fn assign(&mut self, dest: Lvalue<'tcx>, rvalue: Rvalue<'tcx>, span: Span) {
        let data = self.promoted.basic_blocks.last_mut().unwrap();
        data.statements.push(Statement {
            span: span,
            scope: ScopeId::new(0),
            kind: StatementKind::Assign(dest, rvalue)
        });
    }

    /// Copy the initialization of this temp to the
    /// promoted MIR, recursing through temps.
    fn promote_temp(&mut self, index: u32) -> u32 {
        let index = index as usize;
        let old_keep_original = self.keep_original;
        let (bb, stmt_idx) = match self.temps[index] {
            TempState::Defined {
                location: Location { block, statement_index },
                uses
            } if uses > 0 => {
                if uses > 1 {
                    self.keep_original = true;
                }
                (block, statement_index)
            }
            temp =>  {
                span_bug!(self.promoted.span, "tmp{} not promotable: {:?}",
                          index, temp);
            }
        };
        if !self.keep_original {
            self.temps[index] = TempState::PromotedOut;
        }

        let no_stmts = self.source[bb].statements.len();

        // First, take the Rvalue or Call out of the source MIR,
        // or duplicate it, depending on keep_original.
        let (mut rvalue, mut call) = (None, None);
        let span = if stmt_idx < no_stmts {
            let statement = &mut self.source[bb].statements[stmt_idx];
            let StatementKind::Assign(_, ref mut rhs) = statement.kind;
            if self.keep_original {
                rvalue = Some(rhs.clone());
            } else {
                let unit = Rvalue::Aggregate(AggregateKind::Tuple, vec![]);
                rvalue = Some(mem::replace(rhs, unit));
            }
            statement.span
        } else if self.keep_original {
            let terminator = self.source[bb].terminator().clone();
            call = Some(terminator.kind);
            terminator.span
        } else {
            let terminator = self.source[bb].terminator_mut();
            let target = match terminator.kind {
                TerminatorKind::Call {
                    destination: ref mut dest @ Some(_),
                    ref mut cleanup, ..
                } => {
                    // No cleanup necessary.
                    cleanup.take();

                    // We'll put a new destination in later.
                    dest.take().unwrap().1
                }
                ref kind => {
                    span_bug!(terminator.span, "{:?} not promotable", kind);
                }
            };
            call = Some(mem::replace(&mut terminator.kind, TerminatorKind::Goto {
                target: target
            }));
            terminator.span
        };

        // Then, recurse for components in the Rvalue or Call.
        if stmt_idx < no_stmts {
            self.visit_rvalue(rvalue.as_mut().unwrap());
        } else {
            self.visit_terminator_kind(bb, call.as_mut().unwrap());
        }

        let new_index = self.promoted.temp_decls.len() as u32;
        let new_temp = Lvalue::Temp(new_index);
        self.promoted.temp_decls.push(TempDecl {
            ty: self.source.temp_decls[index].ty
        });

        // Inject the Rvalue or Call into the promoted MIR.
        if stmt_idx < no_stmts {
            self.assign(new_temp, rvalue.unwrap(), span);
        } else {
            let last = self.promoted.basic_blocks.len() - 1;
            let new_target = self.new_block();
            let mut call = call.unwrap();
            match call {
                TerminatorKind::Call { ref mut destination, ..}  => {
                    *destination = Some((new_temp, new_target));
                }
                _ => bug!()
            }
            let terminator = &mut self.promoted.basic_blocks[last].terminator_mut();
            terminator.span = span;
            terminator.kind = call;
        }

        // Restore the old duplication state.
        self.keep_original = old_keep_original;

        new_index
    }

    fn promote_candidate(mut self, candidate: Candidate) {
        let span = self.promoted.span;
        let new_operand = Operand::Constant(Constant {
            span: span,
            ty: self.promoted.return_ty.unwrap(),
            literal: Literal::Promoted {
                index: self.source.promoted.len()
            }
        });
        let mut rvalue = match candidate {
            Candidate::Ref(Location { block: bb, statement_index: stmt_idx }) => {
                match self.source[bb].statements[stmt_idx].kind {
                    StatementKind::Assign(_, ref mut rvalue) => {
                        mem::replace(rvalue, Rvalue::Use(new_operand))
                    }
                }
            }
            Candidate::ShuffleIndices(bb) => {
                match self.source[bb].terminator_mut().kind {
                    TerminatorKind::Call { ref mut args, .. } => {
                        Rvalue::Use(mem::replace(&mut args[2], new_operand))
                    }
                    _ => bug!()
                }
            }
        };
        self.visit_rvalue(&mut rvalue);
        self.assign(Lvalue::ReturnPointer, rvalue, span);
        self.source.promoted.push(self.promoted);
    }
}

/// Replaces all temporaries with their promoted counterparts.
impl<'a, 'tcx> MutVisitor<'tcx> for Promoter<'a, 'tcx> {
    fn visit_lvalue(&mut self, lvalue: &mut Lvalue<'tcx>, context: LvalueContext) {
        if let Lvalue::Temp(ref mut index) = *lvalue {
            *index = self.promote_temp(*index);
        }
        self.super_lvalue(lvalue, context);
    }
}

pub fn promote_candidates<'a, 'tcx>(mir: &mut Mir<'tcx>,
                                    tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                    mut temps: Vec<TempState>,
                                    candidates: Vec<Candidate>) {
    // Visit candidates in reverse, in case they're nested.
    for candidate in candidates.into_iter().rev() {
        let (span, ty) = match candidate {
            Candidate::Ref(Location { block: bb, statement_index: stmt_idx }) => {
                let statement = &mir[bb].statements[stmt_idx];
                let StatementKind::Assign(ref dest, _) = statement.kind;
                if let Lvalue::Temp(index) = *dest {
                    if temps[index as usize] == TempState::PromotedOut {
                        // Already promoted.
                        continue;
                    }
                }
                (statement.span, mir.lvalue_ty(tcx, dest).to_ty(tcx))
            }
            Candidate::ShuffleIndices(bb) => {
                let terminator = mir[bb].terminator();
                let ty = match terminator.kind {
                    TerminatorKind::Call { ref args, .. } => {
                        mir.operand_ty(tcx, &args[2])
                    }
                    _ => {
                        span_bug!(terminator.span,
                                  "expected simd_shuffleN call to promote");
                    }
                };
                (terminator.span, ty)
            }
        };

        let mut promoter = Promoter {
            source: mir,
            promoted: Mir {
                basic_blocks: vec![],
                scopes: vec![ScopeData {
                    span: span,
                    parent_scope: None
                }],
                promoted: vec![],
                return_ty: ty::FnConverging(ty),
                var_decls: vec![],
                arg_decls: vec![],
                temp_decls: vec![],
                upvar_decls: vec![],
                span: span
            },
            temps: &mut temps,
            keep_original: false
        };
        assert_eq!(promoter.new_block(), START_BLOCK);
        promoter.promote_candidate(candidate);
    }

    // Eliminate assignments to, and drops of promoted temps.
    let promoted = |index: u32| temps[index as usize] == TempState::PromotedOut;
    for block in &mut mir.basic_blocks {
        block.statements.retain(|statement| {
            match statement.kind {
                StatementKind::Assign(Lvalue::Temp(index), _) => {
                    !promoted(index)
                }
                _ => true
            }
        });
        let terminator = block.terminator_mut();
        match terminator.kind {
            TerminatorKind::Drop { value: Lvalue::Temp(index), target, .. } => {
                if promoted(index) {
                    terminator.kind = TerminatorKind::Goto {
                        target: target
                    };
                }
            }
            _ => {}
        }
    }
}