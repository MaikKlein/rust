// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Mono Item Collection
//! ===========================
//!
//! This module is responsible for discovering all items that will contribute to
//! to code generation of the crate. The important part here is that it not only
//! needs to find syntax-level items (functions, structs, etc) but also all
//! their monomorphized instantiations. Every non-generic, non-const function
//! maps to one LLVM artifact. Every generic function can produce
//! from zero to N artifacts, depending on the sets of type arguments it
//! is instantiated with.
//! This also applies to generic items from other crates: A generic definition
//! in crate X might produce monomorphizations that are compiled into crate Y.
//! We also have to collect these here.
//!
//! The following kinds of "mono items" are handled here:
//!
//! - Functions
//! - Methods
//! - Closures
//! - Statics
//! - Drop glue
//!
//! The following things also result in LLVM artifacts, but are not collected
//! here, since we instantiate them locally on demand when needed in a given
//! codegen unit:
//!
//! - Constants
//! - Vtables
//! - Object Shims
//!
//!
//! General Algorithm
//! -----------------
//! Let's define some terms first:
//!
//! - A "mono item" is something that results in a function or global in
//!   the LLVM IR of a codegen unit. Mono items do not stand on their
//!   own, they can reference other mono items. For example, if function
//!   `foo()` calls function `bar()` then the mono item for `foo()`
//!   references the mono item for function `bar()`. In general, the
//!   definition for mono item A referencing a mono item B is that
//!   the LLVM artifact produced for A references the LLVM artifact produced
//!   for B.
//!
//! - Mono items and the references between them form a directed graph,
//!   where the mono items are the nodes and references form the edges.
//!   Let's call this graph the "mono item graph".
//!
//! - The mono item graph for a program contains all mono items
//!   that are needed in order to produce the complete LLVM IR of the program.
//!
//! The purpose of the algorithm implemented in this module is to build the
//! mono item graph for the current crate. It runs in two phases:
//!
//! 1. Discover the roots of the graph by traversing the HIR of the crate.
//! 2. Starting from the roots, find neighboring nodes by inspecting the MIR
//!    representation of the item corresponding to a given node, until no more
//!    new nodes are found.
//!
//! ### Discovering roots
//!
//! The roots of the mono item graph correspond to the non-generic
//! syntactic items in the source code. We find them by walking the HIR of the
//! crate, and whenever we hit upon a function, method, or static item, we
//! create a mono item consisting of the items DefId and, since we only
//! consider non-generic items, an empty type-substitution set.
//!
//! ### Finding neighbor nodes
//! Given a mono item node, we can discover neighbors by inspecting its
//! MIR. We walk the MIR and any time we hit upon something that signifies a
//! reference to another mono item, we have found a neighbor. Since the
//! mono item we are currently at is always monomorphic, we also know the
//! concrete type arguments of its neighbors, and so all neighbors again will be
//! monomorphic. The specific forms a reference to a neighboring node can take
//! in MIR are quite diverse. Here is an overview:
//!
//! #### Calling Functions/Methods
//! The most obvious form of one mono item referencing another is a
//! function or method call (represented by a CALL terminator in MIR). But
//! calls are not the only thing that might introduce a reference between two
//! function mono items, and as we will see below, they are just a
//! specialized of the form described next, and consequently will don't get any
//! special treatment in the algorithm.
//!
//! #### Taking a reference to a function or method
//! A function does not need to actually be called in order to be a neighbor of
//! another function. It suffices to just take a reference in order to introduce
//! an edge. Consider the following example:
//!
//! ```rust
//! fn print_val<T: Display>(x: T) {
//!     println!("{}", x);
//! }
//!
//! fn call_fn(f: &Fn(i32), x: i32) {
//!     f(x);
//! }
//!
//! fn main() {
//!     let print_i32 = print_val::<i32>;
//!     call_fn(&print_i32, 0);
//! }
//! ```
//! The MIR of none of these functions will contain an explicit call to
//! `print_val::<i32>`. Nonetheless, in order to mono this program, we need
//! an instance of this function. Thus, whenever we encounter a function or
//! method in operand position, we treat it as a neighbor of the current
//! mono item. Calls are just a special case of that.
//!
//! #### Closures
//! In a way, closures are a simple case. Since every closure object needs to be
//! constructed somewhere, we can reliably discover them by observing
//! `RValue::Aggregate` expressions with `AggregateKind::Closure`. This is also
//! true for closures inlined from other crates.
//!
//! #### Drop glue
//! Drop glue mono items are introduced by MIR drop-statements. The
//! generated mono item will again have drop-glue item neighbors if the
//! type to be dropped contains nested values that also need to be dropped. It
//! might also have a function item neighbor for the explicit `Drop::drop`
//! implementation of its type.
//!
//! #### Unsizing Casts
//! A subtle way of introducing neighbor edges is by casting to a trait object.
//! Since the resulting fat-pointer contains a reference to a vtable, we need to
//! instantiate all object-save methods of the trait, as we need to store
//! pointers to these functions even if they never get called anywhere. This can
//! be seen as a special case of taking a function reference.
//!
//! #### Boxes
//! Since `Box` expression have special compiler support, no explicit calls to
//! `exchange_malloc()` and `exchange_free()` may show up in MIR, even if the
//! compiler will generate them. We have to observe `Rvalue::Box` expressions
//! and Box-typed drop-statements for that purpose.
//!
//!
//! Interaction with Cross-Crate Inlining
//! -------------------------------------
//! The binary of a crate will not only contain machine code for the items
//! defined in the source code of that crate. It will also contain monomorphic
//! instantiations of any extern generic functions and of functions marked with
//! #[inline].
//! The collection algorithm handles this more or less mono. If it is
//! about to create a mono item for something with an external `DefId`,
//! it will take a look if the MIR for that item is available, and if so just
//! proceed normally. If the MIR is not available, it assumes that the item is
//! just linked to and no node is created; which is exactly what we want, since
//! no machine code should be generated in the current crate for such an item.
//!
//! Eager and Lazy Collection Mode
//! ------------------------------
//! Mono item collection can be performed in one of two modes:
//!
//! - Lazy mode means that items will only be instantiated when actually
//!   referenced. The goal is to produce the least amount of machine code
//!   possible.
//!
//! - Eager mode is meant to be used in conjunction with incremental compilation
//!   where a stable set of mono items is more important than a minimal
//!   one. Thus, eager mode will instantiate drop-glue for every drop-able type
//!   in the crate, even of no drop call for that type exists (yet). It will
//!   also instantiate default implementations of trait methods, something that
//!   otherwise is only done on demand.
//!
//!
//! Open Issues
//! -----------
//! Some things are not yet fully implemented in the current version of this
//! module.
//!
//! ### Initializers of Constants and Statics
//! Since no MIR is constructed yet for initializer expressions of constants and
//! statics we cannot inspect these properly.
//!
//! ### Const Fns
//! Ideally, no mono item should be generated for const fns unless there
//! is a call to them that cannot be evaluated at compile time. At the moment
//! this is not implemented however: a mono item will be produced
//! regardless of whether it is actually needed or not.

