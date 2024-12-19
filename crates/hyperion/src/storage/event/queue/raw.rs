use std::{
    any::TypeId, ptr::NonNull
};

/// Denotes a pointer that will become invalid at the end of the tick (it is bump allocated)
#[derive(Debug, Copy, Clone)]
pub struct TypedBumpPtr {
    id: TypeId,
    // a ptr to a bump allocated event
    elem: NonNull<()>,
}

unsafe impl Send for TypedBumpPtr {}
unsafe impl Sync for TypedBumpPtr {}

impl TypedBumpPtr {
    #[must_use]
    pub const fn new(id: TypeId, elem: NonNull<()>) -> Self {
        Self { id, elem }
    }

    #[must_use]
    pub const fn id(&self) -> TypeId {
        self.id
    }

    #[must_use]
    pub const fn elem(&self) -> NonNull<()> {
        self.elem
    }
}
