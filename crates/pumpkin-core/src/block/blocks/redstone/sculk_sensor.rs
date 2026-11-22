use std::sync::Arc;

use crate::block::entities::calibrated_sculk_sensor::CalibratedSculkSensorBlockEntity;
use crate::block::entities::sculk_sensor::SculkSensorBlockEntity;
use crate::block::{
    BlockBehaviour, BlockMetadata, EmitsRedstonePowerArgs, GetComparatorOutputArgs,
    GetRedstonePowerArgs, GetStateForNeighborUpdateArgs, OnEntityStepArgs, OnPlaceArgs,
    OnScheduledTickArgs, OnStateReplacedArgs, PathComputationType, PlacedArgs,
};
use crate::entity::EntityBase;
use crate::world::World;
use crate::world::game_event::GameEventContext;
use pumpkin_data::block_properties::{
    CalibratedSculkSensorLikeProperties, HorizontalFacing, SculkSensorLikeProperties,
    SculkSensorPhase,
};
use pumpkin_data::entity::EntityType;
use pumpkin_data::fluid::Fluid;
use pumpkin_data::game_event::GameEvent;
use pumpkin_data::sound::{Sound, SoundCategory};
use pumpkin_data::tag::{self, Taggable};
use pumpkin_data::{Block, BlockDirection, BlockId, BlockState, BlockStateId};
use pumpkin_util::math::position::BlockPos;
use pumpkin_world::tick::TickPriority;
use pumpkin_world::world::BlockFlags;

pub struct SculkSensorBlock;

impl BlockMetadata for SculkSensorBlock {
    fn ids() -> Box<[BlockId]> {
        [BlockId::SCULK_SENSOR, BlockId::CALIBRATED_SCULK_SENSOR].into()
    }
}

/// Vanilla `SculkSensorBlock.RESONANCE_PITCH_BEND`: the chime pitch per vibration
/// frequency, `NoteBlock.getPitchFromNote` over the per-frequency tone map.
const RESONANCE_PITCH_BEND: [f32; 16] = [
    0.5,
    0.5,
    0.561_231,
    0.629_960_5,
    0.707_106_77,
    0.749_153_55,
    0.840_896_4,
    0.890_898_7,
    1.0,
    1.122_462,
    1.189_207_1,
    std::f32::consts::SQRT_2,
    1.498_307_1,
    1.681_792_9,
    1.781_797_4,
    2.0,
];

/// Vanilla `VibrationSystem.RESONANCE_EVENTS`: the resonate event per frequency 1-15.
const RESONANCE_EVENTS: [GameEvent; 15] = [
    GameEvent::Resonate1,
    GameEvent::Resonate2,
    GameEvent::Resonate3,
    GameEvent::Resonate4,
    GameEvent::Resonate5,
    GameEvent::Resonate6,
    GameEvent::Resonate7,
    GameEvent::Resonate8,
    GameEvent::Resonate9,
    GameEvent::Resonate10,
    GameEvent::Resonate11,
    GameEvent::Resonate12,
    GameEvent::Resonate13,
    GameEvent::Resonate14,
    GameEvent::Resonate15,
];

const fn horizontal_facing_to_dir(facing: HorizontalFacing) -> BlockDirection {
    match facing {
        HorizontalFacing::North => BlockDirection::North,
        HorizontalFacing::South => BlockDirection::South,
        HorizontalFacing::West => BlockDirection::West,
        HorizontalFacing::East => BlockDirection::East,
    }
}

/// Vanilla `SculkSensorBlock.updateNeighbours`: notifies both the sensor's own cell
/// and the cell below it, which receives strong direct power downwards.
fn update_neighbors(world: &Arc<World>, pos: &BlockPos) {
    world.update_neighbors(pos, None);
    world.update_neighbors(&pos.down(), None);
}

/// Both sensor variants carry the same phase property under different types.
pub(crate) fn sculk_sensor_phase(block: &Block, state_id: BlockStateId) -> SculkSensorPhase {
    if block.id == BlockId::CALIBRATED_SCULK_SENSOR {
        CalibratedSculkSensorLikeProperties::from_state_id(state_id).sculk_sensor_phase
    } else {
        SculkSensorLikeProperties::from_state_id(state_id).sculk_sensor_phase
    }
}

