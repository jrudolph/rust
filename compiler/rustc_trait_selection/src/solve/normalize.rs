use crate::traits::error_reporting::TypeErrCtxtExt;
use crate::traits::query::evaluate_obligation::InferCtxtExt;
use crate::traits::{BoundVarReplacer, PlaceholderReplacer};
use rustc_data_structures::stack::ensure_sufficient_stack;
use rustc_infer::infer::at::At;
use rustc_infer::infer::type_variable::{TypeVariableOrigin, TypeVariableOriginKind};
use rustc_infer::infer::InferCtxt;
use rustc_infer::traits::TraitEngineExt;
use rustc_infer::traits::{FulfillmentError, Obligation, TraitEngine};
use rustc_middle::infer::unify_key::{ConstVariableOrigin, ConstVariableOriginKind};
use rustc_middle::traits::ObligationCause;
use rustc_middle::ty::{self, AliasTy, Ty, TyCtxt, UniverseIndex};
use rustc_middle::ty::{FallibleTypeFolder, TypeFolder, TypeSuperFoldable};
use rustc_middle::ty::{TypeFoldable, TypeVisitableExt};

use super::FulfillmentCtxt;

/// Deeply normalize all aliases in `value`. This does not handle inference and expects
/// its input to be already fully resolved.
pub fn deeply_normalize<'tcx, T: TypeFoldable<TyCtxt<'tcx>>>(
    at: At<'_, 'tcx>,
    value: T,
) -> Result<T, Vec<FulfillmentError<'tcx>>> {
    assert!(!value.has_escaping_bound_vars());
    deeply_normalize_with_skipped_universes(at, value, vec![])
}

/// Deeply normalize all aliases in `value`. This does not handle inference and expects
/// its input to be already fully resolved.
///
/// Additionally takes a list of universes which represents the binders which have been
/// entered before passing `value` to the function. This is currently needed for
/// `normalize_erasing_regions`, which skips binders as it walks through a type.
pub fn deeply_normalize_with_skipped_universes<'tcx, T: TypeFoldable<TyCtxt<'tcx>>>(
    at: At<'_, 'tcx>,
    value: T,
    universes: Vec<Option<UniverseIndex>>,
) -> Result<T, Vec<FulfillmentError<'tcx>>> {
    let fulfill_cx = FulfillmentCtxt::new(at.infcx);
    let mut folder = NormalizationFolder { at, fulfill_cx, depth: 0, universes };

    value.try_fold_with(&mut folder)
}

struct NormalizationFolder<'me, 'tcx> {
    at: At<'me, 'tcx>,
    fulfill_cx: FulfillmentCtxt<'tcx>,
    depth: usize,
    universes: Vec<Option<UniverseIndex>>,
}

impl<'tcx> NormalizationFolder<'_, 'tcx> {
    fn normalize_alias_ty(
        &mut self,
        alias_ty: Ty<'tcx>,
    ) -> Result<Ty<'tcx>, Vec<FulfillmentError<'tcx>>> {
        assert!(matches!(alias_ty.kind(), ty::Alias(..)));

        let infcx = self.at.infcx;
        let tcx = infcx.tcx;
        let recursion_limit = tcx.recursion_limit();
        if !recursion_limit.value_within_limit(self.depth) {
            self.at.infcx.err_ctxt().report_overflow_error(
                &alias_ty,
                self.at.cause.span,
                true,
                |_| {},
            );
        }

        self.depth += 1;

        let new_infer_ty = infcx.next_ty_var(TypeVariableOrigin {
            kind: TypeVariableOriginKind::NormalizeProjectionType,
            span: self.at.cause.span,
        });
        let obligation = Obligation::new(
            tcx,
            self.at.cause.clone(),
            self.at.param_env,
            ty::PredicateKind::AliasRelate(
                alias_ty.into(),
                new_infer_ty.into(),
                ty::AliasRelationDirection::Equate,
            ),
        );

        self.fulfill_cx.register_predicate_obligation(infcx, obligation);
        let errors = self.fulfill_cx.select_all_or_error(infcx);
        if !errors.is_empty() {
            return Err(errors);
        }

        // Alias is guaranteed to be fully structurally resolved,
        // so we can super fold here.
        let ty = infcx.resolve_vars_if_possible(new_infer_ty);
        let result = ty.try_super_fold_with(self)?;
        self.depth -= 1;
        Ok(result)
    }

    fn normalize_unevaluated_const(
        &mut self,
        ty: Ty<'tcx>,
        uv: ty::UnevaluatedConst<'tcx>,
    ) -> Result<ty::Const<'tcx>, Vec<FulfillmentError<'tcx>>> {
        let infcx = self.at.infcx;
        let tcx = infcx.tcx;
        let recursion_limit = tcx.recursion_limit();
        if !recursion_limit.value_within_limit(self.depth) {
            self.at.infcx.err_ctxt().report_overflow_error(
                &ty::Const::new_unevaluated(tcx, uv, ty),
                self.at.cause.span,
                true,
                |_| {},
            );
        }

        self.depth += 1;

        let new_infer_ct = infcx.next_const_var(
            ty,
            ConstVariableOrigin {
                kind: ConstVariableOriginKind::MiscVariable,
                span: self.at.cause.span,
            },
        );
        let obligation = Obligation::new(
            tcx,
            self.at.cause.clone(),
            self.at.param_env,
            ty::NormalizesTo {
                alias: AliasTy::new(tcx, uv.def, uv.args),
                term: new_infer_ct.into(),
            },
        );

        let result = if infcx.predicate_may_hold(&obligation) {
            self.fulfill_cx.register_predicate_obligation(infcx, obligation);
            let errors = self.fulfill_cx.select_all_or_error(infcx);
            if !errors.is_empty() {
                return Err(errors);
            }
            let ct = infcx.resolve_vars_if_possible(new_infer_ct);
            ct.try_fold_with(self)?
        } else {
            ty::Const::new_unevaluated(tcx, uv, ty).try_super_fold_with(self)?
        };

        self.depth -= 1;
        Ok(result)
    }
}

