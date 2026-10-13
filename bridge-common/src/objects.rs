use abi_stable::std_types::RVec;
use bridge_api::RObjectChannel;

/// Sparse-emit an object↔channel declaration: return it only when it differs
/// from the cached one (or after a cache clear, i.e. pipeline reset).
pub fn declare_object_channels(
    cache: &mut Option<RVec<RObjectChannel>>,
    current: RVec<RObjectChannel>,
) -> RVec<RObjectChannel> {
    if cache.as_deref() == Some(current.as_slice()) {
        RVec::new()
    } else {
        *cache = Some(current.clone());
        current
    }
}