pub(crate) fn get_calibrated_sculk_sensor_input_strength(
    world: &World,
    pos: &BlockPos,
    facing: HorizontalFacing,
) -> u8 {
    let input_dir = horizontal_facing_to_dir(facing).opposite();
    let neighbor_pos = pos.offset(input_dir.to_offset());
    let (neighbor_block, neighbor_state) = world.get_block_and_state(&neighbor_pos);
    super::get_redstone_power(
        neighbor_block,
        neighbor_state,
        world,
        &neighbor_pos,
        input_dir,
    )
}

impl SculkSensorBlock {
    /// Vanilla `SculkSensorBlock.canActivate`: only inactive sensors can activate.
    pub(crate) fn can_activate(block: &Block, state_id: BlockStateId) -> bool {
        sculk_sensor_phase(block, state_id) == SculkSensorPhase::Inactive
    }
}

/// Vanilla `SculkSensorBlock.tryResonateVibration`: resonators next to the sensor re-emit
/// the vibration at its frequency.
fn try_resonate_vibration(
    world: &Arc<World>,
    pos: &BlockPos,
    frequency: u8,
    source: Option<&dyn EntityBase>,
) {
    let context = GameEventContext {
        source_entity: source,
    };
    for direction in BlockDirection::all() {
        let neighbor_pos = pos.offset(direction.to_offset());
        if world
            .get_block(&neighbor_pos)
            .has_tag(&tag::Block::MINECRAFT_VIBRATION_RESONATORS)
        {
            world.emit_game_event_with_context(
                RESONANCE_EVENTS[usize::from(frequency) - 1].name(),
                neighbor_pos.to_centered_f64(),
                &context,
            );
            world.play_sound_fine(
                Sound::BlockAmethystBlockResonate,
                SoundCategory::Blocks,
                &neighbor_pos.to_centered_f64(),
                1.0,
                RESONANCE_PITCH_BEND[usize::from(frequency)],
            );
        }
    }
}

impl SculkSensorBlock {
    /// Vanilla `SculkSensorBlock.activate`: starts the active phase with the
    /// distance-based redstone `power` and the event's vibration `frequency`, which the
    /// comparator reads.
    pub fn activate(
        world: &Arc<World>,
        pos: &BlockPos,
        power: u8,
        frequency: u8,
        source: Option<&dyn EntityBase>,
    ) {
        let block = world.get_block(pos);
        if block.id == BlockId::SCULK_SENSOR {
            let state = world.get_block_state(pos);
            let mut props = SculkSensorLikeProperties::from_state_id(state.id);
            if props.sculk_sensor_phase == SculkSensorPhase::Inactive {
                if let Some(be) = world.get_block_entity(pos)
                    && let Some(sensor_be) = be.as_any().downcast_ref::<SculkSensorBlockEntity>()
                {
                    *sensor_be
                        .last_vibration_frequency
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = i32::from(frequency);
                }

                props.sculk_sensor_phase = SculkSensorPhase::Active;
                props.power = power;
                world.set_block_state(pos, props.to_state_id(block), BlockFlags::NOTIFY_ALL);
                world.schedule_block_tick(block, *pos, 30, TickPriority::Normal);
                update_neighbors(world, pos);
                try_resonate_vibration(world, pos, frequency, source);
                world.emit_game_event_with_context(
                    GameEvent::SculkSensorTendrilsClicking.name(),
                    pos.to_centered_f64(),
                    &GameEventContext {
                        source_entity: source,
                    },
                );
                if !props.waterlogged {
                    world.play_block_sound(
                        Sound::BlockSculkSensorClicking,
                        SoundCategory::Blocks,
                        *pos,
                    );
                }
            }
        } else if block.id == BlockId::CALIBRATED_SCULK_SENSOR {
            let state = world.get_block_state(pos);
            let mut props = CalibratedSculkSensorLikeProperties::from_state_id(state.id);
            if props.sculk_sensor_phase == SculkSensorPhase::Inactive {
                if let Some(be) = world.get_block_entity(pos)
                    && let Some(cal_be) = be
                        .as_any()
                        .downcast_ref::<CalibratedSculkSensorBlockEntity>()
                {
                    *cal_be
                        .last_vibration_frequency
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = i32::from(frequency);
                }

                props.sculk_sensor_phase = SculkSensorPhase::Active;
                props.power = power;
                world.set_block_state(pos, props.to_state_id(block), BlockFlags::NOTIFY_ALL);
                // Vanilla CalibratedSculkSensorBlock.getActiveTicks() is 10 ticks (vs 30 for normal).
                world.schedule_block_tick(block, *pos, 10, TickPriority::Normal);
                update_neighbors(world, pos);
                try_resonate_vibration(world, pos, frequency, source);
                world.emit_game_event_with_context(
                    GameEvent::SculkSensorTendrilsClicking.name(),
                    pos.to_centered_f64(),
                    &GameEventContext {
                        source_entity: source,
                    },
                );
                if !props.waterlogged {
                    world.play_block_sound(
                        Sound::BlockSculkSensorClicking,
                        SoundCategory::Blocks,
                        *pos,
                    );
                }
            }
        }
    }
}