use rustc::hir;
use rustc::hir::itemlikevisit::ItemLikeVisitor;

use rustc::hir::map as hir_map;
use rustc::hir::def_id::DefId;
use rustc::middle::const_val::ConstVal;
use rustc::middle::lang_items::{ExchangeMallocFnLangItem};
use rustc::traits;
use rustc::ty::subst::Substs;
use rustc::ty::{self, TypeFoldable, Ty, TyCtxt};
use rustc::ty::adjustment::CustomCoerceUnsized;
use rustc::mir::{self, Location};
use rustc::mir::visit::Visitor as MirVisitor;
use rustc::mir::mono::MonoItem;

use monomorphize::{self, Instance};
use rustc::util::nodemap::{FxHashSet, FxHashMap, DefIdMap};

use monomorphize::item::{MonoItemExt, DefPathBasedNames, InstantiationMode};

use rustc_data_structures::bitvec::BitVector;

use syntax::attr;

#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug)]
pub enum MonoItemCollectionMode {
    Eager,
    Lazy
}

/// Maps every mono item to all mono items it references in its
/// body.
pub struct InliningMap<'tcx> {
    // Maps a source mono item to the range of mono items
    // accessed by it.
    // The two numbers in the tuple are the start (inclusive) and
    // end index (exclusive) within the `targets` vecs.
    index: FxHashMap<MonoItem<'tcx>, (usize, usize)>,
    targets: Vec<MonoItem<'tcx>>,

    // Contains one bit per mono item in the `targets` field. That bit
    // is true if that mono item needs to be inlined into every CGU.
    inlines: BitVector,
}

impl<'tcx> InliningMap<'tcx> {

