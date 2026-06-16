// TODO: Use this to reduce the physical cache size

pub struct LowCapacityVec<I, T> {
    _len: I,
    _capacity: I,
    _ptr: *mut T,
}
