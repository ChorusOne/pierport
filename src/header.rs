#![allow(clippy::declare_interior_mutable_const)]
use hyper::header::HeaderName;

pub const SIZE: HeaderName = HeaderName::from_static("pierport-size");
