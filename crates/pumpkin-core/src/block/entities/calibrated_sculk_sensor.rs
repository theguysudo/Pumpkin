use std::sync::{Arc, Mutex};

use pumpkin_data::block_properties::CalibratedSculkSensorLikeProperties;
use pumpkin_data::game_event::GameEvent;
use pumpkin_data::tag::{self, Tag};
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;

use super::BlockEntity;
use super::sculk_sensor::SCULK_SENSOR_LISTENER_RADIUS;
use crate::block::blocks::redstone::sculk_sensor::{
    SculkSensorBlock, get_calibrated_sculk_sensor_input_strength,
};
use crate::entity::EntityBase;
use crate::world::World;
use crate::world::game_event::{GameEventContext, GameEventListener};
use crate::world::vibration::{self, VibrationData, VibrationUser};

/// Vanilla `CalibratedSculkSensorBlockEntity.VibrationUser.getListenerRadius` doubles the
/// plain sensor's range.
const CALIBRATED_LISTENER_RADIUS: i32 = SCULK_SENSOR_LISTENER_RADIUS * 2;

pub struct CalibratedSculkSensorBlockEntity {
    pub position: BlockPos,
    pub last_vibration_frequency: Mutex<i32>,
    pub vibration: Mutex<VibrationData>,
}

impl BlockEntity for CalibratedSculkSensorBlockEntity {
    fn resource_location(&self) -> &'static str {
        Self::ID
    }

    fn get_position(&self) -> BlockPos {
        self.position
    }

    fn from_nbt(nbt: &pumpkin_nbt::compound::NbtCompound, position: BlockPos) -> Self
    where
        Self: Sized,
    {
        let last_vibration_frequency = nbt.get_int("last_vibration_frequency").unwrap_or(0);
        let vibration = nbt
            .get_compound("listener")
            .map_or_else(VibrationData::new, VibrationData::read_nbt);
        Self {
            position,
            last_vibration_frequency: Mutex::new(last_vibration_frequency),
            vibration: Mutex::new(vibration),
        }
    }

    fn write_nbt(&self, nbt: &mut NbtCompound) {
        if let Ok(freq) = self.last_vibration_frequency.lock() {
            nbt.put_int("last_vibration_frequency", *freq);
        }
        if let Ok(vibration) = self.vibration.lock() {
            nbt.put_compound("listener", vibration.write_nbt());
        }
    }

    fn chunk_data_nbt(&self) -> Option<NbtCompound> {
        let mut nbt = NbtCompound::new();
        nbt.put_int(
            "last_vibration_frequency",
            *self.last_vibration_frequency.try_lock().ok()?,
        );
        Some(nbt)
    }

    /// Vanilla registers the calibrated sensor's `VibrationSystem.Ticker` through the block
    /// entity ticker, which drives the vibration delivery.
    fn tick(&self, world: &Arc<World>) {
        let delivery = {
            let mut vibration = self
                .vibration
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            vibration.tick(world, self)
        };
        // Deliver without holding the vibration lock: activation emits game events that
        // can dispatch back to this listener.
        if let Some(delivery) = delivery {
            self.on_receive_vibration(
                world,
                delivery.origin,
                delivery.event,
                delivery.source,
                delivery.receiving_distance,
            );
        }
    }

    fn game_event_listener(&self) -> Option<&dyn GameEventListener> {
        Some(self)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl GameEventListener for CalibratedSculkSensorBlockEntity {
    fn listener_position(&self) -> Vector3<f64> {
        self.position.to_centered_f64()
    }

    fn listener_radius(&self) -> i32 {
        CALIBRATED_LISTENER_RADIUS
    }

    fn handle_game_event(
        &self,
        world: &World,
        event: GameEvent,
        context: &GameEventContext<'_>,
        source_position: Vector3<f64>,
    ) -> bool {
        let mut vibration = self
            .vibration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vibration.handle_game_event(world, self, event, context, source_position)
    }
}

impl VibrationUser for CalibratedSculkSensorBlockEntity {
    fn listener_block_position(&self) -> BlockPos {
        self.position
    }

    fn listener_radius(&self) -> i32 {
        CALIBRATED_LISTENER_RADIUS
    }

    fn listenable_events(&self) -> Tag {
        tag::GameEvent::MINECRAFT_VIBRATIONS
    }

    fn can_receive_vibration(
        &self,
        world: &World,
        pos: BlockPos,
        event: GameEvent,
        _context: &GameEventContext<'_>,
    ) -> bool {
        // Vanilla `CalibratedSculkSensorBlockEntity.VibrationUser.canReceiveVibration`: when
        // the back input carries a redstone signal, only events of that frequency pass.
        let state = world.get_block_state(&self.position);
        let facing = CalibratedSculkSensorLikeProperties::from_state_id(state.id).facing;
        let comparison = get_calibrated_sculk_sensor_input_strength(world, &self.position, facing);
        if comparison != 0 && event.vibration_frequency() != comparison {
            return false;
        }
        // ...then the plain sensor's checks from the overridden `super.canReceiveVibration`.
        if pos == self.position && matches!(event, GameEvent::BlockDestroy | GameEvent::BlockPlace)
        {
            return false;
        }
        let (block, state) = world.get_block_and_state(&self.position);
        event.vibration_frequency() != 0 && SculkSensorBlock::can_activate(block, state.id)
    }

    fn on_receive_vibration(
        &self,
        world: &Arc<World>,
        _pos: BlockPos,
        event: GameEvent,
        source: Option<Arc<dyn EntityBase>>,
        receiving_distance: f32,
    ) {
        // Same receive flow as the plain sensor, with the calibrated listener radius.
        let (block, state) = world.get_block_and_state(&self.position);
        if !SculkSensorBlock::can_activate(block, state.id) {
            return;
        }
        *self
            .last_vibration_frequency
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            i32::from(event.vibration_frequency());
        let power = vibration::redstone_strength_for_distance(
            receiving_distance,
            CALIBRATED_LISTENER_RADIUS,
        );
        SculkSensorBlock::activate(
            world,
            &self.position,
            power,
            event.vibration_frequency(),
            source.as_deref(),
        );
    }

    fn requires_adjacent_chunks_ticking(&self) -> bool {
        true
    }
}

impl CalibratedSculkSensorBlockEntity {
    pub const ID: &'static str = "minecraft:calibrated_sculk_sensor";

    #[must_use]
    pub const fn new(position: BlockPos) -> Self {
        Self {
            position,
            last_vibration_frequency: Mutex::new(0),
            vibration: Mutex::new(VibrationData::new()),
        }
    }

    /// Vanilla `SculkSensorBlock.stepOn`, inherited by the calibrated sensor.
    pub(crate) fn step_on(&self, world: &Arc<World>, entity: &dyn EntityBase) {
        let mut vibration = self
            .vibration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vibration::schedule_step_on(world, self, &mut vibration, entity);
    }
}