impl BlockBehaviour for SculkSensorBlock {
    fn on_place(&self, args: OnPlaceArgs<'_>) -> BlockStateId {
        if args.block.id == BlockId::CALIBRATED_SCULK_SENSOR {
            let mut props = CalibratedSculkSensorLikeProperties::default(args.block);
            props.facing = args.player.living_entity.entity.get_horizontal_facing();
            props.waterlogged = args.replacing.water_source();
            props.to_state_id(args.block)
        } else {
            let mut props = SculkSensorLikeProperties::default(args.block);
            props.waterlogged = args.replacing.water_source();
            props.to_state_id(args.block)
        }
    }

    fn on_entity_step(&self, args: OnEntityStepArgs<'_>) {
        // Vanilla `SculkSensorBlock.stepOn`: an entity standing on the sensor schedules a
        // step vibration directly, which is how a sneaking player still triggers it.
        if sculk_sensor_phase(args.block, args.state.id) != SculkSensorPhase::Inactive
            || args.entity.get_entity().entity_type == &EntityType::WARDEN
        {
            return;
        }
        match args.block.id {
            BlockId::SCULK_SENSOR => {
                if let Some(be) = args.world.get_block_entity(args.position)
                    && let Some(sensor) = be.as_any().downcast_ref::<SculkSensorBlockEntity>()
                {
                    sensor.step_on(args.world, args.entity);
                }
            }
            BlockId::CALIBRATED_SCULK_SENSOR => {
                if let Some(be) = args.world.get_block_entity(args.position)
                    && let Some(sensor) = be
                        .as_any()
                        .downcast_ref::<CalibratedSculkSensorBlockEntity>()
                {
                    sensor.step_on(args.world, args.entity);
                }
            }
            _ => {}
        }
    }

    fn get_state_for_neighbor_update(
        &self,
        args: GetStateForNeighborUpdateArgs<'_>,
    ) -> BlockStateId {
        let waterlogged = if args.block.id == BlockId::CALIBRATED_SCULK_SENSOR {
            CalibratedSculkSensorLikeProperties::from_state_id(args.state_id).waterlogged
        } else {
            SculkSensorLikeProperties::from_state_id(args.state_id).waterlogged
        };
        if waterlogged {
            args.world.schedule_fluid_tick(
                &Fluid::WATER,
                *args.position,
                Fluid::WATER.flow_speed as u8,
                TickPriority::Normal,
            );
        }
        args.state_id
    }

    fn placed(&self, args: PlacedArgs<'_>) {
        if args.block.id == BlockId::CALIBRATED_SCULK_SENSOR {
            if args.world.get_block_entity(args.position).is_none() {
                let entity = CalibratedSculkSensorBlockEntity::new(*args.position);
                args.world.add_block_entity(Arc::new(entity));
            }
            if Block::from_state_id(args.old_state_id) != args.block {
                let mut props = CalibratedSculkSensorLikeProperties::from_state_id(args.state_id);
                if (props.power > 0 || props.sculk_sensor_phase != SculkSensorPhase::Inactive)
                    && !args
                        .world
                        .is_block_tick_scheduled(args.position, args.block)
                {
                    props.power = 0;
                    props.sculk_sensor_phase = SculkSensorPhase::Inactive;
                    args.world.set_block_state(
                        args.position,
                        props.to_state_id(args.block),
                        BlockFlags::NOTIFY_LISTENERS | BlockFlags::FORCE_STATE,
                    );
                }
            }
        } else if args.block.id == BlockId::SCULK_SENSOR {
            if args.world.get_block_entity(args.position).is_none() {
                let entity = SculkSensorBlockEntity::new(*args.position);
                args.world.add_block_entity(Arc::new(entity));
            }
            if Block::from_state_id(args.old_state_id) != args.block {
                let mut props = SculkSensorLikeProperties::from_state_id(args.state_id);
                if (props.power > 0 || props.sculk_sensor_phase != SculkSensorPhase::Inactive)
                    && !args
                        .world
                        .is_block_tick_scheduled(args.position, args.block)
                {
                    props.power = 0;
                    props.sculk_sensor_phase = SculkSensorPhase::Inactive;
                    args.world.set_block_state(
                        args.position,
                        props.to_state_id(args.block),
                        BlockFlags::NOTIFY_LISTENERS | BlockFlags::FORCE_STATE,
                    );
                }
            }
        }
    }