    fn new() -> InliningMap<'tcx> {
        InliningMap {
            index: FxHashMap(),
            targets: Vec::new(),
            inlines: BitVector::new(1024),
        }
    }

    fn record_accesses<I>(&mut self,
                          source: MonoItem<'tcx>,
                          new_targets: I)
        where I: Iterator<Item=(MonoItem<'tcx>, bool)> + ExactSizeIterator
    {
        assert!(!self.index.contains_key(&source));

        let start_index = self.targets.len();
        let new_items_count = new_targets.len();
        let new_items_count_total = new_items_count + self.targets.len();

        self.targets.reserve(new_items_count);
        self.inlines.grow(new_items_count_total);

        for (i, (target, inline)) in new_targets.enumerate() {
            self.targets.push(target);
            if inline {
                self.inlines.insert(i + start_index);
            }
        }

        let end_index = self.targets.len();
        self.index.insert(source, (start_index, end_index));
    }

    // Internally iterate over all items referenced by `source` which will be
    // made available for inlining.
    pub fn with_inlining_candidates<F>(&self, source: MonoItem<'tcx>, mut f: F)
        where F: FnMut(MonoItem<'tcx>)
    {
        if let Some(&(start_index, end_index)) = self.index.get(&source) {
            for (i, candidate) in self.targets[start_index .. end_index]
                                      .iter()
                                      .enumerate() {
                if self.inlines.contains(start_index + i) {
                    f(*candidate);
                }
            }
        }
    }

    // Internally iterate over all items and the things each accesses.
    pub fn iter_accesses<F>(&self, mut f: F)
        where F: FnMut(MonoItem<'tcx>, &[MonoItem<'tcx>])
    {
        for (&accessor, &(start_index, end_index)) in &self.index {
            f(accessor, &self.targets[start_index .. end_index])
        }
    }
}

pub fn collect_crate_mono_items<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                          mode: MonoItemCollectionMode)
                                          -> (FxHashSet<MonoItem<'tcx>>,
                                                     InliningMap<'tcx>) {
    let roots = collect_roots(tcx, mode);

    debug!("Building mono item graph, beginning at roots");
    let mut visited = FxHashSet();
    let mut recursion_depths = DefIdMap();
    let mut inlining_map = InliningMap::new();

    for root in roots {
        collect_items_rec(tcx,
                          root,
                          &mut visited,
                          &mut recursion_depths,
                          &mut inlining_map);
    }

    (visited, inlining_map)
}

// Find all non-generic items by walking the HIR. These items serve as roots to
// start monomorphizing from.
fn collect_roots<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                           mode: MonoItemCollectionMode)
                           -> Vec<MonoItem<'tcx>> {
    debug!("Collecting roots");
    let mut roots = Vec::new();

    {
        let entry_fn = tcx.sess.entry_fn.borrow().map(|(node_id, _)| {
            tcx.hir.local_def_id(node_id)
        });

        let mut visitor = RootCollector {
            tcx,
            mode,
            entry_fn,
            output: &mut roots,
        };

        tcx.hir.krate().visit_all_item_likes(&mut visitor);
    }

    // We can only translate items that are instantiable - items all of
    // whose predicates hold. Luckily, items that aren't instantiable
    // can't actually be used, so we can just skip translating them.
    roots.retain(|root| root.is_instantiable(tcx));

    roots
}

// Collect all monomorphized items reachable from `starting_point`
fn collect_items_rec<'a, 'tcx: 'a>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                   starting_point: MonoItem<'tcx>,
                                   visited: &mut FxHashSet<MonoItem<'tcx>>,
                                   recursion_depths: &mut DefIdMap<usize>,
                                   inlining_map: &mut InliningMap<'tcx>) {
    if !visited.insert(starting_point.clone()) {
        // We've been here already, no need to search again.
        return;
    }
    debug!("BEGIN collect_items_rec({})", starting_point.to_string(tcx));

    let mut neighbors = Vec::new();
    let recursion_depth_reset;

    match starting_point {
        MonoItem::Static(node_id) => {
            let def_id = tcx.hir.local_def_id(node_id);
            let instance = Instance::mono(tcx, def_id);

            // Sanity check whether this ended up being collected accidentally
            debug_assert!(should_monomorphize_locally(tcx, &instance));

            let ty = instance.ty(tcx);
            visit_drop_use(tcx, ty, true, &mut neighbors);

            recursion_depth_reset = None;

            collect_neighbours(tcx, instance, true, &mut neighbors);
        }
        MonoItem::Fn(instance) => {
            // Sanity check whether this ended up being collected accidentally
            debug_assert!(should_monomorphize_locally(tcx, &instance));

            // Keep track of the monomorphization recursion depth
            recursion_depth_reset = Some(check_recursion_limit(tcx,
                                                               instance,
                                                               recursion_depths));
            check_type_length_limit(tcx, instance);

            collect_neighbours(tcx, instance, false, &mut neighbors);
        }
        MonoItem::GlobalAsm(..) => {
            recursion_depth_reset = None;
        }
    }

    record_accesses(tcx, starting_point, &neighbors[..], inlining_map);

    for neighbour in neighbors {
        collect_items_rec(tcx, neighbour, visited, recursion_depths, inlining_map);
    }

    if let Some((def_id, depth)) = recursion_depth_reset {
        recursion_depths.insert(def_id, depth);
    }

    debug!("END collect_items_rec({})", starting_point.to_string(tcx));
}

fn record_accesses<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                             caller: MonoItem<'tcx>,
                             callees: &[MonoItem<'tcx>],
                             inlining_map: &mut InliningMap<'tcx>) {
    let is_inlining_candidate = |mono_item: &MonoItem<'tcx>| {
        mono_item.instantiation_mode(tcx) == InstantiationMode::LocalCopy
    };

    let accesses = callees.into_iter()
                          .map(|mono_item| {
                             (*mono_item, is_inlining_candidate(mono_item))
                          });

    inlining_map.record_accesses(caller, accesses);
}

