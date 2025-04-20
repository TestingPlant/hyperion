use std::{borrow::Cow, mem::transmute};

use flecs_ecs::{
    core::{EntityView, EntityViewGet, QueryBuilderImpl, SystemAPI, TermBuilderImpl, World, flecs},
    macros::{Component, observer, system},
    prelude::Module,
};
use hyperion_inventory::{
    CursorItem, Inventory, InventoryState, ItemKindExt, ItemSlot, OpenInventory, PlayerInventory,
};
use hyperion_utils::EntityExt;
use valence_protocol::{
    Decode, VarInt,
    packets::play::{
        self, ClickSlotC2s, UpdateSelectedSlotC2s,
        click_slot_c2s::{ClickMode, SlotChange},
        entity_equipment_update_s2c::EquipmentEntry,
    },
};
use valence_server::ItemStack;
use valence_text::IntoText;

use super::{Player, event};
use crate::{
    net::{Compose, ConnectionId, DataBundle},
    simulation::Position,
};

#[derive(Component)]
pub struct InventoryModule;

impl Module for InventoryModule {
    fn module(world: &World) {
        world.component::<OpenInventory>();
        world.component::<InventoryState>();

        world
            .component::<Player>()
            .add_trait::<(flecs::With, InventoryState)>();
    }
}