    fn emits_redstone_power(&self, _args: EmitsRedstonePowerArgs<'_>) -> bool {
        true
    }

    fn get_weak_redstone_power(&self, args: GetRedstonePowerArgs<'_>) -> u8 {
        if args.block.id == BlockId::SCULK_SENSOR {
            let props = SculkSensorLikeProperties::from_state_id(args.state.id);
            if props.sculk_sensor_phase == SculkSensorPhase::Active {
                props.power
            } else {
                0
            }
        } else if args.block.id == BlockId::CALIBRATED_SCULK_SENSOR {
            let props = CalibratedSculkSensorLikeProperties::from_state_id(args.state.id);
            // Vanilla CalibratedSculkSensorBlock.getSignal: does not emit towards the calibration input face.
            if props.sculk_sensor_phase == SculkSensorPhase::Active
                && args.direction != horizontal_facing_to_dir(props.facing)
            {
                props.power
            } else {
                0
            }
        } else {
            0
        }
    }

    fn get_strong_redstone_power(&self, args: GetRedstonePowerArgs<'_>) -> u8 {
        // Vanilla SculkSensorBlock.getDirectSignal: Direction.UP ? getSignal(...) : 0
        if args.direction == BlockDirection::Up {
            self.get_weak_redstone_power(args)
        } else {
            0
        }
    }

