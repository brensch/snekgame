//! Number conversions the browser forces on us. JS has one number type, so
//! every timestamp, duration and random draw arrives as an `f64`, and the
//! only way to use one as a count or an index is to turn it back into an
//! integer.

/// `x` as a whole `u64`, rounded toward zero. Negative numbers and NaN come
/// out as 0 and anything too large as `u64::MAX` — the right answer for
/// every duration, count and index taken from a JS number here.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the one float-to-integer conversion: Rust has no checked form, \
              and `as` saturates and sends NaN to 0, which is the clamp wanted"
)]
pub(crate) const fn to_u64(x: f64) -> u64 {
    x as u64
}

/// `x` as a whole `u32`, saturating like [`to_u64`].
pub(crate) fn to_u32(x: f64) -> u32 {
    u32::try_from(to_u64(x)).unwrap_or(u32::MAX)
}

/// `x` as a whole `usize`, saturating like [`to_u64`].
pub(crate) fn to_usize(x: f64) -> usize {
    usize::try_from(to_u64(x)).unwrap_or(usize::MAX)
}

/// A count as a float, exactly. Counts here are cells, frames and list
/// lengths, nowhere near `u32::MAX`; a larger one saturates rather than
/// quietly losing precision.
pub(crate) fn from_count(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// A random index below `len` (0 for an empty range).
pub(crate) fn random_index(len: usize) -> usize {
    to_usize(js_sys::Math::random() * from_count(len)).min(len.saturating_sub(1))
}
