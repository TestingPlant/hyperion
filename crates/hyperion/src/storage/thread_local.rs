use std::{
    cell::SyncUnsafeCell,
    ops::{Deref, DerefMut},
};

use flecs_ecs::core::World;

use crate::NUM_THREADS;

/// Thread-local in flecs environment
#[derive(Debug, Default)]
pub struct ThreadLocal<T> {
    locals: [T; NUM_THREADS],
}

unsafe impl<T> Sync for ThreadLocal<T> {}

impl<T> Deref for ThreadLocal<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.locals
    }
}

impl<T> DerefMut for ThreadLocal<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.locals
    }
}

impl<'a, T> IntoIterator for &'a mut ThreadLocal<T> {
    type IntoIter = core::slice::IterMut<'a, T>;
    type Item = &'a mut T;

    fn into_iter(self) -> Self::IntoIter {
        self.locals.iter_mut()
    }
}

impl<T: Default> ThreadLocal<T> {
    #[must_use]
    pub fn new_defaults() -> Self {
        let locals = core::array::from_fn(|_| T::default());
        Self { locals }
    }
}

impl<T> ThreadLocal<T> {
    pub fn new_with<F>(f: F) -> Self
    where
        F: Fn(usize) -> T,
    {
        Self {
            locals: core::array::from_fn(f),
        }
    }

    #[must_use]
    #[expect(clippy::cast_sign_loss)]
    pub fn get(&self, world: &World) -> &T {
        let id = world.stage_id();
        let id = id as usize;
        unsafe { self.locals.get_unchecked(id) }
    }
}

#[derive(Debug)]
pub struct ThreadLocalVec<T> {
    inner: ThreadLocal<SyncUnsafeCell<Vec<T>>>,
}

impl<T> ThreadLocalVec<T> {
    pub fn len(&mut self) -> usize {
        self.inner
            .iter_mut()
            .map(std::cell::SyncUnsafeCell::get_mut)
            .map(|x| x.len())
            .sum()
    }

    pub fn clear(&mut self) {
        self.inner
            .iter_mut()
            .map(SyncUnsafeCell::get_mut)
            .for_each(Vec::clear);
    }
}

impl<T> Default for ThreadLocalVec<T> {
    fn default() -> Self {
        Self {
            inner: ThreadLocal::new_defaults(),
        }
    }
}

impl<T> ThreadLocalVec<T> {
    #[must_use]
    pub fn with_capacity(n: usize) -> Self {
        Self {
            inner: ThreadLocal::new_with(|_| SyncUnsafeCell::new(Vec::with_capacity(n))),
        }
    }

    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.inner
            .iter_mut()
            .map(SyncUnsafeCell::get_mut)
            .flat_map(|x| x.drain(..))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> + '_ {
        self.inner.iter_mut().flat_map(SyncUnsafeCell::get_mut)
    }

    pub fn is_empty(&mut self) -> bool {
        self.inner
            .iter_mut()
            .map(SyncUnsafeCell::get_mut)
            .all(|x| x.is_empty())
    }
}

impl<T> ThreadLocalVec<T> {
    pub fn push(&self, element: T, world: &World) {
        let inner = self.inner.get(world);
        let inner = unsafe { &mut *inner.get() };
        inner.push(element);
    }
}