fn check_recursion_limit<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                   instance: Instance<'tcx>,
                                   recursion_depths: &mut DefIdMap<usize>)
                                   -> (DefId, usize) {
    let def_id = instance.def_id();
    let recursion_depth = recursion_depths.get(&def_id).cloned().unwrap_or(0);
    debug!(" => recursion depth={}", recursion_depth);

    let recursion_depth = if Some(def_id) == tcx.lang_items().drop_in_place_fn() {
        // HACK: drop_in_place creates tight monomorphization loops. Give
        // it more margin.
        recursion_depth / 4
    } else {
        recursion_depth
    };

    // Code that needs to instantiate the same function recursively
    // more than the recursion limit is assumed to be causing an
    // infinite expansion.
    if recursion_depth > tcx.sess.recursion_limit.get() {
        let error = format!("reached the recursion limit while instantiating `{}`",
                            instance);
        if let Some(node_id) = tcx.hir.as_local_node_id(def_id) {
            tcx.sess.span_fatal(tcx.hir.span(node_id), &error);
        } else {
            tcx.sess.fatal(&error);
        }
    }

    recursion_depths.insert(def_id, recursion_depth + 1);

    (def_id, recursion_depth)
}

fn check_type_length_limit<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                     instance: Instance<'tcx>)
{
    let type_length = instance.substs.types().flat_map(|ty| ty.walk()).count();
    debug!(" => type length={}", type_length);

    // Rust code can easily create exponentially-long types using only a
    // polynomial recursion depth. Even with the default recursion
    // depth, you can easily get cases that take >2^60 steps to run,
    // which means that rustc basically hangs.
    //
    // Bail out in these cases to avoid that bad user experience.
    let type_length_limit = tcx.sess.type_length_limit.get();
    if type_length > type_length_limit {
        // The instance name is already known to be too long for rustc. Use
        // `{:.64}` to avoid blasting the user's terminal with thousands of
        // lines of type-name.
        let instance_name = instance.to_string();
        let msg = format!("reached the type-length limit while instantiating `{:.64}...`",
                          instance_name);
        let mut diag = if let Some(node_id) = tcx.hir.as_local_node_id(instance.def_id()) {
            tcx.sess.struct_span_fatal(tcx.hir.span(node_id), &msg)
        } else {
            tcx.sess.struct_fatal(&msg)
        };

        diag.note(&format!(
            "consider adding a `#![type_length_limit=\"{}\"]` attribute to your crate",
            type_length_limit*2));
        diag.emit();
        tcx.sess.abort_if_errors();
    }
}

struct MirNeighborCollector<'a, 'tcx: 'a> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    mir: &'a mir::Mir<'tcx>,
    output: &'a mut Vec<MonoItem<'tcx>>,
    param_substs: &'tcx Substs<'tcx>,
    const_context: bool,
}

