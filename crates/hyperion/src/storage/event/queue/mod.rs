use std::marker::PhantomData;

use flecs_ecs::{
    core::{ComponentId, ComponentType, DataComponent, Struct, World, WorldGet, flecs},
    macros::Component,
};

use crate::simulation::event;

pub mod raw;

struct SendSyncPtr<T>(*const T, PhantomData<T>);

unsafe impl<T> Send for SendSyncPtr<T> {}
unsafe impl<T> Sync for SendSyncPtr<T> {}
