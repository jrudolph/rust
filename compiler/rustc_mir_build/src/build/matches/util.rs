use crate::build::expr::as_place::PlaceBase;
use crate::build::expr::as_place::PlaceBuilder;
use crate::build::matches::MatchPair;
use crate::build::Builder;
use rustc_middle::mir::*;
use rustc_middle::thir::*;
use rustc_middle::ty;
use rustc_middle::ty::TypeVisitableExt;

impl<'a, 'tcx> Builder<'a, 'tcx> {
    pub(crate) fn field_match_pairs<'pat>(
        &mut self,
        place: PlaceBuilder<'tcx>,
        subpatterns: &'pat [FieldPat<'tcx>],
    ) -> Vec<MatchPair<'pat, 'tcx>> {
        subpatterns
            .iter()
            .map(|fieldpat| {
                let place =
                    place.clone_project(PlaceElem::Field(fieldpat.field, fieldpat.pattern.ty));
                MatchPair::new(place, &fieldpat.pattern, self)
            })
            .collect()
    }

    pub(crate) fn prefix_slice_suffix<'pat>(
        &mut self,
        match_pairs: &mut Vec<MatchPair<'pat, 'tcx>>,
        place: &PlaceBuilder<'tcx>,
        prefix: &'pat [Box<Pat<'tcx>>],
        opt_slice: &'pat Option<Box<Pat<'tcx>>>,
        suffix: &'pat [Box<Pat<'tcx>>],
    ) {
        let tcx = self.tcx;
        let (min_length, exact_size) = if let Some(place_resolved) = place.try_to_place(self) {
            match place_resolved.ty(&self.local_decls, tcx).ty.kind() {
                ty::Array(_, length) => (length.eval_target_usize(tcx, self.param_env), true),
                _ => ((prefix.len() + suffix.len()).try_into().unwrap(), false),
            }
        } else {
            ((prefix.len() + suffix.len()).try_into().unwrap(), false)
        };

        match_pairs.extend(prefix.iter().enumerate().map(|(idx, subpattern)| {
            let elem =
                ProjectionElem::ConstantIndex { offset: idx as u64, min_length, from_end: false };
            MatchPair::new(place.clone_project(elem), subpattern, self)
        }));

        if let Some(subslice_pat) = opt_slice {
            let suffix_len = suffix.len() as u64;
            let subslice = place.clone_project(PlaceElem::Subslice {
                from: prefix.len() as u64,
                to: if exact_size { min_length - suffix_len } else { suffix_len },
                from_end: !exact_size,
            });
            match_pairs.push(MatchPair::new(subslice, subslice_pat, self));
        }

        match_pairs.extend(suffix.iter().rev().enumerate().map(|(idx, subpattern)| {
            let end_offset = (idx + 1) as u64;
            let elem = ProjectionElem::ConstantIndex {
                offset: if exact_size { min_length - end_offset } else { end_offset },
                min_length,
                from_end: !exact_size,
            };
            let place = place.clone_project(elem);
            MatchPair::new(place, subpattern, self)
        }));
    }

    /// Creates a false edge to `imaginary_target` and a real edge to
    /// real_target. If `imaginary_target` is none, or is the same as the real
    /// target, a Goto is generated instead to simplify the generated MIR.
    pub(crate) fn false_edges(
        &mut self,
        from_block: BasicBlock,
        real_target: BasicBlock,
        imaginary_target: Option<BasicBlock>,
        source_info: SourceInfo,
    ) {
        match imaginary_target {
            Some(target) if target != real_target => {
                self.cfg.terminate(
                    from_block,
                    source_info,
                    TerminatorKind::FalseEdge { real_target, imaginary_target: target },
                );
            }
            _ => self.cfg.goto(from_block, source_info, real_target),
        }
    }
}

impl<'pat, 'tcx> MatchPair<'pat, 'tcx> {
    pub(in crate::build) fn new(
        mut place: PlaceBuilder<'tcx>,
        pattern: &'pat Pat<'tcx>,
        cx: &mut Builder<'_, 'tcx>,
    ) -> MatchPair<'pat, 'tcx> {
        // Force the place type to the pattern's type.
        // FIXME(oli-obk): can we use this to simplify slice/array pattern hacks?
        if let Some(resolved) = place.resolve_upvar(cx) {
            place = resolved;
        }

        // Only add the OpaqueCast projection if the given place is an opaque type and the
        // expected type from the pattern is not.
        let may_need_cast = match place.base() {
            PlaceBase::Local(local) => {
                let ty = Place::ty_from(local, place.projection(), &cx.local_decls, cx.tcx).ty;
                ty != pattern.ty && ty.has_opaque_types()
            }
            _ => true,
        };
        if may_need_cast {
            place = place.project(ProjectionElem::OpaqueCast(pattern.ty));
        }

        let mut subpairs = Vec::new();
        match pattern.kind {
            PatKind::Constant { .. }
            | PatKind::Range(_)
            | PatKind::Or { .. }
            | PatKind::Never
            | PatKind::Wild
            | PatKind::Error(_) => {}

            PatKind::AscribeUserType { ref subpattern, .. } => {
                subpairs.push(MatchPair::new(place.clone(), subpattern, cx));
            }

            PatKind::Binding { ref subpattern, .. } => {
                if let Some(subpattern) = subpattern.as_ref() {
                    // this is the `x @ P` case; have to keep matching against `P` now
                    subpairs.push(MatchPair::new(place.clone(), subpattern, cx));
                }
            }

            PatKind::InlineConstant { subpattern: ref pattern, .. } => {
                subpairs.push(MatchPair::new(place.clone(), pattern, cx));
            }

            PatKind::Slice { ref prefix, ref slice, ref suffix }
            | PatKind::Array { ref prefix, ref slice, ref suffix } => {
                cx.prefix_slice_suffix(&mut subpairs, &place, prefix, slice, suffix);
            }

            PatKind::Variant { adt_def, variant_index, ref subpatterns, .. } => {
                let downcast_place = place.clone().downcast(adt_def, variant_index); // `(x as Variant)`
                subpairs = cx.field_match_pairs(downcast_place, subpatterns);
            }

            PatKind::Leaf { ref subpatterns } => {
                subpairs = cx.field_match_pairs(place.clone(), subpatterns);
            }

            PatKind::Deref { ref subpattern } => {
                let place_builder = place.clone().deref();
                subpairs.push(MatchPair::new(place_builder, subpattern, cx));
            }
        }

        MatchPair { place, pattern, subpairs }
    }
}