impl<'a, 'tcx> MirVisitor<'tcx> for MirNeighborCollector<'a, 'tcx> {

    fn visit_rvalue(&mut self, rvalue: &mir::Rvalue<'tcx>, location: Location) {
        debug!("visiting rvalue {:?}", *rvalue);

        match *rvalue {
            // When doing an cast from a regular pointer to a fat pointer, we
            // have to instantiate all methods of the trait being cast to, so we
            // can build the appropriate vtable.
            mir::Rvalue::Cast(mir::CastKind::Unsize, ref operand, target_ty) => {
                let target_ty = self.tcx.trans_apply_param_substs(self.param_substs,
                                                                  &target_ty);
                let source_ty = operand.ty(self.mir, self.tcx);
                let source_ty = self.tcx.trans_apply_param_substs(self.param_substs,
                                                                  &source_ty);
                let (source_ty, target_ty) = find_vtable_types_for_unsizing(self.tcx,
                                                                            source_ty,
                                                                            target_ty);
                // This could also be a different Unsize instruction, like
                // from a fixed sized array to a slice. But we are only
                // interested in things that produce a vtable.
                if target_ty.is_trait() && !source_ty.is_trait() {
                    create_mono_items_for_vtable_methods(self.tcx,
                                                         target_ty,
                                                         source_ty,
                                                         self.output);
                }
            }
            mir::Rvalue::Cast(mir::CastKind::ReifyFnPointer, ref operand, _) => {
                let fn_ty = operand.ty(self.mir, self.tcx);
                let fn_ty = self.tcx.trans_apply_param_substs(self.param_substs,
                                                              &fn_ty);
                visit_fn_use(self.tcx, fn_ty, false, &mut self.output);
            }
            mir::Rvalue::Cast(mir::CastKind::ClosureFnPointer, ref operand, _) => {
                let source_ty = operand.ty(self.mir, self.tcx);
                let source_ty = self.tcx.trans_apply_param_substs(self.param_substs,
                                                                  &source_ty);
                match source_ty.sty {
                    ty::TyClosure(def_id, substs) => {
                        let instance = monomorphize::resolve_closure(
                            self.tcx, def_id, substs, ty::ClosureKind::FnOnce);
                        self.output.push(create_fn_mono_item(instance));
                    }
                    _ => bug!(),
                }
            }
            mir::Rvalue::NullaryOp(mir::NullOp::Box, _) => {
                let tcx = self.tcx;
                let exchange_malloc_fn_def_id = tcx
                    .lang_items()
                    .require(ExchangeMallocFnLangItem)
                    .unwrap_or_else(|e| tcx.sess.fatal(&e));
                let instance = Instance::mono(tcx, exchange_malloc_fn_def_id);
                if should_monomorphize_locally(tcx, &instance) {
                    self.output.push(create_fn_mono_item(instance));
                }
            }
            _ => { /* not interesting */ }
        }

        self.super_rvalue(rvalue, location);
    }

    fn visit_const(&mut self, constant: &&'tcx ty::Const<'tcx>, location: Location) {
        debug!("visiting const {:?} @ {:?}", *constant, location);

        if let ConstVal::Unevaluated(def_id, substs) = constant.val {
            let substs = self.tcx.trans_apply_param_substs(self.param_substs,
                                                           &substs);
            let instance = ty::Instance::resolve(self.tcx,
                                                 ty::ParamEnv::empty(traits::Reveal::All),
                                                 def_id,
                                                 substs).unwrap();
            collect_neighbours(self.tcx, instance, true, self.output);
        }

        self.super_const(constant);
    }

    fn visit_terminator_kind(&mut self,
                             block: mir::BasicBlock,
                             kind: &mir::TerminatorKind<'tcx>,
                             location: Location) {
        debug!("visiting terminator {:?} @ {:?}", kind, location);

        let tcx = self.tcx;
        match *kind {
            mir::TerminatorKind::Call { ref func, .. } => {
                let callee_ty = func.ty(self.mir, tcx);
                let callee_ty = tcx.trans_apply_param_substs(self.param_substs, &callee_ty);

                let constness = match (self.const_context, &callee_ty.sty) {
                    (true, &ty::TyFnDef(def_id, substs)) if self.tcx.is_const_fn(def_id) => {
                        let instance =
                            ty::Instance::resolve(self.tcx,
                                                  ty::ParamEnv::empty(traits::Reveal::All),
                                                  def_id,
                                                  substs).unwrap();
                        Some(instance)
                    }
                    _ => None
                };

                if let Some(const_fn_instance) = constness {
                    // If this is a const fn, called from a const context, we
                    // have to visit its body in order to find any fn reifications
                    // it might contain.
                    collect_neighbours(self.tcx,
                                       const_fn_instance,
                                       true,
                                       self.output);
                } else {
                    visit_fn_use(self.tcx, callee_ty, true, &mut self.output);
                }
            }
            mir::TerminatorKind::Drop { ref location, .. } |
            mir::TerminatorKind::DropAndReplace { ref location, .. } => {
                let ty = location.ty(self.mir, self.tcx)
                    .to_ty(self.tcx);
                let ty = tcx.trans_apply_param_substs(self.param_substs, &ty);
                visit_drop_use(self.tcx, ty, true, self.output);
            }
            mir::TerminatorKind::Goto { .. } |
            mir::TerminatorKind::SwitchInt { .. } |
            mir::TerminatorKind::Resume |
            mir::TerminatorKind::Return |
            mir::TerminatorKind::Unreachable |
            mir::TerminatorKind::Assert { .. } => {}
            mir::TerminatorKind::GeneratorDrop |
            mir::TerminatorKind::Yield { .. } |
            mir::TerminatorKind::FalseEdges { .. } => bug!(),
        }

        self.super_terminator_kind(block, kind, location);
    }

    fn visit_static(&mut self,
                    static_: &mir::Static<'tcx>,
                    context: mir::visit::PlaceContext<'tcx>,
                    location: Location) {
        debug!("visiting static {:?} @ {:?}", static_.def_id, location);

        let tcx = self.tcx;
        let instance = Instance::mono(tcx, static_.def_id);
        if should_monomorphize_locally(tcx, &instance) {
            let node_id = tcx.hir.as_local_node_id(static_.def_id).unwrap();
            self.output.push(MonoItem::Static(node_id));
        }

        self.super_static(static_, context, location);
    }
}

fn visit_drop_use<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                            ty: Ty<'tcx>,
                            is_direct_call: bool,
                            output: &mut Vec<MonoItem<'tcx>>)
{
    let instance = monomorphize::resolve_drop_in_place(tcx, ty);
    visit_instance_use(tcx, instance, is_direct_call, output);
}

