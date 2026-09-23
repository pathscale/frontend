//! Support code for encoding and decoding types.

// tidy-alphabetical-start
// tidy-alphabetical-end

// `FileEncoder` writes metadata to a file - `fs::File`, `io::Write`, `Path`. That is a real
// artifact write, and it is the surface this compiler is removing rather than porting: a
// compiler answers a caller. The encoder itself is pure and stays; only its sink is std.
#![allow(internal_features)]
// ---------------------------------------------------------------------------------------------
// STD IS BANNED IN THIS CRATE.
//
// `#![no_std]` above is the ban and the compiler is the enforcer: without `extern crate std;`
// there is no `std` in the extern prelude, so any `std::` path fails to resolve and the build
// stops. Do not add that line back to make an error go away - the error is the point. Whatever
// needed `std` either has a `core`/`alloc` equivalent, belongs in `ekostd`, or is a
// dependency that has to be replaced.
//
// The prelude is the part a grep cannot see: `Vec`, `String`, `Box`, `format!`, `vec!`,
// `thread_local!` and `println!` name no path. Under `#![no_std]` they resolve through `alloc`
// and `eko` instead, which is why those imports appear at the top of every file here.
// ---------------------------------------------------------------------------------------------
// Upstream's `#[cfg(test)] extern crate self as rustc_serialize;` is gone: an `extern crate`
// must sit at the crate root, and this is a module. The tests name it as
// `crate::rustc_serialize`, and the derives root their paths at `frontend::`.

pub use self::serialize::{Decodable, Decoder, Encodable, Encoder};

mod serialize;

pub mod int_overflow;
pub mod leb128;
pub mod opaque;
