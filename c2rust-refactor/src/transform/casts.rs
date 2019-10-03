use rustc::ty::{self, TyKind, ParamEnv};
use syntax::ast::*;
use syntax::ptr::P;

use c2rust_ast_builder::mk;
use crate::command::{CommandState, Registry};
use crate::driver::Phase;
use crate::matcher::{MatchCtxt, mut_visit_match_with, replace_expr};
use crate::transform::Transform;
use crate::RefactorCtxt;

#[cfg(test)]
mod tests;

/// # `remove_redundant_casts` Command
///
/// Usage: `remove_redundant_casts`
///
/// Removes all casts of the form `$e as $t` where the expression already has the `$t` type,
/// and double casts like `$e as $t1 as $t2` where the inner cast is redundant.
pub struct RemoveRedundantCasts;

impl Transform for RemoveRedundantCasts {
    fn transform(&self, krate: &mut Crate, st: &CommandState, cx: &RefactorCtxt) {
        let tcx = cx.ty_ctxt();
        let mut mcx = MatchCtxt::new(st, cx);
        let pat = mcx.parse_expr("$oe:Expr as $ot:Ty");
        mut_visit_match_with(mcx, pat, krate, |ast, mcx| {
            let oe = mcx.bindings.get::<_, P<Expr>>("$oe").unwrap();
            let oe_ty = cx.adjusted_node_type(oe.id);
            let oe_ty = tcx.normalize_erasing_regions(ParamEnv::empty(), oe_ty);

            let ot = mcx.bindings.get::<_, P<Ty>>("$ot").unwrap();
            let ot_ty = cx.adjusted_node_type(ot.id);
            let ot_ty = tcx.normalize_erasing_regions(ParamEnv::empty(), ot_ty);
            if oe_ty == ot_ty {
                *ast = oe.clone();
                return;
            }

            match oe.node {
                ExprKind::Cast(ref ie, ref it) => {
                    // Found a double cast
                    let ie_ty = cx.adjusted_node_type(ie.id);
                    let ie_ty = tcx.normalize_erasing_regions(ParamEnv::empty(), ie_ty);

                    let it_ty = cx.adjusted_node_type(it.id);
                    let it_ty = tcx.normalize_erasing_regions(ParamEnv::empty(), it_ty);
                    assert!(it_ty != ot_ty);

                    match check_double_cast(ie_ty.into(), it_ty.into(), ot_ty.into()) {
                        DoubleCastAction::RemoveBoth => {
                            *ast = ie.clone();
                        }
                        DoubleCastAction::RemoveInner => {
                            // Rewrite to `$ie as $ot`, removing the inner cast
                            *ast = mk().cast_expr(ie, ot);
                        }
                        DoubleCastAction::KeepBoth => { }
                    }
                }

                ExprKind::Lit(ref lit) => {
                    // `X_ty1 as ty2` => `X_ty2`
                    let new_lit = replace_suffix(lit, ot_ty);
                    if let Some(nl) = new_lit {
                        let new_expr = mk().lit_expr(nl);
                        let ast_const = eval_const(ast.clone(), cx);
                        let new_const = eval_const(new_expr.clone(), cx);
                        debug!("checking {:?} == {:?}: {:?} == {:?}",
                               *ast, new_expr, ast_const, new_const);
                        if new_const.is_some() && new_const == ast_const {
                            *ast = new_expr;
                            return;
                        }
                    }
                }

                ExprKind::Unary(UnOp::Neg, ref expr) => match expr.node {
                    ExprKind::Lit(ref lit) => {
                        // `-X_ty1 as ty2` => `-X_ty2`
                        let new_lit = replace_suffix(lit, ot_ty);
                        if let Some(nl) = new_lit {
                            let new_expr = mk().unary_expr(UnOp::Neg, mk().lit_expr(nl));
                            let ast_const = eval_const(ast.clone(), cx);
                            let new_const = eval_const(new_expr.clone(), cx);
                            debug!("checking {:?} == {:?}: {:?} == {:?}",
                                   *ast, new_expr, ast_const, new_const);
                            if new_const.is_some() && new_const == ast_const {
                                *ast = new_expr;
                                return;
                            }
                        }
                    }
                    _ => {}
                }

                // TODO: unary/binaryop op + cast, e.g., `(x as i32 + y as i32) as i8`
                _ => {}
            }
        })
    }