fn visit_fn_use<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                          ty: Ty<'tcx>,
                          is_direct_call: bool,
                          output: &mut Vec<MonoItem<'tcx>>)
{
    if let ty::TyFnDef(def_id, substs) = ty.sty {
        let instance = ty::Instance::resolve(tcx,
                                             ty::ParamEnv::empty(traits::Reveal::All),
                                             def_id,
                                             substs).unwrap();
        visit_instance_use(tcx, instance, is_direct_call, output);
    }
}

fn visit_instance_use<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                instance: ty::Instance<'tcx>,
                                is_direct_call: bool,
                                output: &mut Vec<MonoItem<'tcx>>)
{
    debug!("visit_item_use({:?}, is_direct_call={:?})", instance, is_direct_call);
    if !should_monomorphize_locally(tcx, &instance) {
        return
    }

    match instance.def {
        ty::InstanceDef::Intrinsic(def_id) => {
            if !is_direct_call {
                bug!("intrinsic {:?} being reified", def_id);
            }
        }
        ty::InstanceDef::Virtual(..) |
        ty::InstanceDef::DropGlue(_, None) => {
            // don't need to emit shim if we are calling directly.
            if !is_direct_call {
                output.push(create_fn_mono_item(instance));
            }
        }
        ty::InstanceDef::DropGlue(_, Some(_)) => {
            output.push(create_fn_mono_item(instance));
        }
        ty::InstanceDef::ClosureOnceShim { .. } |
        ty::InstanceDef::Item(..) |
        ty::InstanceDef::FnPtrShim(..) |
        ty::InstanceDef::CloneShim(..) => {
            output.push(create_fn_mono_item(instance));
        }
    }
}

// Returns true if we should translate an instance in the local crate.
// Returns false if we can just link to the upstream crate and therefore don't
// need a mono item.
fn should_monomorphize_locally<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, instance: &Instance<'tcx>)
                                         -> bool {
    let def_id = match instance.def {
        ty::InstanceDef::Item(def_id) => def_id,
        ty::InstanceDef::ClosureOnceShim { .. } |
        ty::InstanceDef::Virtual(..) |
        ty::InstanceDef::FnPtrShim(..) |
        ty::InstanceDef::DropGlue(..) |
        ty::InstanceDef::Intrinsic(_) |
        ty::InstanceDef::CloneShim(..) => return true
    };
    match tcx.hir.get_if_local(def_id) {
        Some(hir_map::NodeForeignItem(..)) => {
            false // foreign items are linked against, not translated.
        }
        Some(_) => true,
        None => {
            if tcx.is_exported_symbol(def_id) ||
                tcx.is_foreign_item(def_id)
            {
                // We can link to the item in question, no instance needed
                // in this crate
                false
            } else {
                if !tcx.is_mir_available(def_id) {
                    bug!("Cannot create local mono-item for {:?}", def_id)
                }
                true
            }
        }
    }
}

/// For given pair of source and target type that occur in an unsizing coercion,
/// this function finds the pair of types that determines the vtable linking
/// them.
///
/// For example, the source type might be `&SomeStruct` and the target type\
/// might be `&SomeTrait` in a cast like:
///
/// let src: &SomeStruct = ...;
/// let target = src as &SomeTrait;
///
/// Then the output of this function would be (SomeStruct, SomeTrait) since for
/// constructing the `target` fat-pointer we need the vtable for that pair.
///
/// Things can get more complicated though because there's also the case where
/// the unsized type occurs as a field:
///
/// ```rust
/// struct ComplexStruct<T: ?Sized> {
///    a: u32,
///    b: f64,
///    c: T
/// }
/// ```
///
/// In this case, if `T` is sized, `&ComplexStruct<T>` is a thin pointer. If `T`
/// is unsized, `&SomeStruct` is a fat pointer, and the vtable it points to is
/// for the pair of `T` (which is a trait) and the concrete type that `T` was
/// originally coerced from:
///
/// let src: &ComplexStruct<SomeStruct> = ...;
/// let target = src as &ComplexStruct<SomeTrait>;
///
/// Again, we want this `find_vtable_types_for_unsizing()` to provide the pair
/// `(SomeStruct, SomeTrait)`.
///
/// Finally, there is also the case of custom unsizing coercions, e.g. for
/// smart pointers such as `Rc` and `Arc`.
fn find_vtable_types_for_unsizing<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                            source_ty: Ty<'tcx>,
                                            target_ty: Ty<'tcx>)
                                            -> (Ty<'tcx>, Ty<'tcx>) {
    let ptr_vtable = |inner_source: Ty<'tcx>, inner_target: Ty<'tcx>| {
        tcx.struct_lockstep_tails(inner_source, inner_target)
    };

    match (&source_ty.sty, &target_ty.sty) {
        (&ty::TyRef(_, ty::TypeAndMut { ty: a, .. }),
         &ty::TyRef(_, ty::TypeAndMut { ty: b, .. })) |
        (&ty::TyRef(_, ty::TypeAndMut { ty: a, .. }),
         &ty::TyRawPtr(ty::TypeAndMut { ty: b, .. })) |
        (&ty::TyRawPtr(ty::TypeAndMut { ty: a, .. }),
         &ty::TyRawPtr(ty::TypeAndMut { ty: b, .. })) => {
            ptr_vtable(a, b)
        }
        (&ty::TyAdt(def_a, _), &ty::TyAdt(def_b, _)) if def_a.is_box() && def_b.is_box() => {
            ptr_vtable(source_ty.boxed_ty(), target_ty.boxed_ty())
        }

        (&ty::TyAdt(source_adt_def, source_substs),
         &ty::TyAdt(target_adt_def, target_substs)) => {
            assert_eq!(source_adt_def, target_adt_def);

            let kind =
                monomorphize::custom_coerce_unsize_info(tcx, source_ty, target_ty);

            let coerce_index = match kind {
                CustomCoerceUnsized::Struct(i) => i
            };

            let source_fields = &source_adt_def.struct_variant().fields;
            let target_fields = &target_adt_def.struct_variant().fields;

            assert!(coerce_index < source_fields.len() &&
                    source_fields.len() == target_fields.len());

            find_vtable_types_for_unsizing(tcx,
                                           source_fields[coerce_index].ty(tcx,
                                                                          source_substs),
                                           target_fields[coerce_index].ty(tcx,
                                                                          target_substs))
        }
        _ => bug!("find_vtable_types_for_unsizing: invalid coercion {:?} -> {:?}",
                  source_ty,
                  target_ty)
    }
}

