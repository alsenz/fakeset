//! Small Arrow array helpers shared by the generation (`executor`) and output (`output`)
//! layers — thin wrappers over the verbose `as_any().downcast_ref::<T>()` / `take`-then-downcast
//! idioms so call sites read in terms of intent.
use anyhow::{Result, anyhow};
use arrow::array::{Array, UInt32Array};
use arrow::compute::take;

/// Downcast an Arrow array to a concrete array type `T`, with an error naming `ctx` on mismatch.
pub(crate) fn downcast<'a, T: Array + 'static>(arr: &'a dyn Array, ctx: &str) -> Result<&'a T> {
    arr.as_any().downcast_ref::<T>().ok_or_else(|| {
        let ty = std::any::type_name::<T>()
            .rsplit("::")
            .next()
            .unwrap_or("the expected array");
        anyhow!("{ctx} is not a {ty}")
    })
}

/// `take` rows from `arr` by `indices`, returning a concrete owned array of type `T`. `take`
/// preserves the input array's type, so the downcast never fails in practice; the `Result` is
/// propagated for the `take` itself.
pub(crate) fn take_as<T: Array + Clone + 'static>(
    arr: &dyn Array,
    indices: &UInt32Array,
) -> Result<T> {
    let taken = take(arr, indices, None)?;
    Ok(downcast::<T>(taken.as_ref(), "take result")?.clone())
}
