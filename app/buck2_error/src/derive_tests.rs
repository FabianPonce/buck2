/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

#![cfg(test)]

use crate as buck2_error;

#[derive(buck2_error_derive::Error, Debug)]
#[error("foo")]
#[buck2(user)]
pub struct Error1;

#[test]
fn test_derive_error1() {
    let e: crate::Error = Error1.into();
    assert_eq!(e.get_category(), Some(crate::Category::User));

    let e: anyhow::Error = Error1.into();
    let e: crate::Error = e.into();
    assert_eq!(e.get_category(), Some(crate::Category::User));
}

#[derive(buck2_error_derive::Error, Debug)]
#[error("foo")]
#[buck2(typ = ActionCommandFailure, infra)]
#[allow(unused)]
struct Error2((), ());

#[test]
fn test_derive_error2() {
    let e: crate::Error = Error2((), ()).into();
    assert_eq!(e.get_category(), Some(crate::Category::Infra));
    assert_eq!(
        e.get_error_type(),
        Some(crate::ErrorType::ActionCommandFailure)
    );
}

#[derive(buck2_error_derive::Error, Debug)]
pub enum Error3 {
    #[error("foo")]
    #[buck2(user)]
    #[buck2(typ = DaemonIsBusy)]
    VariantA,
    #[error("bar")]
    #[buck2(infra)]
    VariantB,
    #[error("baz")]
    VariantC,
}

#[test]
fn test_derive_error3() {
    let e: crate::Error = Error3::VariantA.into();
    assert_eq!(e.get_category(), Some(crate::Category::User));
    assert_eq!(e.get_error_type(), Some(crate::ErrorType::DaemonIsBusy));

    let e: crate::Error = Error3::VariantB.into();
    assert_eq!(e.get_category(), Some(crate::Category::Infra));
    assert_eq!(e.get_error_type(), None);

    let e: crate::Error = Error3::VariantC.into();
    assert_eq!(e.get_category(), None);
    assert_eq!(e.get_error_type(), None);
}

#[derive(buck2_error_derive::Error, Debug)]
#[error("Generic error")]
pub struct GenericError<G>(G);

#[test]
fn test_generic_error() {
    let _e: crate::Error = GenericError(42).into();
}

/// Test that no unused fields warning is emitted.
#[derive(buck2_error_derive::Error, Debug)]
#[error("Unused")]
pub struct WithField {
    x: u8,
}

#[test]
fn test_with_field() {
    let _e: crate::Error = WithField { x: 42 }.into();
}

#[derive(buck2_error_derive::Error, Debug)]
#[error("Unused")]
struct NoAttrsStruct;

#[derive(buck2_error_derive::Error, Debug)]
#[error("Unused")]
enum NoAttrsEnum {
    Variant,
}

#[test]
fn test_source_location_no_attrs() {
    let e: crate::Error = NoAttrsStruct.into();
    assert_eq!(
        e.source_location(),
        Some("buck2_error/src/derive_tests.rs::NoAttrsStruct")
    );
    let e: crate::Error = NoAttrsEnum::Variant.into();
    assert_eq!(
        e.source_location(),
        Some("buck2_error/src/derive_tests.rs::NoAttrsEnum::Variant")
    );
}

#[derive(buck2_error_derive::Error, Debug)]
#[error("Unused")]
#[buck2(user)]
enum EnumWithTypeOption {
    Variant,
}

#[test]
fn test_enum_with_type_option() {
    let e: crate::Error = EnumWithTypeOption::Variant.into();
    assert_eq!(e.get_category(), Some(crate::Category::User));
    assert_eq!(
        e.source_location(),
        Some("buck2_error/src/derive_tests.rs::EnumWithTypeOption::Variant"),
    );
}

#[derive(buck2_error_derive::Error, Debug)]
#[error("Unused")]
#[buck2(category = User)]
struct ErrorWithSpelledOutCategory;

#[test]
fn test_error_with_spelled_out_category() {
    let e: crate::Error = ErrorWithSpelledOutCategory.into();
    assert_eq!(e.get_category(), Some(crate::Category::User));
}

impl ComputedOptionsError {
    fn compute_category(&self) -> Option<crate::Category> {
        match self {
            ComputedOptionsError::A => Some(crate::Category::User),
            ComputedOptionsError::B(_) => Some(crate::Category::Infra),
        }
    }
}

fn compute_type_a() -> Option<crate::ErrorType> {
    Some(crate::ErrorType::Watchman)
}

fn compute_typ_b(val: bool) -> Option<crate::ErrorType> {
    if val {
        None
    } else {
        Some(crate::ErrorType::DaemonIsBusy)
    }
}

#[derive(buck2_error_derive::Error, Debug)]
#[error("Unused")]
#[buck2(category = ComputedOptionsError::compute_category(self))]
enum ComputedOptionsError {
    #[buck2(typ = compute_type_a())]
    A,
    #[buck2(typ = compute_typ_b(*_0))]
    B(bool),
}

#[test]
fn test_computed_options() {
    let e: crate::Error = ComputedOptionsError::A.into();
    assert_eq!(e.get_category(), Some(crate::Category::User));
    assert_eq!(e.get_error_type(), Some(crate::ErrorType::Watchman));

    let e: crate::Error = ComputedOptionsError::B(false).into();
    assert_eq!(e.get_category(), Some(crate::Category::Infra));
    assert_eq!(e.get_error_type(), Some(crate::ErrorType::DaemonIsBusy));

    let e: crate::Error = ComputedOptionsError::B(true).into();
    assert_eq!(e.get_category(), Some(crate::Category::Infra));
    assert_eq!(e.get_error_type(), None);
}

#[test]
fn test_root_is_applied_conditionally() {
    #[derive(buck2_error_derive::Error, Debug)]
    #[error("Unused")]
    #[buck2(typ = Watchman)]
    struct WatchmanError;

    #[derive(buck2_error_derive::Error, Debug)]
    #[error("Unused")]
    #[buck2(typ = compute(self))]
    enum MaybeWatchmanError {
        Some(#[source] WatchmanError),
        None,
    }

    fn compute(x: &MaybeWatchmanError) -> Option<crate::ErrorType> {
        match x {
            MaybeWatchmanError::Some(_) => None,
            MaybeWatchmanError::None => Some(crate::ErrorType::DaemonIsBusy),
        }
    }

    let e: crate::Error = MaybeWatchmanError::None.into();
    assert_eq!(e.get_error_type(), Some(crate::ErrorType::DaemonIsBusy));

    let e: crate::Error = MaybeWatchmanError::Some(WatchmanError).into();
    assert_eq!(e.get_error_type(), Some(crate::ErrorType::Watchman));
}

#[test]
fn test_error_tags() {
    fn f(_: &TaggedError) -> Option<crate::ErrorTag> {
        Some(crate::ErrorTag::StarlarkFail)
    }

    #[derive(buck2_error_derive::Error, Debug)]
    #[error("Unused")]
    #[buck2(tag = WatchmanTimeout)]
    enum TaggedError {
        #[buck2(tag = f(self))]
        A,
        #[buck2(tag = WatchmanTimeout)]
        B,
    }

    let a: crate::Error = TaggedError::A.into();
    assert_eq!(
        &a.get_tags(),
        &[
            crate::ErrorTag::StarlarkFail,
            crate::ErrorTag::WatchmanTimeout
        ]
    );
    let b: crate::Error = TaggedError::B.into();
    assert_eq!(&b.get_tags(), &[crate::ErrorTag::WatchmanTimeout]);
}