    fn min_phase(&self) -> Phase {
        Phase::Phase3
    }
}

enum DoubleCastAction {
    RemoveBoth,
    RemoveInner,
    KeepBoth,
}

// Check and decide what to do for a double-cast, e.g., `$e as $ty1 as $ty2`
fn check_double_cast<'tcx>(
    e_ty: SimpleTy,
    t1_ty: SimpleTy,
    t2_ty: SimpleTy,
) -> DoubleCastAction {
    // WARNING!!! This set of operations is verified for soundness
    // using Z3. If you make any changes, please re-run the verifier using
    // `cargo test --package c2rust-refactor`
    use CastKind::*;
    let inner_cast = cast_kind(e_ty, t1_ty);
    let outer_cast = cast_kind(t1_ty, t2_ty);
    match (inner_cast, outer_cast) {
        // 2 consecutive sign flips or extend-truncate
        // back to the same original type
        (SameWidth, SameWidth) |
        (Extend(_), Truncate) if e_ty == t2_ty => DoubleCastAction::RemoveBoth,

        (Extend(_), Extend(s)) |
        (SameWidth, Extend(s)) |
        (SameWidth, FromPointer(s)) |
        (SameWidth, ToPointer(s)) if s == e_ty.is_signed() => DoubleCastAction::RemoveInner,

        (_, SameWidth) | (_, Truncate) => DoubleCastAction::RemoveInner,

        _ => DoubleCastAction::KeepBoth
    }
}

enum CastKind {
    Extend(bool),
    Truncate,
    SameWidth,
    FromPointer(bool),
    ToPointer(bool),
    Unknown,
}

fn cast_kind(from_ty: SimpleTy, to_ty: SimpleTy) -> CastKind {
    use SimpleTy::*;
    match (from_ty, to_ty) {
        (Int(fw, fs), Int(tw, _)) if fw < tw => CastKind::Extend(fs),
        (Int(fw, _), Int(tw, _)) if fw > tw => CastKind::Truncate,
        (Int(..), Int(..)) => CastKind::SameWidth,

        // Into size/pointer
        (Int(fw, fs), Size(_)) |
        (Int(fw, fs), Pointer) if fw <= 16 => CastKind::Extend(fs),
        (Int(fw, _), Size(_)) |
        (Int(fw, _), Pointer) if fw >= 64 => CastKind::Truncate,
        (Int(..), Size(ts)) => CastKind::ToPointer(ts),
        (Int(..), Pointer) => CastKind::ToPointer(false),

        // From size/pointer
        (Size(fs), Int(tw, _)) if tw >= 64 => CastKind::Extend(fs),
        (Pointer, Int(tw, _)) if tw >= 64 => CastKind::Extend(false),
        (Size(_), Int(tw, _)) |
        (Pointer, Int(tw, _)) if tw <= 16 => CastKind::Truncate,
        (Size(fs), Int(..)) => CastKind::FromPointer(fs),
        (Pointer, Int(..)) => CastKind::FromPointer(false),

        // Pointer-to-size and vice versa
        (Pointer, Pointer) |
        (Pointer, Size(_)) |
        (Size(_), Pointer) |
        (Size(_), Size(_)) => CastKind::SameWidth,

        (Float32, Float32) => CastKind::SameWidth,
        (Float32, Float64) => CastKind::Extend(true),
        (Float64, Float32) => CastKind::Truncate,
        (Float64, Float64) => CastKind::SameWidth,

        //// Any integer that fits into sign+mantissa is getting extended
        //// TODO: these require a Z3 bitwise simulation for the conversions
        //(Int(fw, fs), Float32) if fw <= 23 => CastKind::Extend(fs),
        //(Int(fw, fs), Float64) if fw <= 52 => CastKind::Extend(fs),
        //(Int(..), Float32) => CastKind::Truncate,
        //(Int(..), Float64) => CastKind::Truncate,

        (_, _) => CastKind::Unknown,
    }
}

// We need to lower `ty::Ty` into our own `SimpleTy`
// because the unit tests have no way of creating new `TyS` values
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum SimpleTy {
    Int(usize, bool),
    Size(bool),
    Float32,
    Float64,
    Pointer,
    Other,
}

impl SimpleTy {
    fn is_signed(&self) -> bool {
        match self {
            SimpleTy::Int(_, s) => *s,
            SimpleTy::Size(s) => *s,
            SimpleTy::Float32 => true,
            SimpleTy::Float64 => true,
            _ => false,
        }
    }
}

