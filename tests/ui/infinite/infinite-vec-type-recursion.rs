//@ revisions: feature gated

#![cfg_attr(feature, feature(lazy_type_alias))]
#![allow(incomplete_features)]

type X = Vec<X>;
//[gated]~^ ERROR cycle detected
//[feature]~^^ ERROR: overflow evaluating the requirement `X`

#[rustfmt::skip]
fn main() { let b: X = Vec::new(); }