fn create_fn_mono_item<'a, 'tcx>(instance: Instance<'tcx>) -> MonoItem<'tcx> {
    debug!("create_fn_mono_item(instance={})", instance);
    MonoItem::Fn(instance)
}

/// Creates a `MonoItem` for each method that is referenced by the vtable for
/// the given trait/impl pair.
fn create_mono_items_for_vtable_methods<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                                  trait_ty: Ty<'tcx>,
                                                  impl_ty: Ty<'tcx>,
                                                  output: &mut Vec<MonoItem<'tcx>>) {
    assert!(!trait_ty.needs_subst() && !trait_ty.has_escaping_regions() &&
            !impl_ty.needs_subst() && !impl_ty.has_escaping_regions());

    if let ty::TyDynamic(ref trait_ty, ..) = trait_ty.sty {
        if let Some(principal) = trait_ty.principal() {
            let poly_trait_ref = principal.with_self_ty(tcx, impl_ty);
            assert!(!poly_trait_ref.has_escaping_regions());

            // Walk all methods of the trait, including those of its supertraits
            let methods = tcx.vtable_methods(poly_trait_ref);
            let methods = methods.iter().cloned().filter_map(|method| method)
                .map(|(def_id, substs)| ty::Instance::resolve(
                        tcx,
                        ty::ParamEnv::empty(traits::Reveal::All),
                        def_id,
                        substs).unwrap())
                .filter(|&instance| should_monomorphize_locally(tcx, &instance))
                .map(|instance| create_fn_mono_item(instance));
            output.extend(methods);
        }
        // Also add the destructor
        visit_drop_use(tcx, impl_ty, false, output);
    }
}

//=-----------------------------------------------------------------------------
// Root Collection
//=-----------------------------------------------------------------------------

struct RootCollector<'b, 'a: 'b, 'tcx: 'a + 'b> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    mode: MonoItemCollectionMode,
    output: &'b mut Vec<MonoItem<'tcx>>,
    entry_fn: Option<DefId>,
}