    fn get_comparator_output(&self, args: GetComparatorOutputArgs<'_>) -> Option<u8> {
        // Vanilla reads the frequency only while the sensor is active.
        if sculk_sensor_phase(args.block, args.state.id) != SculkSensorPhase::Active {
            return Some(0);
        }

        let be = args.world.get_block_entity(args.position)?;
        if let Some(sensor_be) = be.as_any().downcast_ref::<SculkSensorBlockEntity>() {
            return Some(
                *sensor_be
                    .last_vibration_frequency
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) as u8,
            );
        }
        if let Some(cal_be) = be
            .as_any()
            .downcast_ref::<CalibratedSculkSensorBlockEntity>()
        {
            return Some(
                *cal_be
                    .last_vibration_frequency
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) as u8,
            );
        }
        None
    }

    fn on_scheduled_tick(&self, args: OnScheduledTickArgs<'_>) {
        let state = args.world.get_block_state(args.position);
        if args.block.id == BlockId::SCULK_SENSOR {
            let mut props = SculkSensorLikeProperties::from_state_id(state.id);
            match props.sculk_sensor_phase {
                SculkSensorPhase::Active => {
                    props.sculk_sensor_phase = SculkSensorPhase::Cooldown;
                    props.power = 0;
                    args.world.set_block_state(
                        args.position,
                        props.to_state_id(args.block),
                        BlockFlags::NOTIFY_ALL,
                    );
                    update_neighbors(args.world, args.position);
                    args.world.schedule_block_tick(
                        args.block,
                        *args.position,
                        10,
                        TickPriority::Normal,
                    );
                }
                SculkSensorPhase::Cooldown => {
                    props.sculk_sensor_phase = SculkSensorPhase::Inactive;
                    props.power = 0;
                    args.world.set_block_state(
                        args.position,
                        props.to_state_id(args.block),
                        BlockFlags::NOTIFY_ALL,
                    );
                    if !props.waterlogged {
                        args.world.play_block_sound(
                            Sound::BlockSculkSensorClickingStop,
                            SoundCategory::Blocks,
                            *args.position,
                        );
                    }
                }
                SculkSensorPhase::Inactive => {}
            }
        } else if args.block.id == BlockId::CALIBRATED_SCULK_SENSOR {
            let mut props = CalibratedSculkSensorLikeProperties::from_state_id(state.id);
            match props.sculk_sensor_phase {
                SculkSensorPhase::Active => {
                    props.sculk_sensor_phase = SculkSensorPhase::Cooldown;
                    props.power = 0;
                    args.world.set_block_state(
                        args.position,
                        props.to_state_id(args.block),
                        BlockFlags::NOTIFY_ALL,
                    );
                    update_neighbors(args.world, args.position);
                    args.world.schedule_block_tick(
                        args.block,
                        *args.position,
                        10,
                        TickPriority::Normal,
                    );
                }
                SculkSensorPhase::Cooldown => {
                    props.sculk_sensor_phase = SculkSensorPhase::Inactive;
                    props.power = 0;
                    args.world.set_block_state(
                        args.position,
                        props.to_state_id(args.block),
                        BlockFlags::NOTIFY_ALL,
                    );
                    if !props.waterlogged {
                        args.world.play_block_sound(
                            Sound::BlockSculkSensorClickingStop,
                            SoundCategory::Blocks,
                            *args.position,
                        );
                    }
                }
                SculkSensorPhase::Inactive => {}
            }
        }
    }

    fn on_state_replaced(&self, args: OnStateReplacedArgs<'_>) {
        if sculk_sensor_phase(args.block, args.old_state_id) == SculkSensorPhase::Active {
            update_neighbors(args.world, args.position);
        }
    }

    fn is_pathfindable(&self, _state: &BlockState, _computation_type: PathComputationType) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sculk_sensor_strong_power_only_directed_downwards() {
        let block = &Block::SCULK_SENSOR;
        let mut props = SculkSensorLikeProperties::default(block);
        props.sculk_sensor_phase = SculkSensorPhase::Active;
        props.power = 12;
        let state = BlockState::from_id(props.to_state_id(block));
        let pos = BlockPos::new(0, 64, 0);

        // SAFETY: World is never dereferenced by get_strong_redstone_power or get_weak_redstone_power.
        let dummy_world: &World = unsafe { std::ptr::NonNull::dangling().as_ref() };

        let sensor = SculkSensorBlock;
        for dir in BlockDirection::all() {
            let strong = sensor.get_strong_redstone_power(GetRedstonePowerArgs {
                world: dummy_world,
                block,
                state,
                position: &pos,
                direction: dir,
            });
            if dir == BlockDirection::Up {
                assert_eq!(
                    strong, 12,
                    "Sculk sensor must emit strong power downwards (querying with Up)"
                );
            } else {
                assert_eq!(
                    strong, 0,
                    "Sculk sensor must not emit strong power in other directions"
                );
            }
        }
    }

    #[test]
    fn calibrated_sculk_sensor_directional_signal_excludes_input_face() {
        let block = &Block::CALIBRATED_SCULK_SENSOR;
        let mut props = CalibratedSculkSensorLikeProperties::default(block);
        props.sculk_sensor_phase = SculkSensorPhase::Active;
        props.power = 15;
        props.facing = HorizontalFacing::North;
        let state = BlockState::from_id(props.to_state_id(block));
        let pos = BlockPos::new(0, 64, 0);

        // SAFETY: World is never dereferenced by get_strong_redstone_power or get_weak_redstone_power.
        let dummy_world: &World = unsafe { std::ptr::NonNull::dangling().as_ref() };

        let sensor = SculkSensorBlock;
        // North is the calibration input face, so querying towards North must yield 0
        let north_power = sensor.get_weak_redstone_power(GetRedstonePowerArgs {
            world: dummy_world,
            block,
            state,
            position: &pos,
            direction: BlockDirection::North,
        });
        assert_eq!(
            north_power, 0,
            "Calibrated sculk sensor must not emit power back towards its input face"
        );

        // Other faces should receive the power
        let south_power = sensor.get_weak_redstone_power(GetRedstonePowerArgs {
            world: dummy_world,
            block,
            state,
            position: &pos,
            direction: BlockDirection::South,
        });
        assert_eq!(south_power, 15);
    }
}