impl<'tcx> From<ty::Ty<'tcx>> for SimpleTy {
    fn from(ty: ty::Ty<'tcx>) -> Self {
        use SimpleTy::*;
        match ty.sty {
            TyKind::Int(IntTy::Isize) => Size(true),
            TyKind::Uint(UintTy::Usize) => Size(false),

            TyKind::Int(int_ty) => Int(int_ty.bit_width().unwrap(), true),
            TyKind::Uint(uint_ty) => Int(uint_ty.bit_width().unwrap(), false),

            TyKind::Float(FloatTy::F32) => Float32,
            TyKind::Float(FloatTy::F64) => Float64,

            TyKind::RawPtr(_) |
            TyKind::Ref(..) |
            TyKind::FnPtr(_) => Pointer,

            _ => Other,
        }
    }
}

fn replace_suffix<'tcx>(lit: &Lit, ty: ty::Ty<'tcx>) -> Option<Lit> {
    match (&lit.node, &ty.sty) {
        // Very conservative approach: only convert to `isize`/`usize`
        // if the value fits in a 16-bit value
        (LitKind::Int(i, _), TyKind::Int(int_ty @ IntTy::Isize))
            if *i <= i16::max_value() as u128 => {
            Some(mk().int_lit(*i, *int_ty))
        }

        (LitKind::Int(i, _), TyKind::Int(int_ty @ IntTy::I8))
            if *i <= i8::max_value() as u128 => {
            Some(mk().int_lit(*i, *int_ty))
        }

        (LitKind::Int(i, _), TyKind::Int(int_ty @ IntTy::I16))
            if *i <= i16::max_value() as u128 => {
            Some(mk().int_lit(*i, *int_ty))
        }

        (LitKind::Int(i, _), TyKind::Int(int_ty @ IntTy::I32))
            if *i <= i32::max_value() as u128 => {
            Some(mk().int_lit(*i, *int_ty))
        }

        (LitKind::Int(i, _), TyKind::Int(int_ty @ IntTy::I64))
            if *i <= i64::max_value() as u128 => {
            Some(mk().int_lit(*i, *int_ty))
        }

        (LitKind::Int(i, _), TyKind::Int(int_ty @ IntTy::I128))
            if *i <= i128::max_value() as u128 => {
            Some(mk().int_lit(*i, *int_ty))
        }

        (LitKind::Int(i, _), TyKind::Uint(uint_ty @ UintTy::Usize))
            if *i <= u16::max_value() as u128 => {
            Some(mk().int_lit(*i, *uint_ty))
        }

        (LitKind::Int(i, _), TyKind::Uint(uint_ty @ UintTy::U8))
            if *i <= u8::max_value() as u128 => {
            Some(mk().int_lit(*i, *uint_ty))
        }

        (LitKind::Int(i, _), TyKind::Uint(uint_ty @ UintTy::U16))
            if *i <= u16::max_value() as u128 => {
            Some(mk().int_lit(*i, *uint_ty))
        }

        (LitKind::Int(i, _), TyKind::Uint(uint_ty @ UintTy::U32))
            if *i <= u32::max_value() as u128 => {
            Some(mk().int_lit(*i, *uint_ty))
        }

        (LitKind::Int(i, _), TyKind::Uint(uint_ty @ UintTy::U64))
            if *i <= u64::max_value() as u128 => {
            Some(mk().int_lit(*i, *uint_ty))
        }

        (LitKind::Int(i, _), TyKind::Uint(uint_ty @ UintTy::U128)) => {
            Some(mk().int_lit(*i, *uint_ty))
        }

        (LitKind::Int(i, _), TyKind::Float(ref float_ty)) => {
            Some(mk().float_lit(i.to_string(), float_ty))
        }

        (LitKind::Float(f, FloatTy::F32), TyKind::Int(ref int_ty)) => {
            let fv = f.as_str().parse::<f32>().ok()?;
            Some(mk().int_lit(fv as u128, *int_ty))
        }

        (LitKind::Float(f, FloatTy::F64), TyKind::Int(ref int_ty)) |
        (LitKind::FloatUnsuffixed(f), TyKind::Int(ref int_ty)) => {
            let fv = f.as_str().parse::<f64>().ok()?;
            Some(mk().int_lit(fv as u128, *int_ty))
        }

        (LitKind::Float(f, FloatTy::F32), TyKind::Uint(ref uint_ty)) => {
            let fv = f.as_str().parse::<f32>().ok()?;
            Some(mk().int_lit(fv as u128, *uint_ty))
        }

        (LitKind::Float(f, FloatTy::F64), TyKind::Uint(ref uint_ty)) |
        (LitKind::FloatUnsuffixed(f), TyKind::Uint(ref uint_ty)) => {
            let fv = f.as_str().parse::<f64>().ok()?;
            Some(mk().int_lit(fv as u128, *uint_ty))
        }

        (LitKind::Float(f, FloatTy::F32), TyKind::Float(ref float_ty)) => {
            let fv = f.as_str().parse::<f32>().ok()?;
            Some(mk().float_lit(fv.to_string(), float_ty))
        }

        (LitKind::Float(f, FloatTy::F64), TyKind::Float(ref float_ty)) |
        (LitKind::FloatUnsuffixed(f), TyKind::Float(ref float_ty)) => {
            let fv = f.as_str().parse::<f64>().ok()?;
            Some(mk().float_lit(fv.to_string(), float_ty))
        }

        _ => None
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum ConstantValue {
    Int(i128),
    Uint(u128),
    Float32(f32),
    Float64(f64),
}

impl ConstantValue {
    fn as_ty<'tcx>(self, ty: ty::Ty<'tcx>) -> Self {
        use ConstantValue::*;
        macro_rules! int_matches {
            ($($ty_kind:ident($int_ty:path) => $const_ty:ident[$($as_ty:ty),*]),*) => {
                match (self, &ty.sty) {
                    $(
                        (Int(v), TyKind::$ty_kind($int_ty)) => return $const_ty(v $(as $as_ty)*),
                        (Uint(v), TyKind::$ty_kind($int_ty)) => return $const_ty(v $(as $as_ty)*),
                        (Float32(v), TyKind::$ty_kind($int_ty)) => return $const_ty(v $(as $as_ty)*),
                        (Float64(v), TyKind::$ty_kind($int_ty)) => return $const_ty(v $(as $as_ty)*),
                     )*
                    _ => {}
                }
            }
        };
        int_matches!{
            Int(IntTy::Isize) => Int[i16, i128],
            Int(IntTy::I8) => Int[i8, i128],
            Int(IntTy::I16) => Int[i16, i128],
            Int(IntTy::I32) => Int[i32, i128],
            Int(IntTy::I64) => Int[i64, i128],
            Int(IntTy::I128) => Int[i128],
            Uint(UintTy::Usize) => Uint[u16, u128],
            Uint(UintTy::U8) => Uint[u8, u128],
            Uint(UintTy::U16) => Uint[u16, u128],
            Uint(UintTy::U32) => Uint[u32, u128],
            Uint(UintTy::U64) => Uint[u64, u128],
            Uint(UintTy::U128) => Uint[u128]
        };
        match (self, &ty.sty) {
            (Int(v), TyKind::Float(FloatTy::F32)) => Float32(v as f32),
            (Int(v), TyKind::Float(FloatTy::F64)) => Float64(v as f64),
            (Uint(v), TyKind::Float(FloatTy::F32)) => Float32(v as f32),
            (Uint(v), TyKind::Float(FloatTy::F64)) => Float64(v as f64),
            (Float32(_), TyKind::Float(FloatTy::F32)) => self,
            (Float32(v), TyKind::Float(FloatTy::F64)) => Float64(v as f64),
            (Float64(v), TyKind::Float(FloatTy::F32)) => Float32(v as f32),
            (Float64(_), TyKind::Float(FloatTy::F64)) => self,
            _ => unreachable!("Unexpected Ty")
        }
    }
}

fn eval_const<'tcx>(e: P<Expr>, cx: &RefactorCtxt) -> Option<ConstantValue> {
    match e.node {
        ExprKind::Lit(ref lit) => {
            match lit.node {
                LitKind::Int(i, LitIntType::Unsuffixed) => {
                    Some(ConstantValue::Uint(i))
                }

                LitKind::Int(i, LitIntType::Signed(IntTy::Isize)) => {
                    Some(ConstantValue::Int(i as i16 as i128))
                }

                LitKind::Int(i, LitIntType::Signed(IntTy::I8)) => {
                    Some(ConstantValue::Int(i as i8 as i128))
                }

                LitKind::Int(i, LitIntType::Signed(IntTy::I16)) => {
                    Some(ConstantValue::Int(i as i16 as i128))
                }

                LitKind::Int(i, LitIntType::Signed(IntTy::I32)) => {
                    Some(ConstantValue::Int(i as i32 as i128))
                }

                LitKind::Int(i, LitIntType::Signed(IntTy::I64)) => {
                    Some(ConstantValue::Int(i as i64 as i128))
                }

                LitKind::Int(i, LitIntType::Signed(IntTy::I128)) => {
                    Some(ConstantValue::Int(i as i128))
                }

                LitKind::Int(i, LitIntType::Unsigned(UintTy::Usize)) => {
                    Some(ConstantValue::Uint(i as u16 as u128))
                }

                LitKind::Int(i, LitIntType::Unsigned(UintTy::U8)) => {
                    Some(ConstantValue::Uint(i as u8 as u128))
                }

                LitKind::Int(i, LitIntType::Unsigned(UintTy::U16)) => {
                    Some(ConstantValue::Uint(i as u16 as u128))
                }

                LitKind::Int(i, LitIntType::Unsigned(UintTy::U32)) => {
                    Some(ConstantValue::Uint(i as u32 as u128))
                }

                LitKind::Int(i, LitIntType::Unsigned(UintTy::U64)) => {
                    Some(ConstantValue::Uint(i as u64 as u128))
                }

                LitKind::Int(i, LitIntType::Unsigned(UintTy::U128)) => {
                    Some(ConstantValue::Uint(i as u128))
                }

                LitKind::Float(f, FloatTy::F32) => {
                    let fv = f.as_str().parse::<f32>().ok()?;
                    Some(ConstantValue::Float32(fv))
                }

                LitKind::Float(f, FloatTy::F64) |
                LitKind::FloatUnsuffixed(f) => {
                    let fv = f.as_str().parse::<f64>().ok()?;
                    Some(ConstantValue::Float64(fv))
                }

                // TODO: Byte
                // TODO: Char
                _ => None
            }
        }

        ExprKind::Unary(UnOp::Neg, ref ie) => {
            let ic = eval_const(ie.clone(), cx)?;
            use ConstantValue::*;
            match ic {
                // Check for overflow for Uint
                Uint(i) if i > (i128::max_value() as u128) => None,
                Uint(i) => Some(Int(-(i as i128))),

                Int(i) => Some(Int(-i)),
                Float32(f) => Some(Float32(-f)),
                Float64(f) => Some(Float64(-f)),
            }
        }

        ExprKind::Cast(ref ie, ref ty) => {
            let tcx = cx.ty_ctxt();
            let ty_ty = cx.adjusted_node_type(ty.id);
            let ty_ty = tcx.normalize_erasing_regions(ParamEnv::empty(), ty_ty);
            let ic = eval_const(ie.clone(), cx)?;
            Some(ic.as_ty(ty_ty))
        }

        _ => unreachable!("Unexpected ExprKind")
    }
}