impl<'b, 'a, 'v> ItemLikeVisitor<'v> for RootCollector<'b, 'a, 'v> {
    fn visit_item(&mut self, item: &'v hir::Item) {
        match item.node {
            hir::ItemExternCrate(..) |
            hir::ItemUse(..)         |
            hir::ItemForeignMod(..)  |
            hir::ItemTy(..)          |
            hir::ItemAutoImpl(..) |
            hir::ItemTrait(..)       |
            hir::ItemMod(..)         => {
                // Nothing to do, just keep recursing...
            }

            hir::ItemImpl(..) => {
                if self.mode == MonoItemCollectionMode::Eager {
                    create_mono_items_for_default_impls(self.tcx,
                                                        item,
                                                        self.output);
                }
            }

            hir::ItemEnum(_, ref generics) |
            hir::ItemStruct(_, ref generics) |
            hir::ItemUnion(_, ref generics) => {
                if !generics.is_parameterized() {
                    if self.mode == MonoItemCollectionMode::Eager {
                        let def_id = self.tcx.hir.local_def_id(item.id);
                        debug!("RootCollector: ADT drop-glue for {}",
                               def_id_to_string(self.tcx, def_id));

                        let ty = Instance::new(def_id, Substs::empty()).ty(self.tcx);
                        visit_drop_use(self.tcx, ty, true, self.output);
                    }
                }
            }
            hir::ItemGlobalAsm(..) => {
                debug!("RootCollector: ItemGlobalAsm({})",
                       def_id_to_string(self.tcx,
                                        self.tcx.hir.local_def_id(item.id)));
                self.output.push(MonoItem::GlobalAsm(item.id));
            }
            hir::ItemStatic(..) => {
                debug!("RootCollector: ItemStatic({})",
                       def_id_to_string(self.tcx,
                                        self.tcx.hir.local_def_id(item.id)));
                self.output.push(MonoItem::Static(item.id));
            }
            hir::ItemConst(..) => {
                // const items only generate mono items if they are
                // actually used somewhere. Just declaring them is insufficient.
            }
            hir::ItemFn(..) => {
                let tcx = self.tcx;
                let def_id = tcx.hir.local_def_id(item.id);

                if self.is_root(def_id) {
                    debug!("RootCollector: ItemFn({})",
                           def_id_to_string(tcx, def_id));

                    let instance = Instance::mono(tcx, def_id);
                    self.output.push(MonoItem::Fn(instance));
                }
            }
        }
    }

    fn visit_trait_item(&mut self, _: &'v hir::TraitItem) {
        // Even if there's a default body with no explicit generics,
        // it's still generic over some `Self: Trait`, so not a root.
    }

    fn visit_impl_item(&mut self, ii: &'v hir::ImplItem) {
        match ii.node {
            hir::ImplItemKind::Method(hir::MethodSig { .. }, _) => {
                let tcx = self.tcx;
                let def_id = tcx.hir.local_def_id(ii.id);

                if self.is_root(def_id) {
                    debug!("RootCollector: MethodImplItem({})",
                           def_id_to_string(tcx, def_id));

                    let instance = Instance::mono(tcx, def_id);
                    self.output.push(MonoItem::Fn(instance));
                }
            }
            _ => { /* Nothing to do here */ }
        }
    }
}

impl<'b, 'a, 'v> RootCollector<'b, 'a, 'v> {
    fn is_root(&self, def_id: DefId) -> bool {
        !item_has_type_parameters(self.tcx, def_id) && match self.mode {
            MonoItemCollectionMode::Eager => {
                true
            }
            MonoItemCollectionMode::Lazy => {
                self.entry_fn == Some(def_id) ||
                self.tcx.is_exported_symbol(def_id) ||
                attr::contains_name(&self.tcx.get_attrs(def_id),
                                    "rustc_std_internal_symbol")
            }
        }
    }
}

fn item_has_type_parameters<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, def_id: DefId) -> bool {
    let generics = tcx.generics_of(def_id);
    generics.parent_types as usize + generics.types.len() > 0
}

fn create_mono_items_for_default_impls<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                                 item: &'tcx hir::Item,
                                                 output: &mut Vec<MonoItem<'tcx>>) {
    match item.node {
        hir::ItemImpl(_,
                      _,
                      _,
                      ref generics,
                      ..,
                      ref impl_item_refs) => {
            if generics.is_type_parameterized() {
                return
            }

            let impl_def_id = tcx.hir.local_def_id(item.id);

            debug!("create_mono_items_for_default_impls(item={})",
                   def_id_to_string(tcx, impl_def_id));

            if let Some(trait_ref) = tcx.impl_trait_ref(impl_def_id) {
                let callee_substs = tcx.erase_regions(&trait_ref.substs);
                let overridden_methods: FxHashSet<_> =
                    impl_item_refs.iter()
                                  .map(|iiref| iiref.name)
                                  .collect();
                for method in tcx.provided_trait_methods(trait_ref.def_id) {
                    if overridden_methods.contains(&method.name) {
                        continue;
                    }

                    if !tcx.generics_of(method.def_id).types.is_empty() {
                        continue;
                    }

                    let instance = ty::Instance::resolve(tcx,
                                                         ty::ParamEnv::empty(traits::Reveal::All),
                                                         method.def_id,
                                                         callee_substs).unwrap();

                    let mono_item = create_fn_mono_item(instance);
                    if mono_item.is_instantiable(tcx)
                        && should_monomorphize_locally(tcx, &instance) {
                        output.push(mono_item);
                    }
                }
            }
        }
        _ => {
            bug!()
        }
    }
}

/// Scan the MIR in order to find function calls, closures, and drop-glue
fn collect_neighbours<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                instance: Instance<'tcx>,
                                const_context: bool,
                                output: &mut Vec<MonoItem<'tcx>>)
{
    let mir = tcx.instance_mir(instance.def);

    let mut visitor = MirNeighborCollector {
        tcx,
        mir: &mir,
        output,
        param_substs: instance.substs,
        const_context,
    };

    visitor.visit_mir(&mir);
    for promoted in &mir.promoted {
        visitor.mir = promoted;
        visitor.visit_mir(promoted);
    }
}

fn def_id_to_string<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                              def_id: DefId)
                              -> String {
    let mut output = String::new();
    let printer = DefPathBasedNames::new(tcx, false, false);
    printer.push_def_path(def_id, &mut output);
    output
}
