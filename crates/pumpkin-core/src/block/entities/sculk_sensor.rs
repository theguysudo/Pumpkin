use std::sync::{Arc, Mutex};

use pumpkin_data::game_event::GameEvent;
use pumpkin_data::tag::{self, Tag};
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;

use super::BlockEntity;
use crate::block::blocks::redstone::sculk_sensor::SculkSensorBlock;
use crate::entity::EntityBase;
use crate::world::World;
use crate::world::game_event::{GameEventContext, GameEventListener};
use crate::world::vibration::{self, VibrationData, VibrationUser};

/// Vanilla `SculkSensorBlockEntity.VibrationUser.LISTENER_RANGE`.
pub(crate) const SCULK_SENSOR_LISTENER_RADIUS: i32 = 8;

pub struct SculkSensorBlockEntity {
    pub position: BlockPos,
    pub last_vibration_frequency: Mutex<i32>,
    pub vibration: Mutex<VibrationData>,
}

impl BlockEntity for SculkSensorBlockEntity {
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
        if let Ok(last_vibration_frequency) = self.last_vibration_frequency.lock() {
            nbt.put_int("last_vibration_frequency", *last_vibration_frequency);
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

    /// Vanilla registers the sensor's `VibrationSystem.Ticker` through the block entity
    /// ticker, which drives the vibration delivery.
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

impl GameEventListener for SculkSensorBlockEntity {
    fn listener_position(&self) -> Vector3<f64> {
        self.position.to_centered_f64()
    }

    fn listener_radius(&self) -> i32 {
        SCULK_SENSOR_LISTENER_RADIUS
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

impl VibrationUser for SculkSensorBlockEntity {
    fn listener_block_position(&self) -> BlockPos {
        self.position
    }

    fn listener_radius(&self) -> i32 {
        SCULK_SENSOR_LISTENER_RADIUS
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
        // Vanilla `SculkSensorBlockEntity.VibrationUser.canReceiveVibration`: a sensor
        // ignores block changes at its own position and only listens to events that carry
        // a vibration frequency while it can activate.
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
        // Vanilla `SculkSensorBlockEntity.VibrationUser.onReceiveVibration`: the comparator
        // frequency comes from the event, the redstone power from the distance.
        let (block, state) = world.get_block_and_state(&self.position);
        if !SculkSensorBlock::can_activate(block, state.id) {
            return;
        }
        *self
            .last_vibration_frequency
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            i32::from(event.vibration_frequency());
        let power =
            vibration::redstone_strength_for_distance(receiving_distance, Self::LISTENER_RANGE);
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

impl SculkSensorBlockEntity {
    pub const ID: &'static str = "minecraft:sculk_sensor";

    /// Vanilla `SculkSensorBlockEntity.VibrationUser.LISTENER_RANGE`.
    pub(crate) const LISTENER_RANGE: i32 = SCULK_SENSOR_LISTENER_RADIUS;

    #[must_use]
    pub const fn new(position: BlockPos) -> Self {
        Self {
            position,
            last_vibration_frequency: Mutex::new(0),
            vibration: Mutex::new(VibrationData::new()),
        }
    }

    /// Vanilla `SculkSensorBlock.stepOn`: schedules a step vibration for the entity
    /// standing on the sensor.
    pub(crate) fn step_on(&self, world: &Arc<World>, entity: &dyn EntityBase) {
        let mut vibration = self
            .vibration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vibration::schedule_step_on(world, self, &mut vibration, entity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::ai::test_support::world;
    use pumpkin_data::Block;
    use pumpkin_data::block_properties::{SculkSensorLikeProperties, SculkSensorPhase};
    use pumpkin_util::math::vector2::Vector2;
    use pumpkin_world::chunk::ChunkData;
    use pumpkin_world::world::BlockFlags;

    fn loaded_test_world(folder: &tempfile::TempDir) -> Arc<World> {
        let world = world(folder);
        // Sensors require the adjacent chunks to be ticking before delivering (vanilla
        // `areAdjacentChunksTicking`), so load the 3x3 area around the origin.
        for x in -1..=1 {
            for z in -1..=1 {
                world
                    .level
                    .loaded_chunks
                    .insert(Vector2::new(x, z), ChunkData::empty_sync(x, z));
            }
        }
        world
    }

    fn place_sensor(world: &Arc<World>, pos: BlockPos) -> Arc<SculkSensorBlockEntity> {
        world.set_block_state(
            &pos,
            Block::SCULK_SENSOR.default_state.id,
            BlockFlags::empty(),
        );
        let sensor = Arc::new(SculkSensorBlockEntity::new(pos));
        world.add_block_entity(sensor.clone());
        sensor
    }

    fn sensor_props(world: &Arc<World>, pos: &BlockPos) -> SculkSensorLikeProperties {
        SculkSensorLikeProperties::from_state_id(world.get_block_state(pos).id)
    }

    fn advance_ticks(world: &Arc<World>, sensor: &Arc<SculkSensorBlockEntity>, ticks: i64) {
        for _ in 0..ticks {
            world
                .level_time
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .world_age += 1;
            sensor.tick(world);
        }
    }

    #[tokio::test]
    async fn step_vibration_activates_the_sensor_with_distance_power_and_event_frequency()
    -> Result<(), Box<dyn std::error::Error>> {
        let folder = tempfile::tempdir()?;
        let world = loaded_test_world(&folder);
        let pos = BlockPos::new(4, 64, 0);
        let sensor = place_sensor(&world, pos);

        let source = BlockPos::new(0, 64, 0).to_centered_f64();
        assert!(
            sensor.handle_game_event(
                &world,
                GameEvent::Step,
                &GameEventContext {
                    source_entity: None
                },
                source,
            ),
            "the sensor must schedule a step vibration"
        );

        // Vanilla delivers after one tick to pick the candidate plus the travel time of
        // four ticks for a source four blocks away.
        advance_ticks(&world, &sensor, 1 + 4);

        let props = sensor_props(&world, &pos);
        assert_eq!(
            props.sculk_sensor_phase,
            SculkSensorPhase::Active,
            "the sensor must be active after the vibration arrives"
        );
        assert_eq!(
            props.power, 8,
            "power follows vanilla's distance scale: 15 - floor(15/8 * 4)"
        );
        assert_eq!(
            *sensor
                .last_vibration_frequency
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            1,
            "the comparator frequency is the step event's frequency"
        );
        world.level.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn wool_between_sensor_and_source_blocks_the_vibration()
    -> Result<(), Box<dyn std::error::Error>> {
        let folder = tempfile::tempdir()?;
        let world = loaded_test_world(&folder);
        let pos = BlockPos::new(4, 64, 0);
        let sensor = place_sensor(&world, pos);
        let blocker = BlockPos::new(2, 64, 0);
        let origin = BlockPos::new(0, 64, 0).to_centered_f64();

        world.set_block_state(
            &blocker,
            Block::WHITE_WOOL.default_state.id,
            BlockFlags::empty(),
        );
        assert!(
            !sensor.handle_game_event(
                &world,
                GameEvent::Step,
                &GameEventContext {
                    source_entity: None
                },
                origin,
            ),
            "wool must occlude the vibration"
        );
        advance_ticks(&world, &sensor, 1 + 4);
        assert_eq!(
            sensor_props(&world, &pos).sculk_sensor_phase,
            SculkSensorPhase::Inactive,
            "an occluded vibration must not activate the sensor"
        );

        world.set_block_state(&blocker, Block::STONE.default_state.id, BlockFlags::empty());
        assert!(
            sensor.handle_game_event(
                &world,
                GameEvent::Step,
                &GameEventContext {
                    source_entity: None
                },
                origin,
            ),
            "stone must not occlude the vibration"
        );
        world.level.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn block_changes_at_the_sensors_own_position_are_ignored()
    -> Result<(), Box<dyn std::error::Error>> {
        let folder = tempfile::tempdir()?;
        let world = loaded_test_world(&folder);
        let pos = BlockPos::new(4, 64, 0);
        let sensor = place_sensor(&world, pos);

        for event in [GameEvent::BlockPlace, GameEvent::BlockDestroy] {
            assert!(
                !sensor.handle_game_event(
                    &world,
                    event,
                    &GameEventContext {
                        source_entity: None
                    },
                    pos.to_centered_f64(),
                ),
                "a sensor must not hear {event:?} at its own position"
            );
        }
        advance_ticks(&world, &sensor, 1 + 4);
        assert_eq!(
            sensor_props(&world, &pos).sculk_sensor_phase,
            SculkSensorPhase::Inactive,
            "ignored events must not activate the sensor"
        );

        // Step at the sensor's own position is still received; vanilla also schedules it
        // directly through `SculkSensorBlock.stepOn` when an entity stands on the sensor.
        assert!(sensor.handle_game_event(
            &world,
            GameEvent::Step,
            &GameEventContext {
                source_entity: None
            },
            pos.to_centered_f64(),
        ));
        world.level.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn active_sensors_ignore_further_vibrations() -> Result<(), Box<dyn std::error::Error>> {
        let folder = tempfile::tempdir()?;
        let world = loaded_test_world(&folder);
        let pos = BlockPos::new(4, 64, 0);
        let sensor = place_sensor(&world, pos);

        assert!(sensor.handle_game_event(
            &world,
            GameEvent::Step,
            &GameEventContext {
                source_entity: None
            },
            BlockPos::new(0, 64, 0).to_centered_f64(),
        ));
        advance_ticks(&world, &sensor, 1 + 4);
        assert!(
            !sensor.handle_game_event(
                &world,
                GameEvent::EntityDamage,
                &GameEventContext {
                    source_entity: None
                },
                BlockPos::new(0, 64, 0).to_centered_f64(),
            ),
            "an active sensor must not schedule further vibrations"
        );
        world.level.shutdown().await;
        Ok(())
    }
}