/// # `convert_cast_as_ptr` Command
///
/// Usage: `convert_cast_as_ptr`
///
/// Converts all expressions like `$e as *const $t` (with mutable or const pointers)
/// where `$e` is a slice or array into `$e.as_ptr()` calls.
pub struct ConvertCastAsPtr;

impl Transform for ConvertCastAsPtr {
    fn transform(&self, krate: &mut Crate, st: &CommandState, cx: &RefactorCtxt) {
        replace_expr(st, cx, krate,
            "typed!($expr:Expr, &[$ty:Ty]) as *const $ty",
            "$expr.as_ptr()");
        replace_expr(st, cx, krate,
            "typed!($expr:Expr, &[$ty:Ty]) as *mut $ty",
            "$expr.as_mut_ptr()");
        replace_expr(st, cx, krate,
            "typed!($expr:Expr, &[$ty:Ty; $len]) as *const $ty",
            "$expr.as_ptr()");
        replace_expr(st, cx, krate,
            "typed!($expr:Expr, &[$ty:Ty; $len]) as *mut $ty",
            "$expr.as_mut_ptr()");
    }

    fn min_phase(&self) -> Phase {
        Phase::Phase3
    }
}

pub fn register_commands(reg: &mut Registry) {
    use super::mk;

    reg.register("remove_redundant_casts", |_| mk(RemoveRedundantCasts));
    reg.register("convert_cast_as_ptr", |_| mk(ConvertCastAsPtr));
}