impl<'tcx> FallibleTypeFolder<TyCtxt<'tcx>> for NormalizationFolder<'_, 'tcx> {
    type Error = Vec<FulfillmentError<'tcx>>;

    fn interner(&self) -> TyCtxt<'tcx> {
        self.at.infcx.tcx
    }

    fn try_fold_binder<T: TypeFoldable<TyCtxt<'tcx>>>(
        &mut self,
        t: ty::Binder<'tcx, T>,
    ) -> Result<ty::Binder<'tcx, T>, Self::Error> {
        self.universes.push(None);
        let t = t.try_super_fold_with(self)?;
        self.universes.pop();
        Ok(t)
    }

    #[instrument(level = "debug", skip(self), ret)]
    fn try_fold_ty(&mut self, ty: Ty<'tcx>) -> Result<Ty<'tcx>, Self::Error> {
        let infcx = self.at.infcx;
        debug_assert_eq!(ty, infcx.shallow_resolve(ty));
        if !ty.has_projections() {
            return Ok(ty);
        }

        let ty::Alias(..) = *ty.kind() else { return ty.try_super_fold_with(self) };

        if ty.has_escaping_bound_vars() {
            let (ty, mapped_regions, mapped_types, mapped_consts) =
                BoundVarReplacer::replace_bound_vars(infcx, &mut self.universes, ty);
            let result = ensure_sufficient_stack(|| self.normalize_alias_ty(ty))?;
            Ok(PlaceholderReplacer::replace_placeholders(
                infcx,
                mapped_regions,
                mapped_types,
                mapped_consts,
                &self.universes,
                result,
            ))
        } else {
            ensure_sufficient_stack(|| self.normalize_alias_ty(ty))
        }
    }

    #[instrument(level = "debug", skip(self), ret)]
    fn try_fold_const(&mut self, ct: ty::Const<'tcx>) -> Result<ty::Const<'tcx>, Self::Error> {
        let infcx = self.at.infcx;
        debug_assert_eq!(ct, infcx.shallow_resolve(ct));
        if !ct.has_projections() {
            return Ok(ct);
        }

        let uv = match ct.kind() {
            ty::ConstKind::Unevaluated(ct) => ct,
            _ => return ct.try_super_fold_with(self),
        };

        if uv.has_escaping_bound_vars() {
            let (uv, mapped_regions, mapped_types, mapped_consts) =
                BoundVarReplacer::replace_bound_vars(infcx, &mut self.universes, uv);
            let result = ensure_sufficient_stack(|| self.normalize_unevaluated_const(ct.ty(), uv))?;
            Ok(PlaceholderReplacer::replace_placeholders(
                infcx,
                mapped_regions,
                mapped_types,
                mapped_consts,
                &self.universes,
                result,
            ))
        } else {
            ensure_sufficient_stack(|| self.normalize_unevaluated_const(ct.ty(), uv))
        }
    }
}

// Deeply normalize a value and return it
pub(crate) fn deeply_normalize_for_diagnostics<'tcx, T: TypeFoldable<TyCtxt<'tcx>>>(
    infcx: &InferCtxt<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    t: T,
) -> T {
    t.fold_with(&mut DeeplyNormalizeForDiagnosticsFolder {
        at: infcx.at(&ObligationCause::dummy(), param_env),
    })
}

struct DeeplyNormalizeForDiagnosticsFolder<'a, 'tcx> {
    at: At<'a, 'tcx>,
}

impl<'tcx> TypeFolder<TyCtxt<'tcx>> for DeeplyNormalizeForDiagnosticsFolder<'_, 'tcx> {
    fn interner(&self) -> TyCtxt<'tcx> {
        self.at.infcx.tcx
    }

    fn fold_ty(&mut self, ty: Ty<'tcx>) -> Ty<'tcx> {
        deeply_normalize_with_skipped_universes(
            self.at,
            ty,
            vec![None; ty.outer_exclusive_binder().as_usize()],
        )
        .unwrap_or_else(|_| ty.super_fold_with(self))
    }

    fn fold_const(&mut self, ct: ty::Const<'tcx>) -> ty::Const<'tcx> {
        deeply_normalize_with_skipped_universes(
            self.at,
            ct,
            vec![None; ct.outer_exclusive_binder().as_usize()],
        )
        .unwrap_or_else(|_| ct.super_fold_with(self))
    }
}
